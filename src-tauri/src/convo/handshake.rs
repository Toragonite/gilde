//! Spawn argv/env (contract §6.1.1), `initialize`, readiness and the runtime
//! compat check (§6.1.2), and the quota readers (`rate_limit_event`,
//! `get_usage`, §6.1.5, §6.1.7).
//!
//! Owner: T05.
//!
//! **Field allowlists** [R6 §3 LP-01, LP-09a, §8 items 3 and 13, §9.7]: the
//! `initialize` response is read through [`InitPayloadIn`], whose only fields
//! are `claude_code_version`, `capabilities`, `session_state`,
//! `current_permission_mode`, `models`, `pid` and `account.tokenSource`;
//! `account.email`, `organization` and `subscriptionType` are skipped by the
//! parser and never become values. `get_usage` is read through
//! [`UsageIn`]: `rate_limits.{five_hour,seven_day}` and `limits[]` only —
//! `behaviors` (it quotes file paths of other projects) is dropped at parse
//! time. Raw responses are never logged.

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::contract::account::{Quota, QuotaWindow, QuotaWindowKind};
use crate::contract::common::{AbsPath, EpochMs, PermissionMode};
use crate::contract::error::IpcError;
use crate::contract::ids::is_uuid;
use crate::contract::ipc::ModelOption;
use crate::convo::wire::{ControlBody, RateLimitIn, ResponseInner};
use crate::convo::Flags;
use crate::render::sanitize::display;
use crate::contract::display::TextCap;

/// How long Gilde waits for the `initialize` response (§6.1.2 step 6; ~13 s
/// observed with the sandbox on [R6 §3 LP-07b]).
pub const INITIALIZE_TIMEOUT_MS: u64 = 30_000;

/// The four launch kinds of §6.1.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchKind {
    /// `--session-id <uuidv4 minted by Gilde>`.
    New { session_id: String },
    /// `--resume <sessionId>`.
    Continue { session_id: String },
    /// `--resume <sessionId> --fork-session`.
    Copy { session_id: String },
    /// `--resume <absolute transcript path> --fork-session` (until LP-25, §6.5).
    OtherAccount { transcript: AbsPath },
}

impl LaunchKind {
    /// The vendor session id the process will have, when known before
    /// `system/init` (a copy gets a new id from the CLI).
    pub fn known_session_id(&self) -> Option<&str> {
        match self {
            LaunchKind::New { session_id } | LaunchKind::Continue { session_id } => Some(session_id),
            LaunchKind::Copy { .. } | LaunchKind::OtherAccount { .. } => None,
        }
    }

    /// Whether this process resumes an earlier conversation (its first
    /// `result` may carry cost restored from before, §6.1.7 usage).
    pub fn resumes(&self) -> bool {
        !matches!(self, LaunchKind::New { .. })
    }
}

/// What the argv depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgvSpec {
    pub launch: LaunchKind,
    /// Always passed (default `default`) [R6 §1.5, §8 item 16].
    pub permission_mode: PermissionMode,
    pub model: Option<String>,
    /// The user chose `bypassPermissions` at start in the New-session sheet.
    pub allow_bypass: bool,
}

/// A model name as an argv element: never an option-looking or blank value.
pub fn valid_model(m: &str) -> bool {
    let b = m.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._:/[]-".contains(c))
}

/// The exact argv after `<claude>` (§6.1.1 table). Flags: P1 drops `-p` in
/// its fallback; P3 drops `--permission-prompts host` in its fallback.
pub fn argv(spec: &ArgvSpec, flags: &Flags) -> Result<Vec<String>, IpcError> {
    let mut a: Vec<String> = Vec::with_capacity(24);
    if flags.handshake_shape {
        a.push("-p".into());
    }
    for s in [
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--replay-user-messages",
        "--permission-prompt-tool",
        "stdio",
    ] {
        a.push(s.into());
    }
    if flags.permission_prompts_host {
        a.push("--permission-prompts".into());
        a.push("host".into());
    }
    match &spec.launch {
        LaunchKind::New { session_id } => {
            if !is_uuid(session_id, Some(4)) {
                return Err(IpcError::invalid_argument("sessionId", "pattern"));
            }
            a.push("--session-id".into());
            a.push(session_id.clone());
        }
        LaunchKind::Continue { session_id } | LaunchKind::Copy { session_id } => {
            if !is_uuid(session_id, None) {
                return Err(IpcError::invalid_argument("sessionId", "pattern"));
            }
            a.push("--resume".into());
            a.push(session_id.clone());
            if matches!(spec.launch, LaunchKind::Copy { .. }) {
                a.push("--fork-session".into());
            }
        }
        LaunchKind::OtherAccount { transcript } => {
            let p = transcript.as_str();
            if !p.starts_with('/') || !p.ends_with(".jsonl") {
                return Err(IpcError::invalid_argument("transcript", "path"));
            }
            a.push("--resume".into());
            a.push(p.to_owned());
            a.push("--fork-session".into());
        }
    }
    if spec.permission_mode == PermissionMode::BypassPermissions && !spec.allow_bypass {
        return Err(IpcError::refused("needsLaunchFlag"));
    }
    a.push("--permission-mode".into());
    a.push(spec.permission_mode.as_cli().into());
    if let Some(m) = spec.model.as_deref().filter(|m| !m.is_empty()) {
        if !valid_model(m) {
            return Err(IpcError::invalid_argument("model", "pattern"));
        }
        a.push("--model".into());
        a.push(m.to_owned());
    }
    if spec.allow_bypass && spec.permission_mode == PermissionMode::BypassPermissions {
        a.push("--allow-dangerously-skip-permissions".into());
    }
    Ok(a)
}

/// The variables Conversation adds to the composed env (§6.1.1): session
/// state events always [R4 §2]; the entrypoint marker per P14. Gilde never
/// adds `DISABLE_AUTOUPDATER` (§6.0, A1).
pub fn env_additions(flags: &Flags) -> Vec<(&'static str, &'static str)> {
    let mut v = vec![("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")];
    if flags.entrypoint_marker {
        v.push(("CLAUDE_CODE_ENTRYPOINT", "gilde"));
    }
    v
}

/// `initialize` with no hooks, no `perTaskStopAffordance`, no dialog kinds
/// (§6.1.2 step 1).
pub fn initialize_body() -> ControlBody {
    ControlBody::Initialize { supported_dialog_kinds: Vec::new() }
}

/// A `--permission-mode`/`permissionMode` spelling back to the enum.
pub fn permission_mode_of(s: &str) -> Option<PermissionMode> {
    Some(match s {
        "default" => PermissionMode::Default,
        "acceptEdits" => PermissionMode::AcceptEdits,
        "plan" => PermissionMode::Plan,
        "dontAsk" => PermissionMode::DontAsk,
        "auto" => PermissionMode::Auto,
        "bypassPermissions" => PermissionMode::BypassPermissions,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// the initialize response (allowlist)
// ---------------------------------------------------------------------------

/// The `initialize` payload, allowlisted (unknown keys are skipped).
#[derive(Debug, Deserialize)]
pub struct InitPayloadIn {
    #[serde(default)]
    pub claude_code_version: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub session_state: Option<String>,
    #[serde(default)]
    pub current_permission_mode: Option<String>,
    #[serde(default)]
    pub models: Option<Vec<ModelIn>>,
    #[serde(default)]
    pub account: Option<AccountAllowIn>,
    #[serde(default)]
    pub pid: Option<i64>,
}

/// `account.tokenSource` only — never `email`, `organization`, `subscriptionType`.
#[derive(Debug, Deserialize)]
pub struct AccountAllowIn {
    #[serde(rename = "tokenSource", default)]
    pub token_source: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelIn {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
}

/// What a successful handshake tells the session.
#[derive(Debug, Clone)]
pub struct InitInfo {
    pub version: Option<String>,
    /// The `initialize` capability list (e.g. `ui_surface_v1`), distinct
    /// from `system/init.capabilities`.
    pub capabilities: Vec<String>,
    pub session_state: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    /// A33.3: `current_permission_mode` was present but is not a mode Gilde
    /// knows (the session's mode is then unknown, never the previous one).
    pub permission_mode_unknown: bool,
    pub models: Vec<ModelOption>,
    /// `account.tokenSource == "none"` → Sign-in needed before any turn.
    pub signed_out: bool,
    pub pid: Option<i64>,
    /// Complete `control_request` frames to card (dedupe by `request_id`).
    pub pending: Vec<Box<RawValue>>,
}

/// The runtime compat check (§6.1.2 step 3).
#[derive(Debug, Clone)]
pub enum InitVerdict {
    Ok(InitInfo),
    /// An `error` subtype, an unparseable response, or no
    /// `pending_permission_requests` array → `conversation: Failed`.
    CompatFailed(&'static str),
}

/// Read the `initialize` reply. `inner` is the `control_response.response`
/// object whose `request_id` is Gilde's `initialize` id.
pub fn parse_initialize(inner: &ResponseInner<'_>) -> InitVerdict {
    if !inner.is_success() {
        return InitVerdict::CompatFailed("initialize answered with an error");
    }
    let Some(pending_raw) = inner.pending_permission_requests else {
        return InitVerdict::CompatFailed("initialize response without pending_permission_requests");
    };
    let Ok(pending) = serde_json::from_str::<Vec<Box<RawValue>>>(pending_raw.get()) else {
        return InitVerdict::CompatFailed("pending_permission_requests is not an array");
    };
    let payload = match inner.response {
        Some(raw) => match serde_json::from_str::<InitPayloadIn>(raw.get()) {
            Ok(p) => p,
            Err(_) => return InitVerdict::CompatFailed("unparseable initialize response"),
        },
        None => return InitVerdict::CompatFailed("initialize response without a payload"),
    };
    let models = payload
        .models
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let id = m.value.filter(|v| !v.is_empty() && v.len() <= 128)?;
            let id = display(&id, TextCap::Chars(128));
            Some(ModelOption {
                is_default: id == "default",
                label: m.display_name.map(|d| display(&d, TextCap::Chars(80))).filter(|d| !d.is_empty()),
                id,
            })
        })
        .take(64)
        .collect();
    InitVerdict::Ok(InitInfo {
        version: payload.claude_code_version.filter(|v| crate::contract::compat::parse_version(v).is_some()),
        capabilities: string_list(payload.capabilities),
        session_state: payload.session_state.filter(|s| s.len() <= 32),
        permission_mode: payload.current_permission_mode.as_deref().and_then(permission_mode_of),
        permission_mode_unknown: payload.current_permission_mode.as_deref().is_some_and(|m| permission_mode_of(m).is_none()),
        models,
        signed_out: payload.account.and_then(|a| a.token_source).as_deref() == Some("none"),
        pid: payload.pid,
        pending,
    })
}

/// A capability list: strings only, short, bounded.
pub fn string_list(v: Option<Vec<serde_json::Value>>) -> Vec<String> {
    v.unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.as_str().filter(|s| !s.is_empty() && s.len() <= 64).map(str::to_owned))
        .take(128)
        .collect()
}

// ---------------------------------------------------------------------------
// quota: rate_limit_event and get_usage
// ---------------------------------------------------------------------------

fn epoch_ms_of(v: f64) -> Option<EpochMs> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    // Seconds (observed: `resetsAt: 1792643400`); a value that is already
    // milliseconds is kept.
    let ms = if v < 1e11 { v * 1000.0 } else { v };
    let ms = ms.round() as i64;
    crate::contract::common::is_epoch_ms(ms).then_some(ms)
}

/// `rate_limit_event.rate_limit_info.unifiedWindows` → the account's quota
/// (utilization is a 0..1 fraction) [R6 §3 LP-01, LP-09].
pub fn quota_from_rate_limit(ev: &RateLimitIn, now: EpochMs) -> Option<Quota> {
    let w = ev.rate_limit_info.as_ref()?.unified_windows.as_ref()?;
    let mut windows = Vec::new();
    for (kind, win) in [(QuotaWindowKind::FiveHour, &w.five_hour), (QuotaWindowKind::Week, &w.seven_day)] {
        let Some(win) = win else { continue };
        let used = win.utilization.and_then(|u| QuotaWindow::normalise_pct(u, true));
        let resets = win.resets_at.and_then(epoch_ms_of);
        if used.is_none() && resets.is_none() {
            continue;
        }
        windows.push(QuotaWindow { kind, label: None, used_pct: used, resets_at: resets });
    }
    if windows.is_empty() {
        return None;
    }
    Some(Quota { windows, fetched_at: now, stale: false, unavailable: None })
}

/// The `get_usage` payload, allowlisted: `rate_limits` and
/// `session.total_cost_usd` [A19.5]; `behaviors` and everything else is
/// skipped by the parser.
#[derive(Debug, Deserialize)]
pub struct UsageIn {
    #[serde(default)]
    pub rate_limits: Option<RateLimitsIn>,
    #[serde(default)]
    pub session: Option<SessionUsageIn>,
}

#[derive(Debug, Deserialize)]
pub struct SessionUsageIn {
    /// Cumulative for the process, restored on `--resume` [R6 §3 LP-09a].
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
}

/// `get_usage.session.total_cost_usd` (finite, ≥ 0) [A19.5].
pub fn session_cost_from_usage(payload: &RawValue) -> Option<f64> {
    let u: UsageIn = serde_json::from_str(payload.get()).ok()?;
    u.session?.total_cost_usd.filter(|c| c.is_finite() && *c >= 0.0)
}

#[derive(Debug, Deserialize)]
pub struct RateLimitsIn {
    #[serde(default)]
    pub five_hour: Option<UsageWindowIn>,
    #[serde(default)]
    pub seven_day: Option<UsageWindowIn>,
    #[serde(default)]
    pub limits: Option<Vec<LimitIn>>,
}

#[derive(Debug, Deserialize)]
pub struct UsageWindowIn {
    /// Integer percent.
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LimitIn {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// `get_usage` → quota (§6.1.7): `five_hour`/`seven_day` (integer %), and a
/// `limits[]` entry of kind `session`/`weekly_all` where the window itself is
/// missing.
pub fn quota_from_usage(payload: &RawValue, now: EpochMs) -> Option<Quota> {
    let u: UsageIn = serde_json::from_str(payload.get()).ok()?;
    let r = u.rate_limits?;
    let iso = |s: &Option<String>| s.as_deref().and_then(crate::render::transcript::parse_rfc3339_ms);
    let mut windows: Vec<QuotaWindow> = Vec::new();
    for (kind, win) in [(QuotaWindowKind::FiveHour, &r.five_hour), (QuotaWindowKind::Week, &r.seven_day)] {
        let Some(win) = win else { continue };
        let used = win.utilization.and_then(|u| QuotaWindow::normalise_pct(u, false));
        let resets = iso(&win.resets_at);
        if used.is_some() || resets.is_some() {
            windows.push(QuotaWindow { kind, label: None, used_pct: used, resets_at: resets });
        }
    }
    for l in r.limits.unwrap_or_default().iter().take(32) {
        let kind = match l.kind.as_deref() {
            Some("session") => QuotaWindowKind::FiveHour,
            Some("weekly_all") => QuotaWindowKind::Week,
            _ => continue,
        };
        if windows.iter().any(|w| w.kind == kind) {
            continue;
        }
        let used = l.percent.and_then(|p| QuotaWindow::normalise_pct(p, false));
        let resets = iso(&l.resets_at);
        if used.is_some() || resets.is_some() {
            windows.push(QuotaWindow { kind, label: None, used_pct: used, resets_at: resets });
        }
    }
    if windows.is_empty() {
        return None;
    }
    Some(Quota { windows, fetched_at: now, stale: false, unavailable: None })
}

// ---------------------------------------------------------------------------
// usage errors naming Gilde's flags
// ---------------------------------------------------------------------------

/// An exit before `initialize` is compat evidence only when its stderr is a
/// usage error naming one of Gilde's flags (§6.1.2 step 6, §6.1.9). Returns
/// the flag. `markers` come from `error-patterns.json` (`usageErrorMarkers`).
pub fn usage_error_flag<'a>(stderr: &str, argv: &'a [String], markers: &[String]) -> Option<&'a str> {
    let low = stderr.to_ascii_lowercase();
    if !markers.iter().any(|m| low.contains(&m.to_ascii_lowercase())) {
        return None;
    }
    argv.iter().map(String::as_str).filter(|a| a.starts_with('-') && a.len() > 2).find(|flag| stderr.contains(flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(launch: LaunchKind) -> ArgvSpec {
        ArgvSpec { launch, permission_mode: PermissionMode::Default, model: None, allow_bypass: false }
    }

    const SID: &str = "0ff2360d-9ebf-4152-ad44-5fb0754c7823";

    #[test]
    fn argv_is_exact_per_launch_kind() {
        let f = Flags::all(true);
        let new = argv(&spec(LaunchKind::New { session_id: SID.into() }), &f).unwrap();
        assert_eq!(
            new.join(" "),
            format!("-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --replay-user-messages --permission-prompt-tool stdio --permission-prompts host --session-id {SID} --permission-mode default")
        );
        let cont = argv(&spec(LaunchKind::Continue { session_id: SID.into() }), &f).unwrap().join(" ");
        assert!(cont.contains(&format!("--resume {SID} --permission-mode default")) && !cont.contains("--session-id"));
        let copy = argv(&spec(LaunchKind::Copy { session_id: SID.into() }), &f).unwrap().join(" ");
        assert!(copy.contains(&format!("--resume {SID} --fork-session")));
        let other = argv(
            &spec(LaunchKind::OtherAccount { transcript: AbsPath::parse("/Users/u/.claude-b/projects/x/s.jsonl").unwrap() }),
            &f,
        )
        .unwrap()
        .join(" ");
        assert!(other.contains("--resume /Users/u/.claude-b/projects/x/s.jsonl --fork-session"));
    }

    #[test]
    fn permission_mode_is_always_passed_and_bypass_needs_the_launch_choice() {
        let f = Flags::all(true);
        let mut s = spec(LaunchKind::New { session_id: SID.into() });
        s.permission_mode = PermissionMode::Plan;
        s.model = Some("claude-opus-5-5[1m]".into());
        let a = argv(&s, &f).unwrap().join(" ");
        assert!(a.ends_with("--permission-mode plan --model claude-opus-5-5[1m]"));
        s.permission_mode = PermissionMode::BypassPermissions;
        assert_eq!(argv(&s, &f).unwrap_err().params["reason"], "needsLaunchFlag");
        s.allow_bypass = true;
        assert!(argv(&s, &f).unwrap().join(" ").ends_with("--permission-mode bypassPermissions --model claude-opus-5-5[1m] --allow-dangerously-skip-permissions"));
        s.model = Some("--evil".into());
        assert_eq!(argv(&s, &f).unwrap_err().params["field"], "model");
        let bad = spec(LaunchKind::New { session_id: "not-a-uuid".into() });
        assert!(argv(&bad, &f).is_err());
    }

    #[test]
    fn fallbacks_drop_the_provisional_flags() {
        let a = argv(&spec(LaunchKind::New { session_id: SID.into() }), &Flags::all(false)).unwrap();
        assert!(!a.contains(&"-p".to_string()));
        assert!(!a.contains(&"--permission-prompts".to_string()));
        assert!(a.contains(&"--permission-mode".to_string()), "the mode is passed in every version");
        assert_eq!(env_additions(&Flags::all(false)), vec![("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")]);
        assert!(env_additions(&Flags::all(true)).contains(&("CLAUDE_CODE_ENTRYPOINT", "gilde")));
    }

    fn inner(json: &str) -> InitVerdict {
        let inner: ResponseInner<'_> = serde_json::from_str(json).unwrap();
        parse_initialize(&inner)
    }

    #[test]
    fn the_handshake_reads_only_its_allowlist() {
        let v = inner(
            r#"{"subtype":"success","request_id":"g","pending_permission_requests":[],"pending_user_dialog_requests":[],
               "response":{"claude_code_version":"2.1.306","capabilities":["ui_surface_v1"],"session_state":"idle",
               "current_permission_mode":"default","pid":4242,"models":[{"value":"default","displayName":"Default"},{"value":"opus"}],
               "account":{"email":"someone@example.com","organization":"Org","subscriptionType":"max","tokenSource":"none"},
               "commands":[{"name":"x"}]}}"#,
        );
        let InitVerdict::Ok(info) = v else { panic!("ok expected") };
        assert_eq!(info.version.as_deref(), Some("2.1.306"));
        assert!(info.signed_out);
        assert_eq!(info.pid, Some(4242));
        assert_eq!(info.models.len(), 2);
        assert!(info.models[0].is_default);
        assert_eq!(info.permission_mode, Some(PermissionMode::Default));
        let dbg = format!("{info:?}");
        assert!(!dbg.contains("someone@example.com") && !dbg.contains("Org") && !dbg.contains("max"));
    }

    #[test]
    fn compat_failures() {
        assert!(matches!(inner(r#"{"subtype":"error","request_id":"g","error":"nope"}"#), InitVerdict::CompatFailed(_)));
        assert!(matches!(inner(r#"{"subtype":"success","request_id":"g","response":{}}"#), InitVerdict::CompatFailed(_)));
        assert!(matches!(
            inner(r#"{"subtype":"success","request_id":"g","response":{},"pending_permission_requests":{}}"#),
            InitVerdict::CompatFailed(_)
        ));
        assert!(matches!(
            inner(r#"{"subtype":"success","request_id":"g","response":{},"pending_permission_requests":[]}"#),
            InitVerdict::Ok(_)
        ));
    }

    #[test]
    fn quota_readers() {
        let ev: RateLimitIn = serde_json::from_str(
            r#"{"rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.42,"resetsAt":1792643400},"seven_day":{"utilization":0.1}}}}"#,
        )
        .unwrap();
        let q = quota_from_rate_limit(&ev, 5).unwrap();
        assert_eq!(q.windows[0].used_pct, Some(42));
        assert_eq!(q.windows[0].resets_at, Some(1_792_643_400_000));
        assert_eq!(q.windows[1].kind, QuotaWindowKind::Week);
        let raw = RawValue::from_string(
            r#"{"rate_limits":{"five_hour":{"utilization":37,"resets_at":"2026-10-22T05:00:00Z"},"limits":[{"kind":"weekly_all","percent":12}]},"behaviors":{"day":"see /Users/x/other-project"}}"#.into(),
        )
        .unwrap();
        let q = quota_from_usage(&raw, 5).unwrap();
        assert_eq!(q.windows[0].used_pct, Some(37));
        assert!(q.windows[0].resets_at.is_some());
        assert_eq!(q.windows[1].kind, QuotaWindowKind::Week);
        assert!(!format!("{q:?}").contains("other-project"));
        let raw = RawValue::from_string(r#"{"session":{"total_cost_usd":0.0024,"total_lines_added":3},"behaviors":{"day":"x"}}"#.into()).unwrap();
        assert_eq!(session_cost_from_usage(&raw), Some(0.0024));
        let bad = RawValue::from_string(r#"{"session":{"total_cost_usd":-1}}"#.into()).unwrap();
        assert_eq!(session_cost_from_usage(&bad), None);
    }

    #[test]
    fn usage_errors_must_name_our_flag() {
        let argv = vec!["--permission-prompts".to_string(), "host".to_string()];
        let markers = vec!["unknown option".to_string()];
        assert_eq!(usage_error_flag("error: unknown option '--permission-prompts'", &argv, &markers), Some("--permission-prompts"));
        assert_eq!(usage_error_flag("error: unknown option '--other'", &argv, &markers), None);
        assert_eq!(usage_error_flag("crash in --permission-prompts", &argv, &markers), None);
    }
}
