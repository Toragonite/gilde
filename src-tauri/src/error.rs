//! Typed errors for the whole crate.
//!
//! Two rules from `docs/DESIGN.md` shape this module:
//!
//! 1. **Rust never returns prose to the UI** (§9-9). Every error that crosses the
//!    IPC boundary is serialized as [`GildeErrorPayload`] — `{ code, key, params }` —
//!    and the frontend `t()` renders it in the active locale. Changing a user
//!    facing sentence must never require a Rust rebuild.
//! 2. **Raw `git` / CLI stderr is not translated** — it travels verbatim in
//!    `detail` and the UI shows it in a monospace block.
//!
//! Every `#[tauri::command]` returns `std::result::Result<T, GildeErrorPayload>`.
//! Inside the crate, use [`Result<T>`] (= `Result<T, GildeError>`); the `?` operator
//! converts to the payload at the command boundary through
//! [`From<GildeError> for GildeErrorPayload`].

use std::fmt::Display;

use serde::Serialize;
use serde_json::{Map, Value};

/// Crate-internal result type. Commands convert this into [`GildeErrorPayload`].
pub type Result<T> = std::result::Result<T, GildeError>;

/// Result type of a `#[tauri::command]`.
pub type CmdResult<T> = std::result::Result<T, GildeErrorPayload>;

/// Every failure mode the backend can report.
///
/// The variants are the ones enumerated in `docs/M1-PLAN.md` §T01 plus
/// [`GildeError::Internal`] for genuinely unexpected failures. Adding a variant
/// means adding a `code`/`key`/`params` mapping below — the payload mapping is
/// exhaustive on purpose so the compiler catches omissions.
#[derive(Debug, thiserror::Error)]
pub enum GildeError {
    /// Filesystem / process I/O.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// SQLite: open, migrate, query.
    #[error("db: {0}")]
    Db(String),

    /// `config.json` / `agent-signatures.json` parsing or validation.
    #[error("config: {0}")]
    Config(String),

    /// A `git` invocation exited non-zero. `stderr` is verbatim, never translated.
    #[error("git exited with {code}")]
    Git { code: i32, stderr: String },

    /// A process could not be started (or the spawn guard aborted it).
    #[error("spawn: {0}")]
    Spawn(String),

    /// The sidecar is not running / not reachable right now. Retryable.
    #[error("sidecar is down")]
    SidecarDown,

    /// The Node runtime we were told to use cannot execute (§5, self-test).
    #[error("sidecar runtime unusable: {path}")]
    SidecarRuntimeUnusable { path: String, stderr: String },

    /// The addressed entity (session, worktree, run, …) does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A policy refusal — the operation is understood and deliberately declined
    /// (e.g. removing a dirty worktree without picking a mode). Never a bug.
    #[error("refused: {reason}")]
    Refused { reason: String },

    /// Worktree lifecycle failure that is not a raw `git` exit.
    #[error("worktree: {0}")]
    Worktree(String),

    /// PTY open / read / write / resize failure.
    #[error("pty: {0}")]
    Pty(String),

    /// Unexpected failure with no better variant. Shows a generic message plus
    /// `detail` in the UI.
    #[error("internal: {0}")]
    Internal(String),
}

impl GildeError {
    /// Stable machine code. Never localized, safe to branch on in TS.
    pub fn code(&self) -> &'static str {
        match self {
            GildeError::Io(_) => "IO",
            GildeError::Db(_) => "DB",
            GildeError::Config(_) => "CONFIG",
            GildeError::Git { .. } => "GIT",
            GildeError::Spawn(_) => "SPAWN",
            GildeError::SidecarDown => "SIDECAR_DOWN",
            GildeError::SidecarRuntimeUnusable { .. } => "SIDECAR_RUNTIME_UNUSABLE",
            GildeError::NotFound(_) => "NOT_FOUND",
            GildeError::Refused { .. } => "REFUSED",
            GildeError::Worktree(_) => "WORKTREE",
            GildeError::Pty(_) => "PTY",
            GildeError::Internal(_) => "INTERNAL",
        }
    }

    /// i18n message key. Must exist in both `src/i18n/ko.ts` and `src/i18n/en.ts`.
    pub fn key(&self) -> &'static str {
        match self {
            GildeError::Io(_) => "error.io",
            GildeError::Db(_) => "error.db",
            GildeError::Config(_) => "error.config",
            GildeError::Git { .. } => "error.git",
            GildeError::Spawn(_) => "error.spawn",
            GildeError::SidecarDown => "error.sidecar.down",
            GildeError::SidecarRuntimeUnusable { .. } => "error.sidecar.runtimeUnusable",
            GildeError::NotFound(_) => "error.notFound",
            GildeError::Refused { .. } => "error.refused",
            GildeError::Worktree(_) => "error.worktree",
            GildeError::Pty(_) => "error.pty",
            GildeError::Internal(_) => "error.internal",
        }
    }

    /// Whether the caller may reasonably retry the exact same operation.
    pub fn retryable(&self) -> bool {
        match self {
            GildeError::Io(_) | GildeError::Spawn(_) | GildeError::SidecarDown => true,
            GildeError::Db(_)
            | GildeError::Config(_)
            | GildeError::Git { .. }
            | GildeError::SidecarRuntimeUnusable { .. }
            | GildeError::NotFound(_)
            | GildeError::Refused { .. }
            | GildeError::Worktree(_)
            | GildeError::Pty(_)
            | GildeError::Internal(_) => false,
        }
    }

    /// Interpolation parameters for the i18n key. Values here ARE rendered into
    /// the localized sentence, so they must be data (numbers, names), not prose.
    pub fn params(&self) -> Map<String, Value> {
        let mut m = Map::new();
        match self {
            GildeError::Git { code, .. } => {
                m.insert("code".into(), Value::from(*code));
            }
            GildeError::NotFound(what) => {
                m.insert("what".into(), Value::from(what.clone()));
            }
            GildeError::Refused { reason } => {
                // `reason` is a machine token (e.g. "dirty", "locked"), not a sentence:
                // the UI maps it to `error.refused.<reason>` when a specific string exists.
                m.insert("reason".into(), Value::from(reason.clone()));
            }
            GildeError::SidecarRuntimeUnusable { path, .. } => {
                m.insert("path".into(), Value::from(path.clone()));
            }
            _ => {}
        }
        m
    }

    /// Untranslated technical detail: raw stderr, an OS message, a path. The UI
    /// shows it verbatim in a monospace block, collapsed by default.
    pub fn detail(&self) -> Option<String> {
        match self {
            GildeError::Io(e) => Some(e.to_string()),
            GildeError::Db(d)
            | GildeError::Config(d)
            | GildeError::Spawn(d)
            | GildeError::Worktree(d)
            | GildeError::Pty(d)
            | GildeError::Internal(d) => Some(d.clone()),
            GildeError::Git { stderr, .. } | GildeError::SidecarRuntimeUnusable { stderr, .. } => {
                let s = stderr.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            }
            GildeError::NotFound(what) => Some(what.clone()),
            GildeError::Refused { .. } | GildeError::SidecarDown => None,
        }
    }

    // -- constructors that keep call sites short ------------------------------

    pub fn db<E: Display>(e: E) -> Self {
        GildeError::Db(e.to_string())
    }

    pub fn config<E: Display>(e: E) -> Self {
        GildeError::Config(e.to_string())
    }

    pub fn spawn<E: Display>(e: E) -> Self {
        GildeError::Spawn(e.to_string())
    }

    pub fn not_found<E: Display>(what: E) -> Self {
        GildeError::NotFound(what.to_string())
    }

    pub fn refused<E: Display>(reason: E) -> Self {
        GildeError::Refused {
            reason: reason.to_string(),
        }
    }

    pub fn worktree<E: Display>(e: E) -> Self {
        GildeError::Worktree(e.to_string())
    }

    pub fn pty<E: Display>(e: E) -> Self {
        GildeError::Pty(e.to_string())
    }

    pub fn internal<E: Display>(e: E) -> Self {
        GildeError::Internal(e.to_string())
    }
}

/// The wire shape of an error. This is what the frontend receives; it contains
/// no localized prose.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GildeErrorPayload {
    /// Stable machine code, e.g. `"GIT"`. Branch on this, not on `key`.
    pub code: String,
    /// i18n key, e.g. `"error.git"`.
    pub key: String,
    /// Interpolation parameters for `key`.
    pub params: Map<String, Value>,
    /// Untranslated technical detail (raw stderr, OS message). May be shown as-is.
    pub detail: Option<String>,
    /// Whether retrying the same call is sensible.
    pub retryable: bool,
}

impl From<GildeError> for GildeErrorPayload {
    fn from(e: GildeError) -> Self {
        GildeErrorPayload {
            code: e.code().to_string(),
            key: e.key().to_string(),
            params: e.params(),
            detail: e.detail(),
            retryable: e.retryable(),
        }
    }
}

impl From<&GildeError> for GildeErrorPayload {
    fn from(e: &GildeError) -> Self {
        GildeErrorPayload {
            code: e.code().to_string(),
            key: e.key().to_string(),
            params: e.params(),
            detail: e.detail(),
            retryable: e.retryable(),
        }
    }
}

impl std::fmt::Display for GildeErrorPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.code, self.key)?;
        if let Some(d) = &self.detail {
            write!(f, ": {d}")?;
        }
        Ok(())
    }
}

impl std::error::Error for GildeErrorPayload {}

// -- interop with the crates that show up in nearly every module --------------

impl From<rusqlite::Error> for GildeError {
    fn from(e: rusqlite::Error) -> Self {
        GildeError::Db(e.to_string())
    }
}

impl From<serde_json::Error> for GildeError {
    fn from(e: serde_json::Error) -> Self {
        GildeError::Config(e.to_string())
    }
}

impl From<anyhow::Error> for GildeError {
    fn from(e: anyhow::Error) -> Self {
        GildeError::Internal(format!("{e:#}"))
    }
}

impl From<tauri::Error> for GildeError {
    fn from(e: tauri::Error) -> Self {
        GildeError::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_carries_no_prose_but_keeps_stderr_verbatim() {
        let stderr = "fatal: 'nope' is not a valid ref\n";
        let payload: GildeErrorPayload = GildeError::Git {
            code: 128,
            stderr: stderr.to_string(),
        }
        .into();

        assert_eq!(payload.code, "GIT");
        assert_eq!(payload.key, "error.git");
        assert_eq!(payload.params.get("code"), Some(&Value::from(128)));
        assert_eq!(
            payload.detail.as_deref(),
            Some("fatal: 'nope' is not a valid ref")
        );
        assert!(!payload.retryable);
    }

    #[test]
    fn payload_serializes_as_camel_case_contract() {
        let payload: GildeErrorPayload = GildeError::Refused {
            reason: "dirty".into(),
        }
        .into();
        let v = serde_json::to_value(&payload).expect("serialize");

        assert_eq!(v["code"], Value::from("REFUSED"));
        assert_eq!(v["key"], Value::from("error.refused"));
        assert_eq!(v["params"]["reason"], Value::from("dirty"));
        assert_eq!(v["detail"], Value::Null);
        assert_eq!(v["retryable"], Value::from(false));
        // Exactly the five documented fields, nothing more.
        let obj = v.as_object().expect("object");
        assert_eq!(obj.len(), 5);
    }

    #[test]
    fn every_variant_has_a_distinct_code_and_a_key() {
        let all = vec![
            GildeError::Io(std::io::Error::other("x")),
            GildeError::Db("x".into()),
            GildeError::Config("x".into()),
            GildeError::Git {
                code: 1,
                stderr: String::new(),
            },
            GildeError::Spawn("x".into()),
            GildeError::SidecarDown,
            GildeError::SidecarRuntimeUnusable {
                path: "/x".into(),
                stderr: String::new(),
            },
            GildeError::NotFound("x".into()),
            GildeError::Refused {
                reason: "x".into(),
            },
            GildeError::Worktree("x".into()),
            GildeError::Pty("x".into()),
            GildeError::Internal("x".into()),
        ];

        let mut codes: Vec<&str> = all.iter().map(|e| e.code()).collect();
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count, "codes must be unique");

        for e in &all {
            assert!(e.key().starts_with("error."), "key: {}", e.key());
            let payload = GildeErrorPayload::from(e);
            assert!(!payload.code.is_empty());
        }
    }

    #[test]
    fn retryable_is_true_only_for_transient_failures() {
        assert!(GildeError::SidecarDown.retryable());
        assert!(GildeError::Io(std::io::Error::other("x")).retryable());
        assert!(!GildeError::Db("x".into()).retryable());
        assert!(!GildeError::Refused {
            reason: "locked".into()
        }
        .retryable());
    }

    #[test]
    fn question_mark_converts_gilde_error_to_payload() {
        fn inner() -> Result<()> {
            Err(GildeError::not_found("session:abc"))
        }
        fn command_like() -> CmdResult<()> {
            inner()?;
            Ok(())
        }

        let err = command_like().expect_err("should fail");
        assert_eq!(err.code, "NOT_FOUND");
        assert_eq!(err.params["what"], Value::from("session:abc"));
    }

    #[test]
    fn foreign_errors_convert_into_typed_variants() {
        let json_err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let e: GildeError = json_err.into();
        assert_eq!(e.code(), "CONFIG");

        let sql_err = rusqlite::Error::QueryReturnedNoRows;
        let e: GildeError = sql_err.into();
        assert_eq!(e.code(), "DB");
    }
}
