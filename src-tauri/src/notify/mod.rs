//! Notifications (contract §8.4–§8.5).
//!
//! Owner: T12. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.
//!
//! - [`policy`]: the pure decision engine (episodes, gates, grace periods,
//!   burst guard, coalescing, reminder, withdrawal).
//! - [`sound`]: the band-1 sound rule.
//! - [`center`]: `UNUserNotificationCenter` through objc2 (PROVISIONAL LP-28,
//!   app flag `notificationCenter`, bundled app only).
//! - [`fallback`]: `tauri-plugin-notification` from Rust (no withdrawal, a
//!   click only activates Gilde) and the disabled sink.
//! - this file: the text of a notification ([`render`], details hidden by
//!   default [F-31]), the OS-boundary traits ([`NotificationSink`],
//!   [`FocusEmitter`], [`LabelSource`], [`Clock`]) and [`NotifyService`], the
//!   `NotifyApi` implementation that runs the policy on its own task.
//!
//! # Threading
//!
//! `NotifyApi::queue_changed` is called by World on every queue change,
//! possibly under World's lock. It therefore only writes the dock badge
//! (deduplicated, non-blocking) and hands a copy of the queue to the worker
//! task; labels are resolved through `WorldApi` **on the worker**, never on
//! the caller's stack (footgun pinned: a synchronous `world.session()` from
//! inside World's callback would deadlock). A queue whose `rev` is not newer
//! than the last one taken is dropped before the badge (A48.2), so a late,
//! older queue can neither set an older count nor reach the policy.
//!
//! # Privacy
//!
//! A notification carries the project name, a content-free session label and
//! the state word by default; titles (which may be a first prompt) and the
//! request's one-line summary appear only with `notify.showDetails` on
//! (§8.4, §9.7, PM-33). Merged, repeat and reminder notifications never carry
//! request text. Logs carry ids, kinds and counts only.
//!
//! The content-free session label (A48.1) is the project name and the vendor,
//! with an ordinal when two sessions of the project share them ("payments ·
//! claude 2"). It is never a teammate name, a worktree name (by default the
//! slug of the first prompt words, §4.2.2) or a title: those are the richer
//! label shown only with details on.
//!
//! # Composition-root wiring (T09b, A10.6) — exactly these calls
//!
//! In `setup()` (main thread), with `app: &AppHandle<Wry>`, `mtm =
//! MainThreadMarker::new().unwrap()`, World `world: Arc<dyn WorldApi>`, compat
//! `compat: Arc<dyn CompatApi>`:
//!
//! 1. Builder: `.plugin(tauri_plugin_notification::init())` (the LP-28
//!    fallback; Rust side only, §9.2) — already in T09's `run()`.
//! 2. `let system = macos::sleep::SystemEvents::new();`
//!    `macos::sleep::install_observers(mtm, system.clone());`
//! 3. `let visibility = macos::visibility::VisibilityHub::new(Arc::new(macos::visibility::TauriVisibilityEmitter::new(app.clone())));`
//!    `visibility.follow(&system);`
//!    `macos::visibility::install_observers(mtm, visibility.clone());`
//! 4. `let sink = notify::build_sink(app, compat.app_feature("notificationCenter"));`
//! 5. `let notify = notify::NotifyService::start(notify::NotifyDeps { sink,
//!    dock: Arc::new(macos::dock::MacDock::new(app.clone())), visibility:
//!    visibility.clone(), system: system.clone(), labels:
//!    Arc::new(notify::WorldLabels::new(world.clone())), focus:
//!    Arc::new(notify::TauriFocusEmitter::new(app.clone())), clock:
//!    Arc::new(notify::SystemClock) }, notify::NotifyConfig::default());`
//! 6. `notify.set_settings(settings.notify.clone()); notify.set_locale(notify::Locale::from_resolved(<"ko"|"en">));`
//!    and the same two calls after every `settings_set` that changes them.
//! 7. World: hand `notify.clone()` (as `Arc<dyn NotifyApi>`) to World so it
//!    calls `queue_changed` on every queue change; `services.bind_notify(notify.clone())`.
//! 8. `notify.on_pause_change(Arc::new(move |until| sink.notify_paused(until)))`
//!    with `sink = world.sink()` (A16.1: World sets `AttentionQueue.paused_until`).
//! 9. `app.manage(notify::NotifyState::new(notify.clone(), world.clone()));`
//! 10. `let owner = macos::owner::OwnerFocusService::new(world.clone(),
//!     Arc::new(macos::owner::MacActivator), compat.app_feature("vscodeWindowFocus"));`
//!     `owner.set_ide(ide)` once T11's `IdeApi` exists;
//!     `app.manage(macos::owner::OwnerState(owner));`
//! 11. After the boot sweep and discovery's first reconcile: `notify.boot_settled();`
//!     (without it the boot settles 15 s after start).
//! 12. Watchdog: on each memory-pressure transition `notify.health_memory(high)`.
//! 13. Other consumers subscribe as they need: `system.subscribe(..)` (wake →
//!     discovery reconcile + watchdog hysteresis reset; sleep → pause scans;
//!     time-zone change → World's "ended today" boundary), and
//!     `visibility.subscribe(..)` (discovery's scan cadence).

pub mod center;
pub mod fallback;
pub mod policy;
pub mod sound;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::sync::{mpsc, oneshot};

use crate::contract::attention::{AttentionQueue, AttentionReason, PAUSED_UNTIL_RESUME};
use crate::contract::activity::ErrorSub;
use crate::contract::common::{EpochMs, Vendor};
use crate::contract::display::TextCap;
use crate::contract::ids::SessionUid;
use crate::contract::ipc::{NotifySettings, PauseUntil, ViewState};
use crate::contract::project::LocationKind;
use crate::contract::session::{Control, Session};
use crate::contract::services::{NotifyApi, WorldApi};
use crate::contract::store::{event_names, FocusRequestEvent, FocusTarget, StoreSnapshot, VisibilityEvent};
use crate::macos::dock::{DockBadge, DockSink};
use crate::macos::sleep::{SystemEvent, SystemEvents};
use crate::macos::visibility::VisibilityHub;
use crate::render::sanitize::{sanitize_display_text, sanitize_label_text};

pub use policy::{Authorization, Now};
use policy::{HealthKind, PlanKind, Planned, Policy};

// ---------------------------------------------------------------------------
// strings (the TS message tables are the source; a test locks the equality)
// ---------------------------------------------------------------------------

/// The resolved UI locale (`app_bootstrap.locale`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    Ko,
    En,
}

impl Locale {
    /// From the resolved locale string; anything but "en" is Korean, the
    /// default locale (M5).
    pub fn from_resolved(locale: &str) -> Locale {
        if locale == "en" {
            Locale::En
        } else {
            Locale::Ko
        }
    }
}

/// `(key, ko, en)` — copied verbatim from `src/i18n/areas/*.ts`
/// (`tests/notify_strings.rs` fails on any drift).
pub const STRINGS: &[(&str, &str, &str)] = &[
    ("notify.body", "{session} · {state}", "{session} · {state}"),
    (
        "notify.many",
        "세션 {n}개가 기다리고 있습니다.",
        "{n, plural, one {# session is} other {# sessions are}} waiting for you.",
    ),
    ("notify.repeat", "{session} 세션이 계속 응답을 기다리고 있습니다.", "{session} needs you repeatedly."),
    ("notify.stillWaiting", "아직 기다리는 중 · {t}", "still waiting · {t}"),
    ("state.needsApproval", "승인 대기", "Needs approval"),
    ("state.needsAnswer", "답변 대기", "Needs answer"),
    ("state.error.usageLimit", "한도 도달", "Usage limit"),
    ("state.error.signIn", "로그인 필요", "Sign-in needed"),
    ("state.error.didntStart", "시작 실패", "Didn't start"),
    ("state.error.agent", "에이전트 오류", "Agent error"),
    ("state.crashed", "비정상 종료", "Crashed"),
    ("health.memHigh", "메모리 압박이 높습니다.", "Memory pressure is high."),
    ("firstRun.notify", "세션이 나를 기다릴 때 알림을 받을 수 있습니다.", "Get a notification when a session needs you."),
];

/// The product name, used as the title where no project applies.
pub const APP_NAME: &str = "Gilde";

fn tr(locale: Locale, key: &str) -> &'static str {
    STRINGS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, ko, en)| if locale == Locale::Ko { *ko } else { *en })
        .unwrap_or("")
}

/// Fills `{name}` placeholders and the one ICU form the tables use,
/// `{n, plural, one {…} other {…}}` (`#` = the number). Unknown
/// placeholders are kept verbatim.
pub fn format_message(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        let Some(close) = matching_brace(tail) else {
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &tail[..close];
        out.push_str(&expand(inner, params));
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Index of the `}` closing a `{` whose content starts at `s[0]`.
fn matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(i),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn expand(inner: &str, params: &[(&str, &str)]) -> String {
    let lookup = |name: &str| params.iter().find(|(k, _)| *k == name.trim()).map(|(_, v)| *v);
    if let Some((name, rest)) = inner.split_once(',') {
        let rest = rest.trim_start();
        if let Some(options) = rest.strip_prefix("plural,") {
            let n = lookup(name).unwrap_or("");
            let pick = if n == "1" { "one" } else { "other" };
            if let Some(text) = plural_option(options, pick).or_else(|| plural_option(options, "other")) {
                return text.replace('#', n);
            }
        }
        return format!("{{{inner}}}");
    }
    lookup(inner).map(str::to_string).unwrap_or_else(|| format!("{{{inner}}}"))
}

fn plural_option<'a>(options: &'a str, which: &str) -> Option<&'a str> {
    let mut rest = options.trim_start();
    while !rest.is_empty() {
        let open = rest.find('{')?;
        let label = rest[..open].trim();
        let tail = &rest[open + 1..];
        let close = matching_brace(tail)?;
        if label == which {
            return Some(&tail[..close]);
        }
        rest = tail[close + 1..].trim_start();
    }
    None
}

/// The state word of a band 1–3 reason (ux-ia #3, #4, #6–#10).
pub fn state_word(reason: &AttentionReason, locale: Locale) -> &'static str {
    let key = match reason {
        AttentionReason::NeedsApproval => "state.needsApproval",
        AttentionReason::NeedsAnswer => "state.needsAnswer",
        AttentionReason::Error { sub } => match sub {
            ErrorSub::UsageLimit { .. } => "state.error.usageLimit",
            ErrorSub::SignInNeeded => "state.error.signIn",
            ErrorSub::DidntStart { .. } => "state.error.didntStart",
            ErrorSub::AgentError => "state.error.agent",
        },
        AttentionReason::Crashed => "state.crashed",
        // never notified (bands 4–6); a neutral word if ever rendered
        AttentionReason::NoProgress { .. } | AttentionReason::NewReply | AttentionReason::Leftovers => {
            "state.error.agent"
        }
    };
    tr(locale, key)
}

/// A short duration like the WebView's `formatDuration` (ko `20분` /
/// `1.5시간` / `2일`, en `20 min` / `1.5 hr` / `2 days`).
pub fn format_duration(ms: u64, locale: Locale) -> String {
    const MIN: f64 = 60_000.0;
    const HOUR: f64 = 60.0 * MIN;
    const DAY: f64 = 24.0 * HOUR;
    let ms = ms as f64;
    let (value, digits, unit) = if ms < HOUR {
        ((ms / MIN).round().max(1.0), 0, 0)
    } else if ms < DAY {
        (ms / HOUR, if ms < 10.0 * HOUR { 1 } else { 0 }, 1)
    } else {
        (ms / DAY, if ms < 10.0 * DAY { 1 } else { 0 }, 2)
    };
    let mut number = format!("{value:.digits$}");
    if number.contains('.') {
        number = number.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    match (locale, unit) {
        (Locale::Ko, 0) => format!("{number}분"),
        (Locale::Ko, 1) => format!("{number}시간"),
        (Locale::Ko, _) => format!("{number}일"),
        (Locale::En, 0) => format!("{number} min"),
        (Locale::En, 1) => format!("{number} hr"),
        (Locale::En, _) if number == "1" => "1 day".to_string(),
        (Locale::En, _) => format!("{number} days"),
    }
}

// ---------------------------------------------------------------------------
// labels and rendering
// ---------------------------------------------------------------------------

/// What a notification may say about one session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionLabels {
    /// The project name (meta, sanitized as a label).
    pub project: Option<String>,
    /// The content-free label shown with details off (A48.1): the project
    /// name and the vendor, plus an ordinal when two sessions of the project
    /// share them ("payments · claude 2"); without a project, the vendor.
    pub session: String,
    /// The richer label, only with `notify.showDetails` (A16.3, A48.1): the
    /// teammate name, else the worktree name (by default the slug of the
    /// first prompt words, §4.2.2), else "{vendor} · {HH:MM start}".
    pub detail: Option<String>,
    /// **[content]** the session title (may be a first prompt): only with
    /// `notify.showDetails`.
    pub title: Option<String>,
    /// **[content]** the request's one-line summary (a command, a path, a
    /// host): only with `notify.showDetails`.
    pub summary: Option<String>,
}

/// Resolves labels for the sessions of one notification. Called only on the
/// notify worker task, never under the caller's locks.
pub trait LabelSource: Send + Sync {
    fn labels(&self, uids: &[SessionUid]) -> HashMap<SessionUid, SessionLabels>;
}

fn clean(text: &str, chars: usize) -> String {
    sanitize_display_text(text, TextCap::Chars(chars)).text.replace(['\n', '\r'], " ")
}

/// A name shown as a label: also without zero-width characters (A48.3).
fn clean_label(text: &str, chars: usize) -> String {
    sanitize_label_text(text, TextCap::Chars(chars)).text.replace(['\n', '\r', '\t'], " ")
}

/// The content-free session label of A48.1: "{project} · {vendor}" (or the
/// vendor alone), with " {ordinal}" when `ordinal` is set.
pub fn content_free_label(project: Option<&str>, vendor: Vendor, ordinal: Option<usize>) -> String {
    let vendor = match ordinal {
        Some(n) => format!("{} {n}", vendor.as_str()),
        None => vendor.as_str().to_string(),
    };
    match project {
        Some(p) if !p.is_empty() => format!("{p} · {vendor}"),
        _ => vendor,
    }
}

/// The 1-based position of `uid` among the sessions of its project and
/// vendor (A48.1), or `None` when it is the only one. Counted: sessions not
/// merged away that run, wait in bands 1–3 (an error or a crash), or are
/// `uid` itself; ordered by start time (unknown last), then uid, so a later
/// session never renumbers an earlier one.
fn ordinal_of(snap: &StoreSnapshot, session: &Session) -> Option<usize> {
    let counted = |s: &&Session| {
        s.merged_into.is_none()
            && s.vendor == session.vendor
            && s.project_id == session.project_id
            && (s.uid == session.uid
                || !matches!(s.control, Control::NotRunning { .. })
                || crate::world::derive::band(&s.activity.state).is_some_and(|b| b <= 3))
    };
    let mut peers: Vec<&Session> = snap.sessions.iter().filter(counted).collect();
    if peers.len() < 2 {
        return None;
    }
    peers.sort_by_key(|s| (s.started_at.unwrap_or(EpochMs::MAX), s.uid.as_str().to_string()));
    peers.iter().position(|s| s.uid == session.uid).map(|i| i + 1)
}

/// Labels from World (one snapshot per notification).
pub struct WorldLabels {
    world: Arc<dyn WorldApi>,
}

impl WorldLabels {
    pub fn new(world: Arc<dyn WorldApi>) -> WorldLabels {
        WorldLabels { world }
    }
}

impl LabelSource for WorldLabels {
    fn labels(&self, uids: &[SessionUid]) -> HashMap<SessionUid, SessionLabels> {
        let snap = self.world.snapshot();
        let mut out = HashMap::new();
        for uid in uids {
            let Some(session) = snap.sessions.iter().find(|s| &s.uid == uid) else { continue };
            let project = session
                .project_id
                .as_ref()
                .and_then(|id| snap.projects.iter().find(|p| &p.id == id))
                .map(|p| clean_label(&p.name, 80))
                .filter(|s| !s.is_empty());
            // A48.1: details off → project + vendor (+ ordinal), never a
            // name that may come from the prompt.
            let label = content_free_label(project.as_deref(), session.vendor, ordinal_of(&snap, session));
            let member = session
                .team
                .as_ref()
                .and_then(|role| snap.members.iter().find(|m| m.key == role.member_key))
                .map(|m| clean_label(&m.name, 64))
                .filter(|s| !s.is_empty());
            let worktree = session
                .location_id
                .as_ref()
                .and_then(|id| snap.locations.iter().find(|l| &l.id == id))
                .filter(|l| l.kind == LocationKind::Worktree)
                .and_then(|l| l.worktree_name.as_deref().map(|n| clean_label(n, 64)))
                .filter(|s| !s.is_empty());
            let detail = member.or(worktree).or_else(|| {
                let vendor = session.vendor.as_str();
                Some(match session.started_at.and_then(crate::macos::local_hhmm) {
                    Some(at) => format!("{vendor} · {at}"),
                    None => vendor.to_string(),
                })
            });
            let title = session.title.as_deref().map(|t| clean(t, 60)).filter(|s| !s.is_empty());
            // the request: this session's own card, else the card a lead
            // shows on this member's behalf (§7.2.6)
            let pending = session.pending.first().or_else(|| {
                let member_key = &session.team.as_ref()?.member_key;
                snap.sessions
                    .iter()
                    .flat_map(|s| s.pending.iter())
                    .find(|p| p.on_behalf_of.as_ref() == Some(member_key))
            });
            let summary = pending
                .map(|p| p.tool.as_ref().and_then(|t| t.object.clone()).unwrap_or_else(|| p.title.clone()))
                .map(|s| clean(&s, 120))
                .filter(|s| !s.is_empty());
            out.insert(uid.clone(), SessionLabels { project, session: label, detail, title, summary });
        }
        out
    }
}

/// A notification ready for macOS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Ids only (`gilde.e.<episode>`, `gilde.many`, …), never content.
    pub id: String,
    pub title: String,
    pub body: String,
    pub sound: bool,
}

/// The sessions a planned notification names.
pub fn uids_of(planned: &Planned) -> Vec<SessionUid> {
    match &planned.kind {
        PlanKind::Single { key, .. } | PlanKind::Reminder { key, .. } | PlanKind::Repeat { key, .. } => {
            vec![key.uid.clone()]
        }
        PlanKind::Many { keys } => keys.iter().map(|k| k.uid.clone()).collect(),
        PlanKind::Health(_) | PlanKind::Test => Vec::new(),
    }
}

/// The text of a planned notification (§8.4 Content, F-31).
pub fn render(
    planned: &Planned,
    labels: &HashMap<SessionUid, SessionLabels>,
    locale: Locale,
    show_details: bool,
) -> Notification {
    let fallback = SessionLabels { session: APP_NAME.to_string(), ..SessionLabels::default() };
    let get = |uid: &SessionUid| labels.get(uid).unwrap_or(&fallback);
    // A48.1: details off → the content-free label only; details on → the
    // title, else the richer label, else the content-free one.
    let name = |l: &SessionLabels| -> String {
        if !show_details {
            return l.session.clone();
        }
        l.title.clone().or_else(|| l.detail.clone()).unwrap_or_else(|| l.session.clone())
    };
    let project = |l: &SessionLabels| l.project.clone().unwrap_or_else(|| APP_NAME.to_string());
    let (title, lines): (String, Vec<String>) = match &planned.kind {
        PlanKind::Single { key, reason, .. } => {
            let l = get(&key.uid);
            let session = name(l);
            let mut lines =
                vec![format_message(tr(locale, "notify.body"), &[("session", &session), ("state", state_word(reason, locale))])];
            if show_details {
                if let Some(summary) = &l.summary {
                    lines.push(summary.clone());
                }
            }
            (project(l), lines)
        }
        PlanKind::Reminder { key, reason, waited_ms, .. } => {
            let l = get(&key.uid);
            let session = name(l);
            let waited = format_duration(*waited_ms, locale);
            (
                project(l),
                vec![
                    format_message(tr(locale, "notify.body"), &[("session", &session), ("state", state_word(reason, locale))]),
                    format_message(tr(locale, "notify.stillWaiting"), &[("t", &waited)]),
                ],
            )
        }
        PlanKind::Repeat { key, .. } => {
            let l = get(&key.uid);
            let session = name(l);
            (project(l), vec![format_message(tr(locale, "notify.repeat"), &[("session", &session)])])
        }
        PlanKind::Many { keys } => {
            let n = keys.len().to_string();
            let mut names: Vec<String> = keys.iter().take(2).map(|k| name(get(&k.uid))).collect();
            if keys.len() > 2 {
                names.push("…".to_string());
            }
            (format_message(tr(locale, "notify.many"), &[("n", &n)]), vec![names.join(", ")])
        }
        PlanKind::Health(HealthKind::MemoryPressure) => (APP_NAME.to_string(), vec![tr(locale, "health.memHigh").to_string()]),
        PlanKind::Test => (APP_NAME.to_string(), vec![tr(locale, "firstRun.notify").to_string()]),
    };
    let body = lines.iter().map(|l| clean(l, 200)).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    Notification { id: planned.id.clone(), title: clean(&title, 120), body, sound: planned.sound }
}

// ---------------------------------------------------------------------------
// OS boundaries
// ---------------------------------------------------------------------------

/// Called with the identifier of a clicked notification (default action).
pub type ClickHandler = Arc<dyn Fn(String) + Send + Sync>;

/// Where notifications go (`center`, `fallback`, or a recorder in tests).
#[async_trait]
pub trait NotificationSink: Send + Sync {
    /// Posts `n` (or replaces the delivered one with the same id). Must not
    /// block; `false` when it could not be handed to macOS.
    fn deliver(&self, n: &Notification) -> bool;
    /// Removes delivered (and pending) notifications by identifier; a no-op
    /// where [`NotificationSink::supports_withdrawal`] is false.
    fn withdraw(&self, ids: &[String]);
    fn supports_withdrawal(&self) -> bool;
    /// The last known permission state; never prompts.
    fn authorization(&self) -> Authorization;
    /// Asks macOS for permission. The only place a prompt may appear, and
    /// only through `notify_permission_request`.
    async fn request_authorization(&self) -> bool;
    /// Where clicks go; sinks without click routing ignore it.
    fn set_click_handler(&self, handler: ClickHandler) {
        let _ = handler;
    }
}

/// Emits `gilde://focus-request` (a notification click).
pub trait FocusEmitter: Send + Sync {
    fn focus_request(&self, target: FocusTarget);
}

/// The real emitter (every webview of the app).
pub struct TauriFocusEmitter<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> TauriFocusEmitter<R> {
    pub fn new(app: AppHandle<R>) -> TauriFocusEmitter<R> {
        TauriFocusEmitter { app }
    }
}

impl<R: Runtime> FocusEmitter for TauriFocusEmitter<R> {
    fn focus_request(&self, target: FocusTarget) {
        if self.app.emit(event_names::FOCUS_REQUEST, FocusRequestEvent { target }).is_err() {
            tracing::debug!(target: "notify", "focus request not delivered");
        }
    }
}

/// The two clocks of §3.5.2.
pub trait Clock: Send + Sync {
    fn now(&self) -> Now;
}

/// `CLOCK_UPTIME_RAW` + the wall clock (the same readings discovery uses).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Now {
        Now { up: crate::discovery::now_up_ms(), wall: crate::discovery::now_wall_ms() }
    }
}

// ---------------------------------------------------------------------------
// sink selection (LP-28)
// ---------------------------------------------------------------------------

/// Which sink a build uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkChoice {
    /// `UNUserNotificationCenter` (identifiers, click routing, withdrawal).
    NotificationCenter,
    /// The plugin fallback (no withdrawal; a click only activates Gilde).
    Plugin,
    /// Nothing can post (no plugin, no bundle): queue and badge still work.
    Disabled,
}

/// PROVISIONAL(LP-28): the notification center only with app flag
/// `notificationCenter` **and** a bundled `.app` (it raises an Objective-C
/// exception in an unbundled process — footgun pinned); else the plugin when
/// it is registered; else disabled.
pub fn choose_sink(notification_center_flag: bool, bundled_app: bool, plugin_registered: bool) -> SinkChoice {
    if notification_center_flag && bundled_app {
        SinkChoice::NotificationCenter
    } else if plugin_registered {
        SinkChoice::Plugin
    } else {
        SinkChoice::Disabled
    }
}

/// The sink for this process (T09b, step 4 of the wiring list).
pub fn build_sink<R: Runtime>(app: &AppHandle<R>, notification_center_flag: bool) -> Arc<dyn NotificationSink> {
    let plugin = app.try_state::<tauri_plugin_notification::Notification<R>>().is_some();
    match choose_sink(notification_center_flag, center::is_bundled_app(), plugin) {
        SinkChoice::NotificationCenter => match center::NotificationCenterSink::install() {
            Some(sink) => Arc::new(sink),
            None if plugin => Arc::new(fallback::PluginSink::new(app.clone())),
            None => Arc::new(fallback::DisabledSink),
        },
        SinkChoice::Plugin => Arc::new(fallback::PluginSink::new(app.clone())),
        SinkChoice::Disabled => Arc::new(fallback::DisabledSink),
    }
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

/// The pause end for a `notify_pause` choice at wall time `now_wall`;
/// "Until I resume" is `attention::PAUSED_UNTIL_RESUME` (A16.2).
pub fn pause_value(until: PauseUntil, now_wall: EpochMs) -> EpochMs {
    match until {
        PauseUntil::OneHour => now_wall.saturating_add(3_600_000),
        PauseUntil::TomorrowNine => crate::macos::tomorrow_nine(now_wall),
        PauseUntil::UntilResume => PAUSED_UNTIL_RESUME,
    }
}

/// Called with the new `paused_until` on pause, resume and expiry.
pub type PauseListener = Arc<dyn Fn(Option<EpochMs>) + Send + Sync>;

/// Everything the service talks to.
pub struct NotifyDeps {
    pub sink: Arc<dyn NotificationSink>,
    pub dock: Arc<dyn DockSink>,
    pub visibility: Arc<VisibilityHub>,
    pub system: Arc<SystemEvents>,
    pub labels: Arc<dyn LabelSource>,
    pub focus: Arc<dyn FocusEmitter>,
    pub clock: Arc<dyn Clock>,
}

/// Start options.
#[derive(Debug, Clone, Copy, Default)]
pub struct NotifyConfig {
    /// The app's launch instant; `None` = the clock reading at start.
    pub launch: Option<Now>,
}

enum Cmd {
    Queue(Box<AttentionQueue>),
    View(Box<ViewState>),
    Visibility(VisibilityEvent),
    Input,
    Pause(Option<EpochMs>),
    Settings(NotifySettings),
    Locale(Locale),
    Settled,
    System(SystemEvent),
    Memory(bool),
    Click(String),
    Test(oneshot::Sender<bool>),
    Poke,
    Barrier(oneshot::Sender<()>),
}

/// A command and the clock reading taken when it was sent (an input is
/// timed when it happened, not when the worker got to it).
type Timed = (Now, Cmd);

#[derive(Default)]
struct Shared {
    pause_listeners: Mutex<Vec<PauseListener>>,
    /// The last value announced; written by the worker only, so pause,
    /// resume and expiry reach the listeners in command order.
    paused_until: Mutex<Option<EpochMs>>,
}

impl Shared {
    fn announce_pause(&self, value: Option<EpochMs>) {
        {
            let mut current = self.paused_until.lock();
            if *current == value {
                return;
            }
            *current = value;
        }
        let listeners = self.pause_listeners.lock().clone();
        for listener in listeners {
            listener(value);
        }
    }
}

/// The `NotifyApi` implementation (§8): the badge, the policy on its own
/// task, the sinks.
pub struct NotifyService {
    tx: mpsc::UnboundedSender<Timed>,
    badge: DockBadge,
    visibility: Arc<VisibilityHub>,
    sink: Arc<dyn NotificationSink>,
    clock: Arc<dyn Clock>,
    shared: Arc<Shared>,
    /// The rev of the last queue taken (A48.2); held across the badge write
    /// and the hand-off, so both happen in rev order.
    last_rev: Mutex<Option<u64>>,
}

impl std::fmt::Debug for NotifyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyService").field("badge", &self.badge).finish()
    }
}

/// Longest the worker sleeps before re-reading the clocks.
const MAX_SLEEP_MS: u64 = 3_600_000;

impl NotifyService {
    /// Builds the service and starts its worker task (Tauri's async runtime).
    pub fn start(deps: NotifyDeps, config: NotifyConfig) -> Arc<NotifyService> {
        let (tx, rx) = mpsc::unbounded_channel();
        let launch = config.launch.unwrap_or_else(|| deps.clock.now());
        let shared = Arc::new(Shared::default());
        let mut policy = Policy::new(launch);
        policy.visibility(deps.visibility.current());
        let worker = Worker {
            policy,
            locale: Locale::Ko,
            sink: deps.sink.clone(),
            labels: deps.labels,
            focus: deps.focus,
            clock: deps.clock.clone(),
            shared: shared.clone(),
        };
        {
            let (tx, clock) = (tx.clone(), deps.clock.clone());
            deps.sink.set_click_handler(Arc::new(move |id| {
                let _ = tx.send((clock.now(), Cmd::Click(id)));
            }));
        }
        {
            let (tx, clock) = (tx.clone(), deps.clock.clone());
            deps.visibility.subscribe(Arc::new(move |event| {
                let _ = tx.send((clock.now(), Cmd::Visibility(event)));
            }));
        }
        {
            let (tx, clock) = (tx.clone(), deps.clock.clone());
            deps.system.subscribe(Arc::new(move |event| {
                let _ = tx.send((clock.now(), Cmd::System(event)));
            }));
        }
        tauri::async_runtime::spawn(worker.run(rx));
        Arc::new(NotifyService {
            tx,
            badge: DockBadge::new(deps.dock),
            visibility: deps.visibility,
            sink: deps.sink,
            clock: deps.clock,
            shared,
            last_rev: Mutex::new(None),
        })
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send((self.clock.now(), cmd)).is_err() {
            tracing::warn!(target: "notify", "notify worker stopped");
        }
    }

    /// The notification settings (§9.6 ordinary keys).
    pub fn set_settings(&self, settings: NotifySettings) {
        self.send(Cmd::Settings(settings));
    }

    /// The resolved UI locale of notification texts.
    pub fn set_locale(&self, locale: Locale) {
        self.send(Cmd::Locale(locale));
    }

    /// The boot settled (boot sweep + first discovery reconcile): §8.5 rule 5.
    pub fn boot_settled(&self) {
        self.send(Cmd::Settled);
    }

    /// The watchdog's memory-pressure state (rising edge alerts, ≤ 1/h).
    pub fn health_memory(&self, high: bool) {
        self.send(Cmd::Memory(high));
    }

    /// Subscribes to `paused_until` changes (pause, resume, expiry), called
    /// from the worker task in command order.
    pub fn on_pause_change(&self, listener: PauseListener) {
        self.shared.pause_listeners.lock().push(listener);
    }

    /// Re-evaluates at the current clock (tests advance a fake clock, then
    /// poke; the app's worker has its own timer).
    pub fn poke(&self) {
        self.send(Cmd::Poke);
    }

    /// Resolves once the worker has handled everything sent before.
    pub async fn settle(&self) {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Barrier(tx));
        let _ = rx.await;
    }
}

#[async_trait]
impl NotifyApi for NotifyService {
    fn view_state(&self, state: ViewState) {
        self.send(Cmd::View(Box::new(state)));
    }

    fn input_activity(&self) {
        self.send(Cmd::Input);
    }

    fn queue_changed(&self, queue: &AttentionQueue) {
        let mut last = self.last_rev.lock();
        if last.is_some_and(|rev| queue.rev <= rev) {
            tracing::debug!(target: "notify", rev = queue.rev, "an older queue arrived late; dropped");
            return;
        }
        *last = Some(queue.rev);
        self.badge.show(queue.need_you);
        self.send(Cmd::Queue(Box::new(queue.clone())));
    }

    fn visibility(&self) -> VisibilityEvent {
        self.visibility.current()
    }

    fn pause(&self, until: PauseUntil) -> Option<EpochMs> {
        let value = pause_value(until, self.clock.now().wall);
        self.send(Cmd::Pause(Some(value)));
        Some(value)
    }

    fn resume(&self) {
        self.send(Cmd::Pause(None));
    }

    async fn permission_request(&self) -> bool {
        let granted = self.sink.request_authorization().await;
        self.send(Cmd::Poke);
        granted
    }

    async fn test(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Test(tx));
        rx.await.unwrap_or(false)
    }
}

struct Worker {
    policy: Policy,
    locale: Locale,
    sink: Arc<dyn NotificationSink>,
    labels: Arc<dyn LabelSource>,
    focus: Arc<dyn FocusEmitter>,
    clock: Arc<dyn Clock>,
    shared: Arc<Shared>,
}

impl Worker {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Timed>) {
        loop {
            let now = self.clock.now();
            let wait = self.policy.next_deadline(now).map(|at| at.saturating_sub(now.up).min(MAX_SLEEP_MS));
            let next = match wait {
                Some(ms) => {
                    tokio::select! {
                        timed = rx.recv() => timed,
                        _ = tokio::time::sleep(Duration::from_millis(ms)) => Some((self.clock.now(), Cmd::Poke)),
                    }
                }
                None => rx.recv().await,
            };
            let Some((at, cmd)) = next else { break };
            self.handle(at, cmd);
            while let Ok((at, cmd)) = rx.try_recv() {
                self.handle(at, cmd);
            }
        }
    }

    fn handle(&mut self, now: Now, cmd: Cmd) {
        self.policy.set_authorization(self.sink.authorization());
        let effects = match cmd {
            Cmd::Queue(queue) => self.policy.queue(&queue, now),
            Cmd::View(view) => {
                self.policy.view_state(*view);
                Vec::new()
            }
            Cmd::Visibility(event) => {
                self.policy.visibility(event);
                Vec::new()
            }
            Cmd::Input => {
                self.policy.input_activity(now);
                Vec::new()
            }
            Cmd::Pause(until) => {
                self.policy.pause(until);
                Vec::new()
            }
            Cmd::Settings(settings) => {
                self.policy.set_settings(settings);
                Vec::new()
            }
            Cmd::Locale(locale) => {
                self.locale = locale;
                Vec::new()
            }
            Cmd::Settled => {
                self.policy.boot_settled();
                Vec::new()
            }
            Cmd::System(SystemEvent::WillSleep) => {
                self.policy.sleep(now);
                Vec::new()
            }
            Cmd::System(SystemEvent::DidWake) => self.policy.wake(now),
            Cmd::System(SystemEvent::TimeZoneChanged) => Vec::new(),
            Cmd::Memory(high) => self.policy.health_memory(high, now),
            Cmd::Click(id) => {
                // a click on a notification from an earlier run (unknown id) opens Home
                let target = self.policy.click_target(&id).unwrap_or(FocusTarget::Home);
                self.focus.focus_request(target);
                Vec::new()
            }
            Cmd::Test(reply) => {
                let delivered = self.policy.authorization().allows_delivery() && {
                    let planned = self.policy.test_notification();
                    self.deliver(&planned)
                };
                let _ = reply.send(delivered);
                Vec::new()
            }
            Cmd::Poke => self.policy.tick(now),
            Cmd::Barrier(reply) => {
                let _ = reply.send(());
                Vec::new()
            }
        };
        self.apply(effects);
        // pause, resume, or an expiry inside the policy (worker-only writes)
        self.shared.announce_pause(self.policy.paused_until());
    }

    fn apply(&mut self, effects: Vec<policy::Effect>) {
        for effect in effects {
            match effect {
                policy::Effect::Withdraw(ids) => {
                    if self.sink.supports_withdrawal() {
                        self.sink.withdraw(&ids);
                    }
                }
                policy::Effect::Deliver(planned) => {
                    self.deliver(&planned);
                }
            }
        }
    }

    fn deliver(&self, planned: &Planned) -> bool {
        let uids = uids_of(planned);
        let labels = if uids.is_empty() { HashMap::new() } else { self.labels.labels(&uids) };
        let show_details = self.policy.settings().show_details;
        let n = render(planned, &labels, self.locale, show_details);
        let ok = self.sink.deliver(&n);
        if !ok {
            tracing::debug!(target: "notify", sessions = uids.len(), "notification not handed to macOS");
        }
        ok
    }
}

// ---------------------------------------------------------------------------
// the managed state of the commands
// ---------------------------------------------------------------------------

/// `notify_test` is class `probe`: one call per 60 s (§4.1 rule 6).
pub const TEST_RATE_MS: u64 = 60_000;

/// What `commands/notify.rs` needs; managed by the composition root (T09b,
/// step 9). Until it is managed, those commands answer `UNSUPPORTED{notBuilt}`.
pub struct NotifyState {
    pub notify: Arc<dyn NotifyApi>,
    pub world: Arc<dyn WorldApi>,
    last_test: Mutex<Option<std::time::Instant>>,
}

impl NotifyState {
    pub fn new(notify: Arc<dyn NotifyApi>, world: Arc<dyn WorldApi>) -> NotifyState {
        NotifyState { notify, world, last_test: Mutex::new(None) }
    }

    /// The `probe` rate class of `notify_test`: `Err(retry_after_ms)` within
    /// 60 s of the last permitted call.
    pub fn test_permit(&self) -> Result<(), u64> {
        let mut last = self.last_test.lock();
        let now = std::time::Instant::now();
        if let Some(at) = *last {
            let elapsed = now.duration_since(at).as_millis() as u64;
            if elapsed < TEST_RATE_MS {
                return Err(TEST_RATE_MS - elapsed);
            }
        }
        *last = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_message_fills_placeholders_and_the_plural() {
        assert_eq!(format_message("{session} · {state}", &[("session", "a"), ("state", "b")]), "a · b");
        let many = tr(Locale::En, "notify.many");
        assert_eq!(format_message(many, &[("n", "1")]), "1 session is waiting for you.");
        assert_eq!(format_message(many, &[("n", "3")]), "3 sessions are waiting for you.");
        assert_eq!(format_message(tr(Locale::Ko, "notify.many"), &[("n", "3")]), "세션 3개가 기다리고 있습니다.");
        assert_eq!(format_message("{missing} x", &[]), "{missing} x");
        assert_eq!(format_message("unclosed {x", &[("x", "1")]), "unclosed {x");
    }

    #[test]
    fn every_string_key_exists_in_both_locales() {
        for (key, ko, en) in STRINGS {
            assert!(!ko.is_empty() && !en.is_empty(), "{key}");
            assert_eq!(tr(Locale::Ko, key), *ko);
            assert_eq!(tr(Locale::En, key), *en);
        }
    }

    #[test]
    fn durations_follow_the_webview_formatter() {
        assert_eq!(format_duration(20 * 60_000, Locale::Ko), "20분");
        assert_eq!(format_duration(20 * 60_000, Locale::En), "20 min");
        assert_eq!(format_duration(20 * 60_000 + 31_000, Locale::En), "21 min");
        assert_eq!(format_duration(90 * 60_000, Locale::Ko), "1.5시간");
        assert_eq!(format_duration(120 * 60_000, Locale::En), "2 hr");
        assert_eq!(format_duration(11 * 3_600_000, Locale::En), "11 hr");
        assert_eq!(format_duration(24 * 3_600_000, Locale::En), "1 day");
        assert_eq!(format_duration(36 * 3_600_000, Locale::Ko), "1.5일");
        assert_eq!(format_duration(0, Locale::En), "1 min");
    }

    #[test]
    fn sink_choice_follows_the_flag_the_bundle_and_the_plugin() {
        assert_eq!(choose_sink(true, true, true), SinkChoice::NotificationCenter);
        assert_eq!(choose_sink(true, false, true), SinkChoice::Plugin, "unbundled: never the notification center");
        assert_eq!(choose_sink(false, true, true), SinkChoice::Plugin, "flag absent = fallback");
        assert_eq!(choose_sink(false, true, false), SinkChoice::Disabled);
        assert_eq!(choose_sink(true, false, false), SinkChoice::Disabled);
    }

    #[test]
    fn pause_values() {
        let now = 1_792_000_000_000;
        assert_eq!(pause_value(PauseUntil::OneHour, now), now + 3_600_000);
        assert_eq!(pause_value(PauseUntil::UntilResume, now), PAUSED_UNTIL_RESUME);
        let nine = pause_value(PauseUntil::TomorrowNine, now);
        assert!(nine > now && nine - now <= 33 * 3_600_000);
    }
}
