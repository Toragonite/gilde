//! Provider path alias layer (DESIGN §7-6).
//!
//! Coding agents have opinions about where a worktree lives: Claude Code's `-w`
//! flag puts one under `<repo>/.claude/worktrees/<name>`, Cursor's `/worktree`
//! has its own convention. **We do not use `claude -w`.** We own the worktree
//! (it lives in the central `~/.gilde/worktrees` tree, DESIGN §7-1)
//! and expose a *presentational* symlink where each provider — and the user's
//! muscle memory — expects to find it.
//!
//! **Gilde 2:** symlink creation is off by default — `GitApi::create_worktree`
//! never asks for an alias; only an explicit `alias_providers` request on the
//! kept v1 manager does. What stays is detect/adopt of provider worktrees
//! (to map `claude -w` sessions to their location, §2.2.1).
//!
//! Nothing on a hot path may depend on an alias existing. If a provider changes
//! its convention the failure mode is "no symlink, plus a hint in the UI", never
//! "broken session".
//!
//! The rules below are not stylistic. Each one closes a concrete way this layer
//! could destroy something that is not ours:
//!
//! 1. **Create only where nothing exists.** A real directory, a file, or a
//!    symlink pointing somewhere else is left strictly alone — the thing sitting
//!    there may be a genuine worktree the provider made.
//! 2. **The target is always the canonical central path.**
//! 3. **Git never goes through an alias.** `git worktree list` reports real
//!    paths; driving git through a symlink makes `remove`/`prune` unpredictable.
//! 4. **Ignore hygiene is written to `<git-common-dir>/info/exclude` only —
//!    never to a tracked `.gitignore`.** Appending `/.claude/worktrees/` to a
//!    tracked file shows up as an uncommitted change in the user's repository;
//!    an app that dirties your repo to manage its worktrees is worse than no
//!    app. `tests/alias_safety.rs` greps this source to keep it that way.
//! 5. **Teardown verifies before it deletes**: `read_link(alias) == target` must
//!    hold, and a row must exist in `worktree_alias`. Anything else is reported,
//!    not removed. (The same hard invariant claude-mesh's `MeshBridge` keeps:
//!    only ever delete a file you made.)
//! 6. **Adopt rather than collide.** A real worktree already sitting at the
//!    convention path is reported as [`AliasOutcome::Adopted`] and offered for
//!    registration with `worktree.origin = 'adopted'`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::config::model::AliasProviderKey;
use crate::error::{GildeError, Result};
use crate::paths::AppPaths;
use crate::store::models::{AliasKind, FromRow, WorktreeAliasRow, WorktreeOrigin};
use crate::store::{dao, now_ms, Db};
use crate::worktree::git::{self, BoxFuture};
use crate::worktree::{AliasHook, Worktree};

// ---------------------------------------------------------------------------
// exclude block
// ---------------------------------------------------------------------------

/// Opening marker of the managed block in `info/exclude`.
pub const EXCLUDE_BEGIN: &str = "# >>> gilde-code (managed) >>>";
/// Closing marker of the managed block in `info/exclude`.
pub const EXCLUDE_END: &str = "# <<< gilde-code (managed) <<<";

/// The patterns the managed block contains, in order.
///
/// Only the `worktrees/` subdirectory of each provider directory is excluded —
/// `.claude/` itself commonly holds files the user tracks on purpose.
pub const EXCLUDE_ENTRIES: &[&str] = &[
    "/.claude/worktrees/",
    "/.cursor/worktrees/",
    "/.worktrees/",
];

/// The exact text of the managed block, newline terminated.
pub fn exclude_block() -> String {
    let mut s = String::with_capacity(160);
    s.push_str(EXCLUDE_BEGIN);
    s.push('\n');
    s.push_str("# Alias symlinks for gilde-code worktrees (DESIGN 7-6).\n");
    s.push_str("# Written here, never to a tracked ignore file. Safe to delete.\n");
    for entry in EXCLUDE_ENTRIES {
        s.push_str(entry);
        s.push('\n');
    }
    s.push_str(EXCLUDE_END);
    s.push('\n');
    s
}

// ---------------------------------------------------------------------------
// providers
// ---------------------------------------------------------------------------

/// Where a provider expects to find a worktree (DESIGN §7-6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AliasProvider {
    /// `<repo>/.claude/worktrees/<task-slug>` — observed with claude 2.1.227.
    ClaudeW,
    /// `<repo>/.cursor/worktrees/<task-slug>` — unverified, disabled by default.
    CursorWorktree,
    /// `<repo>/.worktrees/<task-slug>` — our own, provider agnostic.
    Generic,
    /// A path the user chose. Relative paths resolve against the repository.
    Custom { link_path: PathBuf },
}

/// The static facts about a provider's convention (DESIGN §7-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AliasConvention {
    pub provider: AliasProvider,
    /// Human readable template, e.g. `<repo>/.claude/worktrees/<task-slug>`.
    pub template: &'static str,
    /// The version this convention was actually observed against, or
    /// `"unverified"` when we have never seen the tool run.
    pub verified_against: &'static str,
    /// Default for the user toggle. Unverified providers default to off, so a
    /// guessed convention never silently plants links in someone's repository.
    pub enabled: bool,
}

impl AliasProvider {
    /// Stable wire key, matching `AliasProviderKey` in config and `ipc.ts`.
    pub fn key(&self) -> &'static str {
        match self {
            AliasProvider::ClaudeW => "claude-w",
            AliasProvider::CursorWorktree => "cursor-worktree",
            AliasProvider::Generic => "generic",
            AliasProvider::Custom { .. } => "custom",
        }
    }

    /// The config vocabulary carries no path, so `custom` has no counterpart.
    pub fn from_key(key: AliasProviderKey) -> Option<AliasProvider> {
        match key {
            AliasProviderKey::ClaudeW => Some(AliasProvider::ClaudeW),
            AliasProviderKey::CursorWorktree => Some(AliasProvider::CursorWorktree),
            AliasProviderKey::Generic => Some(AliasProvider::Generic),
            AliasProviderKey::Custom => None,
        }
    }

    /// Directory, relative to the repository root, that holds this provider's
    /// aliases. `None` for [`AliasProvider::Custom`], whose path is arbitrary.
    pub fn relative_dir(&self) -> Option<&'static str> {
        match self {
            AliasProvider::ClaudeW => Some(".claude/worktrees"),
            AliasProvider::CursorWorktree => Some(".cursor/worktrees"),
            AliasProvider::Generic => Some(".worktrees"),
            AliasProvider::Custom { .. } => None,
        }
    }

    pub fn convention(&self) -> AliasConvention {
        let (template, verified_against, enabled) = match self {
            AliasProvider::ClaudeW => (
                "<repo>/.claude/worktrees/<task-slug>",
                "claude 2.1.227",
                true,
            ),
            AliasProvider::CursorWorktree => (
                "<repo>/.cursor/worktrees/<task-slug>",
                // DESIGN §14: cursor-agent is not installed on this machine, so
                // this lane ships untested and says so.
                "unverified",
                false,
            ),
            AliasProvider::Generic => ("<repo>/.worktrees/<task-slug>", "n/a", true),
            AliasProvider::Custom { .. } => ("<link-path>", "user", true),
        };
        AliasConvention {
            provider: self.clone(),
            template,
            verified_against,
            enabled,
        }
    }

    /// Absolute path the alias should occupy for `slug` inside `repo`.
    pub fn alias_path(&self, repo: &Path, slug: &str) -> PathBuf {
        match self {
            AliasProvider::Custom { link_path } => {
                if link_path.is_absolute() {
                    link_path.clone()
                } else {
                    repo.join(link_path)
                }
            }
            other => {
                let rel = other.relative_dir().expect("non-custom provider has a dir");
                let mut p = repo.to_path_buf();
                for part in rel.split('/') {
                    p.push(part);
                }
                p.push(slug);
                p
            }
        }
    }

    /// Every convention path for `repo`, used by the adoption scan.
    pub fn convention_roots(repo: &Path) -> Vec<(AliasProvider, PathBuf)> {
        [
            AliasProvider::ClaudeW,
            AliasProvider::CursorWorktree,
            AliasProvider::Generic,
        ]
        .into_iter()
        .map(|p| {
            let rel = p.relative_dir().expect("non-custom");
            let mut path = repo.to_path_buf();
            for part in rel.split('/') {
                path.push(part);
            }
            (p, path)
        })
        .collect()
    }
}

// `{ "provider": "claude-w" }` / `{ "provider": "custom", "linkPath": "…" }`,
// matching `AliasProvider` in `src/lib/ipc.ts`. A bare `"claude-w"` is accepted
// too, because that is the shape `config.worktree.aliasProviders` uses.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderObj {
    provider: String,
    #[serde(default)]
    link_path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProviderRaw {
    Key(String),
    Obj(ProviderObj),
}

impl<'de> Deserialize<'de> for AliasProvider {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw = ProviderRaw::deserialize(d)?;
        let (key, link_path) = match raw {
            ProviderRaw::Key(k) => (k, None),
            ProviderRaw::Obj(o) => (o.provider, o.link_path),
        };
        match key.as_str() {
            "claude-w" => Ok(AliasProvider::ClaudeW),
            "cursor-worktree" => Ok(AliasProvider::CursorWorktree),
            "generic" => Ok(AliasProvider::Generic),
            "custom" => match link_path {
                Some(link_path) if !link_path.as_os_str().is_empty() => {
                    Ok(AliasProvider::Custom { link_path })
                }
                _ => Err(D::Error::custom("alias provider `custom` needs `linkPath`")),
            },
            other => Err(D::Error::custom(format!("unknown alias provider `{other}`"))),
        }
    }
}

impl Serialize for AliasProvider {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            AliasProvider::Custom { link_path } => {
                let mut st = s.serialize_struct("AliasProvider", 2)?;
                st.serialize_field("provider", "custom")?;
                st.serialize_field("linkPath", link_path)?;
                st.end()
            }
            other => {
                let mut st = s.serialize_struct("AliasProvider", 1)?;
                st.serialize_field("provider", other.key())?;
                st.end()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// outcomes
// ---------------------------------------------------------------------------

/// Why we refused to touch a path. Every variant means "something that is not
/// ours is already there".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    /// A real directory — quite possibly a worktree the provider made.
    ExistingDirectory,
    /// A regular file (or socket, or fifo).
    ExistingFile,
    /// A symlink pointing anywhere other than our canonical target.
    ForeignSymlink,
    /// Someone won the race between our check and our `symlink(2)`.
    RaceLost,
    /// A custom link path outside the repository whose parent does not exist.
    /// We create directories inside the repository only.
    ParentMissing,
    /// The alias path and the worktree path are the same thing.
    SameAsTarget,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::ExistingDirectory => "existingDirectory",
            SkipReason::ExistingFile => "existingFile",
            SkipReason::ForeignSymlink => "foreignSymlink",
            SkipReason::RaceLost => "raceLost",
            SkipReason::ParentMissing => "parentMissing",
            SkipReason::SameAsTarget => "sameAsTarget",
        }
    }
}

/// Result of one `alias_ensure`.
///
/// The wire shape is the `AliasOutcome` union in `src/lib/ipc.ts` (T04):
/// `{ outcome, aliasPath, … }`, with `AlreadyOurs` tagged `"existing"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome")]
pub enum AliasOutcome {
    /// We created the symlink. `not_ignored` is a *warning*: the alias exists
    /// but `git check-ignore` says the repository would show it as untracked.
    #[serde(rename = "created", rename_all = "camelCase")]
    Created {
        #[serde(rename = "aliasPath")]
        path: PathBuf,
        #[serde(rename = "targetPath")]
        target: PathBuf,
        not_ignored: bool,
    },
    /// The correct symlink was already there. No-op.
    #[serde(rename = "existing", rename_all = "camelCase")]
    AlreadyOurs {
        #[serde(rename = "aliasPath")]
        path: PathBuf,
    },
    /// Something that is not ours occupies the path. Nothing was touched.
    #[serde(rename = "skipped", rename_all = "camelCase")]
    Skipped {
        #[serde(rename = "aliasPath")]
        path: PathBuf,
        reason: SkipReason,
    },
    /// A real git worktree already lives at the convention path. Register it
    /// with `origin = 'adopted'` rather than fighting it.
    #[serde(rename = "adopted", rename_all = "camelCase")]
    Adopted {
        #[serde(rename = "aliasPath")]
        path: PathBuf,
    },
}

impl AliasOutcome {
    pub fn path(&self) -> &Path {
        match self {
            AliasOutcome::Created { path, .. }
            | AliasOutcome::AlreadyOurs { path }
            | AliasOutcome::Skipped { path, .. }
            | AliasOutcome::Adopted { path } => path,
        }
    }
}

/// Why an owned-alias removal did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemovalSkip {
    /// The alias is gone already; only the stale row was dropped.
    Missing,
    /// The path is no longer a symlink. Never unlinked.
    NotASymlink,
    /// The symlink now points somewhere else — someone repointed it, or the
    /// path was reused. Left exactly as found.
    TargetMismatch,
    /// An adopted worktree, not a link we made.
    Adopted,
    /// `unlink(2)` failed; see the log.
    IoError,
}

/// One line of the teardown report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AliasRemoval {
    pub provider: String,
    pub alias_path: String,
    pub target_path: String,
    /// True only when we actually unlinked the symlink.
    pub removed: bool,
    #[serde(default)]
    pub reason: Option<RemovalSkip>,
}

/// A real worktree found sitting at a convention path (DESIGN §7-6 rule 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptionCandidate {
    pub provider: String,
    /// Where it was found — this is also the worktree's real path.
    pub path: String,
    /// Short branch name, when it is not detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    /// Slug we would register it under.
    pub slug: String,
}

// ---------------------------------------------------------------------------
// info/exclude
// ---------------------------------------------------------------------------

/// Resolve `<repo>/.git` even when `repo` is itself a linked worktree.
async fn git_common_dir(repo: &Path) -> Result<PathBuf> {
    let out = git::run_git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let dir = PathBuf::from(out.text());
    if dir.as_os_str().is_empty() {
        return Err(GildeError::worktree(format!(
            "git reported no common dir for {}",
            repo.display()
        )));
    }
    Ok(dir)
}

/// Idempotently install the managed ignore block in
/// `<git-common-dir>/info/exclude`.
///
/// Returns `true` when the file was changed. **This is the only file this
/// module writes inside the user's repository**, and it is deliberately an
/// untracked one: DESIGN §7-6 rule 4.
pub async fn ensure_info_exclude(repo: &Path) -> Result<bool> {
    let exclude = git_common_dir(repo).await?.join("info").join("exclude");
    ensure_info_exclude_at(&exclude)
}

/// The pure part, so tests can drive it without a repository.
pub fn ensure_info_exclude_at(exclude: &Path) -> Result<bool> {
    let existing = match std::fs::read_to_string(exclude) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    let block = exclude_block();
    let (before, after, had_block) = split_block(&existing);
    if had_block && existing.contains(&block) {
        return Ok(false);
    }

    let mut next = String::with_capacity(before.len() + block.len() + after.len() + 2);
    next.push_str(before);
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&block);
    next.push_str(after);

    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(exclude, next.as_bytes())?;
    Ok(true)
}

/// Split `text` around a previously written managed block.
fn split_block(text: &str) -> (&str, &str, bool) {
    let Some(start) = text.find(EXCLUDE_BEGIN) else {
        return (text, "", false);
    };
    let rest = &text[start..];
    let Some(end_rel) = rest.find(EXCLUDE_END) else {
        // Truncated block (a crash mid-write, or a hand edit). Everything from
        // the opening marker on is ours to replace.
        return (&text[..start], "", true);
    };
    let end = start + end_rel + EXCLUDE_END.len();
    let tail = text[end..].strip_prefix('\n').unwrap_or(&text[end..]);
    (&text[..start], tail, true)
}

/// Write through a sibling temp file so a crash never leaves a half `exclude`.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.gilde-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "exclude".into()),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

// ---------------------------------------------------------------------------
// the layer
// ---------------------------------------------------------------------------

/// Owns the `worktree_alias` table and the symlinks it describes.
pub struct AliasLayer {
    db: Db,
    paths: Option<Arc<AppPaths>>,
}

impl AliasLayer {
    pub fn new(db: Db, paths: Option<Arc<AppPaths>>) -> Arc<Self> {
        Arc::new(AliasLayer { db, paths })
    }

    // -- ensure -------------------------------------------------------------

    /// DESIGN §7-6. Never overwrites, never follows, never touches a tracked file.
    pub async fn ensure(
        &self,
        wt: &Worktree,
        repo: &Path,
        provider: AliasProvider,
    ) -> Result<AliasOutcome> {
        let repo = canonical(repo);

        // Rule 2: the target is always the canonical central path. If the
        // worktree is not on disk there is nothing to point at, and a dangling
        // alias is worse than no alias.
        let target = std::fs::canonicalize(&wt.path).map_err(|e| {
            GildeError::worktree(format!("worktree path {} is unusable: {e}", wt.path))
        })?;

        let alias_path = provider.alias_path(&repo, &wt.slug);
        // Resolve the *parent* only: canonicalizing `alias_path` itself would
        // follow the very symlink we may have made last time and report the
        // worktree as its own alias.
        if canonical_leaf(&alias_path) == target {
            return Ok(AliasOutcome::Skipped {
                path: alias_path,
                reason: SkipReason::SameAsTarget,
            });
        }

        let inside_repo = alias_path.starts_with(&repo);

        // Ignore hygiene happens *before* the link exists, so `git status` in
        // the user's repository is never dirty, not even for an instant.
        if inside_repo {
            ensure_info_exclude(&repo).await?;
        }

        match std::fs::symlink_metadata(&alias_path) {
            Ok(md) => {
                let ft = md.file_type();
                if ft.is_symlink() {
                    let link = std::fs::read_link(&alias_path)?;
                    if link == target {
                        // Ours already. Re-record so a lost row cannot orphan a
                        // link we are responsible for removing later.
                        self.record(wt, &provider, &alias_path, &target, AliasKind::Symlink)
                            .await?;
                        return Ok(AliasOutcome::AlreadyOurs { path: alias_path });
                    }
                    tracing::info!(
                        target: "worktree::alias",
                        alias = %alias_path.display(),
                        points_to = %link.display(),
                        "alias path holds a foreign symlink; leaving it alone"
                    );
                    return Ok(AliasOutcome::Skipped {
                        path: alias_path,
                        reason: SkipReason::ForeignSymlink,
                    });
                }
                if ft.is_dir() {
                    // Rule 6: adopt rather than collide.
                    if self.is_git_worktree(&repo, &alias_path).await {
                        tracing::info!(
                            target: "worktree::alias",
                            alias = %alias_path.display(),
                            "convention path already holds a real worktree; proposing adoption"
                        );
                        return Ok(AliasOutcome::Adopted { path: alias_path });
                    }
                    return Ok(AliasOutcome::Skipped {
                        path: alias_path,
                        reason: SkipReason::ExistingDirectory,
                    });
                }
                Ok(AliasOutcome::Skipped {
                    path: alias_path,
                    reason: SkipReason::ExistingFile,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.create(wt, &repo, &provider, alias_path, target, inside_repo)
                    .await
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn create(
        &self,
        wt: &Worktree,
        repo: &Path,
        provider: &AliasProvider,
        alias_path: PathBuf,
        target: PathBuf,
        inside_repo: bool,
    ) -> Result<AliasOutcome> {
        if let Some(parent) = alias_path.parent() {
            if !parent.exists() {
                if !inside_repo {
                    return Ok(AliasOutcome::Skipped {
                        path: alias_path,
                        reason: SkipReason::ParentMissing,
                    });
                }
                std::fs::create_dir_all(parent)?;
            }
        }

        if let Err(e) = std::os::unix::fs::symlink(&target, &alias_path) {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                return Ok(AliasOutcome::Skipped {
                    path: alias_path,
                    reason: SkipReason::RaceLost,
                });
            }
            return Err(e.into());
        }

        // Rule 4's verification step: the block above is only a claim until git
        // agrees. A `false` here becomes a visible warning, not a silent miss.
        let not_ignored = if inside_repo {
            match git::check_ignore(repo, &alias_path).await {
                Ok(ignored) => !ignored,
                Err(e) => {
                    tracing::warn!(
                        target: "worktree::alias",
                        error = %e,
                        "check-ignore failed; reporting the alias as not ignored"
                    );
                    true
                }
            }
        } else {
            // Outside the repository it cannot dirty anything.
            false
        };
        if not_ignored {
            tracing::warn!(
                target: "worktree::alias",
                alias = %alias_path.display(),
                "alias is not ignored by git; the repository will show it as untracked"
            );
        }

        self.record(wt, provider, &alias_path, &target, AliasKind::Symlink)
            .await?;

        Ok(AliasOutcome::Created {
            path: alias_path,
            target,
            not_ignored,
        })
    }

    /// Upsert the `worktree_alias` row. `alias_path` is unique, so a stale row
    /// for a path we just took over is replaced rather than colliding.
    async fn record(
        &self,
        wt: &Worktree,
        provider: &AliasProvider,
        alias_path: &Path,
        target: &Path,
        kind: AliasKind,
    ) -> Result<()> {
        let row = WorktreeAliasRow {
            id: uuid::Uuid::now_v7().to_string(),
            worktree_id: wt.id.clone(),
            provider: provider.key().to_string(),
            alias_path: alias_path.display().to_string(),
            target_path: target.display().to_string(),
            kind,
            created_at: now_ms(),
        };
        let existing_path = row.alias_path.clone();
        self.db
            .write(move |conn| {
                if let Some(old) = dao::worktree_alias::get_by_alias_path(conn, &existing_path)? {
                    if old.worktree_id == row.worktree_id
                        && old.target_path == row.target_path
                        && old.kind == row.kind
                    {
                        return Ok(());
                    }
                    dao::worktree_alias::delete(conn, &old.id)?;
                }
                dao::worktree_alias::insert(conn, &row)
            })
            .await?;
        self.mirror().await;
        Ok(())
    }

    async fn is_git_worktree(&self, repo: &Path, path: &Path) -> bool {
        let wanted = canonical(path);
        match git::worktree_list(repo).await {
            Ok(entries) => entries.iter().any(|e| canonical(&e.path) == wanted),
            Err(e) => {
                tracing::warn!(target: "worktree::alias", error = %e, "worktree list failed");
                false
            }
        }
    }

    // -- teardown -----------------------------------------------------------

    /// Remove only symlinks this app made for `worktree_id` (DESIGN §7-6 rule 5).
    ///
    /// Two conditions must both hold before `unlink(2)`: a row exists in
    /// `worktree_alias`, and `read_link(alias) == target_path`. Everything else
    /// is reported and left exactly as found — including a link whose target was
    /// changed behind our back, which is the case this check exists for.
    pub async fn remove_owned(&self, worktree_id: &str) -> Result<Vec<AliasRemoval>> {
        let id = worktree_id.to_string();
        let rows = self
            .db
            .read(move |conn| dao::worktree_alias::list_by_worktree(conn, &id))
            .await?;

        let mut report = Vec::with_capacity(rows.len());
        let mut drop_rows: Vec<String> = Vec::new();

        for row in rows {
            let alias = PathBuf::from(&row.alias_path);
            let target = PathBuf::from(&row.target_path);
            let mut removal = AliasRemoval {
                provider: row.provider.clone(),
                alias_path: row.alias_path.clone(),
                target_path: row.target_path.clone(),
                removed: false,
                reason: None,
            };

            if row.kind == AliasKind::Adopted {
                // Not a link we made — it *is* the worktree. Never touch it.
                removal.reason = Some(RemovalSkip::Adopted);
                report.push(removal);
                continue;
            }

            match std::fs::symlink_metadata(&alias) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    removal.reason = Some(RemovalSkip::Missing);
                    drop_rows.push(row.id.clone());
                }
                Err(e) => {
                    tracing::warn!(
                        target: "worktree::alias",
                        alias = %alias.display(), error = %e,
                        "cannot stat alias; leaving it alone"
                    );
                    removal.reason = Some(RemovalSkip::IoError);
                }
                Ok(md) if !md.file_type().is_symlink() => {
                    removal.reason = Some(RemovalSkip::NotASymlink);
                }
                Ok(_) => match std::fs::read_link(&alias) {
                    Ok(link) if link == target => match std::fs::remove_file(&alias) {
                        Ok(()) => {
                            removal.removed = true;
                            drop_rows.push(row.id.clone());
                            self.prune_empty_parents(&alias);
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "worktree::alias",
                                alias = %alias.display(), error = %e, "unlink failed"
                            );
                            removal.reason = Some(RemovalSkip::IoError);
                        }
                    },
                    Ok(link) => {
                        tracing::warn!(
                            target: "worktree::alias",
                            alias = %alias.display(),
                            points_to = %link.display(),
                            expected = %target.display(),
                            "alias target changed; refusing to remove"
                        );
                        removal.reason = Some(RemovalSkip::TargetMismatch);
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "worktree::alias",
                            alias = %alias.display(), error = %e, "read_link failed"
                        );
                        removal.reason = Some(RemovalSkip::IoError);
                    }
                },
            }

            report.push(removal);
        }

        if !drop_rows.is_empty() {
            self.db
                .write(move |conn| {
                    for id in &drop_rows {
                        dao::worktree_alias::delete(conn, id)?;
                    }
                    Ok(())
                })
                .await?;
            self.mirror().await;
        }

        Ok(report)
    }

    /// Remove `<repo>/.claude/worktrees` (and its parent) when we emptied them.
    /// Only ever removes directories that are already empty, which is why
    /// `remove_dir` is used and its failure ignored.
    fn prune_empty_parents(&self, alias: &Path) {
        let mut cur = alias.parent().map(Path::to_path_buf);
        // At most two levels: `.../worktrees` and `.../.claude`.
        for _ in 0..2 {
            let Some(dir) = cur else { return };
            if std::fs::remove_dir(&dir).is_err() {
                return;
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
    }

    // -- adoption -----------------------------------------------------------

    /// Real worktrees of `repo` that sit at a provider convention path
    /// (DESIGN §7-6 rule 6). Reporting only — registration is the caller's call.
    pub async fn scan_adoptable(&self, repo: &Path) -> Result<Vec<AdoptionCandidate>> {
        let repo = canonical(repo);
        let entries = git::worktree_list(&repo).await?;
        let roots = AliasProvider::convention_roots(&repo);

        let mut out = Vec::new();
        for entry in entries {
            let path = canonical(&entry.path);
            if path == repo {
                continue;
            }
            let Some((provider, _)) = roots.iter().find(|(_, root)| path.starts_with(root)) else {
                continue;
            };
            // Ours already? Then it is not a candidate.
            let key = path.display().to_string();
            let known = self
                .db
                .read(move |conn| dao::worktree_alias::get_by_alias_path(conn, &key))
                .await?;
            if known.is_some() {
                continue;
            }
            let slug = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(AdoptionCandidate {
                provider: provider.key().to_string(),
                path: path.display().to_string(),
                branch: entry.short_branch().map(str::to_string),
                head: entry.head.clone(),
                slug,
            });
        }
        Ok(out)
    }

    /// Register a candidate with `worktree.origin = 'adopted'` (DESIGN §7-6
    /// rule 6). The row records the discovered path verbatim: an adopted
    /// worktree lives outside the central directory and its removal must not go
    /// anywhere near the central-directory logic.
    pub async fn register_adopted(
        &self,
        project_id: &str,
        candidate: &AdoptionCandidate,
    ) -> Result<String> {
        let now = now_ms();
        let branch = candidate
            .branch
            .clone()
            .unwrap_or_else(|| format!("detached@{}", candidate.head.as_deref().unwrap_or("?")));
        let row = crate::store::models::WorktreeRow {
            id: uuid::Uuid::now_v7().to_string(),
            project_id: project_id.to_string(),
            slug: candidate.slug.clone(),
            branch,
            path: candidate.path.clone(),
            base_sha: candidate.head.clone(),
            origin: WorktreeOrigin::Adopted,
            created_at: now,
            removed_at: None,
            locked: false,
            verified_at: Some(now),
            post_create_cmd: None,
            cached_dirty: 0,
            cached_added: 0,
            cached_removed: 0,
            cached_ahead: 0,
            cached_behind: 0,
        };
        let id = row.id.clone();
        let alias = WorktreeAliasRow {
            id: uuid::Uuid::now_v7().to_string(),
            worktree_id: id.clone(),
            provider: candidate.provider.clone(),
            alias_path: candidate.path.clone(),
            target_path: candidate.path.clone(),
            kind: AliasKind::Adopted,
            created_at: now,
        };
        self.db
            .write(move |conn| {
                dao::worktree::insert(conn, &row)?;
                dao::worktree_alias::insert(conn, &alias)
            })
            .await?;
        self.mirror().await;
        Ok(id)
    }

    // -- disk mirror --------------------------------------------------------

    /// `~/.gilde/aliases.json` (DESIGN §7-1): a recovery mirror of
    /// the table, so a lost `state.db` does not turn our symlinks into litter
    /// nobody is allowed to remove. Best effort — a failure here is logged and
    /// never fails the operation that triggered it.
    async fn mirror(&self) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
        let rows = match self.db.read(all_alias_rows).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(target: "worktree::alias", error = %e, "alias mirror read failed");
                return;
            }
        };
        let file = paths.aliases_file();
        let payload = match serde_json::to_vec_pretty(&rows) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(target: "worktree::alias", error = %e, "alias mirror encode failed");
                return;
            }
        };
        let written = tokio::task::spawn_blocking(move || write_atomic(&file, &payload)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(target: "worktree::alias", error = %e, "alias mirror write failed")
            }
            Err(e) => {
                tracing::warn!(target: "worktree::alias", error = %e, "alias mirror task failed")
            }
        }
    }
}

fn all_alias_rows(conn: &rusqlite::Connection) -> Result<Vec<WorktreeAliasRow>> {
    let mut stmt = conn.prepare("SELECT * FROM worktree_alias ORDER BY created_at")?;
    let mapped = stmt.query_map([], WorktreeAliasRow::from_row)?;
    let mut out = Vec::new();
    for row in mapped {
        out.push(row?);
    }
    Ok(out)
}

/// `canonicalize` when it works, the input otherwise. Used for comparisons only.
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Canonicalize everything *except* the last component, so a symlink at `p` is
/// compared as a path rather than resolved to what it points at.
fn canonical_leaf(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => canonical(parent).join(name),
        _ => canonical(p),
    }
}

// ---------------------------------------------------------------------------
// AliasHook — the seam `worktree::mod` calls (T13 owns the call sites)
// ---------------------------------------------------------------------------

impl AliasHook for AliasLayer {
    fn ensure<'a>(
        &'a self,
        worktree: &'a Worktree,
        repo: &'a Path,
        providers: &'a [Value],
    ) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(providers.len());
            for raw in providers {
                let provider: AliasProvider = match serde_json::from_value(raw.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            target: "worktree::alias",
                            error = %e, raw = %raw,
                            "ignoring unusable alias provider"
                        );
                        continue;
                    }
                };
                match AliasLayer::ensure(self, worktree, repo, provider).await {
                    Ok(outcome) => {
                        if let Ok(v) = serde_json::to_value(&outcome) {
                            out.push(v);
                        }
                    }
                    Err(e) => {
                        // An alias is a convenience. Failing to make one must
                        // never fail worktree creation.
                        tracing::warn!(
                            target: "worktree::alias",
                            error = %e, "alias_ensure failed"
                        );
                    }
                }
            }
            out
        })
    }

    fn remove_owned<'a>(&'a self, worktree_id: &'a str) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            match AliasLayer::remove_owned(self, worktree_id).await {
                Ok(report) => report
                    .into_iter()
                    .filter(|r| r.removed)
                    .map(|r| r.alias_path)
                    .collect(),
                Err(e) => {
                    tracing::warn!(
                        target: "worktree::alias",
                        error = %e, "remove_owned_aliases failed"
                    );
                    Vec::new()
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// process-wide handle + the free functions T14 must expose
// ---------------------------------------------------------------------------

static LAYER: OnceLock<Arc<AliasLayer>> = OnceLock::new();

/// Build the layer and register it with `worktree::mod` (T13's `AliasHook`
/// seam). Idempotent: the first installation wins and is returned again.
pub fn install(db: Db, paths: Option<Arc<AppPaths>>) -> Arc<AliasLayer> {
    let layer = AliasLayer::new(db, paths);
    let _ = LAYER.set(layer);
    let installed = LAYER.get().expect("just set").clone();
    let hook: Arc<dyn AliasHook> = installed.clone();
    crate::worktree::install_alias_hook(hook);
    installed
}

/// The installed layer, if `install` has run.
pub fn layer() -> Option<Arc<AliasLayer>> {
    LAYER.get().cloned()
}

fn require_layer() -> Result<Arc<AliasLayer>> {
    layer().ok_or_else(|| GildeError::internal("alias layer is not installed"))
}

/// DESIGN §7-6 / M1-PLAN T14. See [`AliasLayer::ensure`].
pub async fn alias_ensure(
    wt: &Worktree,
    repo: &Path,
    provider: AliasProvider,
) -> Result<AliasOutcome> {
    require_layer()?.ensure(wt, repo, provider).await
}

/// DESIGN §7-6 rule 5. See [`AliasLayer::remove_owned`].
pub async fn remove_owned_aliases(worktree_id: &str) -> Result<Vec<AliasRemoval>> {
    require_layer()?.remove_owned(worktree_id).await
}

/// DESIGN §7-6 rule 6. See [`AliasLayer::scan_adoptable`].
pub async fn scan_adoptable(repo: &Path) -> Result<Vec<AdoptionCandidate>> {
    require_layer()?.scan_adoptable(repo).await
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convention_paths_match_design() {
        let repo = Path::new("/repo");
        assert_eq!(
            AliasProvider::ClaudeW.alias_path(repo, "fix-login-ab12cd"),
            Path::new("/repo/.claude/worktrees/fix-login-ab12cd")
        );
        assert_eq!(
            AliasProvider::CursorWorktree.alias_path(repo, "fix-login-ab12cd"),
            Path::new("/repo/.cursor/worktrees/fix-login-ab12cd")
        );
        assert_eq!(
            AliasProvider::Generic.alias_path(repo, "fix-login-ab12cd"),
            Path::new("/repo/.worktrees/fix-login-ab12cd")
        );
        let custom = AliasProvider::Custom {
            link_path: PathBuf::from("wt/here"),
        };
        assert_eq!(custom.alias_path(repo, "ignored"), Path::new("/repo/wt/here"));
        let abs = AliasProvider::Custom {
            link_path: PathBuf::from("/elsewhere/x"),
        };
        assert_eq!(abs.alias_path(repo, "ignored"), Path::new("/elsewhere/x"));
    }

    #[test]
    fn cursor_is_unverified_and_off_by_default() {
        let c = AliasProvider::CursorWorktree.convention();
        assert_eq!(c.verified_against, "unverified");
        assert!(!c.enabled, "an unverified convention must not default to on");

        let c = AliasProvider::ClaudeW.convention();
        assert!(c.enabled);
        assert_ne!(c.verified_against, "unverified");
    }

    #[test]
    fn provider_accepts_both_config_and_ipc_shapes() {
        let from_key: AliasProvider = serde_json::from_str("\"claude-w\"").unwrap();
        assert_eq!(from_key, AliasProvider::ClaudeW);

        let from_obj: AliasProvider =
            serde_json::from_value(serde_json::json!({ "provider": "cursor-worktree" })).unwrap();
        assert_eq!(from_obj, AliasProvider::CursorWorktree);

        let custom: AliasProvider =
            serde_json::from_value(serde_json::json!({ "provider": "custom", "linkPath": "/a/b" }))
                .unwrap();
        assert_eq!(
            custom,
            AliasProvider::Custom {
                link_path: PathBuf::from("/a/b")
            }
        );

        assert!(serde_json::from_str::<AliasProvider>("\"nope\"").is_err());
        assert!(
            serde_json::from_value::<AliasProvider>(serde_json::json!({ "provider": "custom" }))
                .is_err(),
            "custom without a path is not a provider"
        );

        let round = serde_json::to_value(&AliasProvider::ClaudeW).unwrap();
        assert_eq!(round, serde_json::json!({ "provider": "claude-w" }));
        let round = serde_json::to_value(&custom).unwrap();
        assert_eq!(
            round,
            serde_json::json!({ "provider": "custom", "linkPath": "/a/b" })
        );
    }

    #[test]
    fn config_keys_map_onto_providers() {
        assert_eq!(
            AliasProvider::from_key(AliasProviderKey::ClaudeW),
            Some(AliasProvider::ClaudeW)
        );
        assert_eq!(AliasProvider::from_key(AliasProviderKey::Custom), None);
    }

    #[test]
    fn outcome_wire_shape_matches_ipc() {
        let v = serde_json::to_value(AliasOutcome::Created {
            path: PathBuf::from("/repo/.claude/worktrees/x"),
            target: PathBuf::from("/central/x"),
            not_ignored: false,
        })
        .unwrap();
        assert_eq!(v["outcome"], "created");
        assert_eq!(v["aliasPath"], "/repo/.claude/worktrees/x");
        assert_eq!(v["targetPath"], "/central/x");
        assert_eq!(v["notIgnored"], false);

        let v = serde_json::to_value(AliasOutcome::AlreadyOurs {
            path: PathBuf::from("/a"),
        })
        .unwrap();
        assert_eq!(v["outcome"], "existing");

        let v = serde_json::to_value(AliasOutcome::Skipped {
            path: PathBuf::from("/a"),
            reason: SkipReason::ExistingDirectory,
        })
        .unwrap();
        assert_eq!(v["outcome"], "skipped");
        assert_eq!(v["reason"], "existingDirectory");

        let v = serde_json::to_value(AliasOutcome::Adopted {
            path: PathBuf::from("/a"),
        })
        .unwrap();
        assert_eq!(v["outcome"], "adopted");
    }

    #[test]
    fn exclude_block_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let exclude = dir.path().join("info").join("exclude");

        assert!(ensure_info_exclude_at(&exclude).unwrap(), "first write");
        let once = std::fs::read_to_string(&exclude).unwrap();
        assert!(once.contains("/.claude/worktrees/"));

        assert!(!ensure_info_exclude_at(&exclude).unwrap(), "second is a no-op");
        assert_eq!(std::fs::read_to_string(&exclude).unwrap(), once);
        assert_eq!(once.matches(EXCLUDE_BEGIN).count(), 1);
    }

    #[test]
    fn exclude_preserves_user_content_around_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let exclude = dir.path().join("exclude");
        std::fs::write(&exclude, "# user's own\n*.tmp\n").unwrap();

        assert!(ensure_info_exclude_at(&exclude).unwrap());
        let text = std::fs::read_to_string(&exclude).unwrap();
        assert!(text.starts_with("# user's own\n*.tmp\n"));
        assert!(text.contains(EXCLUDE_BEGIN));

        // A stale block is replaced in place, not appended to.
        let stale = format!("head\n{EXCLUDE_BEGIN}\n/old/\n{EXCLUDE_END}\ntail\n");
        std::fs::write(&exclude, stale).unwrap();
        assert!(ensure_info_exclude_at(&exclude).unwrap());
        let text = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(text.matches(EXCLUDE_BEGIN).count(), 1);
        assert!(!text.contains("/old/"));
        assert!(text.starts_with("head\n"));
        assert!(text.ends_with("tail\n"));
    }

    #[test]
    fn truncated_block_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let exclude = dir.path().join("exclude");
        std::fs::write(&exclude, format!("keep\n{EXCLUDE_BEGIN}\n/half/\n")).unwrap();
        assert!(ensure_info_exclude_at(&exclude).unwrap());
        let text = std::fs::read_to_string(&exclude).unwrap();
        assert!(text.starts_with("keep\n"));
        assert_eq!(text.matches(EXCLUDE_BEGIN).count(), 1);
        assert_eq!(text.matches(EXCLUDE_END).count(), 1);
        assert!(!text.contains("/half/"));
    }

    /// The managed block only ever names paths under a repository, and only
    /// the three convention directories. DESIGN §7-6 rule 4's other half — that
    /// no tracked ignore file is ever named in this tree — is asserted by
    /// `tests/alias_safety.rs`, which greps the source.
    #[test]
    fn exclude_block_covers_exactly_the_convention_dirs() {
        let block = exclude_block();
        let patterns: Vec<&str> = block
            .lines()
            .filter(|l| l.starts_with('/'))
            .collect();
        assert_eq!(
            patterns,
            vec!["/.claude/worktrees/", "/.cursor/worktrees/", "/.worktrees/"]
        );
        // `.claude/` itself often holds tracked files; only the subdirectory is
        // excluded.
        assert!(!block.contains("\n/.claude/\n"));
    }
}
