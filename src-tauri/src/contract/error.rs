//! The one IPC error shape (contract §4.4, D11).
//!
//! Every command returns `Result<<Name>Res, IpcError>`. `key` is an i18n key
//! that exists in both message tables (test-locked by T13); `params` carries
//! only strings, numbers and booleans — never content; `detail` **is** content
//! (tool stderr, sanitized, home paths as `~/…`, ≤ 4 KiB) and is never logged:
//! logs carry only `code`, `key` and the detail length [F-27].
//!
//! Interaction races are outcomes, not errors [C12]. The crate-internal
//! `GildeError` (`crate::error`) maps into this type.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use ts_rs::TS;

/// Cap on [`IpcError::detail`] in bytes (§4.4).
pub const MAX_DETAIL_BYTES: usize = 4 * 1024;

/// The closed set of error codes (§4.4 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum ErrorCode {
    InvalidArgument,
    NotFound,
    NotReady,
    Busy,
    Unsupported,
    Refused,
    Conflict,
    StalePrompt,
    SessionLiveElsewhere,
    PermissionDenied,
    DiskFull,
    SpawnFailed,
    GitFailed,
    VendorError,
    SidecarDown,
    Timeout,
    VersionMismatch,
    Internal,
}

impl ErrorCode {
    /// Every code, in the order of the §4.4 table.
    pub const ALL: [ErrorCode; 18] = [
        ErrorCode::InvalidArgument,
        ErrorCode::NotFound,
        ErrorCode::NotReady,
        ErrorCode::Busy,
        ErrorCode::Unsupported,
        ErrorCode::Refused,
        ErrorCode::Conflict,
        ErrorCode::StalePrompt,
        ErrorCode::SessionLiveElsewhere,
        ErrorCode::PermissionDenied,
        ErrorCode::DiskFull,
        ErrorCode::SpawnFailed,
        ErrorCode::GitFailed,
        ErrorCode::VendorError,
        ErrorCode::SidecarDown,
        ErrorCode::Timeout,
        ErrorCode::VersionMismatch,
        ErrorCode::Internal,
    ];

    /// The wire spelling, e.g. `"INVALID_ARGUMENT"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::NotReady => "NOT_READY",
            ErrorCode::Busy => "BUSY",
            ErrorCode::Unsupported => "UNSUPPORTED",
            ErrorCode::Refused => "REFUSED",
            ErrorCode::Conflict => "CONFLICT",
            ErrorCode::StalePrompt => "STALE_PROMPT",
            ErrorCode::SessionLiveElsewhere => "SESSION_LIVE_ELSEWHERE",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::DiskFull => "DISK_FULL",
            ErrorCode::SpawnFailed => "SPAWN_FAILED",
            ErrorCode::GitFailed => "GIT_FAILED",
            ErrorCode::VendorError => "VENDOR_ERROR",
            ErrorCode::SidecarDown => "SIDECAR_DOWN",
            ErrorCode::Timeout => "TIMEOUT",
            ErrorCode::VersionMismatch => "VERSION_MISMATCH",
            ErrorCode::Internal => "INTERNAL",
        }
    }

    pub fn parse(s: &str) -> Option<ErrorCode> {
        ErrorCode::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// The §4.4 "Retryable" column. `VENDOR_ERROR` is "maybe": `None`, the
    /// producer decides per instance (see [`IpcError::vendor_error`]).
    pub const fn default_retryable(self) -> Option<bool> {
        match self {
            ErrorCode::NotReady
            | ErrorCode::Busy
            | ErrorCode::PermissionDenied
            | ErrorCode::DiskFull
            | ErrorCode::SpawnFailed
            | ErrorCode::SidecarDown
            | ErrorCode::Timeout => Some(true),
            ErrorCode::VendorError => None,
            ErrorCode::InvalidArgument
            | ErrorCode::NotFound
            | ErrorCode::Unsupported
            | ErrorCode::Refused
            | ErrorCode::Conflict
            | ErrorCode::StalePrompt
            | ErrorCode::SessionLiveElsewhere
            | ErrorCode::GitFailed
            | ErrorCode::VersionMismatch
            | ErrorCode::Internal => Some(false),
        }
    }

    /// The default i18n key of a code. Specific keys (e.g. `error.compatFailed`,
    /// §9.12) may replace it per instance; every key must exist in ko and en.
    pub const fn default_key(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "error.invalidArgument",
            ErrorCode::NotFound => "error.notFound",
            ErrorCode::NotReady => "error.notReady",
            ErrorCode::Busy => "error.busy",
            ErrorCode::Unsupported => "error.unsupported",
            ErrorCode::Refused => "error.refused",
            ErrorCode::Conflict => "error.conflict",
            ErrorCode::StalePrompt => "error.stalePrompt",
            ErrorCode::SessionLiveElsewhere => "error.sessionLiveElsewhere",
            ErrorCode::PermissionDenied => "error.permissionDenied",
            ErrorCode::DiskFull => "error.diskFull",
            ErrorCode::SpawnFailed => "error.spawnFailed",
            ErrorCode::GitFailed => "error.gitFailed",
            ErrorCode::VendorError => "error.vendorError",
            ErrorCode::SidecarDown => "error.sidecarDown",
            ErrorCode::Timeout => "error.timeout",
            // §4.4: "Restart Gilde to finish updating." is §9.12 `update.restart`.
            ErrorCode::VersionMismatch => "update.restart",
            ErrorCode::Internal => "error.internal",
        }
    }

    /// The name of the code's documented parameter (§4.4 "Meaning" column).
    pub const fn param_name(self) -> Option<&'static str> {
        match self {
            ErrorCode::NotFound => Some("entity"),
            ErrorCode::NotReady => Some("reason"),
            ErrorCode::Busy => Some("retryAfterMs"),
            ErrorCode::Unsupported => Some("cap"),
            ErrorCode::Refused => Some("reason"),
            ErrorCode::Conflict => Some("changed"),
            ErrorCode::PermissionDenied => Some("category"),
            ErrorCode::VendorError => Some("class"),
            // INVALID_ARGUMENT carries two: `field` and `rule`.
            _ => None,
        }
    }
}

/// `VENDOR_ERROR` classes (`params.class`, §4.4, §4.5 `TurnEvent::Error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub enum VendorErrorClass {
    Auth,
    Quota,
    Exec,
    Unknown,
}

impl VendorErrorClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            VendorErrorClass::Auth => "auth",
            VendorErrorClass::Quota => "quota",
            VendorErrorClass::Exec => "exec",
            VendorErrorClass::Unknown => "unknown",
        }
    }
}

/// The one error shape (§4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", ts(export))]
pub struct IpcError {
    pub code: ErrorCode,
    /// i18n key, exists in ko and en (test-locked).
    pub key: String,
    /// Only strings/numbers/bools; no content.
    #[ts(type = "Record<string, string | number | boolean>")]
    pub params: Map<String, Value>,
    /// **[content]** tool stderr (git/CLI), sanitized, home paths as `~/…`,
    /// shown in mono, ≤ 4 KiB; NEVER logged.
    pub detail: Option<String>,
    pub retryable: bool,
}

impl IpcError {
    /// An error with the code's default key and retryability.
    pub fn new(code: ErrorCode) -> IpcError {
        IpcError {
            code,
            key: code.default_key().to_string(),
            params: Map::new(),
            detail: None,
            retryable: code.default_retryable().unwrap_or(false),
        }
    }

    /// Add a string parameter. Parameters are data, never prose or content.
    pub fn param(mut self, name: &str, value: impl Into<String>) -> IpcError {
        self.params.insert(name.to_string(), Value::String(value.into()));
        self
    }

    /// Add a numeric parameter. Values above 2^53 − 1 are clamped (§3.1 rule 3).
    pub fn param_num(mut self, name: &str, value: u64) -> IpcError {
        let v = value.min(crate::contract::common::MAX_SAFE_INTEGER as u64);
        self.params.insert(name.to_string(), Value::from(v));
        self
    }

    pub fn param_bool(mut self, name: &str, value: bool) -> IpcError {
        self.params.insert(name.to_string(), Value::Bool(value));
        self
    }

    /// Replace the default key with a more specific one (e.g. `error.compatFailed`).
    pub fn with_key(mut self, key: &str) -> IpcError {
        self.key = key.to_string();
        self
    }

    /// Attach a detail. The caller passes already-sanitized text; it is cut to
    /// 4 KiB at a char boundary here as a last guard.
    pub fn with_detail(mut self, detail: impl Into<String>) -> IpcError {
        let mut d: String = detail.into();
        if d.len() > MAX_DETAIL_BYTES {
            let mut cut = MAX_DETAIL_BYTES;
            while !d.is_char_boundary(cut) {
                cut -= 1;
            }
            d.truncate(cut);
        }
        self.detail = Some(d);
        self
    }

    pub fn with_retryable(mut self, retryable: bool) -> IpcError {
        self.retryable = retryable;
        self
    }

    // -- one constructor per code -------------------------------------------

    /// `INVALID_ARGUMENT{field, rule}`.
    pub fn invalid_argument(field: &str, rule: &str) -> IpcError {
        IpcError::new(ErrorCode::InvalidArgument)
            .param("field", field)
            .param("rule", rule)
    }

    /// `NOT_FOUND{entity}`.
    pub fn not_found(entity: &str) -> IpcError {
        IpcError::new(ErrorCode::NotFound).param("entity", entity)
    }

    /// `NOT_READY{reason}`.
    pub fn not_ready(reason: &str) -> IpcError {
        IpcError::new(ErrorCode::NotReady).param("reason", reason)
    }

    /// `BUSY{retryAfterMs}`.
    pub fn busy(retry_after_ms: u64) -> IpcError {
        IpcError::new(ErrorCode::Busy).param_num("retryAfterMs", retry_after_ms)
    }

    /// `UNSUPPORTED{cap}`.
    pub fn unsupported(cap: &str) -> IpcError {
        IpcError::new(ErrorCode::Unsupported).param("cap", cap)
    }

    /// `REFUSED{reason}`.
    pub fn refused(reason: &str) -> IpcError {
        IpcError::new(ErrorCode::Refused).param("reason", reason)
    }

    /// `CONFLICT{changed}`.
    pub fn conflict(changed: &str) -> IpcError {
        IpcError::new(ErrorCode::Conflict).param("changed", changed)
    }

    pub fn stale_prompt() -> IpcError {
        IpcError::new(ErrorCode::StalePrompt)
    }

    pub fn session_live_elsewhere() -> IpcError {
        IpcError::new(ErrorCode::SessionLiveElsewhere)
    }

    /// `PERMISSION_DENIED{category}` — a category, never a path.
    pub fn permission_denied(category: &str) -> IpcError {
        IpcError::new(ErrorCode::PermissionDenied).param("category", category)
    }

    pub fn disk_full() -> IpcError {
        IpcError::new(ErrorCode::DiskFull)
    }

    pub fn spawn_failed() -> IpcError {
        IpcError::new(ErrorCode::SpawnFailed)
    }

    pub fn git_failed() -> IpcError {
        IpcError::new(ErrorCode::GitFailed)
    }

    /// `VENDOR_ERROR{class}`; retryability is the producer's call (§4.4 "maybe").
    pub fn vendor_error(class: VendorErrorClass, retryable: bool) -> IpcError {
        IpcError::new(ErrorCode::VendorError)
            .param("class", class.as_str())
            .with_retryable(retryable)
    }

    pub fn sidecar_down() -> IpcError {
        IpcError::new(ErrorCode::SidecarDown)
    }

    pub fn timeout() -> IpcError {
        IpcError::new(ErrorCode::Timeout)
    }

    pub fn version_mismatch() -> IpcError {
        IpcError::new(ErrorCode::VersionMismatch)
    }

    /// `INTERNAL` with a correlation id in `detail` (never content).
    pub fn internal(correlation_id: &str) -> IpcError {
        IpcError::new(ErrorCode::Internal).with_detail(correlation_id)
    }

    /// The wave-0 stub answer of every command until its owner builds it:
    /// `UNSUPPORTED{cap: "notBuilt"}` (contract §15.1 H1).
    pub fn not_built() -> IpcError {
        IpcError::unsupported(NOT_BUILT)
    }

    /// What may be logged about this error: code, key and the detail length
    /// only [F-27].
    pub fn log_fields(&self) -> (&'static str, &str, usize) {
        (
            self.code.as_str(),
            self.key.as_str(),
            self.detail.as_ref().map_or(0, String::len),
        )
    }
}

/// `params.cap` of the wave-0 stubs (H1).
pub const NOT_BUILT: &str = "notBuilt";

impl std::fmt::Display for IpcError {
    /// Code and key only — `detail` is content and never formatted here.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.code.as_str(), self.key)
    }
}

impl std::error::Error for IpcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_the_five_documented_fields() {
        let e = IpcError::not_built();
        let v = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        assert_eq!(v["code"], "UNSUPPORTED");
        assert_eq!(v["params"]["cap"], "notBuilt");
        assert_eq!(v["detail"], Value::Null);
        assert_eq!(v["retryable"], false);
    }

    #[test]
    fn every_code_round_trips_and_has_a_key() {
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(c.as_str()), Some(c));
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            assert!(!c.default_key().is_empty());
        }
    }

    #[test]
    fn retryable_column_matches_the_table() {
        assert!(IpcError::new(ErrorCode::NotReady).retryable);
        assert!(IpcError::busy(10).retryable);
        assert!(!IpcError::refused("x").retryable);
        assert!(IpcError::vendor_error(VendorErrorClass::Quota, true).retryable);
        assert!(!IpcError::vendor_error(VendorErrorClass::Auth, false).retryable);
    }

    #[test]
    fn detail_is_capped_and_display_never_shows_it() {
        let e = IpcError::git_failed().with_detail("é".repeat(4000));
        assert!(e.detail.as_ref().unwrap().len() <= MAX_DETAIL_BYTES);
        assert!(!e.to_string().contains('é'));
    }
}
