//! The static literal canary over Mach-O binaries (§3.10 input 2).
//!
//! A streaming byte search of an installed `claude` binary for the pinned
//! entries of `resources/canary-literals.json`, grouped in three sets:
//! `conversation`, `paneHosting` and `teamStore` (§3.10). The scan only
//! **reads bytes**: it never executes the binary.
//!
//! Rules this module enforces:
//! - every entry is `{"literal": …}` or `{"regex": …}`; a lint rejects
//!   literals that pin a minified identifier (F-08) — those changed within one
//!   patch (2.1.306 → 2.1.307, R6 §6 A-P5), shapes did not;
//! - presence only: hit counts are never compared;
//! - only Mach-O files are scanned; anything else, an unreadable file, an I/O
//!   error or a scan past its deadline is [`ScanOutcome::Unknown`] — which
//!   keeps a version `Unverified` and never makes it `Failed` (N-07, PM-43).
//!
//! The script bundle a single-file executable carries sits near the end of the
//! file, so the scan walks the file from the end in [`CHUNK_BYTES`] steps
//! (with an [`OVERLAP_BYTES`] seam shared by neighbouring steps) and stops as
//! soon as every entry was seen. A missing entry means a full pass.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use aho_corasick::{AhoCorasick, MatchKind};
use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use super::CompatDataError;
use crate::contract::compat::CanaryResult;
use crate::contract::ids::hash16;
use crate::contract::session::BinaryFingerprint;

/// The bundled pinned entries.
pub const BUNDLED_CANARY_LITERALS: &str = include_str!("../../resources/canary-literals.json");

/// Bytes read per step.
pub const CHUNK_BYTES: usize = 8 << 20;
/// Bytes shared by neighbouring steps, so a match that straddles a step
/// boundary is still seen whole. It bounds every literal (and, in practice,
/// every shape-regex match: minified functions are short).
pub const OVERLAP_BYTES: usize = 4 << 10;
pub const MAX_LITERAL_BYTES: usize = 256;
pub const MAX_REGEX_BYTES: usize = 512;

/// The three pinned sets of §3.10 input 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CanarySet {
    /// Missing → `conversation: Failed`.
    Conversation,
    /// Missing → `paneHosting: Failed`.
    PaneHosting,
    /// Missing → feature `teamStoreVerified` false (§5.2.4).
    TeamStore,
}

impl CanarySet {
    pub const ALL: [CanarySet; 3] = [
        CanarySet::Conversation,
        CanarySet::PaneHosting,
        CanarySet::TeamStore,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CanarySet::Conversation => "conversation",
            CanarySet::PaneHosting => "paneHosting",
            CanarySet::TeamStore => "teamStore",
        }
    }
}

/// One pinned entry: exactly one of `literal` / `regex` [F-08].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CanaryEntry {
    Literal(String),
    Regex(String),
}

impl CanaryEntry {
    /// How the entry appears in a `Missing` result: the literal, or `/regex/`.
    pub fn label(&self) -> String {
        match self {
            CanaryEntry::Literal(s) => s.clone(),
            CanaryEntry::Regex(s) => format!("/{s}/"),
        }
    }
}

// ---------------------------------------------------------------------------
// canary-literals.json
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLiterals {
    schema: u32,
    provenance: String,
    evidence: BTreeMap<CanarySet, String>,
    sets: BTreeMap<CanarySet, Vec<CanaryEntry>>,
}

/// The parsed and linted pinned entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanaryLiterals {
    sets: BTreeMap<CanarySet, Vec<CanaryEntry>>,
    /// First 16 hex of SHA-256 over the file: a cached scan made with another
    /// entry list is stale and runs again.
    hash: String,
}

impl CanaryLiterals {
    pub fn parse(json: &str) -> Result<CanaryLiterals, CompatDataError> {
        let err = |m: String| CompatDataError::new(format!("canary-literals.json: {m}"));
        let raw: RawLiterals = serde_json::from_str(json).map_err(|e| err(e.to_string()))?;
        if raw.schema != 1 {
            return Err(err(format!("unsupported schema {}", raw.schema)));
        }
        if raw.provenance.trim().is_empty() {
            return Err(err("empty provenance".into()));
        }
        for set in CanarySet::ALL {
            let entries = raw.sets.get(&set).map(Vec::as_slice).unwrap_or_default();
            if entries.is_empty() {
                return Err(err(format!("set {} is missing or empty", set.name())));
            }
            if raw.evidence.get(&set).is_none_or(|e| e.trim().is_empty()) {
                return Err(err(format!("set {} has no evidence", set.name())));
            }
            for entry in entries {
                let checked = match entry {
                    CanaryEntry::Literal(lit) => lint_literal(lit),
                    CanaryEntry::Regex(src) => lint_regex(src),
                };
                checked.map_err(|m| err(format!("{}: {m}", set.name())))?;
            }
        }
        Ok(CanaryLiterals {
            sets: raw.sets,
            hash: hash16(json.as_bytes()),
        })
    }

    /// The bundled file. The T-compat tests parse it, so a failure here is a
    /// build defect, never a user condition.
    pub fn bundled() -> CanaryLiterals {
        CanaryLiterals::parse(BUNDLED_CANARY_LITERALS)
            .expect("the bundled canary-literals.json must parse")
    }

    pub fn sets(&self) -> &BTreeMap<CanarySet, Vec<CanaryEntry>> {
        &self.sets
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }
}

// ---------------------------------------------------------------------------
// the identifier lint [F-08]
// ---------------------------------------------------------------------------

/// Reserved words that may precede `(` without being a minified call.
const CALL_KEYWORDS: &[&str] = &["if", "for", "new", "do", "in", "of"];

fn is_ident_start(b: u8) -> bool {
    b == b'$' || b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_part(b: u8) -> bool {
    b == b'$' || b == b'_' || b.is_ascii_alphanumeric()
}

/// The whole literal is a short identifier: `^[$_A-Za-z][$\w]{0,4}$` (§3.10).
fn identifier_like(lit: &str) -> bool {
    let b = lit.as_bytes();
    (1..=5).contains(&b.len()) && is_ident_start(b[0]) && b[1..].iter().all(|c| is_ident_part(*c))
}

/// The first identifier token of `text` that has the shape minifiers produce:
/// a `$` inside, a short token mixing letters and digits or both cases or
/// starting with `_` (`F$r`, `_Lr`, `y0o`), or a short lowercase name called
/// like a function (`z()`, `gs()`). Plain short words (`var g=5000`, `-d`,
/// `in-process`) pass: they are argv or syntax, not renamed symbols.
pub fn minified_token(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            // a number (or a number-led run): never a name
            while i < b.len() && is_ident_part(b[i]) {
                i += 1;
            }
            continue;
        }
        if !is_ident_start(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_ident_part(b[i]) {
            i += 1;
        }
        let tok = &text[start..i];
        let next = b.get(i).copied();
        let short = tok.len() <= 4;
        let has_digit = tok.bytes().any(|c| c.is_ascii_digit());
        let has_upper = tok.bytes().any(|c| c.is_ascii_uppercase());
        let has_lower = tok.bytes().any(|c| c.is_ascii_lowercase());
        let minified = tok.contains('$')
            || (short && has_digit)
            || (short && has_upper && has_lower)
            || (short && tok.starts_with('_'))
            || (tok.len() <= 3
                && tok.bytes().all(|c| c.is_ascii_lowercase())
                && next == Some(b'(')
                && !CALL_KEYWORDS.contains(&tok));
        if minified {
            return Some(tok.to_string());
        }
    }
    None
}

/// Lint one `{"literal": …}` entry [F-08].
pub fn lint_literal(lit: &str) -> Result<(), String> {
    if lit.is_empty() || lit.len() > MAX_LITERAL_BYTES {
        return Err(format!(
            "literal {lit:?}: length must be 1..={MAX_LITERAL_BYTES} bytes"
        ));
    }
    if lit.bytes().any(|c| c.is_ascii_control()) {
        return Err(format!("literal {lit:?}: control character"));
    }
    if identifier_like(lit) {
        return Err(format!(
            "literal {lit:?} is identifier-like (a minified name)"
        ));
    }
    if let Some(tok) = minified_token(lit) {
        return Err(format!(
            "literal {lit:?} pins the minified name {tok:?}; use a shape regex"
        ));
    }
    Ok(())
}

/// The literal text a regex spells out, with every class, escape class,
/// quantifier and operator replaced by a space — what the identifier lint
/// reads for a `{"regex": …}` entry.
pub fn regex_skeleton(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => {
                match c.get(i + 1) {
                    Some(n) if n.is_ascii_alphanumeric() => out.push(' '), // \w \d \b \x…
                    Some(n) => out.push(*n),                               // \( \{ \$ …
                    None => {}
                }
                i += 2;
            }
            '[' => {
                // skip the class: `[`, optional `^`, a leading `]`, escapes
                i += 1;
                if c.get(i) == Some(&'^') {
                    i += 1;
                }
                if c.get(i) == Some(&']') {
                    i += 1;
                }
                while i < c.len() && c[i] != ']' {
                    i += if c[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
                out.push(' ');
            }
            '{' => {
                // a quantifier {m}, {m,}, {m,n}
                let mut j = i + 1;
                while j < c.len() && (c[j].is_ascii_digit() || c[j] == ',') {
                    j += 1;
                }
                if c.get(j) == Some(&'}') {
                    i = j + 1;
                } else {
                    i += 1;
                }
                out.push(' ');
            }
            '(' => {
                // a group, possibly with flags `(?i)` / `(?:`
                i += 1;
                if c.get(i) == Some(&'?') {
                    while i < c.len() && c[i] != ':' && c[i] != ')' {
                        i += 1;
                    }
                    i += 1;
                }
                out.push(' ');
            }
            '.' | '^' | '$' | '|' | '*' | '+' | '?' | ')' => {
                out.push(' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

fn compile_regex(src: &str) -> Result<Regex, String> {
    // ASCII classes (`\w` = [0-9A-Za-z_]) over raw bytes: minified JS names are ASCII.
    RegexBuilder::new(src)
        .unicode(false)
        .build()
        .map_err(|e| format!("regex {src:?} does not compile: {e}"))
}

/// Lint one `{"regex": …}` entry: it compiles, cannot match the empty input,
/// and its literal skeleton pins no minified name [F-08].
pub fn lint_regex(src: &str) -> Result<(), String> {
    if src.is_empty() || src.len() > MAX_REGEX_BYTES {
        return Err(format!(
            "regex {src:?}: length must be 1..={MAX_REGEX_BYTES} bytes"
        ));
    }
    let re = compile_regex(src)?;
    if re.is_match(b"") {
        return Err(format!("regex {src:?} matches the empty input"));
    }
    if let Some(tok) = minified_token(&regex_skeleton(src)) {
        return Err(format!(
            "regex {src:?} pins the minified name {tok:?}; use a class such as [$\\w]+"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// scanning
// ---------------------------------------------------------------------------

/// Why a scan could not give a verdict (`CanaryResult::Unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    /// Open failed (permissions, gone).
    Unreadable,
    /// A directory, FIFO, device …
    NotRegularFile,
    /// Not a Mach-O image (a launcher script, a JS file) [N-07].
    NotMachO,
    /// Past the deadline (a cold or stalled disk) [PM-43].
    Timeout,
    /// A read failed or the file shrank mid-scan.
    Io,
    /// The path no longer holds the binary that was asked for (replaced by an
    /// update between the request and the scan, A1): its bytes are gone.
    Changed,
}

impl UnknownReason {
    /// Whether a later request scans the same file again. A wrong file type or
    /// a replaced file is a property of that fingerprint; the other reasons
    /// may pass next time.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            UnknownReason::Unreadable | UnknownReason::Timeout | UnknownReason::Io
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UnknownReason::Unreadable => "unreadable",
            UnknownReason::NotRegularFile => "notRegularFile",
            UnknownReason::NotMachO => "notMachO",
            UnknownReason::Timeout => "timeout",
            UnknownReason::Io => "io",
            UnknownReason::Changed => "changed",
        }
    }
}

/// Whether one set was found in a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetStatus {
    Present,
    Missing,
    /// No verdict (not scanned, or the scan could not run).
    Unknown,
}

/// The result of scanning one binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    /// The scan covered the binary; `missing` lists, per set, the labels of the
    /// entries not found (empty map = every set present).
    Scanned {
        missing: BTreeMap<CanarySet, Vec<String>>,
    },
    Unknown(UnknownReason),
}

impl ScanOutcome {
    pub fn passed(&self) -> bool {
        matches!(self, ScanOutcome::Scanned { missing } if missing.is_empty())
    }

    /// A result that a rescan of the same bytes cannot change.
    pub fn is_final(&self) -> bool {
        match self {
            ScanOutcome::Scanned { .. } => true,
            ScanOutcome::Unknown(r) => !r.retryable(),
        }
    }

    pub fn set_status(&self, set: CanarySet) -> SetStatus {
        match self {
            ScanOutcome::Scanned { missing } if missing.contains_key(&set) => SetStatus::Missing,
            ScanOutcome::Scanned { .. } => SetStatus::Present,
            ScanOutcome::Unknown(_) => SetStatus::Unknown,
        }
    }

    /// The contract value: `Missing` lists `"<set>: <label>"`.
    pub fn to_result(&self) -> CanaryResult {
        match self {
            ScanOutcome::Scanned { missing } if missing.is_empty() => CanaryResult::Pass,
            ScanOutcome::Scanned { missing } => CanaryResult::Missing {
                literals: missing
                    .iter()
                    .flat_map(|(set, labels)| {
                        labels.iter().map(move |l| format!("{}: {l}", set.name()))
                    })
                    .collect(),
            },
            ScanOutcome::Unknown(_) => CanaryResult::Unknown,
        }
    }
}

/// Whether `head` (the first 8 bytes) starts a Mach-O image: thin 32/64-bit
/// in either byte order, or a universal ("fat") file. A fat header shares its
/// magic with another class-file format; its second word (the architecture count) is
/// small, where a class file has its version numbers (≥ 45).
pub fn is_macho(head: &[u8]) -> bool {
    let Some(word) = head.get(0..8) else {
        return false;
    };
    let le = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
    if matches!(le, 0xfeed_face | 0xfeed_facf | 0xcefa_edfe | 0xcffa_edfe) {
        return true;
    }
    let be = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    if be == 0xcafe_babe || be == 0xcafe_babf {
        let narch = u32::from_be_bytes([word[4], word[5], word[6], word[7]]);
        return (1..=32).contains(&narch);
    }
    false
}

/// `(size, mtime, inode)` of the file `path` resolves to (`stat`, follows
/// symlinks) — the `claude_compat_observed` key (§3.9).
pub fn fingerprint_of(path: &Path) -> io::Result<BinaryFingerprint> {
    Ok(fingerprint_of_meta(&std::fs::metadata(path)?))
}

fn fingerprint_of_meta(m: &std::fs::Metadata) -> BinaryFingerprint {
    BinaryFingerprint {
        size: m.len(),
        mtime_ms: m
            .mtime()
            .saturating_mul(1000)
            .saturating_add(m.mtime_nsec() / 1_000_000),
        inode: m.ino(),
    }
}

/// Run the calling thread at `QOS_CLASS_UTILITY` (§2.4 B10, §2.5): the canary
/// never competes with the main thread.
pub fn enter_utility_qos() {
    // SAFETY: a plain call on the current thread with a valid class constant.
    #[cfg(target_os = "macos")]
    let _ = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
}

/// The compiled matcher for one [`CanaryLiterals`].
#[derive(Debug, Clone)]
pub struct Scanner {
    literals: Option<AhoCorasick>,
    /// Aho-Corasick pattern index → (set, label).
    literal_ids: Vec<(CanarySet, String)>,
    regexes: Vec<(CanarySet, String, Regex)>,
    /// Every entry in file order, for the `missing` lists.
    order: Vec<(CanarySet, String)>,
}

struct Progress {
    literal_found: Vec<bool>,
    regex_found: Vec<bool>,
    left: usize,
}

impl Scanner {
    pub fn new(lits: &CanaryLiterals) -> Result<Scanner, CompatDataError> {
        let mut literal_ids = Vec::new();
        let mut patterns: Vec<&str> = Vec::new();
        let mut regexes = Vec::new();
        let mut order = Vec::new();
        for (set, entries) in &lits.sets {
            for entry in entries {
                order.push((*set, entry.label()));
                match entry {
                    CanaryEntry::Literal(lit) => {
                        if lit.len() > OVERLAP_BYTES {
                            return Err(CompatDataError::new(format!(
                                "literal longer than the scan seam: {lit:?}"
                            )));
                        }
                        literal_ids.push((*set, entry.label()));
                        patterns.push(lit);
                    }
                    CanaryEntry::Regex(src) => {
                        let re = compile_regex(src).map_err(CompatDataError::new)?;
                        regexes.push((*set, entry.label(), re));
                    }
                }
            }
        }
        let literals = if patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::Standard)
                    .build(&patterns)
                    .map_err(|e| CompatDataError::new(format!("literal automaton: {e}")))?,
            )
        };
        Ok(Scanner {
            literals,
            literal_ids,
            regexes,
            order,
        })
    }

    fn progress(&self) -> Progress {
        Progress {
            literal_found: vec![false; self.literal_ids.len()],
            regex_found: vec![false; self.regexes.len()],
            left: self.literal_ids.len() + self.regexes.len(),
        }
    }

    fn feed(&self, chunk: &[u8], p: &mut Progress) {
        if let Some(ac) = &self.literals {
            if p.literal_found.iter().any(|f| !f) {
                // Overlapping: a literal inside another literal's match still counts.
                for m in ac.find_overlapping_iter(chunk) {
                    let i = m.pattern().as_usize();
                    if !p.literal_found[i] {
                        p.literal_found[i] = true;
                        p.left -= 1;
                        if p.left == 0 {
                            return;
                        }
                    }
                }
            }
        }
        for (i, (_, _, re)) in self.regexes.iter().enumerate() {
            if !p.regex_found[i] && re.is_match(chunk) {
                p.regex_found[i] = true;
                p.left -= 1;
            }
        }
    }

    fn outcome(&self, p: &Progress) -> ScanOutcome {
        let mut found: Vec<&str> = Vec::new();
        for (i, (_, label)) in self.literal_ids.iter().enumerate() {
            if p.literal_found[i] {
                found.push(label);
            }
        }
        for (i, (_, label, _)) in self.regexes.iter().enumerate() {
            if p.regex_found[i] {
                found.push(label);
            }
        }
        let mut missing: BTreeMap<CanarySet, Vec<String>> = BTreeMap::new();
        for (set, label) in &self.order {
            if !found.contains(&label.as_str()) {
                missing.entry(*set).or_default().push(label.clone());
            }
        }
        ScanOutcome::Scanned { missing }
    }

    /// Match an in-memory buffer (no Mach-O check, no chunking).
    pub fn scan_bytes(&self, bytes: &[u8]) -> ScanOutcome {
        let mut p = self.progress();
        self.feed(bytes, &mut p);
        self.outcome(&p)
    }

    /// Scan the file at `path` (§3.10 input 2). Never executes it. With
    /// `expected`, the opened file must still have that fingerprint, so the
    /// bytes scanned are the binary that was asked about [A1].
    pub fn scan_path(
        &self,
        path: &Path,
        expected: Option<&BinaryFingerprint>,
        deadline: Duration,
    ) -> ScanOutcome {
        let started = Instant::now();
        // O_NONBLOCK: opening a FIFO must not wait for a writer.
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
        {
            Ok(f) => f,
            Err(_) => return ScanOutcome::Unknown(UnknownReason::Unreadable),
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(_) => return ScanOutcome::Unknown(UnknownReason::Io),
        };
        if !meta.is_file() {
            return ScanOutcome::Unknown(UnknownReason::NotRegularFile);
        }
        if expected.is_some_and(|fp| *fp != fingerprint_of_meta(&meta)) {
            return ScanOutcome::Unknown(UnknownReason::Changed);
        }
        let len = meta.len();
        let mut head = [0u8; 8];
        if len < head.len() as u64 {
            return ScanOutcome::Unknown(UnknownReason::NotMachO);
        }
        if file.read_exact_at(&mut head, 0).is_err() {
            return ScanOutcome::Unknown(UnknownReason::Io);
        }
        if !is_macho(&head) {
            return ScanOutcome::Unknown(UnknownReason::NotMachO);
        }

        let step = CHUNK_BYTES as u64;
        let seam = OVERLAP_BYTES as u64;
        let mut buf = vec![0u8; (step + seam).min(len) as usize];
        let mut p = self.progress();
        let mut end = len;
        while end > 0 && p.left > 0 {
            if started.elapsed() > deadline {
                return ScanOutcome::Unknown(UnknownReason::Timeout);
            }
            let start = end.saturating_sub(step);
            let stop = (end + seam).min(len);
            let n = (stop - start) as usize;
            if file.read_exact_at(&mut buf[..n], start).is_err() {
                return ScanOutcome::Unknown(UnknownReason::Io);
            }
            self.feed(&buf[..n], &mut p);
            end = start;
        }
        self.outcome(&p)
    }
}
