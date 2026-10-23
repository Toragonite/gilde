//! ToolSummary, one table for all vendors (contract §3.7.3), plus the bounded
//! previews and the `FileChange` counts of tool calls.
//!
//! Owner: T04.
//!
//! | Tool (Claude names; codex/mesh equivalents) | `verb` | `object` |
//! |---|---|---|
//! | Read, NotebookRead | `read` | relative path |
//! | Edit, MultiEdit, NotebookEdit / Write, codex `FileChange` | `edit` / `create` | relative path (+ `FileChange`) |
//! | Bash, codex `CommandExecution` | `run` | first line of the command, ≤ 200 chars |
//! | Grep, Glob | `search` | pattern, ≤ 120 chars |
//! | WebFetch, WebSearch, SandboxNetworkAccess | `reach` | host only (WebSearch has none) |
//! | Task, Agent | `delegate` | agent name / description ≤ 120 |
//! | SendMessage | `message` | recipient name |
//! | TaskCreate / TaskUpdate | `task` | task id |
//! | AskUserQuestion, ExitPlanMode | `ask` | — |
//! | `mcp__<server>__<tool>` | `use` | `<server> · <tool>` (the server name is untrusted text) |
//! | anything else | `use` | tool name ≤ 64 |
//!
//! Paths are made relative to the session's location; a path outside it is
//! shown absolute with the home prefix replaced by `~`. Every string passes
//! `sanitize_display_text`.

use std::io::Write as _;

use serde_json::Value;

use crate::contract::content::{ChangeKind, FileChange, ToolSummary, ToolVerb};
use crate::contract::display::{Sanitized, TextCap};
use crate::render::sanitize::{display, sanitize_display_text};

/// `ToolSummary.tool` (§3.7.3 "tool name ≤ 64").
pub const TOOL_NAME_CAP: TextCap = TextCap::Chars(64);
/// A path in a summary or a `FileChange`.
pub const PATH_CAP: TextCap = TextCap::Chars(512);
/// The first line of a command (§3.7.3).
pub const COMMAND_CAP: TextCap = TextCap::Chars(200);
/// A search pattern, an agent name or description (§3.7.3).
pub const SHORT_CAP: TextCap = TextCap::Chars(120);
/// A host name.
pub const HOST_CAP: TextCap = TextCap::Chars(255);
/// `ToolCall.input_preview` (§3.7.1).
pub const INPUT_PREVIEW_CAP: TextCap = TextCap::Bytes(4 * 1024);
/// `ToolCall.result_preview` (§3.7.1).
pub const RESULT_PREVIEW_CAP: TextCap = TextCap::Bytes(8 * 1024);

/// Where paths are made relative (§3.7.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathBase<'a> {
    /// The session's location (a canonical absolute path, no trailing `/`).
    pub location: Option<&'a str>,
    /// The user's home directory (replaced by `~`).
    pub home: Option<&'a str>,
}

/// A path for display: relative to the location when inside it, `~/…` when
/// under home, otherwise as given. Relative inputs stay relative.
pub fn display_path(path: &str, base: PathBase<'_>) -> String {
    let rel = relative_path(path, base);
    display(&rel, PATH_CAP)
}

fn strip_dir<'p>(path: &'p str, dir: &str) -> Option<&'p str> {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return None;
    }
    let rest = path.strip_prefix(dir)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix('/')
    }
}

fn relative_path(path: &str, base: PathBase<'_>) -> String {
    if !path.starts_with('/') {
        return path.to_owned();
    }
    if let Some(loc) = base.location {
        if let Some(rest) = strip_dir(path, loc) {
            return if rest.is_empty() { ".".to_owned() } else { rest.to_owned() };
        }
    }
    if let Some(home) = base.home {
        if let Some(rest) = strip_dir(path, home) {
            return if rest.is_empty() { "~".to_owned() } else { format!("~/{rest}") };
        }
    }
    path.to_owned()
}

fn str_field<'v>(input: &'v Value, key: &str) -> Option<&'v str> {
    input.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The first non-empty line of a command, trimmed.
pub fn first_line(command: &str) -> &str {
    command.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// The host of an http(s) URL; userinfo, port and path are dropped.
pub fn url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?;
    Some(display(host, HOST_CAP))
}

fn summary(tool: &str, verb: ToolVerb, object: Option<String>) -> ToolSummary {
    ToolSummary { tool: display(tool, TOOL_NAME_CAP), verb, object: object.filter(|o| !o.is_empty()) }
}

/// Split `mcp__<server>__<tool>`.
pub fn split_mcp(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// The ToolSummary of a Claude tool call (`tool_use` block or `can_use_tool`
/// request) from its name and input.
pub fn claude_tool_summary(name: &str, input: &Value, base: PathBase<'_>) -> ToolSummary {
    let path = |keys: &[&str]| keys.iter().find_map(|k| str_field(input, k)).map(|p| display_path(p, base));
    match name {
        "Read" | "NotebookRead" => summary(name, ToolVerb::Read, path(&["file_path", "notebook_path", "path"])),
        "Edit" | "MultiEdit" | "NotebookEdit" => summary(name, ToolVerb::Edit, path(&["file_path", "notebook_path"])),
        "Write" => summary(name, ToolVerb::Create, path(&["file_path"])),
        "Bash" => summary(name, ToolVerb::Run, str_field(input, "command").map(|c| display(first_line(c), COMMAND_CAP))),
        "Grep" | "Glob" => summary(name, ToolVerb::Search, str_field(input, "pattern").map(|p| display(p, SHORT_CAP))),
        "WebFetch" => summary(name, ToolVerb::Reach, str_field(input, "url").and_then(url_host)),
        "WebSearch" => summary(name, ToolVerb::Reach, None),
        "SandboxNetworkAccess" => summary(name, ToolVerb::Reach, str_field(input, "host").map(|h| display(h, HOST_CAP))),
        "Task" | "Agent" => summary(
            name,
            ToolVerb::Delegate,
            ["name", "description", "subagent_type"].iter().find_map(|k| str_field(input, k)).map(|d| display(d, SHORT_CAP)),
        ),
        "SendMessage" => summary(
            name,
            ToolVerb::Message,
            ["to", "recipient"].iter().find_map(|k| str_field(input, k)).map(|t| display(t, TOOL_NAME_CAP)),
        ),
        "TaskCreate" | "TaskUpdate" => summary(name, ToolVerb::Task, task_id(input).map(|t| display(&t, TOOL_NAME_CAP))),
        "AskUserQuestion" | "ExitPlanMode" => summary(name, ToolVerb::Ask, None),
        _ => match split_mcp(name) {
            Some((server, tool)) => summary(
                name,
                ToolVerb::Use,
                Some(format!("{} · {}", display(server, TOOL_NAME_CAP), display(tool, TOOL_NAME_CAP))),
            ),
            None => summary(name, ToolVerb::Use, Some(display(name, TOOL_NAME_CAP))),
        },
    }
}

/// A task id from a TaskCreate/TaskUpdate input (string or number).
pub fn task_id(input: &Value) -> Option<String> {
    ["taskId", "task_id", "id"].iter().find_map(|k| match input.get(*k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// Lines of a string as the change counts use them: `""` → 0, `"a"` → 1,
/// `"a\n"` → 1, `"a\nb"` → 2.
pub fn line_count(s: &str) -> u32 {
    if s.is_empty() {
        0
    } else {
        u32::try_from(s.split_terminator('\n').count()).unwrap_or(u32::MAX)
    }
}

/// `+`/`−` counts of a `structuredPatch` (`[{lines: ["-a", "+b", " c"]}]`).
pub fn patch_counts(patch: &Value) -> Option<(u32, u32)> {
    let hunks = patch.as_array()?;
    let (mut added, mut removed) = (0u32, 0u32);
    for hunk in hunks {
        for line in hunk.get("lines").and_then(Value::as_array).into_iter().flatten() {
            match line.as_str().and_then(|l| l.chars().next()) {
                Some('+') => added = added.saturating_add(1),
                Some('-') => removed = removed.saturating_add(1),
                _ => {}
            }
        }
    }
    Some((added, removed))
}

/// The `FileChange` of a Claude edit/write tool call from its input alone
/// (line counts of old/new strings, §3.7.3).
pub fn claude_file_change(name: &str, input: &Value, base: PathBase<'_>) -> Option<FileChange> {
    let path = str_field(input, "file_path").or_else(|| str_field(input, "notebook_path"))?;
    let (added, removed, change) = match name {
        "Edit" => (
            line_count(str_field(input, "new_string").unwrap_or("")),
            line_count(str_field(input, "old_string").unwrap_or("")),
            ChangeKind::Modified,
        ),
        "MultiEdit" => {
            let (mut a, mut r) = (0u32, 0u32);
            for e in input.get("edits").and_then(Value::as_array).into_iter().flatten() {
                a = a.saturating_add(line_count(str_field(e, "new_string").unwrap_or("")));
                r = r.saturating_add(line_count(str_field(e, "old_string").unwrap_or("")));
            }
            (a, r, ChangeKind::Modified)
        }
        "Write" => (line_count(str_field(input, "content").unwrap_or("")), 0, ChangeKind::Added),
        "NotebookEdit" => {
            let new = line_count(str_field(input, "new_source").unwrap_or(""));
            match str_field(input, "edit_mode") {
                Some("delete") => (0, 0, ChangeKind::Modified),
                _ => (new, 0, ChangeKind::Modified),
            }
        }
        _ => return None,
    };
    Some(FileChange { path_rel: display_path(path, base), added, removed, change })
}

/// Refine a `FileChange` with the tool result (`toolUseResult`): the
/// `structuredPatch` counts win; a Write result says `create` or `update`.
pub fn refine_file_change(name: &str, input: &Value, result: &Value, base: PathBase<'_>) -> Option<FileChange> {
    let mut change = claude_file_change(name, input, base);
    if !result.is_object() {
        return change;
    }
    if change.is_none() {
        let path = str_field(result, "filePath")?;
        change = Some(FileChange { path_rel: display_path(path, base), added: 0, removed: 0, change: ChangeKind::Modified });
    }
    let fc = change.as_mut()?;
    if let Some((added, removed)) = result.get("structuredPatch").and_then(patch_counts) {
        if added > 0 || removed > 0 || result.get("type").and_then(Value::as_str) != Some("create") {
            fc.added = added;
            fc.removed = removed;
        }
    }
    match result.get("type").and_then(Value::as_str) {
        Some("create") => fc.change = ChangeKind::Added,
        Some("update") => fc.change = ChangeKind::Modified,
        _ => {}
    }
    change
}

/// The verb a Write ends up with once its result says whether it created or
/// updated the file.
pub fn refine_verb(name: &str, result: &Value) -> Option<ToolVerb> {
    match (name, result.get("type").and_then(Value::as_str)) {
        ("Write", Some("update")) => Some(ToolVerb::Edit),
        ("Write", Some("create")) => Some(ToolVerb::Create),
        _ => None,
    }
}

/// As [`refine_verb`] from an already-reduced result (`ResultMeta.write_kind`).
pub fn refine_verb_kind(name: &str, write_kind: Option<ChangeKind>) -> Option<ToolVerb> {
    match (name, write_kind) {
        ("Write", Some(ChangeKind::Modified)) => Some(ToolVerb::Edit),
        ("Write", Some(ChangeKind::Added)) => Some(ToolVerb::Create),
        _ => None,
    }
}

/// A writer that keeps at most `cap` bytes and then refuses more, which stops
/// `serde_json` early on huge values.
struct CapWriter {
    buf: Vec<u8>,
    cap: usize,
    over: bool,
}

impl std::io::Write for CapWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = self.cap.saturating_sub(self.buf.len());
        if data.len() > room {
            self.buf.extend_from_slice(&data[..room]);
            self.over = true;
            return Err(std::io::Error::other("cap"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Pretty JSON of `value`, bounded: serialization stops a little past the cap
/// and the result is cut at a grapheme boundary by the sanitizer.
pub fn bounded_json(value: &Value, cap: TextCap) -> Sanitized {
    let limit = match cap {
        TextCap::Bytes(n) => n,
        TextCap::Chars(n) => n.saturating_mul(4),
    };
    let mut w = CapWriter { buf: Vec::new(), cap: limit.saturating_add(64), over: false };
    let _ = serde_json::to_writer_pretty(&mut w, value);
    let _ = w.flush();
    let mut out = crate::render::sanitize::sanitize_display_bytes(&w.buf, cap);
    if w.over && !out.truncated {
        // The writer stopped inside the value: the preview is a cut.
        out = sanitize_display_text(&format!("{}{}", out.text, '\u{2026}'), cap);
        out.truncated = true;
    }
    out
}

/// The input preview of a tool call (≤ 4 KiB): the command for Bash, the
/// pretty JSON input otherwise.
pub fn input_preview(name: &str, input: &Value) -> Sanitized {
    match (name, str_field(input, "command")) {
        ("Bash", Some(cmd)) => sanitize_display_text(cmd, INPUT_PREVIEW_CAP),
        _ => match input {
            Value::Null => Sanitized::default(),
            Value::String(s) => sanitize_display_text(s, INPUT_PREVIEW_CAP),
            v => bounded_json(v, INPUT_PREVIEW_CAP),
        },
    }
}

/// The text of a tool result's `content` (a string, or an array of
/// `{type:"text", text}` / `{type:"image"}` items), bounded to 8 KiB.
pub fn result_preview(content: &Value) -> Sanitized {
    match content {
        Value::Null => Sanitized::default(),
        Value::String(s) => sanitize_display_text(s, RESULT_PREVIEW_CAP),
        Value::Array(items) => {
            let mut joined = String::new();
            let limit = 8 * 1024 + 256;
            for item in items {
                if joined.len() > limit {
                    break;
                }
                if !joined.is_empty() {
                    joined.push('\n');
                }
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => joined.push_str(item.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("image") => joined.push_str("[image]"),
                    Some(other) => {
                        joined.push('[');
                        joined.push_str(&display(other, TextCap::Chars(64)));
                        joined.push(']');
                    }
                    None => joined.push_str(item.as_str().unwrap_or("")),
                }
            }
            sanitize_display_text(&joined, RESULT_PREVIEW_CAP)
        }
        other => bounded_json(other, RESULT_PREVIEW_CAP),
    }
}

// ---------------------------------------------------------------------------
// codex and agent-mesh equivalents
// ---------------------------------------------------------------------------

/// The ToolSummary of a codex rollout item (`item_completed.item`).
pub fn codex_tool_summary(item_type: &str, item: &Value, base: PathBase<'_>) -> ToolSummary {
    match item_type {
        "CommandExecution" | "commandExecution" | "command_execution" => {
            let command = match item.get("command") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
                _ => String::new(),
            };
            summary("CommandExecution", ToolVerb::Run, Some(display(first_line(&command), COMMAND_CAP)))
        }
        "FileChange" | "fileChange" | "file_change" => {
            let first = codex_changes_in(item, base).into_iter().next();
            let verb = match first.as_ref().map(|c| c.change) {
                Some(ChangeKind::Added) => ToolVerb::Create,
                _ => ToolVerb::Edit,
            };
            summary("FileChange", verb, first.map(|c| c.path_rel))
        }
        "McpToolCall" | "mcpToolCall" | "mcp_tool_call" => {
            let server = str_field(item, "server").unwrap_or("");
            let tool = str_field(item, "tool").unwrap_or("");
            summary(
                "McpToolCall",
                ToolVerb::Use,
                Some(format!("{} · {}", display(server, TOOL_NAME_CAP), display(tool, TOOL_NAME_CAP))),
            )
        }
        "WebSearch" | "webSearch" | "web_search" => summary("WebSearch", ToolVerb::Reach, None),
        other => summary(other, ToolVerb::Use, Some(display(other, TOOL_NAME_CAP))),
    }
}

/// The file changes of a codex `FileChange` item: `changes` as a list of
/// `{path, kind}` or a map `path → {type|kind}`.
pub fn codex_changes(item: &Value) -> Vec<FileChange> {
    codex_changes_in(item, PathBase::default())
}

/// As [`codex_changes`] with paths made relative to `base`.
pub fn codex_changes_in(item: &Value, base: PathBase<'_>) -> Vec<FileChange> {
    fn kind_of(v: &Value) -> ChangeKind {
        let k = v
            .get("kind")
            .and_then(|k| k.as_str().or_else(|| k.get("type").and_then(Value::as_str)))
            .or_else(|| v.get("type").and_then(Value::as_str))
            .unwrap_or("");
        match k {
            "add" | "added" | "create" => ChangeKind::Added,
            "delete" | "deleted" | "remove" => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        }
    }
    fn counts(v: &Value) -> (u32, u32) {
        let diff = v.get("unified_diff").or_else(|| v.get("diff")).and_then(Value::as_str).unwrap_or("");
        let mut a = 0u32;
        let mut r = 0u32;
        for l in diff.lines() {
            if l.starts_with("+++") || l.starts_with("---") {
                continue;
            }
            if l.starts_with('+') {
                a = a.saturating_add(1);
            } else if l.starts_with('-') {
                r = r.saturating_add(1);
            }
        }
        if a == 0 && r == 0 {
            if let Some(content) = v.get("content").and_then(Value::as_str) {
                a = line_count(content);
            }
        }
        (a, r)
    }
    let mut out = Vec::new();
    match item.get("changes") {
        Some(Value::Array(list)) => {
            for c in list {
                if let Some(p) = str_field(c, "path") {
                    let (added, removed) = counts(c);
                    out.push(FileChange { path_rel: display_path(p, base), added, removed, change: kind_of(c) });
                }
            }
        }
        Some(Value::Object(map)) => {
            for (p, c) in map {
                let (added, removed) = counts(c);
                out.push(FileChange { path_rel: display_path(p, base), added, removed, change: kind_of(c) });
            }
        }
        _ => {}
    }
    out
}

/// The ToolSummary of an agent-mesh `Tool{name, detailPreview}` event (§6.3):
/// the Claude table when the name is a Claude tool, otherwise `use`.
pub fn mesh_tool_summary(name: &str, detail: Option<&str>) -> ToolSummary {
    let lower = name.to_ascii_lowercase();
    let detail_line = detail.map(first_line).filter(|d| !d.is_empty());
    match lower.as_str() {
        "bash" | "shell" | "command" | "commandexecution" | "command_execution" | "exec" | "run_shell_command" => {
            summary(name, ToolVerb::Run, detail_line.map(|d| display(d, COMMAND_CAP)))
        }
        "read" | "read_file" | "readfile" => summary(name, ToolVerb::Read, detail_line.map(|d| display(d, PATH_CAP))),
        "edit" | "write" | "filechange" | "file_change" | "apply_patch" | "edit_file" | "write_file" | "replace" => {
            summary(name, ToolVerb::Edit, detail_line.map(|d| display(d, PATH_CAP)))
        }
        "grep" | "glob" | "search" | "search_file_content" | "codebase_search" => {
            summary(name, ToolVerb::Search, detail_line.map(|d| display(d, SHORT_CAP)))
        }
        "webfetch" | "web_fetch" | "websearch" | "web_search" | "google_web_search" => {
            summary(name, ToolVerb::Reach, detail_line.and_then(url_host))
        }
        _ => match split_mcp(name) {
            Some((server, tool)) => summary(
                name,
                ToolVerb::Use,
                Some(format!("{} · {}", display(server, TOOL_NAME_CAP), display(tool, TOOL_NAME_CAP))),
            ),
            None => summary(name, ToolVerb::Use, Some(display(name, TOOL_NAME_CAP))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: PathBase<'static> = PathBase { location: Some("/w/proj"), home: Some("/Users/u") };

    #[test]
    fn paths_relative_home_and_absolute() {
        assert_eq!(display_path("/w/proj/src/a.rs", BASE), "src/a.rs");
        assert_eq!(display_path("/w/proj", BASE), ".");
        assert_eq!(display_path("/w/projx/a", BASE), "/w/projx/a");
        assert_eq!(display_path("/Users/u/notes.md", BASE), "~/notes.md");
        assert_eq!(display_path("/etc/hosts", BASE), "/etc/hosts");
        assert_eq!(display_path("rel/x", BASE), "rel/x");
    }

    #[test]
    fn the_table() {
        let s = |n: &str, i: Value| claude_tool_summary(n, &i, BASE);
        assert_eq!(s("Read", json!({"file_path": "/w/proj/a.rs"})).object.as_deref(), Some("a.rs"));
        assert_eq!(s("Write", json!({"file_path": "/w/proj/n.rs"})).verb, ToolVerb::Create);
        assert_eq!(s("Bash", json!({"command": "\n  cargo test\nmore"})).object.as_deref(), Some("cargo test"));
        assert_eq!(s("Grep", json!({"pattern": "fn main"})).verb, ToolVerb::Search);
        assert_eq!(s("WebFetch", json!({"url": "https://user:pw@example.com:8443/x?y"})).object.as_deref(), Some("example.com"));
        assert_eq!(s("WebSearch", json!({"query": "q"})).object, None);
        assert_eq!(s("SandboxNetworkAccess", json!({"host": "api.example.com"})).object.as_deref(), Some("api.example.com"));
        assert_eq!(s("Agent", json!({"description": "find bugs", "prompt": "…"})).verb, ToolVerb::Delegate);
        assert_eq!(s("SendMessage", json!({"to": "alpha"})).object.as_deref(), Some("alpha"));
        assert_eq!(s("TaskUpdate", json!({"taskId": "3"})).object.as_deref(), Some("3"));
        assert_eq!(s("AskUserQuestion", json!({})).verb, ToolVerb::Ask);
        let m = s("mcp__git\u{202e}hub__create_issue", json!({}));
        assert_eq!(m.verb, ToolVerb::Use);
        assert_eq!(m.object.as_deref(), Some("github · create_issue"));
        assert_eq!(s("Frobnicate", json!({})).object.as_deref(), Some("Frobnicate"));
    }

    #[test]
    fn file_changes() {
        let fc = claude_file_change("Edit", &json!({"file_path": "/w/proj/a", "old_string": "a\nb", "new_string": "c"}), BASE).unwrap();
        assert_eq!((fc.added, fc.removed, fc.change), (1, 2, ChangeKind::Modified));
        let result = json!({"type": "update", "structuredPatch": [{"lines": ["-x", "+y", "+z", " k"]}]});
        let fc = refine_file_change("Write", &json!({"file_path": "/w/proj/a", "content": "1\n2\n3\n"}), &result, BASE).unwrap();
        assert_eq!((fc.added, fc.removed, fc.change), (2, 1, ChangeKind::Modified));
        assert_eq!(refine_verb("Write", &result), Some(ToolVerb::Edit));
    }

    #[test]
    fn bounded_json_stops_early() {
        let big = json!({"content": "x".repeat(2_000_000)});
        let p = bounded_json(&big, INPUT_PREVIEW_CAP);
        assert!(p.truncated);
        assert!(p.text.len() <= 4096);
    }

    #[test]
    fn result_previews() {
        let p = result_preview(&json!([{"type": "text", "text": "\u{1b}[32mok\u{1b}[0m"}, {"type": "image"}]));
        assert_eq!(p.text, "ok\n[image]");
    }
}
