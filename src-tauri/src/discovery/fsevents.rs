//! FSEvents roots, latencies and drop handling (contract §5.9).
//!
//! One FSEvents stream (the `notify` crate's FSEvents backend, file-level
//! events) carries every watched root; [`route`] maps each event to the root
//! it belongs to (longest prefix), so a handler never re-derives it. Roots and
//! their coalescing windows are data ([`RootTag::latency_ms`]):
//!
//! | root | recursive | window |
//! |---|---|---|
//! | `<cfg>/sessions` | no | 300 ms |
//! | `<cfg>/teams` | yes | 300 ms |
//! | `<cfg>/projects` | yes | 500 ms |
//! | `<cfg>/tasks` | yes | 500 ms |
//! | `<cfg>/jobs` | yes | 1 s |
//! | codex `sessions` | yes | 500 ms |
//! | codex dir (`state_5.sqlite*`) | no | 2 s |
//! | `~/.cursor/chats` | yes | 1 s |
//! | agy dir | no | 2 s |
//!
//! `$HOME` is watched with kqueue instead (see `kqueue.rs`). **Never** watched:
//! a Claude config dir root recursively (25,700 `session-env` dirs), `<cfg>/ide`,
//! agy `log/`.
//!
//! `MustScanSubDirs`, `UserDropped`, `KernelDropped` (notify's `Rescan` flag)
//! and stream errors become [`FsSignal::Rescan`] → reconcile that root. Every
//! handler re-stats the file it is told about (out-of-order and coalesced
//! events are harmless, §13.4).
//!
//! Note: notify's FSEvents callback assumes UTF-8 paths. APFS and HFS+ only
//! store valid Unicode names, and every watched root is under `$HOME`, so
//! vendor roots on such volumes cannot produce a non-UTF-8 event path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{RecursiveMode, Watcher};

/// Which root an event belongs to. `u16` = the account index of the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RootTag {
    ClaudeSessions(u16),
    ClaudeTeams(u16),
    ClaudeProjects(u16),
    ClaudeTasks(u16),
    ClaudeJobs(u16),
    CodexSessions(u16),
    CodexIndex(u16),
    CursorChats,
    AgyDir,
    /// `$HOME` (kqueue) — siblings and the orchestrator presence.
    Home,
}

impl RootTag {
    /// The coalescing window (the per-path debounce is never under 250 ms).
    pub const fn latency_ms(self) -> u64 {
        match self {
            RootTag::ClaudeSessions(_) | RootTag::ClaudeTeams(_) => 300,
            RootTag::ClaudeProjects(_) | RootTag::ClaudeTasks(_) | RootTag::CodexSessions(_) => 500,
            RootTag::ClaudeJobs(_) | RootTag::CursorChats => 1_000,
            RootTag::CodexIndex(_) | RootTag::AgyDir | RootTag::Home => 2_000,
        }
    }

    pub const fn recursive(self) -> bool {
        matches!(
            self,
            RootTag::ClaudeTeams(_)
                | RootTag::ClaudeProjects(_)
                | RootTag::ClaudeTasks(_)
                | RootTag::ClaudeJobs(_)
                | RootTag::CodexSessions(_)
                | RootTag::CursorChats
        )
    }
}

/// One watched root (canonical path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchSpec {
    pub tag: RootTag,
    pub path: PathBuf,
}

/// What the stream reports to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsSignal {
    Changed { tag: RootTag, path: PathBuf },
    Rescan { tag: RootTag },
}

/// Map one notify event onto the watched roots.
pub fn route(specs: &[WatchSpec], ev: &notify::Result<notify::Event>) -> Vec<FsSignal> {
    let mut out = Vec::new();
    match ev {
        Err(_) => {
            for s in specs {
                out.push(FsSignal::Rescan { tag: s.tag });
            }
        }
        Ok(e) => {
            let rescan = e.need_rescan();
            if rescan && e.paths.is_empty() {
                for s in specs {
                    out.push(FsSignal::Rescan { tag: s.tag });
                }
                return out;
            }
            for p in &e.paths {
                let Some(spec) = owner_of(specs, p) else { continue };
                if rescan {
                    out.push(FsSignal::Rescan { tag: spec.tag });
                } else {
                    out.push(FsSignal::Changed { tag: spec.tag, path: p.clone() });
                }
            }
        }
    }
    out
}

/// The spec whose root is the longest prefix of `p` and which covers it
/// (non-recursive roots cover only their direct entries).
pub fn owner_of<'a>(specs: &'a [WatchSpec], p: &Path) -> Option<&'a WatchSpec> {
    specs
        .iter()
        .filter(|s| {
            if !p.starts_with(&s.path) {
                return false;
            }
            s.tag.recursive() || p == s.path || p.parent() == Some(s.path.as_path())
        })
        .max_by_key(|s| s.path.as_os_str().len())
}

/// The running stream. Dropping it stops the stream.
pub struct FsWatch {
    _watcher: Option<notify::FsEventWatcher>,
    pub watched: Vec<WatchSpec>,
    pub failed: Vec<WatchSpec>,
}

impl FsWatch {
    /// Start one stream over `specs` (roots that do not exist are skipped; the
    /// reconcile notices when they appear and the engine restarts the stream).
    pub fn start(specs: Vec<WatchSpec>, on: Arc<dyn Fn(FsSignal) + Send + Sync>) -> FsWatch {
        let live: Vec<WatchSpec> = specs.iter().filter(|s| s.path.is_dir()).cloned().collect();
        let routing = live.clone();
        let handler = move |ev: notify::Result<notify::Event>| {
            for sig in route(&routing, &ev) {
                on(sig);
            }
        };
        let mut watched = Vec::new();
        let mut failed: Vec<WatchSpec> = specs.iter().filter(|s| !s.path.is_dir()).cloned().collect();
        let watcher = match notify::FsEventWatcher::new(handler, notify::Config::default()) {
            Ok(mut w) => {
                for s in live {
                    let mode = if s.tag.recursive() { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
                    match w.watch(&s.path, mode) {
                        Ok(()) => watched.push(s),
                        Err(_) => failed.push(s),
                    }
                }
                Some(w)
            }
            Err(_) => {
                failed.extend(live);
                None
            }
        };
        FsWatch { _watcher: watcher, watched, failed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(tag: RootTag, p: &str) -> WatchSpec {
        WatchSpec { tag, path: PathBuf::from(p) }
    }

    #[test]
    fn longest_prefix_and_non_recursive_scope() {
        let specs = vec![
            spec(RootTag::ClaudeSessions(0), "/h/.claude/sessions"),
            spec(RootTag::ClaudeProjects(0), "/h/.claude/projects"),
            spec(RootTag::CodexIndex(0), "/h/.codex"),
            spec(RootTag::CodexSessions(0), "/h/.codex/sessions"),
            spec(RootTag::AgyDir, "/h/.gemini/antigravity-cli"),
        ];
        let tag = |p: &str| owner_of(&specs, Path::new(p)).map(|s| s.tag);
        assert_eq!(tag("/h/.claude/sessions/1.json"), Some(RootTag::ClaudeSessions(0)));
        assert_eq!(tag("/h/.claude/sessions/sub/1.json"), None, "non-recursive");
        assert_eq!(tag("/h/.claude/projects/-a/x.jsonl"), Some(RootTag::ClaudeProjects(0)));
        assert_eq!(tag("/h/.codex/state_5.sqlite-wal"), Some(RootTag::CodexIndex(0)));
        assert_eq!(tag("/h/.codex/sessions/2026/10/22/r.jsonl"), Some(RootTag::CodexSessions(0)));
        assert_eq!(tag("/h/.gemini/antigravity-cli/log/cli-1.log"), None, "agy log/ never watched");
        assert_eq!(tag("/h/.claude/session-env/x"), None, "never the config root");
    }

    #[test]
    fn drops_and_errors_become_rescans() {
        let specs = vec![spec(RootTag::ClaudeTeams(1), "/h/.claude/teams")];
        let ev = notify::Event::new(notify::EventKind::Other)
            .set_flag(notify::event::Flag::Rescan)
            .add_path(PathBuf::from("/h/.claude/teams/x"));
        assert_eq!(route(&specs, &Ok(ev)), vec![FsSignal::Rescan { tag: RootTag::ClaudeTeams(1) }]);
        let err: notify::Result<notify::Event> = Err(notify::Error::generic("x"));
        assert_eq!(route(&specs, &err), vec![FsSignal::Rescan { tag: RootTag::ClaudeTeams(1) }]);
    }
}
