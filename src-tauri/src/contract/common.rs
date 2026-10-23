//! Common value types (contract §3.4.1) and the rules shared by every type
//! (§3.1): the integer rule, `EpochMs`, canonical paths, vendors.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::contract::validate::{Checked, Invalid, Validate};

/// The largest integer JSON/JS can carry exactly: 2^53 − 1 (§3.1 rule 3).
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Wall clock, UTC milliseconds, `0 < v < 2^53` (§3.1 rule 3, §3.4.1).
/// Every field of this type carries `#[ts(type = "number")]`.
pub type EpochMs = i64;

/// Clamp a 64-bit count to 2^53 − 1 before it crosses IPC (§3.1 rule 3,
/// T-contract-ints).
pub fn clamp_safe_u64(v: u64) -> u64 {
    v.min(MAX_SAFE_INTEGER as u64)
}

/// Clamp a signed 64-bit value into `±(2^53 − 1)`.
pub fn clamp_safe_i64(v: i64) -> i64 {
    v.clamp(-MAX_SAFE_INTEGER, MAX_SAFE_INTEGER)
}

/// `0 < v < 2^53` (§3.4.1 `EpochMs`).
pub fn is_epoch_ms(v: EpochMs) -> bool {
    v > 0 && v <= MAX_SAFE_INTEGER
}

/// Value rule for an `EpochMs` request field.
pub fn epoch_ms(field: &str, v: EpochMs) -> Checked {
    if is_epoch_ms(v) {
        Ok(())
    } else {
        Err(Invalid::new(field, "range"))
    }
}

/// The four vendors. agent-mesh's "gemini" adapter maps to `Agy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Vendor {
    Claude,
    Codex,
    Cursor,
    Agy,
}

impl Vendor {
    pub const ALL: [Vendor; 4] = [Vendor::Claude, Vendor::Codex, Vendor::Cursor, Vendor::Agy];

    /// The wire spelling, also the input of id derivations (§3.2).
    pub const fn as_str(self) -> &'static str {
        match self {
            Vendor::Claude => "claude",
            Vendor::Codex => "codex",
            Vendor::Cursor => "cursor",
            Vendor::Agy => "agy",
        }
    }

    pub fn parse(s: &str) -> Option<Vendor> {
        Vendor::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

impl Validate for Vendor {}

/// An absolute, canonical (realpath) UTF-8 path, ≤ 1024 bytes, no NUL, no
/// trailing `/`, not `/` (§3.4.1). Paths cross IPC already realpath-ed by the
/// core (§3.1 rule 7); the WebView never constructs one it sends back except a
/// session-relative `path` for `ide_open`.
///
/// A45.1: an `AbsPath` never carries a character that changes how it reads —
/// C0 controls, DEL, C1 controls, the bidi controls and the zero-width
/// characters ([`AbsPath::is_forbidden_char`]). Such a value is refused with
/// rule `path`; any other form error with rule `absPath`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(transparent)]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct AbsPath(String);

impl AbsPath {
    pub const MAX_BYTES: usize = 1024;

    /// Accept `s` if it has the canonical form. This is a form check only; the
    /// core realpaths before constructing one (it never trusts the WebView).
    pub fn parse(s: impl Into<String>) -> Result<AbsPath, Invalid> {
        let s = s.into();
        match Self::violation(&s) {
            None => Ok(AbsPath(s)),
            Some(rule) => Err(Invalid::new("$", rule)),
        }
    }

    pub fn is_canonical_form(s: &str) -> bool {
        Self::violation(s).is_none()
    }

    /// The rule `s` breaks, if any: `path` for a forbidden character
    /// (A45.1), `absPath` for any other form error.
    pub fn violation(s: &str) -> Option<&'static str> {
        if s.chars().any(Self::is_forbidden_char) {
            return Some("path");
        }
        let form = s.len() <= Self::MAX_BYTES
            && s.starts_with('/')
            && s != "/"
            && !s.ends_with('/')
            && !s.contains("//")
            && !s.split('/').any(|c| c == "." || c == "..");
        (!form).then_some("absPath")
    }

    /// A45.1: C0 controls (U+0000–001F, NUL included), DEL, C1 controls
    /// (U+0080–009F), the bidi controls (U+200E/200F, U+202A–202E,
    /// U+2066–2069) and the zero-width characters (U+200B–200D, U+FEFF).
    pub const fn is_forbidden_char(c: char) -> bool {
        matches!(
            c,
            '\u{0}'..='\u{1F}'
                | '\u{7F}'..='\u{9F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for AbsPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AbsPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Validate for AbsPath {
    fn validate(&self) -> Checked {
        match Self::violation(&self.0) {
            None => Ok(()),
            Some(rule) => Err(Invalid::new("", rule)),
        }
    }
}

/// Identity colours [M12, R3 §2.2].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum IdentityColour {
    Red,
    Blue,
    Green,
    Yellow,
    Purple,
    Orange,
    Pink,
    Cyan,
}

/// `Likely` = PTY engine confidence < 0.6, or a source in `permissionDenied`
/// [ux-ia §4.4].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum Confidence {
    High,
    Likely,
}

/// Claude permission modes [R4 §1.9].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    DontAsk,
    Auto,
    BypassPermissions,
}

impl PermissionMode {
    /// The CLI spelling for `--permission-mode` (camelCase, as on the wire).
    pub const fn as_cli(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Auto => "auto",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }
}

impl Validate for PermissionMode {}

/// Per-version compat state (§3.10) [C2].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum CompatState {
    Verified,
    Unverified,
    Failed,
}

/// A localized line: an i18n key plus data parameters (never content). Used by
/// notices, sheet texts and source notes (`Notice` in §4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct Notice {
    pub key: String,
    #[ts(type = "Record<string, string | number | boolean>")]
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl Notice {
    pub fn new(key: &str) -> Notice {
        Notice {
            key: key.to_string(),
            params: serde_json::Map::new(),
        }
    }

    pub fn param(mut self, name: &str, value: impl Into<String>) -> Notice {
        self.params
            .insert(name.to_string(), serde_json::Value::String(value.into()));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_path_form() {
        for good in ["/a", "/Users/x/proj", "/private/tmp/x y"] {
            assert!(AbsPath::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "/", "a/b", "/a/", "/a/../b", "/a/./b", "/a//b", "/a\0b"] {
            assert!(AbsPath::parse(bad).is_err(), "{bad:?}");
        }
        assert!(AbsPath::parse(format!("/{}", "a".repeat(1023))).is_ok());
        assert!(AbsPath::parse(format!("/{}", "a".repeat(1024))).is_err());
    }

    #[test]
    fn abs_path_refuses_every_character_that_changes_how_it_reads() {
        // A45.1: C0 (NUL, \t, \n, \r, ESC …), DEL, C1, bidi, zero-width.
        let forbidden: Vec<char> = ('\u{0}'..='\u{1F}')
            .chain('\u{7F}'..='\u{9F}')
            .chain(['\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{FEFF}'])
            .chain('\u{202A}'..='\u{202E}')
            .chain('\u{2066}'..='\u{2069}')
            .collect();
        for c in forbidden {
            for s in [format!("/a{c}b"), format!("/{c}"), format!("/proj/tools{c}{c}/usr/local/bin/claude")] {
                let err = AbsPath::parse(s.clone()).expect_err(&format!("{s:?}"));
                assert_eq!(err.rule, "path", "{s:?}");
                assert_eq!(AbsPath(s.clone()).validate().unwrap_err().rule, "path", "{s:?}");
            }
        }
        // Printable non-ASCII stays a path (Korean folder names, spaces, accents).
        for good in ["/Users/x/프로젝트", "/a b/c", "/caf\u{e9}"] {
            assert!(AbsPath::parse(good).is_ok(), "{good:?}");
        }
        assert_eq!(AbsPath::parse("/a/../b").unwrap_err().rule, "absPath");
        assert_eq!(AbsPath(String::from("rel")).validate().unwrap_err().rule, "absPath");
    }

    #[test]
    fn epoch_bounds() {
        assert!(!is_epoch_ms(0));
        assert!(is_epoch_ms(1));
        assert!(is_epoch_ms(MAX_SAFE_INTEGER));
        assert!(!is_epoch_ms(MAX_SAFE_INTEGER + 1));
        assert_eq!(clamp_safe_u64(u64::MAX), MAX_SAFE_INTEGER as u64);
    }

    #[test]
    fn vendor_wire_names() {
        for v in Vendor::ALL {
            assert_eq!(serde_json::to_string(&v).unwrap(), format!("\"{}\"", v.as_str()));
            assert_eq!(Vendor::parse(v.as_str()), Some(v));
        }
    }
}
