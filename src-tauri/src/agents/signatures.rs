//! The hot-reloadable signature file — DESIGN §6-4, M1-PLAN T09.9.
//!
//! Two consumers, one file:
//!
//! * **Status inference** (T16) matches the `agents.<key>.{working,waiting,idle}`
//!   patterns against the visible tail of a PTY session.
//! * **The Process Monitor** (T07) uses `monitorPatterns` to classify processes
//!   into [`crate::supervise::ProcGroupKind`].
//!
//! ## Why the file is user-editable
//!
//! DESIGN §0 rule 7: status is reported *with its evidence*, and these patterns
//! are the weakest link. Claude Code is 2.1.263 today and "esc to interrupt"
//! will change. The design absorbs that instead of denying it — the Inspector
//! shows which pattern matched and offers to open this file. A shipped app that
//! needed a release to follow a vendor's spinner text would be broken by
//! design.
//!
//! ## Failure policy
//!
//! Every failure here is a warning, never an error:
//!
//! * unreadable / unparsable user file ⇒ the bundled defaults, plus a warning
//! * a regex that does not compile ⇒ **that pattern** is dropped, the rest of
//!   the agent's set stays live (M1-PLAN T09.9, explicitly)
//! * an unknown agent key ⇒ kept and compiled; a future CLI must not need a
//!   Rust change to be recognised
//!
//! The alternative — refusing to load — would leave the app with no status
//! inference at all because of one stray bracket.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{GildeError, Result};
use crate::model::status::PatternClass;
use crate::paths::AppPaths;
use crate::supervise::procscan;

/// The bundled defaults, compiled into the binary.
///
/// Embedded rather than shipped as a Tauri resource so that a mis-packaged
/// bundle degrades to "the defaults" instead of "no signatures at all", and so
/// `cargo test` needs no bundle at all.
pub const DEFAULT_SIGNATURES_JSON: &str =
    include_str!("../../resources/agent-signatures.default.json");

/// Quiet period before a file edit is picked up. Well inside the 2 s that
/// M1-PLAN T09.9 requires.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

/// Fallback when an agent's entry does not pin one (DESIGN §10-3
/// `status.idleAfterMs`).
pub const DEFAULT_IDLE_AFTER_MS: u64 = 1_200;

// ---------------------------------------------------------------------------
// on-disk shape
// ---------------------------------------------------------------------------

/// One agent's entry. Every field is `Option` so that a user file which only
/// pins `idleAfterMs` keeps the default patterns — "absent" and "empty list"
/// are different intents and the merge relies on telling them apart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentSignatureSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<Vec<String>>,
}

impl AgentSignatureSpec {
    /// Field-wise merge: `other` wins where it says something.
    fn merged_with(&self, other: &AgentSignatureSpec) -> AgentSignatureSpec {
        AgentSignatureSpec {
            idle_after_ms: other.idle_after_ms.or(self.idle_after_ms),
            working: other.working.clone().or_else(|| self.working.clone()),
            waiting: other.waiting.clone().or_else(|| self.waiting.clone()),
            idle: other.idle.clone().or_else(|| self.idle.clone()),
        }
    }
}

/// `monitorPatterns` — the Process Monitor half of the file (DESIGN §8-7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MonitorPatternsSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_residue: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<Vec<String>>,
}

impl MonitorPatternsSpec {
    fn merged_with(&self, other: &MonitorPatternsSpec) -> MonitorPatternsSpec {
        MonitorPatternsSpec {
            agent: other.agent.clone().or_else(|| self.agent.clone()),
            external_residue: other
                .external_residue
                .clone()
                .or_else(|| self.external_residue.clone()),
            mcp_server: other.mcp_server.clone().or_else(|| self.mcp_server.clone()),
        }
    }
}

/// `readySignature.<vendor>` — the vendor's input line (and footer) that makes
/// a settled grid `Idle` at 0.9 (contract §6.2.6, F-48, N-03/A5). A vendor (or
/// a version) without one gets the settled-grid fallback at 0.5.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReadySignatureSpec {
    /// Matched against the shapes of a real TUI on this machine. An
    /// unverified signature still yields `Idle` 0.9 when it matches, but
    /// readiness then also accepts the 0.5 fallback (§6.2.2), so a wrong guess
    /// can never keep a session in Starting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// The input row (`❯ …`, `› …`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// A footer row that must also be visible (mode bar, shortcut hint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    /// How many bottom rows are searched (default 8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<usize>,
}

impl ReadySignatureSpec {
    fn merged_with(&self, other: &ReadySignatureSpec) -> ReadySignatureSpec {
        ReadySignatureSpec {
            verified: other.verified.or(self.verified),
            input: other.input.clone().or_else(|| self.input.clone()),
            footer: other.footer.clone().or_else(|| self.footer.clone()),
            rows: other.rows.or(self.rows),
        }
    }
}

/// `promptCard.<vendor>` — the screen-prompt grammar (contract §6.2.4). Every
/// pattern is one Rust `regex` matched against one grid row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptCardSpec {
    /// An option line: group 1 = the digit, group 2 = the label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option: Option<String>,
    /// The selection marker before the digit (`❯`, `>`, `›`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    /// An indented line without a digit: a wrapped label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
    /// An option's own description line (indented ≥ 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A plain or titled rule between options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divider: Option<String>,
    /// The `☐ <header>` line above the question: group 1 = the header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The footer below the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    /// The footer phrase that offers *Allow with a note*.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amend: Option<String>,
    /// A cursor line of an unnumbered setup menu: group 1 = the indent,
    /// group 2 = the label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_marker: Option<String>,
    /// Option 1's label while *Allow with a note* is being typed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amend_label: Option<String>,
}

impl PromptCardSpec {
    fn merged_with(&self, other: &PromptCardSpec) -> PromptCardSpec {
        let pick = |a: &Option<String>, b: &Option<String>| a.clone().or_else(|| b.clone());
        PromptCardSpec {
            option: pick(&other.option, &self.option),
            marker: pick(&other.marker, &self.marker),
            continuation: pick(&other.continuation, &self.continuation),
            description: pick(&other.description, &self.description),
            divider: pick(&other.divider, &self.divider),
            header: pick(&other.header, &self.header),
            footer: pick(&other.footer, &self.footer),
            amend: pick(&other.amend, &self.amend),
            setup_marker: pick(&other.setup_marker, &self.setup_marker),
            amend_label: pick(&other.amend_label, &self.amend_label),
        }
    }
}

/// `trustDialog.<vendor>` — how the folder-trust dialog is recognised (A6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrustDialogSpec {
    /// The "Yes, I trust this folder" family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<Vec<String>>,
    /// The "No, exit" family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<Vec<String>>,
    /// `Enter to confirm · Esc to cancel`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
}

impl TrustDialogSpec {
    fn merged_with(&self, other: &TrustDialogSpec) -> TrustDialogSpec {
        TrustDialogSpec {
            trust: other.trust.clone().or_else(|| self.trust.clone()),
            exit: other.exit.clone().or_else(|| self.exit.clone()),
            footer: other.footer.clone().or_else(|| self.footer.clone()),
        }
    }
}

/// The whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SignatureFile {
    pub version: u32,
    pub agents: BTreeMap<String, AgentSignatureSpec>,
    pub monitor_patterns: MonitorPatternsSpec,
    /// Per vendor key (§6.2.6).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ready_signature: BTreeMap<String, ReadySignatureSpec>,
    /// Per vendor key (§6.2.4).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub prompt_card: BTreeMap<String, PromptCardSpec>,
    /// Per vendor key (A6).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub trust_dialog: BTreeMap<String, TrustDialogSpec>,
}

/// Merge `user` over `base` per key, field-wise inside each key.
fn merge_keyed<T: Clone>(
    base: &BTreeMap<String, T>,
    user: &BTreeMap<String, T>,
    merge: impl Fn(&T, &T) -> T,
) -> BTreeMap<String, T> {
    let mut out = base.clone();
    for (raw_key, spec) in user {
        let key = canonical_agent_key(raw_key);
        let merged = match out.get(&key) {
            Some(existing) => merge(existing, spec),
            None => spec.clone(),
        };
        out.insert(key, merged);
    }
    out
}

impl SignatureFile {
    /// The compiled-in defaults. Panics only if the *bundled* file is broken,
    /// which a unit test in this module rules out at build time.
    pub fn defaults() -> SignatureFile {
        serde_json::from_str(DEFAULT_SIGNATURES_JSON)
            .expect("the bundled agent-signatures.default.json must parse")
    }

    /// Merge a user file over the defaults, **per agent key** (M1-PLAN T09.9),
    /// field-wise inside each key. Keys the defaults do not know about are kept.
    pub fn merge_over_defaults(&self, user: &SignatureFile) -> SignatureFile {
        let mut agents = self.agents.clone();
        for (raw_key, spec) in &user.agents {
            let key = canonical_agent_key(raw_key);
            let merged = match agents.get(&key) {
                Some(base) => base.merged_with(spec),
                None => spec.clone(),
            };
            agents.insert(key, merged);
        }
        SignatureFile {
            version: if user.version == 0 { self.version } else { user.version },
            agents,
            monitor_patterns: self.monitor_patterns.merged_with(&user.monitor_patterns),
            ready_signature: merge_keyed(
                &self.ready_signature,
                &user.ready_signature,
                ReadySignatureSpec::merged_with,
            ),
            prompt_card: merge_keyed(&self.prompt_card, &user.prompt_card, PromptCardSpec::merged_with),
            trust_dialog: merge_keyed(
                &self.trust_dialog,
                &user.trust_dialog,
                TrustDialogSpec::merged_with,
            ),
        }
    }
}

/// `cursorAgent` and `cursor` name the same CLI; DESIGN §6-4 writes the file
/// with `cursor` while `ipc.ts::AgentKindKey` uses `cursorAgent`. Accept both so
/// a user edit lands where they expect.
pub fn canonical_agent_key(key: &str) -> String {
    match key {
        "cursorAgent" | "cursor-agent" => "cursor".to_string(),
        "antigravity" => "agy".to_string(),
        "geminiCli" | "gemini-cli" => "gemini".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// compiled shape
// ---------------------------------------------------------------------------

/// One compiled pattern, carrying the id that ends up in
/// [`crate::model::status::StatusEvidence::PatternMatch`].
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    /// `"<agent>.<class>.<index>"`, e.g. `claude.idle.0`.
    pub id: String,
    pub class: PatternClass,
    pub regex: Regex,
    /// The source text, so the Inspector can show what it is about to open.
    pub source: String,
}

/// One agent's compiled patterns, in evaluation order.
#[derive(Debug, Clone, Default)]
pub struct AgentSignatures {
    pub key: String,
    pub idle_after_ms: u64,
    pub waiting: Vec<CompiledPattern>,
    pub working: Vec<CompiledPattern>,
    pub idle: Vec<CompiledPattern>,
}

impl AgentSignatures {
    /// First pattern that matches `line`, in DESIGN §6-4 precedence:
    /// **waiting → working → idle**.
    ///
    /// The order is not cosmetic. "Do you want to proceed? (esc to interrupt)"
    /// contains both a waiting and a working marker, and calling that session
    /// `Working` would hide a prompt that is blocking the agent.
    pub fn match_line(&self, line: &str) -> Option<&CompiledPattern> {
        for bucket in [&self.waiting, &self.working, &self.idle] {
            if let Some(hit) = bucket.iter().find(|p| p.regex.is_match(line)) {
                return Some(hit);
            }
        }
        None
    }

    /// Scan a visible tail newest-line-first and return the first hit.
    pub fn match_tail<S: AsRef<str>>(&self, lines: &[S]) -> Option<(&CompiledPattern, String)> {
        for line in lines.iter().rev() {
            let line = line.as_ref();
            if let Some(hit) = self.match_line(line) {
                return Some((hit, line.to_string()));
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.waiting.len() + self.working.len() + self.idle.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Bottom non-empty rows a ready signature is searched in, by default.
pub const DEFAULT_READY_ROWS: usize = 6;

/// A compiled `readySignature` (§6.2.6).
#[derive(Debug, Clone)]
pub struct ReadySignature {
    pub verified: bool,
    pub input: Regex,
    pub footer: Option<Regex>,
    pub rows: usize,
}

impl ReadySignature {
    /// True when the input row — and the footer, when the signature has one —
    /// is among the bottom `rows` non-empty rows of `screen` (topmost first).
    pub fn matches<S: AsRef<str>>(&self, screen: &[S]) -> bool {
        let bottom: Vec<&str> = screen
            .iter()
            .rev()
            .map(|l| l.as_ref())
            .filter(|l| !l.trim().is_empty())
            .take(self.rows.max(1))
            .collect();
        let input = bottom.iter().any(|l| self.input.is_match(l));
        let footer = match &self.footer {
            Some(f) => bottom.iter().any(|l| f.is_match(l)),
            None => true,
        };
        input && footer
    }
}

/// The compiled `promptCard` grammar of one vendor (§6.2.4).
#[derive(Debug, Clone)]
pub struct PromptCardPatterns {
    pub option: Regex,
    pub marker: Regex,
    pub continuation: Regex,
    pub description: Regex,
    pub divider: Regex,
    pub header: Regex,
    pub footer: Regex,
    pub amend: Regex,
    pub setup_marker: Regex,
    pub amend_label: Regex,
}

/// The compiled `trustDialog` patterns of one vendor (A6).
#[derive(Debug, Clone)]
pub struct TrustDialogPatterns {
    pub trust: Vec<Regex>,
    pub exit: Vec<Regex>,
    pub footer: Option<Regex>,
}

impl TrustDialogPatterns {
    pub fn is_trust(&self, label: &str) -> bool {
        self.trust.iter().any(|r| r.is_match(label))
    }

    pub fn is_exit(&self, label: &str) -> bool {
        self.exit.iter().any(|r| r.is_match(label))
    }
}

/// The grammar the contract spells out (§6.2.4); a `promptCard` entry that
/// leaves a field out gets this one.
pub mod default_grammar {
    pub const OPTION: &str = r"^\s*(?:[❯>›]\s*)?(\d)\.\s+(\S.*)$";
    pub const MARKER: &str = r"^\s*[❯>›]";
    pub const CONTINUATION: &str = r"^\s+\S";
    pub const DESCRIPTION: &str = r"^\s{5,}\S";
    pub const DIVIDER: &str = r"^\s*─{3,}\s*$|^\s*─{2,}.+─{2,}\s*$";
    pub const HEADER: &str = r"^\s*☐\s+(\S.*)$";
    pub const FOOTER: &str =
        r"(?i)esc to (cancel|close)|enter to (select|confirm|continue)|tab to amend|press enter";
    pub const AMEND: &str = r"(?i)tab to amend";
    pub const SETUP_MARKER: &str = r"^(\s*)[❯›>]\s+(\S.*)$";
    pub const AMEND_LABEL: &str = r"(?i)^yes, and tell \S+ what to do next";
}

/// Everything the status engine and the monitor need, compiled once.
#[derive(Debug, Clone, Default)]
pub struct Signatures {
    agents: BTreeMap<String, AgentSignatures>,
    monitor: procscan::Signatures,
    ready: BTreeMap<String, ReadySignature>,
    cards: BTreeMap<String, PromptCardPatterns>,
    trust: BTreeMap<String, TrustDialogPatterns>,
    warnings: Vec<String>,
    /// Absolute path of the user file this was loaded from, for the "edit
    /// signatures" affordance.
    source_path: Option<PathBuf>,
}

fn compile_one(what: &str, source: &str, warnings: &mut Vec<String>) -> Option<Regex> {
    match Regex::new(source) {
        Ok(regex) => Some(regex),
        Err(e) => {
            warnings.push(format!("{what} {source:?} did not compile: {e}"));
            None
        }
    }
}

fn compile_ready(key: &str, spec: &ReadySignatureSpec, warnings: &mut Vec<String>) -> Option<ReadySignature> {
    let Some(input) = spec.input.as_deref() else {
        warnings.push(format!("readySignature.{key} has no input pattern; using the settled-grid fallback"));
        return None;
    };
    let input = compile_one(&format!("readySignature.{key}.input"), input, warnings)?;
    let footer = match spec.footer.as_deref() {
        Some(f) => Some(compile_one(&format!("readySignature.{key}.footer"), f, warnings)?),
        None => None,
    };
    Some(ReadySignature {
        verified: spec.verified.unwrap_or(false),
        input,
        footer,
        rows: spec.rows.unwrap_or(DEFAULT_READY_ROWS).clamp(1, 40),
    })
}

fn compile_card(key: &str, spec: &PromptCardSpec, warnings: &mut Vec<String>) -> Option<PromptCardPatterns> {
    use default_grammar as g;
    let mut field = |name: &str, value: &Option<String>, default: &str| {
        compile_one(
            &format!("promptCard.{key}.{name}"),
            value.as_deref().unwrap_or(default),
            warnings,
        )
    };
    // A grammar with a hole cannot parse safely: one broken field drops the
    // vendor's cards (the screen then never yields a card for it).
    Some(PromptCardPatterns {
        option: field("option", &spec.option, g::OPTION)?,
        marker: field("marker", &spec.marker, g::MARKER)?,
        continuation: field("continuation", &spec.continuation, g::CONTINUATION)?,
        description: field("description", &spec.description, g::DESCRIPTION)?,
        divider: field("divider", &spec.divider, g::DIVIDER)?,
        header: field("header", &spec.header, g::HEADER)?,
        footer: field("footer", &spec.footer, g::FOOTER)?,
        amend: field("amend", &spec.amend, g::AMEND)?,
        setup_marker: field("setupMarker", &spec.setup_marker, g::SETUP_MARKER)?,
        amend_label: field("amendLabel", &spec.amend_label, g::AMEND_LABEL)?,
    })
}

fn compile_trust(key: &str, spec: &TrustDialogSpec, warnings: &mut Vec<String>) -> Option<TrustDialogPatterns> {
    let mut list = |name: &str, sources: &Option<Vec<String>>| -> Vec<Regex> {
        sources
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|s| compile_one(&format!("trustDialog.{key}.{name}"), s, warnings))
            .collect()
    };
    let trust = list("trust", &spec.trust);
    let exit = list("exit", &spec.exit);
    if trust.is_empty() || exit.is_empty() {
        warnings.push(format!("trustDialog.{key} needs both trust and exit patterns; the dialog is not recognised"));
        return None;
    }
    let footer = match spec.footer.as_deref() {
        Some(f) => Some(compile_one(&format!("trustDialog.{key}.footer"), f, warnings)?),
        None => None,
    };
    Some(TrustDialogPatterns { trust, exit, footer })
}

impl Signatures {
    /// Compile a merged [`SignatureFile`].
    pub fn compile(file: &SignatureFile) -> Signatures {
        let mut warnings = Vec::new();
        let mut agents = BTreeMap::new();

        for (key, spec) in &file.agents {
            let mut compiled = AgentSignatures {
                key: key.clone(),
                idle_after_ms: spec.idle_after_ms.unwrap_or(DEFAULT_IDLE_AFTER_MS),
                ..AgentSignatures::default()
            };
            compiled.waiting = compile_bucket(
                key,
                "waiting",
                PatternClass::Waiting,
                spec.waiting.as_deref().unwrap_or_default(),
                &mut warnings,
            );
            compiled.working = compile_bucket(
                key,
                "working",
                PatternClass::Working,
                spec.working.as_deref().unwrap_or_default(),
                &mut warnings,
            );
            compiled.idle = compile_bucket(
                key,
                "idle",
                PatternClass::Idle,
                spec.idle.as_deref().unwrap_or_default(),
                &mut warnings,
            );
            agents.insert(key.clone(), compiled);
        }

        let defaults = SignatureFile::defaults();
        let monitor_spec = &file.monitor_patterns;
        let agent_pats = monitor_spec
            .agent
            .clone()
            .or_else(|| defaults.monitor_patterns.agent.clone())
            .unwrap_or_default();
        let residue_pats = monitor_spec
            .external_residue
            .clone()
            .or_else(|| defaults.monitor_patterns.external_residue.clone())
            .unwrap_or_default();
        let mcp_pats = monitor_spec
            .mcp_server
            .clone()
            .or_else(|| defaults.monitor_patterns.mcp_server.clone())
            .unwrap_or_default();

        let (monitor, monitor_warnings) =
            procscan::Signatures::compile(&agent_pats, &mcp_pats, &residue_pats);
        warnings.extend(monitor_warnings);

        let mut ready = BTreeMap::new();
        for (key, spec) in &file.ready_signature {
            if let Some(sig) = compile_ready(key, spec, &mut warnings) {
                ready.insert(canonical_agent_key(key), sig);
            }
        }
        let mut cards = BTreeMap::new();
        for (key, spec) in &file.prompt_card {
            if let Some(card) = compile_card(key, spec, &mut warnings) {
                cards.insert(canonical_agent_key(key), card);
            }
        }
        let mut trust = BTreeMap::new();
        for (key, spec) in &file.trust_dialog {
            if let Some(dialog) = compile_trust(key, spec, &mut warnings) {
                trust.insert(canonical_agent_key(key), dialog);
            }
        }

        Signatures {
            agents,
            monitor,
            ready,
            cards,
            trust,
            warnings,
            source_path: None,
        }
    }

    /// The vendor's ready signature, if it has one (§6.2.6).
    pub fn ready_signature(&self, key: &str) -> Option<&ReadySignature> {
        self.ready.get(&canonical_agent_key(key))
    }

    /// The vendor's screen-prompt grammar; `None` = this vendor never yields a
    /// screen-prompt card (§6.2.4).
    pub fn prompt_card(&self, key: &str) -> Option<&PromptCardPatterns> {
        self.cards.get(&canonical_agent_key(key))
    }

    /// The vendor's folder-trust dialog patterns (A6).
    pub fn trust_dialog(&self, key: &str) -> Option<&TrustDialogPatterns> {
        self.trust.get(&canonical_agent_key(key))
    }

    /// Patterns for one agent. Accepts either spelling of the Cursor key.
    pub fn for_agent(&self, key: &str) -> Option<&AgentSignatures> {
        self.agents.get(&canonical_agent_key(key))
    }

    /// The Process Monitor classification set — hand this to
    /// [`crate::supervise::classify`] (M1-PLAN T09.9: "wired into T07's
    /// `classify`").
    pub fn monitor(&self) -> &procscan::Signatures {
        &self.monitor
    }

    /// One warning per pattern that did not compile.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Number of agent keys that carry at least one pattern.
    pub fn agent_count(&self) -> usize {
        self.agents.values().filter(|a| !a.is_empty()).count()
    }

    pub fn agent_keys(&self) -> impl Iterator<Item = &str> {
        self.agents.keys().map(String::as_str)
    }

    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    /// `idleAfterMs` for one agent, falling back to the global default.
    pub fn idle_after_ms(&self, key: &str) -> u64 {
        self.for_agent(key)
            .map(|a| a.idle_after_ms)
            .unwrap_or(DEFAULT_IDLE_AFTER_MS)
    }
}

fn compile_bucket(
    agent: &str,
    bucket: &str,
    class: PatternClass,
    patterns: &[String],
    warnings: &mut Vec<String>,
) -> Vec<CompiledPattern> {
    patterns
        .iter()
        .enumerate()
        .filter_map(|(i, source)| match Regex::new(source) {
            Ok(regex) => Some(CompiledPattern {
                id: format!("{agent}.{bucket}.{i}"),
                class,
                regex,
                source: source.clone(),
            }),
            Err(e) => {
                warnings.push(format!("{agent}.{bucket}[{i}] {source:?} did not compile: {e}"));
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// load + seed
// ---------------------------------------------------------------------------

/// Seed on first run, then load, merge and compile.
///
/// Returns the compiled set plus the warnings (also reachable through
/// [`Signatures::warnings`]) so `agents_reload_signatures` can answer with the
/// `SignaturesReport` shape `ipc.ts` declares.
pub fn load_signatures(paths: &AppPaths) -> (Signatures, Vec<String>) {
    let file_path = paths.signatures_file();
    let defaults = SignatureFile::defaults();
    let mut warnings = Vec::new();

    // Seed. A write failure is survivable: the defaults still load.
    if !file_path.exists() {
        if let Err(e) = seed_default_file(&file_path) {
            warnings.push(format!(
                "could not seed {}: {e}",
                file_path.display()
            ));
        }
    } else if let Err(e) = reseed_untouched_file(&file_path, SUPERSEDED_DEFAULT_DIGESTS) {
        warnings.push(format!("could not refresh {}: {e}", file_path.display()));
    }

    let merged = match std::fs::read_to_string(&file_path) {
        Ok(text) => match serde_json::from_str::<SignatureFile>(&text) {
            Ok(user) => defaults.merge_over_defaults(&user),
            Err(e) => {
                warnings.push(format!(
                    "{} is not valid JSON ({e}); using the built-in signatures",
                    file_path.display()
                ));
                defaults
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => defaults,
        Err(e) => {
            warnings.push(format!("{} is unreadable ({e})", file_path.display()));
            defaults
        }
    };

    let mut signatures = Signatures::compile(&merged);
    signatures.warnings.splice(0..0, warnings.clone());
    signatures.source_path = Some(file_path);
    let all = signatures.warnings.clone();
    (signatures, all)
}

/// SHA-256 of every `agent-signatures.default.json` this app has shipped before
/// the current one, newest last.
///
/// ## Why a shipped fix needs this list
///
/// The seed is written **once**, and `merge_over_defaults` is field-wise: a user
/// file that carries `agents.claude.waiting` replaces the bundled list outright,
/// which is exactly right for an edit and exactly wrong for a seed nobody ever
/// touched. Without this list, BACKLOG M1.5 #2 (the claude selection-menu
/// patterns) would ship and reach nobody who had already run the app once — the
/// stale copy of the v1 defaults on their disk would keep overriding it, silently
/// and forever.
///
/// A file whose bytes hash to one of these entries **is** an unedited seed of an
/// older build, so rewriting it is not data loss: it is the same first-run seed
/// the user already got, refreshed. One byte of user editing changes the hash and
/// the file is then left strictly alone.
const SUPERSEDED_DEFAULT_DIGESTS: &[&str] = &[
    // v1 — shipped up to 2026-09-08 (M1).
    "d096171d855cb6715c6bdb0ab231da3baa796d3e205df166a99c6602acec951a",
    // v2 — shipped 2026-09-08 (the first M1.5 hands-on fixes). Superseded by
    // v3, which teaches claude the `Esc to close` footer its full-screen views
    // print (BACKLOG M1.5 15b).
    "e0fe529428fbef944b6b989e4c66982a7c81f8ebba2e390cc05b7ee23160f80f",
    // v3 — shipped up to Gilde 2. Superseded by v4 (readySignature,
    // promptCard and trustDialog, contract §6.2.4/§6.2.6, A5/A6).
    "8bda70d4ed5a1d1aef99417e518ceb2cb6f38423e25bf57c9bd6435f6faf9956",
];

/// Rewrite `path` with the bundled defaults when it is an untouched seed of an
/// older build. Returns `Ok(true)` when it was rewritten.
///
/// `superseded` is a parameter rather than a direct read of
/// [`SUPERSEDED_DEFAULT_DIGESTS`] so the policy can be tested without shipping a
/// copy of every historical default file.
fn reseed_untouched_file(path: &Path, superseded: &[&str]) -> std::io::Result<bool> {
    let bytes = std::fs::read(path)?;
    if bytes == DEFAULT_SIGNATURES_JSON.as_bytes() {
        return Ok(false);
    }
    let digest = hex::encode(Sha256::digest(&bytes));
    if !superseded.contains(&digest.as_str()) {
        return Ok(false);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, DEFAULT_SIGNATURES_JSON)?;
    std::fs::rename(&tmp, path)?;
    tracing::info!(
        target: "agents",
        path = %path.display(),
        digest = %digest,
        "agent-signatures.json was an unedited seed of an older build; refreshed with the bundled defaults"
    );
    Ok(true)
}

/// Write the bundled defaults to `path` (atomically) if it is not there yet.
fn seed_default_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, DEFAULT_SIGNATURES_JSON)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// hot reload
// ---------------------------------------------------------------------------

/// Keeps the `notify` debouncer alive. Dropping it stops the watch.
pub struct Watcher {
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
    watched_dir: PathBuf,
}

impl Watcher {
    pub fn watched_dir(&self) -> &Path {
        &self.watched_dir
    }
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("watched_dir", &self.watched_dir)
            .finish()
    }
}

/// Call `on_change` after a 500 ms quiet period whenever
/// `agent-signatures.json` is touched.
///
/// The **parent directory** is watched, not the file: editors save by writing a
/// temp file and renaming it over the original, which detaches an inode-bound
/// watch after the very first edit. Matching is on the file name because macOS
/// FSEvents reports the resolved `/private/...` form of a path.
pub fn watch_signatures(
    paths: &AppPaths,
    on_change: impl Fn() + Send + 'static,
) -> Result<Watcher> {
    let file = paths.signatures_file();
    let dir = file
        .parent()
        .ok_or_else(|| GildeError::config("signature file path has no parent directory"))?
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;

    let target = file
        .file_name()
        .ok_or_else(|| GildeError::config("signature file path has no file name"))?
        .to_os_string();

    let mut debouncer = new_debouncer(
        WATCH_DEBOUNCE,
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                let touched = events.iter().any(|event| {
                    event
                        .paths
                        .iter()
                        .any(|p| p.file_name().is_some_and(|name| name == target))
                });
                if touched {
                    on_change();
                }
            }
            Err(errors) => {
                for error in errors {
                    tracing::warn!(error = %error, "agent signature watch error");
                }
            }
        },
    )
    .map_err(|e| GildeError::config(format!("cannot watch the signature file: {e}")))?;

    debouncer
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| GildeError::config(format!("cannot watch {}: {e}", dir.display())))?;

    Ok(Watcher {
        _debouncer: debouncer,
        watched_dir: dir,
    })
}

/// `agents_reload_signatures()` result — `ipc.ts::SignaturesReport`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignaturesReport {
    pub agents: u32,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn temp_paths() -> (tempfile::TempDir, AppPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::for_home(dir.path());
        paths.ensure_all().expect("dirs");
        (dir, paths)
    }

    #[test]
    fn the_bundled_defaults_parse_and_every_pattern_compiles() {
        let file = SignatureFile::defaults();
        assert_eq!(file.version, 4);

        let sigs = Signatures::compile(&file);
        assert!(
            sigs.warnings().is_empty(),
            "shipped patterns must compile: {:?}",
            sigs.warnings()
        );
        for key in ["claude", "codex", "agy", "cursor", "gemini", "shell"] {
            assert!(sigs.for_agent(key).is_some(), "missing agent key {key}");
        }
        assert_eq!(sigs.agent_count(), 6);
    }

    #[test]
    fn measured_prompts_from_this_machine_match_the_shipped_patterns() {
        let sigs = Signatures::compile(&SignatureFile::defaults());

        let claude = sigs.for_agent("claude").expect("claude");
        let hit = claude
            .match_line("·  Whirring…  ❯  · esc to interrupt")
            .expect("claude working");
        assert_eq!(hit.class, PatternClass::Working);
        assert_eq!(
            claude
                .match_line("  ❯ 1. Yes, I trust this folder")
                .map(|p| p.class),
            Some(PatternClass::Waiting)
        );

        let codex = sigs.for_agent("codex").expect("codex");
        assert_eq!(
            codex
                .match_line("• Starting MCP servers (1/2): codex_apps (0s • esc to interrupt)")
                .map(|p| p.class),
            Some(PatternClass::Working),
            "codex renders the marker lower-case on 0.145.0"
        );

        let shell = sigs.for_agent("shell").expect("shell");
        assert_eq!(
            shell.match_line("user@mac ~ %").map(|p| p.class),
            Some(PatternClass::Idle)
        );
    }

    /// BACKLOG M1.5 #2. The screen the user was actually looking at while the
    /// pill said 유휴: a selection menu, whose only markers are the highlighted
    /// row and the footer. Not one word of it matched the v1 `waiting` set.
    #[test]
    fn a_selection_menu_is_waiting_for_every_agent_that_draws_one() {
        let sigs = Signatures::compile(&SignatureFile::defaults());

        let claude = sigs.for_agent("claude").expect("claude");
        for line in [
            "  ❯ 1. Keep going",
            "│ ❯ 2. Start over                     │",
            "  Enter to select · ↑/↓ to navigate · Esc to cancel",
            "  Type something",
        ] {
            assert_eq!(
                claude.match_line(line).map(|p| p.class),
                Some(PatternClass::Waiting),
                "claude menu line not recognised: {line:?}"
            );
        }

        let codex = sigs.for_agent("codex").expect("codex");
        for line in [
            "  › 1. Update now",
            "  Press Enter to continue",
            "  Enter to select · Esc to cancel",
            "  Overwrite? [Y/n]",
        ] {
            assert_eq!(
                codex.match_line(line).map(|p| p.class),
                Some(PatternClass::Waiting),
                "codex menu line not recognised: {line:?}"
            );
        }

        // agy's exact prompt strings could not be captured on this machine, so
        // its set is the union of the wordings the other four CLIs use. It is a
        // guess and is written down as one (docs/PINNED-VERSIONS.md §31).
        let agy = sigs.for_agent("agy").expect("agy");
        for line in [
            "  ❯ 1. Allow",
            "  Do you want to proceed?",
            "  Press enter to continue",
            "  Continue? (y/n)",
        ] {
            assert_eq!(
                agy.match_line(line).map(|p| p.class),
                Some(PatternClass::Waiting),
                "agy prompt line not recognised: {line:?}"
            );
        }
    }

    /// The other half of #2: the new patterns must not turn a resting composer
    /// or ordinary numbered output into a prompt, because `waiting` is searched
    /// before `idle` over the whole tail and a false positive there is a pill
    /// that cries for a human who is not needed.
    #[test]
    fn the_menu_patterns_do_not_fire_on_ordinary_output() {
        let sigs = Signatures::compile(&SignatureFile::defaults());
        let claude = sigs.for_agent("claude").expect("claude");

        assert_eq!(
            claude.match_line("│ > │").map(|p| p.class),
            Some(PatternClass::Idle),
            "the composer is still idle"
        );
        for line in [
            "  1. read the file",
            "  2. write the test",
            "Steps: 1. build 2. test",
            "· Whirring…  ❯  · esc to interrupt",
        ] {
            assert_ne!(
                claude.match_line(line).map(|p| p.class),
                Some(PatternClass::Waiting),
                "a numbered list is not a prompt: {line:?}"
            );
        }

        let codex = sigs.for_agent("codex").expect("codex");
        assert_eq!(
            codex.match_line("› Find and fix a bug").map(|p| p.class),
            Some(PatternClass::Idle),
            "codex's composer placeholder is still idle"
        );
    }

    #[test]
    fn waiting_beats_working_on_the_same_line() {
        let sigs = Signatures::compile(&SignatureFile::defaults());
        let claude = sigs.for_agent("claude").expect("claude");
        let hit = claude
            .match_line("Do you want to proceed? (esc to interrupt)")
            .expect("match");
        assert_eq!(
            hit.class,
            PatternClass::Waiting,
            "a blocking prompt must not be reported as work in progress"
        );
    }

    #[test]
    fn match_tail_scans_newest_line_first() {
        let sigs = Signatures::compile(&SignatureFile::defaults());
        let claude = sigs.for_agent("claude").expect("claude");
        let tail = ["esc to interrupt", "│ > │"];
        let (hit, line) = claude.match_tail(&tail).expect("match");
        assert_eq!(hit.class, PatternClass::Idle);
        assert_eq!(line, "│ > │");
    }

    #[test]
    fn a_broken_regex_is_a_warning_and_costs_only_that_one_pattern() {
        let user: SignatureFile = serde_json::from_str(
            r#"{ "agents": { "claude": { "working": ["(((", "still-fine"] } } }"#,
        )
        .expect("parse");
        let merged = SignatureFile::defaults().merge_over_defaults(&user);
        let sigs = Signatures::compile(&merged);

        assert_eq!(sigs.warnings().len(), 1, "{:?}", sigs.warnings());
        assert!(sigs.warnings()[0].contains("claude.working[0]"));

        let claude = sigs.for_agent("claude").expect("claude");
        assert_eq!(claude.working.len(), 1, "the good pattern survives");
        assert!(
            !claude.idle.is_empty() && !claude.waiting.is_empty(),
            "the other buckets are untouched"
        );
        assert!(
            claude.match_line("still-fine").is_some(),
            "the agent stays enabled"
        );
    }

    #[test]
    fn a_user_edit_merges_per_key_and_field_over_the_defaults() {
        let defaults = SignatureFile::defaults();
        let user: SignatureFile = serde_json::from_str(
            r#"{
                 "agents": {
                   "claude": { "idleAfterMs": 300 },
                   "cursorAgent": { "working": ["my-cursor-marker"] },
                   "brandNew": { "idle": ["^ok$"] }
                 }
               }"#,
        )
        .expect("parse");
        let merged = defaults.merge_over_defaults(&user);
        let sigs = Signatures::compile(&merged);

        let claude = sigs.for_agent("claude").expect("claude");
        assert_eq!(claude.idle_after_ms, 300, "the pinned field wins");
        assert!(
            !claude.working.is_empty(),
            "fields the user did not mention keep their defaults"
        );

        let cursor = sigs.for_agent("cursor").expect("cursor");
        assert_eq!(cursor.working.len(), 1, "cursorAgent is the same key as cursor");
        assert!(!cursor.idle.is_empty());

        assert!(
            sigs.for_agent("brandNew").is_some(),
            "an unknown agent key is kept, not dropped"
        );
    }

    #[test]
    fn monitor_patterns_reach_t07_classify() {
        let user: SignatureFile = serde_json::from_str(
            r#"{ "monitorPatterns": { "externalResidue": ["MyEditor Helper"] } }"#,
        )
        .expect("parse");
        let sigs = Signatures::compile(&SignatureFile::defaults().merge_over_defaults(&user));

        let monitor = sigs.monitor();
        assert!(monitor.is_external_residue("/Applications/MyEditor Helper (Renderer)"));
        assert!(
            !monitor.is_external_residue("Cursor Helper (Renderer)"),
            "an explicit list replaces the default one"
        );
        assert!(
            monitor.is_mcp_server("node /x/mcp-server-github/index.js"),
            "buckets the user did not mention keep their defaults"
        );
        assert!(monitor.is_agent("/Users/x/.nvm/versions/node/v20.19.2/bin/claude"));
    }

    #[test]
    fn first_load_seeds_the_file_verbatim_and_is_idempotent() {
        let (_dir, paths) = temp_paths();
        let file = paths.signatures_file();
        assert!(!file.exists());

        let (sigs, warnings) = load_signatures(&paths);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(file.exists());
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            DEFAULT_SIGNATURES_JSON
        );
        assert_eq!(sigs.agent_count(), 6);
        assert_eq!(sigs.source_path(), Some(file.as_path()));

        // A second load must not clobber a user edit.
        let edited = serde_json::json!({
            "version": 1,
            "agents": { "claude": { "idleAfterMs": 42 } }
        })
        .to_string();
        std::fs::write(&file, &edited).expect("write");
        let (sigs, _) = load_signatures(&paths);
        assert_eq!(sigs.idle_after_ms("claude"), 42);
        assert_eq!(std::fs::read_to_string(&file).expect("read"), edited);
    }

    /// BACKLOG M1.5 #2: a fix to the bundled patterns has to reach a machine
    /// that already ran an older build, and must not touch a file the user
    /// edited.
    #[test]
    fn an_untouched_seed_of_an_older_build_is_refreshed_and_an_edited_one_is_not() {
        let (_dir, paths) = temp_paths();
        let file = paths.signatures_file();

        // Stand in for a previously shipped default: any bytes, hashed here so
        // the test needs no copy of a historical file.
        let old_default = r#"{ "version": 1, "agents": { "claude": { "waiting": ["only-this"] } } }"#;
        let old_digest = hex::encode(Sha256::digest(old_default.as_bytes()));
        std::fs::write(&file, old_default).expect("write");

        assert!(
            reseed_untouched_file(&file, &[old_digest.as_str()]).expect("reseed"),
            "an unedited seed of an older build is rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            DEFAULT_SIGNATURES_JSON
        );

        // Re-running is a no-op: the file now *is* the current default.
        assert!(!reseed_untouched_file(&file, &[old_digest.as_str()]).expect("reseed"));

        // One edited byte and the file is off limits for good.
        let edited = format!("{old_default}\n");
        std::fs::write(&file, &edited).expect("write");
        assert!(
            !reseed_untouched_file(&file, &[old_digest.as_str()]).expect("reseed"),
            "an edited file is never rewritten"
        );
        assert_eq!(std::fs::read_to_string(&file).expect("read"), edited);
    }

    /// The digest of the file we ship must never appear in the superseded list:
    /// that would make every load rewrite the file it just read.
    #[test]
    fn the_current_default_is_not_listed_as_superseded() {
        let current = hex::encode(Sha256::digest(DEFAULT_SIGNATURES_JSON.as_bytes()));
        assert!(
            !SUPERSEDED_DEFAULT_DIGESTS.contains(&current.as_str()),
            "the shipped default is listed as superseded; every boot would rewrite it"
        );
        for digest in SUPERSEDED_DEFAULT_DIGESTS {
            assert_eq!(digest.len(), 64, "not a sha-256 hex digest: {digest}");
        }
    }

    /// The v1 seed on a machine that ran M1 must be recognised: its digest is
    /// the first entry of the list, and a load has to end with the patterns this
    /// build ships rather than the ones that shipped in M1.
    #[test]
    fn a_v1_seed_on_disk_is_upgraded_by_load_signatures() {
        let (_dir, paths) = temp_paths();
        let file = paths.signatures_file();
        let v1 = include_str!("../../tests/fixtures/agent-signatures.v1.json");
        assert_eq!(
            hex::encode(Sha256::digest(v1.as_bytes())),
            SUPERSEDED_DEFAULT_DIGESTS[0],
            "the fixture is not the v1 file that shipped"
        );
        std::fs::write(&file, v1).expect("write");

        let (sigs, warnings) = load_signatures(&paths);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            DEFAULT_SIGNATURES_JSON,
            "the stale seed was refreshed"
        );
        assert_eq!(
            sigs.for_agent("claude")
                .expect("claude")
                .match_line("Enter to select · ↑/↓ to navigate · Esc to cancel")
                .map(|p| p.class),
            Some(PatternClass::Waiting),
            "the v2 menu patterns are live after the upgrade"
        );
    }

    #[test]
    fn a_corrupt_user_file_degrades_to_the_defaults_with_a_warning() {
        let (_dir, paths) = temp_paths();
        std::fs::write(paths.signatures_file(), "{ not json at all").expect("write");

        let (sigs, warnings) = load_signatures(&paths);
        assert!(
            warnings.iter().any(|w| w.contains("not valid JSON")),
            "{warnings:?}"
        );
        assert_eq!(sigs.agent_count(), 6, "the built-ins still load");
    }

    #[test]
    fn the_report_shape_matches_ipc_ts() {
        let value = serde_json::to_value(SignaturesReport {
            agents: 6,
            warnings: vec!["x".into()],
        })
        .expect("json");
        assert_eq!(value["agents"], serde_json::json!(6));
        assert_eq!(value["warnings"], serde_json::json!(["x"]));
    }

    /// M1-PLAN T09.9: an edit is picked up within 2 s.
    #[test]
    fn an_edit_hot_reloads_within_two_seconds() {
        let (_dir, paths) = temp_paths();
        let (_sigs, _) = load_signatures(&paths);

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let _watcher = watch_signatures(&paths, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .expect("watch");

        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(
            paths.signatures_file(),
            serde_json::json!({ "version": 1, "agents": { "claude": { "idleAfterMs": 7 } } })
                .to_string(),
        )
        .expect("write");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while hits.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(hits.load(Ordering::SeqCst) > 0, "no reload within 2s");

        let (reloaded, _) = load_signatures(&paths);
        assert_eq!(reloaded.idle_after_ms("claude"), 7);
    }
    #[test]
    fn every_vendor_ships_a_ready_signature_and_a_prompt_grammar() {
        let sigs = Signatures::compile(&SignatureFile::defaults());
        for key in ["claude", "codex", "cursor", "agy", "cursorAgent", "antigravity"] {
            assert!(sigs.ready_signature(key).is_some(), "{key}");
            assert!(sigs.prompt_card(key).is_some(), "{key}");
        }
        assert!(sigs.ready_signature("claude").unwrap().verified);
        assert!(sigs.ready_signature("codex").unwrap().verified);
        assert!(!sigs.ready_signature("cursor").unwrap().verified);
        assert!(!sigs.ready_signature("agy").unwrap().verified);
        assert!(sigs.trust_dialog("claude").is_some());
        assert!(sigs.prompt_card("shell").is_none(), "a vendor without a grammar never gets a card");
    }

    #[test]
    fn a_ready_input_never_matches_a_highlighted_numbered_option() {
        let sigs = Signatures::compile(&SignatureFile::defaults());
        let claude = sigs.ready_signature("claude").unwrap();
        assert!(claude.input.is_match("❯ Try \"fix lint errors\""));
        assert!(claude.input.is_match("❯"));
        assert!(!claude.input.is_match(" ❯ 1. Yes"));
        let codex = sigs.ready_signature("codex").unwrap();
        assert!(codex.input.is_match("› Find and fix a bug in @filename"));
        assert!(!codex.input.is_match("› 1. Update now"));
        // Input and footer must both be among the bottom rows.
        let screen = ["❯ ", "", "  ⏸ manual mode on · ? for shortcuts"];
        assert!(claude.matches(&screen));
        assert!(!claude.matches(&["❯ "]), "no footer, no signature");
    }

    #[test]
    fn the_new_sections_merge_per_vendor_and_a_broken_field_drops_only_that_vendor() {
        let user: SignatureFile = serde_json::from_str(
            r#"{"readySignature":{"claude":{"footer":"CUSTOM FOOTER"}},
                "promptCard":{"codex":{"option":"(unclosed"}},
                "trustDialog":{"claude":{"exit":[]}}}"#,
        )
        .expect("json");
        let merged = SignatureFile::defaults().merge_over_defaults(&user);
        let sigs = Signatures::compile(&merged);
        let claude = sigs.ready_signature("claude").unwrap();
        assert!(claude.footer.as_ref().unwrap().is_match("CUSTOM FOOTER"));
        assert!(claude.input.is_match("❯ "), "the input pattern is inherited");
        assert!(sigs.prompt_card("codex").is_none(), "a broken grammar drops that vendor's cards");
        assert!(sigs.prompt_card("claude").is_some(), "and only that vendor's");
        assert!(sigs.trust_dialog("claude").is_none(), "an empty exit family disables recognition");
        assert!(sigs.warnings().iter().any(|w| w.contains("promptCard.codex.option")));
    }
}
