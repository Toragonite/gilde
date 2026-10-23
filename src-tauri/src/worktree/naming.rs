//! Slug and branch naming rules (DESIGN §7-1).
//!
//! Three rules, and each of them exists because of a concrete failure:
//!
//! * `repo_slug` carries an **8 hex digit path hash**. Two repositories with the
//!   same basename (`~/work/api`, `~/oss/api`) must never share a worktree
//!   namespace; silently interleaving their worktrees would be a disaster.
//! * `task_slug` ends in a **6 character short id**, so uniqueness is a property
//!   of the slug and not something the UI has to police. Two worktrees called
//!   "fix login" simply get two directories.
//! * Branches live under **`gilde/`**. `git branch --list 'gilde/*'` then enumerates
//!   exactly our branches and nothing of the user's.

use std::path::Path;

use sha2::{Digest, Sha256};

/// Longest sanitized prefix kept before the short id is appended.
pub const MAX_SLUG_BODY: usize = 40;
/// Hex digits of `sha256(canonical repo path)` appended to the repo slug.
pub const REPO_HASH_LEN: usize = 8;
/// Characters of the per-worktree short id.
pub const SHORT_ID_LEN: usize = 6;
/// Namespace every branch this app creates lives in.
pub const BRANCH_PREFIX: &str = "gilde/";
/// Fallback body when a name sanitizes to nothing (e.g. pure Hangul or emoji).
const FALLBACK_BODY: &str = "task";

/// `<basename>-<sha256(canonical path)[..8]>`.
///
/// `canonical_repo` must be the path [`crate::worktree::git::repo_root`]
/// returned — the hash is only stable if the input is.
pub fn repo_slug(canonical_repo: &Path) -> String {
    let base = canonical_repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let body = truncate_body(&sanitize(&base));
    let body = if body.is_empty() {
        "repo".to_string()
    } else {
        body
    };
    format!("{body}-{}", path_hash(canonical_repo))
}

/// `sanitize(name)[..40] + "-" + short_id(6)`.
pub fn task_slug(name: &str) -> String {
    task_slug_with_id(name, &short_id())
}

/// [`task_slug`] with the short id supplied — used by tests and by any caller
/// that needs a reproducible slug.
pub fn task_slug_with_id(name: &str, short: &str) -> String {
    let body = truncate_body(&sanitize(name));
    let body = if body.is_empty() {
        FALLBACK_BODY.to_string()
    } else {
        body
    };
    format!("{body}-{short}")
}

/// `"gilde/" + task_slug`.
pub fn branch_name(task_slug: &str) -> String {
    format!("{BRANCH_PREFIX}{task_slug}")
}

/// True when `branch` is inside the namespace this app owns.
pub fn is_gilde_branch(branch: &str) -> bool {
    branch
        .trim_start_matches("refs/heads/")
        .starts_with(BRANCH_PREFIX)
}

/// Six lowercase hex characters from a fresh v4 UUID.
pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..SHORT_ID_LEN].to_string()
}

/// First [`REPO_HASH_LEN`] hex digits of `sha256(path bytes)`.
pub fn path_hash(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())[..REPO_HASH_LEN].to_string()
}

/// Lowercase, fold compatibility characters, collapse everything outside
/// `[a-z0-9]` into single `-`, trim the ends.
///
/// DESIGN specifies NFKD followed by the `[a-z0-9]` filter. We do not depend on
/// a Unicode normalization crate (none is pinned by T00); instead we fold the
/// two compatibility ranges that actually change the *result* of that filter —
/// Latin-1/Latin-Extended-A letters (`é` → `e`) and fullwidth ASCII
/// (`Ａ` → `a`). Every other non-ASCII character — Hangul, CJK, emoji — decomposes
/// to code points that the `[a-z0-9]` filter would drop anyway, so it becomes
/// `-` here exactly as it would after a real NFKD pass.
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_dash = false;

    for ch in name.chars() {
        for folded in fold(ch) {
            for lower in folded.to_lowercase() {
                if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
                    if pending_dash && !out.is_empty() {
                        out.push('-');
                    }
                    pending_dash = false;
                    out.push(lower);
                } else {
                    pending_dash = true;
                }
            }
        }
    }

    out
}

/// Compatibility fold for the ranges that survive `[a-z0-9]` after NFKD.
fn fold(ch: char) -> impl Iterator<Item = char> {
    let folded: &'static str = match ch {
        'À'..='Å' | 'à'..='å' | 'Ā' | 'ā' | 'Ă' | 'ă' | 'Ą' | 'ą' => "a",
        'Æ' | 'æ' => "ae",
        'Ç' | 'ç' | 'Ć' | 'ć' | 'Ĉ' | 'ĉ' | 'Ċ' | 'ċ' | 'Č' | 'č' => "c",
        'Ď' | 'ď' | 'Đ' | 'đ' => "d",
        'È'..='Ë' | 'è'..='ë' | 'Ē' | 'ē' | 'Ĕ' | 'ĕ' | 'Ė' | 'ė' | 'Ę' | 'ę' | 'Ě' | 'ě' => "e",
        'Ĝ' | 'ĝ' | 'Ğ' | 'ğ' | 'Ġ' | 'ġ' | 'Ģ' | 'ģ' => "g",
        'Ĥ' | 'ĥ' | 'Ħ' | 'ħ' => "h",
        'Ì'..='Ï' | 'ì'..='ï' | 'Ĩ' | 'ĩ' | 'Ī' | 'ī' | 'Ĭ' | 'ĭ' | 'Į' | 'į' | 'İ' | 'ı' => "i",
        'Ĵ' | 'ĵ' => "j",
        'Ķ' | 'ķ' => "k",
        'Ĺ' | 'ĺ' | 'Ļ' | 'ļ' | 'Ľ' | 'ľ' | 'Ŀ' | 'ŀ' | 'Ł' | 'ł' => "l",
        'Ñ' | 'ñ' | 'Ń' | 'ń' | 'Ņ' | 'ņ' | 'Ň' | 'ň' => "n",
        'Ò'..='Ö' | 'ò'..='ö' | 'Ø' | 'ø' | 'Ō' | 'ō' | 'Ŏ' | 'ŏ' | 'Ő' | 'ő' => "o",
        'Œ' | 'œ' => "oe",
        'Ŕ' | 'ŕ' | 'Ŗ' | 'ŗ' | 'Ř' | 'ř' => "r",
        'Ś' | 'ś' | 'Ŝ' | 'ŝ' | 'Ş' | 'ş' | 'Š' | 'š' => "s",
        'ß' => "ss",
        'Ţ' | 'ţ' | 'Ť' | 'ť' | 'Ŧ' | 'ŧ' => "t",
        'Ù'..='Ü' | 'ù'..='ü' | 'Ũ' | 'ũ' | 'Ū' | 'ū' | 'Ŭ' | 'ŭ' | 'Ů' | 'ů' | 'Ű' | 'ű' | 'Ų'
        | 'ų' => "u",
        'Ŵ' | 'ŵ' => "w",
        'Ý' | 'ý' | 'ÿ' | 'Ŷ' | 'ŷ' | 'Ÿ' => "y",
        'Ź' | 'ź' | 'Ż' | 'ż' | 'Ž' | 'ž' => "z",
        _ => "",
    };

    let fullwidth = if folded.is_empty() {
        // U+FF01..U+FF5E map onto U+0021..U+007E.
        match ch {
            '\u{FF01}'..='\u{FF5E}' => {
                char::from_u32(ch as u32 - 0xFF01 + 0x21).unwrap_or(ch)
            }
            _ => ch,
        }
    } else {
        ch
    };

    let mut buf: Vec<char> = if folded.is_empty() {
        vec![fullwidth]
    } else {
        folded.chars().collect()
    };
    buf.reverse();
    std::iter::from_fn(move || buf.pop())
}

/// Keep at most [`MAX_SLUG_BODY`] characters and never end on a `-`.
fn truncate_body(sanitized: &str) -> String {
    let mut body: String = sanitized.chars().take(MAX_SLUG_BODY).collect();
    while body.ends_with('-') {
        body.pop();
    }
    body
}

// ---------------------------------------------------------------------------
// Gilde 2: `session_start{NewWorktree}` naming (contract §4.2.2, F-25, A6)
// ---------------------------------------------------------------------------

/// `NewWorktree.slug` is at most this many bytes (`^[a-z0-9][a-z0-9-]{0,62}$`).
pub const MAX_V2_SLUG: usize = 63;
/// The default slug is made of this many words of the prompt.
pub const PROMPT_WORDS: usize = 5;
/// The worktrees directory next to the repository: `<root>/../<repo>-wt/`.
pub const SIBLING_SUFFIX: &str = "-wt";
/// The LP-40 layout root inside the repository (A6): `<repo>/.gilde/worktrees/`.
pub const IN_REPO_DIR: &str = ".gilde";
/// The directory under [`IN_REPO_DIR`] that holds the worktrees.
pub const IN_REPO_WORKTREES: &str = "worktrees";
/// The `.git/info/exclude` entry of the LP-40 layout (never `.gitignore`).
pub const IN_REPO_EXCLUDE: &str = "/.gilde/";

/// The value rule of `NewWorktree.slug` (the contract's own check).
pub fn is_worktree_slug(s: &str) -> bool {
    crate::contract::ipc::sessions::is_worktree_slug(s)
}

/// The slug of the first [`PROMPT_WORDS`] words of `prompt` (ux-ia §3.12):
/// sanitized to `[a-z0-9-]`, at most [`MAX_V2_SLUG`] bytes, never ending on
/// `-`. Empty when nothing survives (a Korean prompt, emoji, punctuation).
pub fn slug_from_prompt(prompt: &str) -> String {
    let words: Vec<&str> = prompt.split_whitespace().take(PROMPT_WORDS).collect();
    let mut slug = sanitize(&words.join(" "));
    slug.truncate(MAX_V2_SLUG);
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// The default when the prompt gives no slug [F-25]: `<yyyymmdd>-<4 hex>`.
pub fn fallback_slug(yyyymmdd: &str, hex4: &str) -> String {
    format!("{yyyymmdd}-{hex4}")
}

/// The local date of `now_secs` as `yyyymmdd` (the chip shows it).
pub fn local_yyyymmdd(now_secs: i64) -> String {
    let t = now_secs as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid out-parameter; `localtime_r` writes it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; `localtime_r` is reentrant.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return "19700101".to_string();
    }
    format!("{:04}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// The default `NewWorktree.slug` for `prompt` (§4.2.2, F-25): the prompt's
/// slug, or `<yyyymmdd>-<4 hex>` when that is empty. Always a valid slug.
pub fn default_worktree_slug(prompt: Option<&str>) -> String {
    let from_prompt = prompt.map(slug_from_prompt).unwrap_or_default();
    if is_worktree_slug(&from_prompt) {
        return from_prompt;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let hex4 = &uuid::Uuid::new_v4().simple().to_string()[..4];
    fallback_slug(&local_yyyymmdd(now), hex4)
}

/// Where a Gilde 2 worktree is created (§4.2.2, PROVISIONAL(LP-40)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeLayout {
    /// `<project root>/../<repo>-wt/<slug>` — the shipped default (LP-40 open).
    Sibling,
    /// `<repo>/.gilde/worktrees/<slug>` with `/.gilde/` in `.git/info/exclude`;
    /// only when the LP-40 flag is on (A6).
    InRepo,
}

/// The directory that holds the worktrees of `layout` for the repository at
/// `project_root` (canonical). `None` when the root has no parent or no UTF-8
/// basename.
pub fn worktree_root(layout: WorktreeLayout, project_root: &Path) -> Option<std::path::PathBuf> {
    match layout {
        WorktreeLayout::Sibling => {
            let parent = project_root.parent()?;
            let repo = project_root.file_name()?.to_str()?;
            Some(parent.join(format!("{repo}{SIBLING_SUFFIX}")))
        }
        WorktreeLayout::InRepo => Some(project_root.join(IN_REPO_DIR).join(IN_REPO_WORKTREES)),
    }
}

/// The path of the worktree `slug` of the repository at `project_root`.
pub fn worktree_path(layout: WorktreeLayout, project_root: &Path, slug: &str) -> Option<std::path::PathBuf> {
    Some(worktree_root(layout, project_root)?.join(slug))
}

/// Whether the location at `location_path` was created by Gilde (A17.7, the
/// one rule; World calls it and never re-implements it): its `branch` is in
/// the `gilde/` namespace **and** the location lies strictly under one of the
/// worktree roots `create_worktree` uses for the repository at
/// `project_root` — the sibling `<repo>-wt/` or `<repo>/.gilde/worktrees/`.
/// Both roots count whatever the LP-40 flag says now: a worktree made while
/// it was on stays Gilde's. Paths are compared as World holds them
/// (canonical), NFC on both sides and case-folded on a case-insensitive
/// volume, with the trailing-`/` prefix rule. A detached HEAD (`None`) is
/// never Gilde-created.
pub fn is_gilde_created(project_root: &Path, location_path: &Path, branch: Option<&str>) -> bool {
    let Some(branch) = branch else { return false };
    if !is_gilde_branch(branch) {
        return false;
    }
    let fold = crate::ide::paths::case_insensitive(project_root);
    [WorktreeLayout::Sibling, WorktreeLayout::InRepo]
        .into_iter()
        .filter_map(|layout| worktree_root(layout, project_root))
        .any(|root| {
            crate::ide::paths::within(location_path, &root, fold) && !crate::ide::paths::within(&root, location_path, fold)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn repo_slug_disambiguates_same_named_repositories() {
        let a = repo_slug(&PathBuf::from("/Users/x/work/api"));
        let b = repo_slug(&PathBuf::from("/Users/x/oss/api"));
        assert!(a.starts_with("api-"), "{a}");
        assert!(b.starts_with("api-"), "{b}");
        assert_ne!(a, b, "the path hash must break the tie");
        assert_eq!(a.len(), "api-".len() + REPO_HASH_LEN);
        // Stable across calls.
        assert_eq!(a, repo_slug(&PathBuf::from("/Users/x/work/api")));
    }

    #[test]
    fn repo_slug_survives_a_weird_basename() {
        let s = repo_slug(&PathBuf::from("/Users/x/  "));
        assert!(s.starts_with("repo-"), "{s}");
        let s = repo_slug(&PathBuf::from("/Users/x/한글-저장소"));
        assert!(s.starts_with("repo-") || s.starts_with("-"), "{s}");
        assert!(!s.contains("한"), "{s}");
    }

    #[test]
    fn sanitize_matches_the_documented_pipeline() {
        assert_eq!(sanitize("Fix PTY Resize"), "fix-pty-resize");
        assert_eq!(sanitize("  ///lots__of---junk!!  "), "lots-of-junk");
        assert_eq!(sanitize("Café Déjà-Vu"), "cafe-deja-vu");
        assert_eq!(sanitize("Ｈｅｌｌｏ１２３"), "hello123");
        assert_eq!(sanitize("한글만"), "");
        assert_eq!(sanitize("v1.2.3"), "v1-2-3");
    }

    #[test]
    fn task_slug_is_bounded_and_unique() {
        let long = "a".repeat(200);
        let slug = task_slug(&long);
        assert_eq!(slug.len(), MAX_SLUG_BODY + 1 + SHORT_ID_LEN);
        assert!(slug.starts_with(&"a".repeat(MAX_SLUG_BODY)));

        let a = task_slug("same name");
        let b = task_slug("same name");
        assert_ne!(a, b, "the short id makes collisions impossible");
        assert!(a.starts_with("same-name-"));
    }

    #[test]
    fn task_slug_never_produces_an_empty_or_dash_edged_body() {
        assert!(task_slug("한글 태스크").starts_with("task-"));
        assert!(task_slug("!!!").starts_with("task-"));
        let slug = task_slug_with_id("trailing dashes ---", "abc123");
        assert_eq!(slug, "trailing-dashes-abc123");
        // A cut exactly on a separator must not leave a trailing dash.
        let name = format!("{}-tail", "b".repeat(MAX_SLUG_BODY - 1));
        let slug = task_slug_with_id(&name, "abc123");
        assert_eq!(slug, format!("{}-abc123", "b".repeat(MAX_SLUG_BODY - 1)));
    }

    #[test]
    fn branches_live_in_the_gilde_namespace() {
        assert_eq!(branch_name("fix-login-ab12cd"), "gilde/fix-login-ab12cd");
        assert!(is_gilde_branch("gilde/fix-login-ab12cd"));
        assert!(is_gilde_branch("refs/heads/gilde/fix-login-ab12cd"));
        assert!(!is_gilde_branch("main"));
        assert!(!is_gilde_branch("refs/heads/feature/gilde-thing"));
    }

    #[test]
    fn prompt_slugs_take_five_words_and_stay_valid() {
        assert_eq!(slug_from_prompt("Fix the login redirect bug in the settings page"), "fix-the-login-redirect-bug");
        assert_eq!(slug_from_prompt("  Café   Déjà  vu "), "cafe-deja-vu");
        assert_eq!(slug_from_prompt("로그인 버그를 고쳐 주세요"), "", "a Korean prompt has no slug");
        assert_eq!(slug_from_prompt("!!! ???"), "");
        let long = format!("{} b c d e", "a".repeat(100));
        let s = slug_from_prompt(&long);
        assert_eq!(s.len(), MAX_V2_SLUG);
        assert!(is_worktree_slug(&s), "{s}");
        // A cut on a separator never leaves a trailing dash.
        let edge = format!("{} b", "a".repeat(62));
        assert_eq!(slug_from_prompt(&edge), "a".repeat(62));
    }

    /// F-25: an empty prompt slug falls back to `<yyyymmdd>-<4 hex>`.
    #[test]
    fn the_empty_slug_default_is_dated_and_valid() {
        assert_eq!(fallback_slug("20261023", "a1b2"), "20261023-a1b2");
        for prompt in [None, Some(""), Some("한글만 있는 프롬프트"), Some("🙂🙂")] {
            let s = default_worktree_slug(prompt);
            assert!(is_worktree_slug(&s), "{prompt:?} → {s}");
            let (date, hex) = s.split_once('-').expect("date-hex");
            assert_eq!(date.len(), 8, "{s}");
            assert!(date.chars().all(|c| c.is_ascii_digit()), "{s}");
            assert_eq!(hex.len(), 4, "{s}");
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{s}");
        }
        assert_eq!(default_worktree_slug(Some("add dark mode")), "add-dark-mode");
        assert_eq!(local_yyyymmdd(0).len(), 8);
    }

    #[test]
    fn worktree_paths_for_both_layouts() {
        let root = PathBuf::from("/Users/x/work/api");
        assert_eq!(
            worktree_path(WorktreeLayout::Sibling, &root, "fix-login"),
            Some(PathBuf::from("/Users/x/work/api-wt/fix-login"))
        );
        assert_eq!(
            worktree_path(WorktreeLayout::InRepo, &root, "fix-login"),
            Some(PathBuf::from("/Users/x/work/api/.gilde/worktrees/fix-login"))
        );
        assert_eq!(worktree_path(WorktreeLayout::Sibling, Path::new("/"), "x"), None);
    }

    /// A17.7: both roots pass; a matching branch outside the roots, or a
    /// location under a root without a `gilde/` branch, does not.
    #[test]
    fn gilde_created_needs_the_branch_and_a_worktree_root() {
        let root = Path::new("/w/api");
        let sibling = worktree_path(WorktreeLayout::Sibling, root, "feat").unwrap();
        let in_repo = worktree_path(WorktreeLayout::InRepo, root, "feat").unwrap();
        assert_eq!(sibling, PathBuf::from("/w/api-wt/feat"));
        assert_eq!(in_repo, PathBuf::from("/w/api/.gilde/worktrees/feat"));
        for loc in [&sibling, &in_repo] {
            assert!(is_gilde_created(root, loc, Some("gilde/feat")), "{loc:?}");
            assert!(is_gilde_created(root, loc, Some("refs/heads/gilde/feat")), "{loc:?}");
        }
        // The branch matches, the path does not.
        for loc in [
            "/w/elsewhere/feat",
            "/w/api-wt2/feat",
            "/w/api-wtx",
            "/w/api/feat",
            "/w/api/.gilde/feat",
            "/w/api-wt",
            "/w/api/.gilde/worktrees",
            "/w/api",
        ] {
            assert!(!is_gilde_created(root, Path::new(loc), Some("gilde/feat")), "{loc}");
        }
        // The path matches, the branch does not.
        for branch in [None, Some("main"), Some("feature/x"), Some("refs/heads/feature/gilde/x"), Some("gildex/feat")] {
            assert!(!is_gilde_created(root, &sibling, branch), "{branch:?}");
            assert!(!is_gilde_created(root, &in_repo, branch), "{branch:?}");
        }
        // Another repository's sibling root is not this one's.
        assert!(!is_gilde_created(Path::new("/w/ap"), &sibling, Some("gilde/feat")));
    }

    #[test]
    fn gilde_created_compares_unicode_forms() {
        use unicode_normalization::UnicodeNormalization as _;
        let root = Path::new("/w/저장소");
        let nfd: String = "/w/저장소-wt/feat".nfd().collect();
        assert!(is_gilde_created(root, Path::new(&nfd), Some("gilde/feat")));
    }

    #[test]
    fn short_id_is_hex_and_the_documented_length() {
        let id = short_id();
        assert_eq!(id.len(), SHORT_ID_LEN);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
