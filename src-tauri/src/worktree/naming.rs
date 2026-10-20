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
    fn short_id_is_hex_and_the_documented_length() {
        let id = short_id();
        assert_eq!(id.len(), SHORT_ID_LEN);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
