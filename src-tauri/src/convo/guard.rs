//! The silent auto-approval guard (contract §6.1.11), the AllowSession
//! settings-file stat guard (§3.10 input 4) and the AllowSession mode-flip
//! guard [F-01].
//!
//! Owner: T05.
//!
//! - **Silent approval** — in `default` or `plan` mode, a `tool_use` of
//!   `Bash`/`Edit`/`Write`/`MultiEdit`/`NotebookEdit` followed by its
//!   `tool_result` with no `can_use_tool` and no `system/permission_denied`
//!   for that `tool_use_id` is surfaced (`approval_channel_unconfirmed`, a
//!   compat note, diagnostics). It never blocks and never changes compat (P13
//!   fallback: user allow rules and hooks legitimately skip the prompt). Tools
//!   Gilde itself allowed for the session (AllowSession rules) are not flagged.
//! - **Settings-file guard** — after an AllowSession answer, the three
//!   settings files are `stat`ed (never opened) at the answer and again for
//!   2 s; a create or modify turns the `allowSession` flag off for that
//!   version (`deviation`), PM-42.
//! - **Mode-flip guard** — a `permissionMode` change within 2 s after an
//!   AllowSession answer that the user did not request turns `allowSession`
//!   off for that version and records a note [F-01].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::contract::common::PermissionMode;

/// The window after an AllowSession answer (§3.10 input 4, F-01).
pub const GUARD_WINDOW_MS: u64 = 2_000;
/// How often the settings files are re-`stat`ed inside the window.
pub const STAT_EVERY_MS: u64 = 250;
/// The tools whose unprompted run is suspicious (§6.1.11).
pub const GUARDED_TOOLS: [&str; 5] = ["Bash", "Edit", "Write", "MultiEdit", "NotebookEdit"];
/// Tool uses remembered while their result is outstanding.
const MAX_OPEN_TOOL_USES: usize = 1_024;

/// Whether a mode asks before running guarded tools.
pub fn prompting_mode(mode: Option<PermissionMode>) -> bool {
    matches!(mode, None | Some(PermissionMode::Default) | Some(PermissionMode::Plan))
}

// ---------------------------------------------------------------------------
// silent approval
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SilentApprovalGuard {
    open: HashMap<String, String>,
    order: VecDeque<String>,
    prompted: HashSet<String>,
    /// `(tool, ruleContent)` of the rules Gilde forwarded with AllowSession.
    session_rules: Vec<(String, Option<String>)>,
    /// How often the guard fired in this process (diagnostics).
    pub fired: u64,
}

impl SilentApprovalGuard {
    pub fn new() -> SilentApprovalGuard {
        SilentApprovalGuard::default()
    }

    /// Whether a forwarded session rule covers this call: a rule without
    /// content covers the tool; a `Bash` rule covers its exact command, or the
    /// commands starting with its prefix when it ends in `:*`.
    fn covered(&self, tool: &str, key: Option<&str>) -> bool {
        self.session_rules.iter().any(|(t, content)| {
            t == tool
                && match (content.as_deref(), key) {
                    (None, _) => true,
                    (Some(c), Some(k)) if tool == "Bash" => match c.strip_suffix(":*") {
                        Some(prefix) => k.starts_with(prefix),
                        None => k == c,
                    },
                    _ => false,
                }
        })
    }

    /// A `tool_use` block was seen (main thread or subagent); `key` is the
    /// Bash command when there is one.
    pub fn on_tool_use(&mut self, tool_use_id: &str, tool: &str, key: Option<&str>, mode: Option<PermissionMode>) {
        if !GUARDED_TOOLS.contains(&tool) || !prompting_mode(mode) || self.covered(tool, key) {
            return;
        }
        if self.open.insert(tool_use_id.to_owned(), tool.to_owned()).is_none() {
            self.order.push_back(tool_use_id.to_owned());
        }
        while self.order.len() > MAX_OPEN_TOOL_USES {
            if let Some(old) = self.order.pop_front() {
                self.open.remove(&old);
                self.prompted.remove(&old);
            }
        }
    }

    /// A `can_use_tool` or `system/permission_denied` named this tool use.
    pub fn on_prompt(&mut self, tool_use_id: &str) {
        self.prompted.insert(tool_use_id.to_owned());
        if self.prompted.len() > MAX_OPEN_TOOL_USES * 2 {
            self.prompted.retain(|id| self.open.contains_key(id));
        }
    }

    /// Gilde forwarded an AllowSession rule: later unprompted runs it covers
    /// are expected.
    pub fn on_session_rule(&mut self, tool: &str, content: Option<&str>) {
        if self.session_rules.len() < 256 {
            self.session_rules.push((tool.to_owned(), content.map(str::to_owned)));
        }
    }

    /// The `tool_result` arrived: `Some(tool)` when no prompt was seen for it.
    pub fn on_result(&mut self, tool_use_id: &str) -> Option<String> {
        let tool = self.open.remove(tool_use_id)?;
        self.order.retain(|id| id != tool_use_id);
        if self.prompted.remove(tool_use_id) {
            return None;
        }
        self.fired += 1;
        Some(tool)
    }
}

// ---------------------------------------------------------------------------
// settings files (stat only)
// ---------------------------------------------------------------------------

/// What `stat` says about one file (never its content).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub exists: bool,
    pub len: u64,
    pub mtime_ns: i128,
    pub ino: u64,
}

/// `stat` one path (follows symlinks; a missing file is `exists: false`).
pub fn stamp(path: &Path) -> FileStamp {
    use std::os::unix::fs::MetadataExt as _;
    match std::fs::metadata(path) {
        Ok(m) => FileStamp {
            exists: true,
            len: m.len(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            ino: m.ino(),
        },
        Err(_) => FileStamp { exists: false, len: 0, mtime_ns: 0, ino: 0 },
    }
}

/// The files an AllowSession answer must never touch: `<project>/.claude/
/// settings.local.json`, `<project>/.claude/settings.json`, `<cfg>/settings.json`.
pub fn settings_paths(project: &Path, config_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![project.join(".claude").join("settings.local.json"), project.join(".claude").join("settings.json")];
    if let Some(cfg) = config_dir {
        v.push(cfg.join("settings.json"));
    }
    v
}

#[derive(Debug)]
pub struct SettingsFileGuard {
    paths: Vec<PathBuf>,
    armed: Option<Armed>,
}

#[derive(Debug)]
struct Armed {
    until: u64,
    next: u64,
    before: Vec<FileStamp>,
}

impl SettingsFileGuard {
    pub fn new(paths: Vec<PathBuf>) -> SettingsFileGuard {
        SettingsFileGuard { paths, armed: None }
    }

    /// An AllowSession answer was sent at `now`: take the before-stamps.
    pub fn arm(&mut self, now: u64) {
        if self.paths.is_empty() {
            return;
        }
        let before = match self.armed.take() {
            // A second answer inside the window keeps the first baseline.
            Some(a) => a.before,
            None => self.paths.iter().map(|p| stamp(p)).collect(),
        };
        self.armed = Some(Armed { until: now + GUARD_WINDOW_MS, next: now + STAT_EVERY_MS, before });
    }

    /// The next time [`SettingsFileGuard::check`] wants to run.
    pub fn deadline(&self) -> Option<u64> {
        self.armed.as_ref().map(|a| a.next.min(a.until))
    }

    /// Re-`stat` when due; `Some(path)` names a file that was created or
    /// modified inside the window (the guard then disarms).
    pub fn check(&mut self, now: u64) -> Option<PathBuf> {
        let a = self.armed.as_mut()?;
        if now < a.next && now < a.until {
            return None;
        }
        for (p, before) in self.paths.iter().zip(a.before.iter()) {
            let after = stamp(p);
            if after != *before && after.exists {
                self.armed = None;
                return Some(p.clone());
            }
        }
        if now >= a.until {
            self.armed = None;
        } else {
            a.next = now + STAT_EVERY_MS;
        }
        None
    }
}

// ---------------------------------------------------------------------------
// mode flip
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ModeFlipGuard {
    allow_session_at: Option<u64>,
    user_request_at: Option<u64>,
}

impl ModeFlipGuard {
    pub fn new() -> ModeFlipGuard {
        ModeFlipGuard::default()
    }

    pub fn on_allow_session(&mut self, now: u64) {
        self.allow_session_at = Some(now);
    }

    /// The user asked for a mode change (`session_set_permission_mode`, or
    /// ApprovePlan, which leaves plan mode by design).
    pub fn on_user_request(&mut self, now: u64) {
        self.user_request_at = Some(now);
    }

    /// The CLI reported a different mode at `now`: whether it is the F-01 flip.
    pub fn on_mode_change(&mut self, now: u64) -> bool {
        let Some(at) = self.allow_session_at else { return false };
        if now.saturating_sub(at) > GUARD_WINDOW_MS {
            return false;
        }
        let user_asked = self.user_request_at.is_some_and(|u| now.saturating_sub(u) <= GUARD_WINDOW_MS);
        if user_asked {
            return false;
        }
        self.allow_session_at = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unprompted_guarded_tools_fire_once() {
        let mut g = SilentApprovalGuard::new();
        g.on_tool_use("t1", "Bash", Some("rm x"), Some(PermissionMode::Default));
        g.on_tool_use("t2", "Bash", Some("ls"), Some(PermissionMode::Default));
        g.on_tool_use("t3", "Read", None, Some(PermissionMode::Default));
        g.on_tool_use("t4", "Edit", None, Some(PermissionMode::AcceptEdits));
        g.on_prompt("t2");
        assert_eq!(g.on_result("t1").as_deref(), Some("Bash"));
        assert_eq!(g.on_result("t1"), None, "once per tool use");
        assert_eq!(g.on_result("t2"), None, "it was asked");
        assert_eq!(g.on_result("t3"), None, "not a guarded tool");
        assert_eq!(g.on_result("t4"), None, "accept-edits mode legitimately skips the prompt");
        g.on_session_rule("Bash", Some("npm test:*"));
        g.on_session_rule("Bash", Some("ls"));
        g.on_tool_use("t5", "Bash", Some("npm test -- --watch"), Some(PermissionMode::Plan));
        assert_eq!(g.on_result("t5"), None, "a prefix rule Gilde forwarded covers it");
        g.on_tool_use("t6", "Bash", Some("ls"), Some(PermissionMode::Plan));
        assert_eq!(g.on_result("t6"), None, "an exact rule covers it");
        g.on_tool_use("t7", "Bash", Some("rm -rf /"), Some(PermissionMode::Plan));
        assert_eq!(g.on_result("t7").as_deref(), Some("Bash"), "a rule for another command does not");
        assert_eq!(g.fired, 2);
    }

    #[test]
    fn the_settings_guard_sees_a_create_inside_the_window_only() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let cfg = dir.path().join("cfg");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        let mut g = SettingsFileGuard::new(settings_paths(&project, Some(&cfg)));
        assert_eq!(g.deadline(), None);
        g.arm(1_000);
        assert_eq!(g.check(1_100), None, "not due yet");
        assert_eq!(g.check(1_250), None, "nothing changed");
        std::fs::write(project.join(".claude/settings.local.json"), b"{}").unwrap();
        assert_eq!(g.check(1_500), Some(project.join(".claude/settings.local.json")));
        assert_eq!(g.deadline(), None, "disarmed after firing");
        g.arm(5_000);
        assert_eq!(g.check(7_000), None, "a pre-existing unchanged file is fine");
        assert_eq!(g.deadline(), None, "the window closed");
        g.arm(10_000);
        std::fs::write(cfg.join("settings.json"), b"{\"x\":1}").unwrap();
        assert_eq!(g.check(12_000), Some(cfg.join("settings.json")), "the final check still looks");
    }

    #[test]
    fn a_mode_flip_after_allow_session_is_caught_unless_the_user_asked() {
        let mut g = ModeFlipGuard::new();
        assert!(!g.on_mode_change(100), "no AllowSession yet");
        g.on_allow_session(1_000);
        assert!(g.on_mode_change(1_500));
        assert!(!g.on_mode_change(1_600), "reported once");
        g.on_allow_session(5_000);
        assert!(!g.on_mode_change(7_500), "outside the window");
        g.on_allow_session(10_000);
        g.on_user_request(10_100);
        assert!(!g.on_mode_change(10_200), "the user asked for it");
    }
}
