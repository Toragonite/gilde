//! Transcript head/tail metadata (contract §5.2.5) — never a whole-file read.
//!
//! * Located by glob `<cfg>/projects/*/<sessionId>.jsonl` (never by building
//!   the slug: lossy, and `-w` workers write under the worktree slug).
//! * **Head**: 8 KiB steps up to 64 KiB, whole lines only, until `sessionId`,
//!   `cwd`, `gitBranch`, `entrypoint`, `version` (+ `agentName`/`teamName` for
//!   teammates) and the first `type:"user"` row with string content are found;
//!   a first prompt beyond 64 KiB is "unavailable".
//! * **Tail**: backwards in 256 KiB chunks up to 4 MiB for the last
//!   timestamped row, the last `ai-title`, `agent-name` (the user-set name),
//!   `custom-title`, `continued-in`, `permission-mode`, and the S3 facts of the
//!   current turn (open `tool_use`, turn end, human prompt, api error).
//! * Last activity = the last row that carries `timestamp`, never mtime.
//!
//! Titles, prompts and replies are **[content]**: they are returned to the
//! caller (memory only) and never logged. Rows longer than the scan window are
//! skipped (counted), rows that do not parse are skipped (counted).

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::contract::activity::{ErrorSub, OpenToolUse, TranscriptFacts};
use crate::contract::common::{EpochMs, PermissionMode};
use crate::discovery::is_vendor_uuid;
use crate::discovery::safe_read::{self, EntryKind, Opened, ReadError, Root};

pub const HEAD_STEP: usize = 8 * 1024;
pub const HEAD_CAP: usize = 64 * 1024;
pub const TAIL_CHUNK: usize = 256 * 1024;
pub const TAIL_CAP: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// time
// ---------------------------------------------------------------------------

/// RFC 3339 / ISO 8601 (`2026-10-21T04:53:14.861Z`, `…+09:00`) → UTC ms.
pub fn parse_iso8601_ms(s: &str) -> Option<EpochMs> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        if t.bytes().all(|c| c.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut i = 19;
    let mut ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        if frac.is_empty() {
            return None;
        }
        let mut digits: String = frac.chars().take(3).collect();
        while digits.len() < 3 {
            digits.push('0');
        }
        ms = digits.parse().ok()?;
    }
    let offset_secs = match b.get(i) {
        Some(b'Z') | Some(b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
            let oh = n(i + 1..i + 3)?;
            let om = n(i + 4..i + 6)?;
            let v = oh * 3600 + om * 60;
            if *sign == b'+' {
                v
            } else {
                -v
            }
        }
        _ => return None,
    };
    let days = super::registry::days_from_civil(y, mo as u32, d as u32);
    Some((days * 86_400 + h * 3600 + mi * 60 + se - offset_secs) * 1000 + ms)
}

/// A timestamp field: ISO string or epoch ms number.
fn ts_of(v: Option<&TsField<'_>>) -> Option<EpochMs> {
    match v? {
        TsField::Text(s) => parse_iso8601_ms(s),
        TsField::Num(n) => Some(*n).filter(|n| *n > 0),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TsField<'a> {
    #[serde(borrow)]
    Text(Cow<'a, str>),
    Num(i64),
}

// ---------------------------------------------------------------------------
// rows
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    subtype: Option<Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<TsField<'a>>,
    #[serde(borrow)]
    session_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    cwd: Option<Cow<'a, str>>,
    #[serde(borrow)]
    git_branch: Option<Cow<'a, str>>,
    #[serde(borrow)]
    entrypoint: Option<Cow<'a, str>>,
    #[serde(borrow)]
    version: Option<Cow<'a, str>>,
    #[serde(borrow)]
    agent_name: Option<Cow<'a, str>>,
    #[serde(borrow)]
    team_name: Option<Cow<'a, str>>,
    #[serde(borrow)]
    ai_title: Option<Cow<'a, str>>,
    #[serde(borrow)]
    custom_title: Option<Cow<'a, str>>,
    #[serde(borrow)]
    continued_in_session_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    permission_mode: Option<Cow<'a, str>>,
    is_meta: Option<bool>,
    is_sidechain: Option<bool>,
    #[serde(borrow)]
    message: Option<&'a RawValue>,
    #[serde(borrow)]
    error: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Msg<'a> {
    #[serde(borrow)]
    content: Option<&'a RawValue>,
    #[serde(borrow)]
    stop_reason: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow)]
    tool_use_id: Option<Cow<'a, str>>,
}

enum Content<'a> {
    Text(Cow<'a, str>),
    Blocks(Vec<Block<'a>>),
    None,
}

fn content_of<'a>(msg: &'a RawValue) -> (Content<'a>, Option<Cow<'a, str>>) {
    let Ok(m) = serde_json::from_str::<Msg<'a>>(msg.get()) else { return (Content::None, None) };
    let content = match m.content {
        None => Content::None,
        Some(raw) => {
            let t = raw.get().trim_start();
            if t.starts_with('"') {
                serde_json::from_str::<Cow<'a, str>>(raw.get()).map(Content::Text).unwrap_or(Content::None)
            } else if t.starts_with('[') {
                serde_json::from_str::<Vec<Block<'a>>>(raw.get()).map(Content::Blocks).unwrap_or(Content::None)
            } else {
                Content::None
            }
        }
    };
    (content, m.stop_reason)
}

const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// What a `user` row is.
enum UserKind {
    /// A prompt typed by the human (string content or text blocks).
    Prompt,
    /// The interrupt marker row: a `TurnEnd{Interrupted}` (§3.5.3).
    Interrupt,
    /// Tool results (ids).
    ToolResults(Vec<String>),
    /// A teammate/peer message or anything else.
    Other,
}

fn user_kind(row: &Row<'_>) -> (UserKind, Option<String>) {
    if row.is_meta == Some(true) || row.is_sidechain == Some(true) {
        return (UserKind::Other, None);
    }
    let Some(msg) = row.message else { return (UserKind::Other, None) };
    match content_of(msg).0 {
        Content::Text(t) => classify_user_text(&t),
        Content::Blocks(blocks) => {
            let results: Vec<String> = blocks
                .iter()
                .filter(|b| b.kind.as_deref() == Some("tool_result"))
                .filter_map(|b| b.tool_use_id.as_ref().map(|s| s.to_string()))
                .collect();
            if !results.is_empty() {
                return (UserKind::ToolResults(results), None);
            }
            let text = blocks.iter().find(|b| b.kind.as_deref() == Some("text")).and_then(|b| b.text.as_ref());
            match text {
                Some(t) => {
                    let (k, _) = classify_user_text(t);
                    (k, None) // only string content counts as the first prompt [R2 Q2]
                }
                None => (UserKind::Other, None),
            }
        }
        Content::None => (UserKind::Other, None),
    }
}

fn classify_user_text(t: &str) -> (UserKind, Option<String>) {
    let trimmed = t.trim_start();
    if trimmed.starts_with(INTERRUPT_MARKER) {
        (UserKind::Interrupt, None)
    } else if trimmed.starts_with("<teammate-message") || trimmed.starts_with("<agent-message") || trimmed.starts_with("<cross-session-message") {
        (UserKind::Other, None)
    } else {
        (UserKind::Prompt, Some(t.to_string()))
    }
}

/// `system/api_error` → an `ErrorSub` by the error's type codes.
fn api_error_sub(row: &Row<'_>) -> ErrorSub {
    let text = row.error.map(|e| e.get().to_ascii_lowercase()).unwrap_or_default();
    if text.contains("authentication") || text.contains("not logged in") || text.contains("401") {
        ErrorSub::SignInNeeded
    } else if text.contains("usage limit") || text.contains("rate_limit") || text.contains("429") {
        ErrorSub::UsageLimit { account_id: None, resets_at: None }
    } else {
        ErrorSub::AgentError
    }
}

fn mode_of(s: &str) -> Option<PermissionMode> {
    Some(match s {
        "default" => PermissionMode::Default,
        "acceptEdits" => PermissionMode::AcceptEdits,
        "plan" => PermissionMode::Plan,
        "dontAsk" => PermissionMode::DontAsk,
        "auto" => PermissionMode::Auto,
        "bypassPermissions" => PermissionMode::BypassPermissions,
        _ => return None,
    })
}

fn opt(s: &Option<Cow<'_, str>>, max: usize) -> Option<String> {
    s.as_ref().filter(|v| !v.is_empty() && v.len() <= max).map(|v| v.to_string())
}

// ---------------------------------------------------------------------------
// head
// ---------------------------------------------------------------------------

/// The first prompt (**[content]**).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FirstPrompt {
    Found(String),
    /// Not within the first 64 KiB.
    Unavailable,
    /// The whole file was read and holds no user string row.
    #[default]
    None,
}

/// Head metadata (identity facts are **[meta]**; `first_prompt` is content).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadMeta {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub version: Option<String>,
    /// Teammate rows carry `agentName`/`teamName` (metadata, not the user-set name).
    pub agent_name: Option<String>,
    pub team_name: Option<String>,
    pub first_prompt: FirstPrompt,
    pub first_row_at: Option<EpochMs>,
    /// The first `permission-mode` row (the tail's latest one wins when seen).
    pub permission_mode: Option<PermissionMode>,
}

impl HeadMeta {
    fn identity_done(&self) -> bool {
        self.session_id.is_some() && self.cwd.is_some() && self.version.is_some() && self.entrypoint.is_some()
    }
}

/// Scan the head of a transcript (≤ 64 KiB, whole lines only).
pub fn scan_head(f: &Opened) -> Result<HeadMeta, ReadError> {
    let mut meta = HeadMeta::default();
    let mut carry: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    let mut saw_content_row = false;
    let mut eof = false;
    while (offset as usize) < HEAD_CAP {
        let want = HEAD_STEP.min(HEAD_CAP - offset as usize);
        let chunk = f.read_range(offset, want)?;
        if chunk.is_empty() {
            eof = true;
            break;
        }
        offset += chunk.len() as u64;
        carry.extend_from_slice(&chunk);
        let mut start = 0usize;
        while let Some(nl) = memchr::memchr(b'\n', &carry[start..]) {
            let line = &carry[start..start + nl];
            start += nl + 1;
            head_line(line, &mut meta, &mut saw_content_row);
        }
        carry.drain(..start);
        let teammate_done = !saw_content_row || meta.agent_name.is_some() || meta.team_name.is_none();
        if meta.identity_done() && !matches!(meta.first_prompt, FirstPrompt::None) && teammate_done {
            break;
        }
        if (chunk.len()) < want {
            eof = true;
            break;
        }
    }
    if matches!(meta.first_prompt, FirstPrompt::None) && !eof {
        meta.first_prompt = FirstPrompt::Unavailable;
    }
    zeroize::Zeroize::zeroize(&mut carry);
    Ok(meta)
}

fn head_line(line: &[u8], meta: &mut HeadMeta, saw_content: &mut bool) {
    let Ok(row) = serde_json::from_slice::<Row<'_>>(line) else { return };
    if meta.first_row_at.is_none() {
        meta.first_row_at = ts_of(row.timestamp.as_ref());
    }
    let kind = row.kind.as_deref().unwrap_or("");
    if matches!(kind, "user" | "assistant") {
        *saw_content = true;
    }
    if meta.session_id.is_none() {
        meta.session_id = row.session_id.as_deref().filter(|s| is_vendor_uuid(s)).map(str::to_string);
    }
    meta.cwd = meta.cwd.take().or_else(|| opt(&row.cwd, 4096));
    meta.git_branch = meta.git_branch.take().or_else(|| opt(&row.git_branch, 255));
    meta.entrypoint = meta.entrypoint.take().or_else(|| opt(&row.entrypoint, 64));
    meta.version = meta.version.take().or_else(|| opt(&row.version, 32));
    if kind != "agent-name" {
        meta.agent_name = meta.agent_name.take().or_else(|| opt(&row.agent_name, 64));
    }
    meta.team_name = meta.team_name.take().or_else(|| opt(&row.team_name, 128));
    if kind == "permission-mode" && meta.permission_mode.is_none() {
        meta.permission_mode = row.permission_mode.as_deref().and_then(mode_of);
    }
    if kind == "user" && matches!(meta.first_prompt, FirstPrompt::None) {
        if let Some(msg) = row.message {
            if let Content::Text(t) = content_of(msg).0 {
                if let (UserKind::Prompt, Some(p)) = classify_user_text(&t) {
                    if row.is_meta != Some(true) {
                        meta.first_prompt = FirstPrompt::Found(p);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tail
// ---------------------------------------------------------------------------

/// Why the tail is scanned: live sessions need the S3 window back to the
/// current turn's human prompt; history rows only need the metadata tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailMode {
    Live,
    History,
    /// The on-demand History preview: never stops early (still ≤ 4 MiB), so
    /// `turns` is known for every file that fits the cap.
    Full,
}

/// Tail metadata. Titles, replies and the user-set name are **[content]**.
#[derive(Debug, Clone, PartialEq)]
pub struct TailMeta {
    pub last_row_at: Option<EpochMs>,
    pub ai_title: Option<String>,
    /// The last `agent-name` row: the user's `/rename` value [R5 §11 Q12].
    pub user_name: Option<String>,
    pub custom_title: Option<String>,
    pub continued_in: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    /// The last assistant text (History preview).
    pub last_reply: Option<String>,
    pub facts: TranscriptFacts,
    /// Human prompts, only when the whole file was scanned.
    pub turns: Option<u32>,
    pub whole_file: bool,
    pub rows_too_large: u32,
    pub rows_bad: u32,
}

/// Empty S3 facts.
pub fn empty_facts() -> TranscriptFacts {
    TranscriptFacts {
        last_row_at: None,
        open_tool_use: None,
        last_turn_end_at: None,
        last_human_prompt_at: None,
        last_api_error: None,
    }
}

impl Default for TailMeta {
    fn default() -> Self {
        TailMeta {
            last_row_at: None,
            ai_title: None,
            user_name: None,
            custom_title: None,
            continued_in: None,
            permission_mode: None,
            last_reply: None,
            facts: empty_facts(),
            turns: None,
            whole_file: false,
            rows_too_large: 0,
            rows_bad: 0,
        }
    }
}

struct TailState {
    mode: TailMode,
    meta: TailMeta,
    tool_results_after: HashSet<String>,
    assistant_after: bool,
    boundaries: u32,
    prompt_found: bool,
    turn_end_found: bool,
    prompts: u32,
}

impl TailState {
    fn done(&self) -> bool {
        let m = &self.meta;
        let titles = m.ai_title.is_some() && m.custom_title.is_some() && m.user_name.is_some();
        let meta_done = m.last_row_at.is_some() && (titles || self.boundaries >= 2);
        match self.mode {
            TailMode::History => meta_done,
            TailMode::Live => meta_done && self.prompt_found,
            TailMode::Full => false,
        }
    }

    /// One row, newest first.
    fn row(&mut self, line: &[u8]) {
        let Ok(row) = serde_json::from_slice::<Row<'_>>(line) else {
            self.meta.rows_bad += 1;
            return;
        };
        let at = ts_of(row.timestamp.as_ref());
        if self.meta.last_row_at.is_none() {
            self.meta.last_row_at = at;
        }
        let kind = row.kind.as_deref().unwrap_or("");
        match kind {
            "ai-title" if self.meta.ai_title.is_none() => self.meta.ai_title = opt(&row.ai_title, 1024),
            "custom-title" if self.meta.custom_title.is_none() => self.meta.custom_title = opt(&row.custom_title, 1024),
            "agent-name" if self.meta.user_name.is_none() => self.meta.user_name = opt(&row.agent_name, 512),
            "continued-in" if self.meta.continued_in.is_none() => {
                self.meta.continued_in = row
                    .continued_in_session_id
                    .as_deref()
                    .filter(|s| is_vendor_uuid(s))
                    .map(str::to_string);
            }
            "permission-mode" if self.meta.permission_mode.is_none() => {
                self.meta.permission_mode = row.permission_mode.as_deref().and_then(mode_of);
            }
            "system" => match row.subtype.as_deref() {
                Some("turn_duration") | Some("stop_hook_summary") => self.turn_end(at),
                Some("api_error") if !self.prompt_found && !self.assistant_after && self.meta.facts.last_api_error.is_none() => {
                    if let Some(at) = at {
                        self.meta.facts.last_api_error = Some((at, api_error_sub(&row)));
                    }
                }
                _ => {}
            },
            "assistant" if row.is_sidechain != Some(true) => self.assistant(&row, at),
            "user" => {
                let (k, _) = user_kind(&row);
                match k {
                    UserKind::Prompt => {
                        self.prompts += 1;
                        self.boundaries += 1;
                        if !self.prompt_found {
                            self.prompt_found = true;
                            self.meta.facts.last_human_prompt_at = at;
                        }
                    }
                    UserKind::Interrupt => self.turn_end(at),
                    UserKind::ToolResults(ids) => {
                        if !self.prompt_found {
                            self.tool_results_after.extend(ids);
                        }
                    }
                    UserKind::Other => {}
                }
            }
            _ => {}
        }
    }

    fn turn_end(&mut self, at: Option<EpochMs>) {
        self.boundaries += 1;
        if !self.turn_end_found && !self.prompt_found {
            self.turn_end_found = true;
            self.meta.facts.last_turn_end_at = at;
        }
    }

    fn assistant(&mut self, row: &Row<'_>, at: Option<EpochMs>) {
        let Some(msg) = row.message else { return };
        let (content, stop) = content_of(msg);
        if let Content::Blocks(blocks) = &content {
            if self.meta.last_reply.is_none() {
                if let Some(t) = blocks.iter().find(|b| b.kind.as_deref() == Some("text")).and_then(|b| b.text.as_ref()) {
                    self.meta.last_reply = Some(t.chars().take(2000).collect());
                }
            }
            if !self.prompt_found && !self.turn_end_found && self.meta.facts.open_tool_use.is_none() {
                for b in blocks {
                    if b.kind.as_deref() != Some("tool_use") {
                        continue;
                    }
                    let (Some(id), Some(name)) = (b.id.as_deref(), b.name.as_deref()) else { continue };
                    if self.tool_results_after.contains(id) || id.len() > 128 || name.len() > 128 {
                        continue;
                    }
                    if let Some(at) = at {
                        self.meta.facts.open_tool_use = Some(OpenToolUse {
                            tool_use_id: id.to_string(),
                            tool_name: name.to_string(),
                            at,
                            is_question: matches!(name, "AskUserQuestion" | "ExitPlanMode"),
                        });
                    }
                    break;
                }
            }
        }
        if stop.as_deref() == Some("end_turn") {
            self.turn_end(at);
        }
        self.assistant_after = true;
    }
}

/// Scan the tail backwards (256 KiB chunks, ≤ 4 MiB, complete lines only).
pub fn scan_tail(f: &Opened, mode: TailMode) -> Result<TailMeta, ReadError> {
    let size = f.id.size;
    let mut st = TailState {
        mode,
        meta: TailMeta::default(),
        tool_results_after: HashSet::new(),
        assistant_after: false,
        boundaries: 0,
        prompt_found: false,
        turn_end_found: false,
        prompts: 0,
    };
    // `pending` = the file bytes [pos, pos + pending.len()) not yet consumed.
    let mut pos = size;
    let mut pending: Vec<u8> = Vec::new();
    let mut read_total = 0usize;
    // A trailing partial row (no '\n' yet) is never parsed.
    let mut tail_trimmed = false;
    let mut reached_start = false;
    'outer: while pos > 0 && read_total < TAIL_CAP {
        let take = (TAIL_CHUNK as u64).min(pos).min((TAIL_CAP - read_total) as u64) as usize;
        let start = pos - take as u64;
        let chunk = f.read_range(start, take)?;
        read_total += take;
        let mut joined = Vec::with_capacity(chunk.len() + pending.len());
        joined.extend_from_slice(&chunk);
        joined.extend_from_slice(&pending);
        zeroize::Zeroize::zeroize(&mut pending);
        pending = joined;
        pos = start;
        if !tail_trimmed {
            match memchr::memrchr(b'\n', &pending) {
                Some(nl) => {
                    pending.truncate(nl + 1);
                    tail_trimmed = true;
                }
                None => {
                    if pos == 0 {
                        pending.clear();
                    }
                    continue;
                }
            }
        }
        // `pending` ends with '\n': consume complete rows from the end.
        while !pending.is_empty() {
            let body_end = pending.len() - 1;
            match memchr::memrchr(b'\n', &pending[..body_end]) {
                Some(prev) => {
                    if prev + 1 < body_end {
                        st.row(&pending[prev + 1..body_end]);
                    }
                    pending.truncate(prev + 1);
                    if st.done() {
                        break 'outer;
                    }
                }
                None => {
                    if pos == 0 {
                        if body_end > 0 {
                            st.row(&pending[..body_end]);
                        }
                        pending.clear();
                    }
                    break; // a row that starts before `pos`: read further back
                }
            }
        }
    }
    if pos == 0 && pending.is_empty() {
        reached_start = true;
    }
    if !reached_start && !st.done() && read_total >= TAIL_CAP && !pending.is_empty() {
        st.meta.rows_too_large += 1;
    }
    zeroize::Zeroize::zeroize(&mut pending);
    st.meta.whole_file = reached_start;
    if reached_start {
        st.meta.turns = Some(st.prompts);
    }
    Ok(st.meta)
}

// ---------------------------------------------------------------------------
// location
// ---------------------------------------------------------------------------

/// `<projects>/*/<sessionId>.jsonl` — never by building the slug [R2 C3].
pub fn find_transcript(projects: &Root, session_id: &str) -> Option<PathBuf> {
    find_transcript_checked(projects, session_id).0
}

/// [`find_transcript`], also reporting whether `projects/` or one of its
/// project dirs could not be searched (`EACCES`/`EPERM`, §5.8 / A33.4): a
/// transcript that was not found may then still exist.
pub fn find_transcript_checked(projects: &Root, session_id: &str) -> (Option<PathBuf>, bool) {
    if !is_vendor_uuid(session_id) {
        return (None, false);
    }
    let file = format!("{session_id}.jsonl");
    let listing = match safe_read::list_dir(projects, projects.path(), safe_read::DIR_ENTRIES_CAP) {
        Ok(l) => l,
        Err(ReadError::PermissionDenied) => return (None, true),
        Err(_) => return (None, false),
    };
    let mut denied = false;
    let mut best: Option<(PathBuf, i64)> = None;
    for e in listing.entries.iter().filter(|e| e.kind == EntryKind::Dir) {
        let p = projects.join(&e.name).join(&file);
        match safe_read::lstat(&p) {
            Ok(st) => {
                if st.kind == EntryKind::File && best.as_ref().is_none_or(|(_, m)| st.id.mtime_ms > *m) {
                    best = Some((p, st.id.mtime_ms));
                }
            }
            Err(ReadError::PermissionDenied) => denied = true,
            Err(_) => {}
        }
    }
    (best.map(|(p, _)| p), denied)
}

/// The session id of a top-level transcript path (`…/<uuid>.jsonl`).
pub fn session_id_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl")?;
    is_vendor_uuid(stem).then(|| stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_iso8601_ms("1970-01-01T09:00:00+09:00"), Some(0));
        assert_eq!(parse_iso8601_ms("2026-10-21T04:53:14.861Z"), Some((super::super::registry::days_from_civil(2026, 10, 21) * 86_400 + 4 * 3600 + 53 * 60 + 14) * 1000 + 861));
        for bad in ["", "2026-10-21", "2026-13-01T00:00:00Z", "2026-10-21T04:53:14", "2026-10-21T04:53:14.Z", "x026-10-21T04:53:14Z"] {
            assert_eq!(parse_iso8601_ms(bad), None, "{bad}");
        }
    }
}
