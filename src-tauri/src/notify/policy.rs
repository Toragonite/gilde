//! The delivery rules and never-spam invariants (contract §8.4–§8.5).
//!
//! Owner: T12. [`Policy`] is a pure state machine: every input carries its own
//! clock reading ([`Now`]: the uptime clock for every duration, the wall clock
//! only for the pause and for "existed before launch"), and every output is an
//! [`Effect`] (deliver a [`Planned`] notification, withdraw identifiers). It
//! knows no OS, no World and no timer — `NotifyService` (this module's
//! parent) owns those and asks [`Policy::next_deadline`] when to tick.
//!
//! # The rules, as pinned here
//!
//! An item is tracked per **key** `(session uid, episode)` (§8.4 Delivery) and
//! only for bands 1–3 of sessions (rule 1; band 4–6 items, the leftovers item
//! and Working/Ready sessions never notify — §8.5 rule 4, rule 8). The first
//! sight of a key fixes its fate:
//!
//! - **Existed before launch** (§8.5 rule 5): first seen before the boot
//!   settled ([`Policy::boot_settled`], or [`BOOT_SETTLE_FALLBACK_MS`] after
//!   launch), or it began before the launch on both clocks — its `since` is
//!   older than the launch wall time **and** its start (`now − waited_ms`) is
//!   older than the launch uptime → never notifies; the queue and the badge
//!   still show it. Both, so that neither a wall clock set back after the
//!   launch (every new episode would look older than the launch) nor a long
//!   sleep (an episode that began during it looks old on the uptime clock)
//!   silences a new episode (A48.4).
//! - **Decision time:** In Gilde at once; Watching when the episode has lasted
//!   [`WATCHED_GRACE_MS`] on the uptime clock (rule 3, `start = now −
//!   waited_ms` at first sight). After a wake, an episode first seen within
//!   [`WAKE_SETTLE_MS`] of the wake — or first seen while asleep — is decided
//!   no earlier than wake + 60 s (Watching) / 5 s (In Gilde) (§8.5 rule 5).
//!   While asleep nothing is decided or delivered.
//! - **At decision time, once per key** (repeats within an episode never
//!   notify, rule 1), in this order: Watching with "Notify about sessions
//!   started elsewhere" off → never; Later-ed → never; permission denied or
//!   not determined → never (T-notify-denied; no backlog when permission
//!   arrives later); paused → never (rule 4); the user typed into Gilde in the
//!   last [`TYPING_QUIET_MS`] → never, the WebView announces it in-app
//!   (rule 5); on screen → never (rule 2: frontmost, not occluded, awake,
//!   `documentVisible`, and the uid — or the `answer_in` session whose card
//!   answers it — among `visibleUids`; no `app_view_state` yet = not on
//!   screen [F-04]).
//! - **Burst guard** (bands 1–2, §8.4, ux-ia #123): a session that entered
//!   bands 1–2 more than [`BURST_ENTRIES`] times in [`BURST_WINDOW_MS`] is
//!   notified at most once per [`BURST_MIN_GAP_MS`], with the "needs you
//!   repeatedly" text; inside the gap the key is consumed silently.
//! - **Coalescing** (§8.4): a delivery opens a [`COALESCE_MS`] window; further
//!   deliveries inside it are held and, when the window closes, merged with
//!   every still-unresolved key of the window's notification (and of an
//!   earlier merged notification) into one "{n} sessions are waiting for you"
//!   notification ([`MANY_ID`], replacing the previous one; click → Home); the
//!   window's individual notification is withdrawn as merged. A window that
//!   would merge a single key delivers it individually. At most one banner per
//!   window. Pause, typing and visibility are re-checked at the flush.
//! - **Reminder** (§8.4): bands 1–2 only, once per key, [`REMINDER_AFTER_MS`]
//!   after its notification if still unresolved; it replaces the original in
//!   Notification Center (same identifier), carries no request text, goes
//!   through the same gates and coalescing; a key whose first notification
//!   was suppressed gets no reminder.
//! - **Re-entry** (T-att-reentry, §3.5.8): a key whose band changes 1 ↔ 2
//!   keeps its state; a new episode id for the same session that follows a
//!   band 1 ↔ 2 flip with no gap between the two queues inherits the old
//!   key's state (no second alert). A new episode in the same band is new.
//! - **Rev order** (A48.2, §8.5 rule 1): a queue whose `rev` is not newer
//!   than the last applied one is ignored, so a late, older queue can neither
//!   withdraw a delivered notification nor re-deliver one.
//! - **Later** (Q5, A48.4): an item is Later-ed while World marks it so
//!   (`later_until` present). World times Later on the uptime clock and sends a
//!   new queue when it ends; the policy never compares `later_until` with the
//!   wall clock, so a wall-clock jump neither stretches nor cuts it.
//! - **Withdrawal** (§8.4): when every key of a delivered notification has
//!   resolved (left bands 1–3 / the queue, or its episode ended), its
//!   identifier is withdrawn. Sinks that cannot withdraw ignore it.
//! - **Health** (§8.4): memory pressure on a rising edge, at most once per
//!   [`MEMORY_ALERT_GAP_MS`], never while paused, typing or asleep.
//!
//! The text of a notification is not decided here: [`Planned`] names the kind
//! and the keys; `render` in the parent module turns it into a title and a
//! body (details hidden by default, F-31).

use std::collections::{HashMap, VecDeque};

use crate::contract::attention::{AttentionQueue, AttentionReason, AttentionSubject};
use crate::contract::common::EpochMs;
use crate::contract::ids::{EpisodeId, SessionUid};
use crate::contract::ipc::{NotifySettings, ViewState};
use crate::contract::store::{FocusTarget, VisibilityEvent};

/// Watched sessions notify after the episode lasted this long (rule 3, M8).
pub const WATCHED_GRACE_MS: u64 = 60_000;
/// After a wake, an In Gilde episode that began during sleep waits this long.
pub const WAKE_GRACE_IN_GILDE_MS: u64 = 5_000;
/// After a wake, a Watching episode that began during sleep waits this long.
pub const WAKE_GRACE_WATCHED_MS: u64 = 60_000;
/// An episode first seen this soon after a wake is treated as "began during
/// sleep" (discovery's wake reconcile reports it in this window).
pub const WAKE_SETTLE_MS: u64 = 10_000;
/// Nothing within this long of the user typing in Gilde (rule 5).
pub const TYPING_QUIET_MS: u64 = 10_000;
/// The one reminder for an unresolved band 1–2 key.
pub const REMINDER_AFTER_MS: u64 = 20 * 60_000;
/// The coalescing window.
pub const COALESCE_MS: u64 = 15_000;
/// The burst guard counts band 1–2 entries of one session in this window…
pub const BURST_WINDOW_MS: u64 = 60_000;
/// …and trips when there are more than this many.
pub const BURST_ENTRIES: usize = 3;
/// A bursting session notifies at most once per this long.
pub const BURST_MIN_GAP_MS: u64 = 5 * 60_000;
/// Without an explicit [`Policy::boot_settled`], the boot settles this long
/// after launch (uptime).
pub const BOOT_SETTLE_FALLBACK_MS: u64 = 15_000;
/// Memory-pressure alerts at most this often.
pub const MEMORY_ALERT_GAP_MS: u64 = 60 * 60_000;

/// The identifier of the merged notification (ux-ia #122).
pub const MANY_ID: &str = "gilde.many";
/// The identifier of the memory-pressure alert.
pub const MEMORY_ID: &str = "gilde.health.memory";
/// The identifier of the Settings test notification.
pub const TEST_ID: &str = "gilde.test";

/// The identifier of a key's own notification (and of its reminder, which
/// replaces it): `gilde.e.<episode id>` — ids only, never content.
pub fn episode_id_of(key: &EpisodeKey) -> String {
    format!("gilde.e.{}", key.episode.as_str())
}

/// One clock reading: `up` = uptime ms (`CLOCK_UPTIME_RAW`, stops during
/// sleep), `wall` = UTC ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub up: u64,
    pub wall: EpochMs,
}

/// The notification key `(uid, episode)` of §8.4.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EpisodeKey {
    pub uid: SessionUid,
    pub episode: EpisodeId,
}

/// What macOS lets Gilde do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    /// Not asked yet by this process (deliveries are attempted).
    Unknown,
    /// The user has not answered the prompt (nothing would show).
    NotDetermined,
    Denied,
    Granted,
}

impl Authorization {
    pub fn allows_delivery(self) -> bool {
        matches!(self, Authorization::Unknown | Authorization::Granted)
    }
}

/// Why a key will never notify (observable in tests through [`Policy::outcome`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppressed {
    PreExisting,
    WatchedOff,
    Later,
    Denied,
    Paused,
    Typing,
    Visible,
    Burst,
}

/// The state of a key as the tests see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Its decision time has not come yet.
    Waiting,
    /// Held in the coalescing window.
    Held,
    /// Notified (alone or merged).
    Delivered,
    Suppressed(Suppressed),
}

/// What kind of notification to post.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanKind {
    /// The first notification of a key.
    Single { key: EpisodeKey, band: u8, reason: AttentionReason },
    /// The one 20-minute reminder ("still waiting · {t}").
    Reminder { key: EpisodeKey, band: u8, reason: AttentionReason, waited_ms: u64 },
    /// The burst guard's notification ("{session} needs you repeatedly.").
    Repeat { key: EpisodeKey, band: u8, reason: AttentionReason },
    /// The merged notification ("{n} sessions are waiting for you."), keys in
    /// queue rank order.
    Many { keys: Vec<EpisodeKey> },
    /// A health alert.
    Health(HealthKind),
    /// The Settings test notification (`notify_test`).
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthKind {
    /// ux-ia #115.
    MemoryPressure,
}

/// A notification the policy decided to post.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub id: String,
    pub kind: PlanKind,
    /// One system sound (band 1 only, setting on; `sound::wants_sound`).
    pub sound: bool,
    /// Where a click goes (`gilde://focus-request`).
    pub target: FocusTarget,
}

/// The policy's output.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Deliver(Planned),
    Withdraw(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldKind {
    Initial,
    Repeat,
    Reminder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Waiting,
    Held(HeldKind),
    Delivered,
    Suppressed(Suppressed),
}

#[derive(Debug, Clone)]
struct Track {
    key: EpisodeKey,
    band: u8,
    reason: AttentionReason,
    watched: bool,
    answer_in: Option<SessionUid>,
    later: bool,
    rank: usize,
    present: bool,
    start_up: u64,
    first_seen_up: u64,
    decide_at: u64,
    stage: Stage,
    reminder_at: Option<u64>,
    reminded: bool,
}

#[derive(Debug, Clone)]
struct Posted {
    id: String,
    keys: Vec<EpisodeKey>,
}

/// The notification policy (§8.4–§8.5).
#[derive(Debug)]
pub struct Policy {
    launch: Now,
    settled: bool,
    settings: NotifySettings,
    authorization: Authorization,
    view: Option<ViewState>,
    frontmost: bool,
    occluded: bool,
    asleep: bool,
    sleep_up: Option<u64>,
    wake_up: Option<u64>,
    last_input_up: Option<u64>,
    paused_until: Option<EpochMs>,
    /// The rev of the last queue applied (A48.2).
    last_rev: Option<u64>,
    /// Ranked like the latest queue.
    tracks: Vec<Track>,
    /// Band 1–2 entries per session (uptime), pruned to the burst window.
    entries: HashMap<SessionUid, VecDeque<u64>>,
    /// The last notification that named a session (uptime).
    last_notified: HashMap<SessionUid, u64>,
    window_until: Option<u64>,
    window_lead: Option<String>,
    held: Vec<EpisodeKey>,
    posted: Vec<Posted>,
    targets: HashMap<String, FocusTarget>,
    memory_high: bool,
    last_memory_alert_up: Option<u64>,
}

/// The ux-ia §6.1 defaults: watched sessions notify (after 60 s), no sound,
/// details hidden [F-31].
pub fn default_settings() -> NotifySettings {
    NotifySettings { watched_sessions: true, sound: false, show_details: false }
}

impl Policy {
    /// A policy for an app launched at `launch`.
    pub fn new(launch: Now) -> Policy {
        Policy {
            launch,
            settled: false,
            settings: default_settings(),
            authorization: Authorization::Unknown,
            view: None,
            frontmost: false,
            occluded: false,
            asleep: false,
            sleep_up: None,
            wake_up: None,
            last_input_up: None,
            paused_until: None,
            last_rev: None,
            tracks: Vec::new(),
            entries: HashMap::new(),
            last_notified: HashMap::new(),
            window_until: None,
            window_lead: None,
            held: Vec::new(),
            posted: Vec::new(),
            targets: HashMap::new(),
            memory_high: false,
            last_memory_alert_up: None,
        }
    }

    // -- inputs that never deliver by themselves ---------------------------------

    pub fn set_settings(&mut self, settings: NotifySettings) {
        self.settings = settings;
    }

    pub fn settings(&self) -> &NotifySettings {
        &self.settings
    }

    pub fn set_authorization(&mut self, authorization: Authorization) {
        self.authorization = authorization;
    }

    pub fn authorization(&self) -> Authorization {
        self.authorization
    }

    /// The boot sweep and the first discovery reconcile are done: keys first
    /// seen from now on are not "existed before launch" (unless they began
    /// before the launch on both clocks).
    pub fn boot_settled(&mut self) {
        self.settled = true;
    }

    /// `app_view_state` [F-04]: replaces the previous one.
    pub fn view_state(&mut self, view: ViewState) {
        self.view = Some(view);
    }

    /// Frontmost and occlusion (`gilde://visibility`). `asleep` comes from
    /// [`Policy::sleep`]/[`Policy::wake`] only.
    pub fn visibility(&mut self, event: VisibilityEvent) {
        self.frontmost = event.frontmost;
        self.occluded = event.occluded;
    }

    /// `app_input_activity` (rule 5).
    pub fn input_activity(&mut self, now: Now) {
        self.last_input_up = Some(now.up);
    }

    /// Sets (`Some`) or clears (`None`) the pause.
    pub fn pause(&mut self, until: Option<EpochMs>) {
        self.paused_until = until;
    }

    pub fn paused_until(&self) -> Option<EpochMs> {
        self.paused_until
    }

    /// The rev of the last queue applied (A48.2).
    pub fn last_rev(&self) -> Option<u64> {
        self.last_rev
    }

    /// `NSWorkspaceWillSleepNotification`.
    pub fn sleep(&mut self, now: Now) {
        self.asleep = true;
        self.sleep_up = Some(now.up);
    }

    /// `NSWorkspaceDidWakeNotification`: applies the wake grace (§8.5 rule 5)
    /// to keys first seen while asleep, then evaluates.
    pub fn wake(&mut self, now: Now) -> Vec<Effect> {
        let since_sleep = self.sleep_up.unwrap_or(now.up);
        self.asleep = false;
        self.wake_up = Some(now.up);
        for t in &mut self.tracks {
            if t.stage == Stage::Waiting && t.first_seen_up >= since_sleep {
                t.decide_at = t.decide_at.max(now.up + wake_grace(t.watched));
            }
        }
        self.evaluate(now)
    }

    /// Where a click on `id` goes, while the notification is tracked.
    pub fn click_target(&self, id: &str) -> Option<FocusTarget> {
        self.targets.get(id).cloned()
    }

    /// The state of a key (tests and diagnostics).
    pub fn outcome(&self, key: &EpisodeKey) -> Option<Outcome> {
        self.tracks.iter().find(|t| &t.key == key).map(|t| match t.stage {
            Stage::Waiting => Outcome::Waiting,
            Stage::Held(_) => Outcome::Held,
            Stage::Delivered => Outcome::Delivered,
            Stage::Suppressed(s) => Outcome::Suppressed(s),
        })
    }

    // -- the queue ----------------------------------------------------------------

    /// Every queue change (`NotifyApi::queue_changed`). A queue whose `rev`
    /// is not newer than the last applied one changes nothing (A48.2).
    pub fn queue(&mut self, queue: &AttentionQueue, now: Now) -> Vec<Effect> {
        if self.last_rev.is_some_and(|last| queue.rev <= last) {
            return Vec::new();
        }
        self.last_rev = Some(queue.rev);
        let mut withdraw = Vec::new();
        // uid → (episode, band) of the keys present in the previous queue
        let previous: HashMap<SessionUid, (EpisodeId, u8)> = self
            .tracks
            .iter()
            .filter(|t| t.present)
            .map(|t| (t.key.uid.clone(), (t.key.episode.clone(), t.band)))
            .collect();
        for t in &mut self.tracks {
            t.present = false;
        }
        let settled = self.is_settled(now);
        for (rank, item) in queue.items.iter().enumerate() {
            let AttentionSubject::Session { uid, answer_in, .. } = &item.subject else {
                continue;
            };
            if !(1..=3).contains(&item.band) {
                continue;
            }
            let key = EpisodeKey { uid: uid.clone(), episode: item.episode.clone() };
            // World decides Later on the uptime clock (A48.4)
            let later = item.later_until.is_some();
            if let Some(t) = self.tracks.iter_mut().find(|t| t.key == key) {
                t.band = item.band;
                t.reason = item.reason.clone();
                t.watched = item.watched;
                t.answer_in = answer_in.clone();
                t.later = later;
                t.rank = rank;
                t.present = true;
                continue;
            }
            // a direct band 1 ↔ 2 flip with a new episode id: same wait
            let inherit = previous.get(uid).is_some_and(|(episode, band)| {
                episode != &item.episode && *band <= 2 && item.band <= 2 && *band != item.band
            });
            if inherit {
                if let Some(t) = self.tracks.iter_mut().find(|t| &t.key.uid == uid && !t.present) {
                    let old = std::mem::replace(&mut t.key, key.clone());
                    t.band = item.band;
                    t.reason = item.reason.clone();
                    t.watched = item.watched;
                    t.answer_in = answer_in.clone();
                    t.later = later;
                    t.rank = rank;
                    t.present = true;
                    self.rekey(&old, &key);
                    continue;
                }
            }
            // a fresh key
            let start_up = now.up.saturating_sub(item.waited_ms);
            let pre_existing = !settled || (item.since < self.launch.wall && start_up < self.launch.up);
            let mut decide_at = if item.watched { start_up + WATCHED_GRACE_MS } else { now.up };
            let woke_recently = self.wake_up.is_some_and(|w| now.up < w + WAKE_SETTLE_MS);
            if woke_recently || self.asleep {
                let from = if self.asleep { now.up } else { self.wake_up.unwrap_or(now.up) };
                decide_at = decide_at.max(from + wake_grace(item.watched));
            }
            if item.band <= 2 {
                self.entries.entry(uid.clone()).or_default().push_back(now.up);
            }
            self.tracks.push(Track {
                key,
                band: item.band,
                reason: item.reason.clone(),
                watched: item.watched,
                answer_in: answer_in.clone(),
                later,
                rank,
                present: true,
                start_up,
                first_seen_up: now.up,
                decide_at,
                stage: if pre_existing { Stage::Suppressed(Suppressed::PreExisting) } else { Stage::Waiting },
                reminder_at: None,
                reminded: false,
            });
        }
        // resolved keys
        let resolved: Vec<EpisodeKey> = self.tracks.iter().filter(|t| !t.present).map(|t| t.key.clone()).collect();
        self.tracks.retain(|t| t.present);
        self.tracks.sort_by_key(|t| t.rank);
        for key in &resolved {
            self.held.retain(|k| k != key);
            for p in &mut self.posted {
                p.keys.retain(|k| k != key);
            }
        }
        let emptied: Vec<String> = self.posted.iter().filter(|p| p.keys.is_empty()).map(|p| p.id.clone()).collect();
        self.posted.retain(|p| !p.keys.is_empty());
        for id in emptied {
            self.targets.remove(&id);
            if self.window_lead.as_deref() == Some(id.as_str()) {
                self.window_lead = None;
            }
            withdraw.push(id);
        }
        self.prune(now);
        let mut effects = Vec::new();
        if !withdraw.is_empty() {
            effects.push(Effect::Withdraw(withdraw));
        }
        effects.extend(self.evaluate(now));
        effects
    }

    /// Re-evaluates due decisions, reminders, the coalescing flush and the
    /// pause expiry at `now`.
    pub fn tick(&mut self, now: Now) -> Vec<Effect> {
        self.evaluate(now)
    }

    /// The next uptime instant at which [`Policy::tick`] has something to do
    /// (`None` = nothing scheduled). The pause expiry is converted from the
    /// wall clock.
    pub fn next_deadline(&self, now: Now) -> Option<u64> {
        let mut next: Option<u64> = None;
        let mut consider = |at: u64| next = Some(next.map_or(at, |n: u64| n.min(at)));
        if let Some(until) = self.paused_until {
            consider(now.up + (until - now.wall).max(0) as u64);
        }
        if self.asleep {
            return next;
        }
        for t in &self.tracks {
            match t.stage {
                Stage::Waiting => consider(t.decide_at),
                Stage::Delivered if !t.reminded && t.band <= 2 => {
                    if let Some(at) = t.reminder_at {
                        consider(at);
                    }
                }
                _ => {}
            }
        }
        if let (Some(until), false) = (self.window_until, self.held.is_empty()) {
            consider(until);
        }
        next
    }

    // -- health and test -----------------------------------------------------------

    /// The watchdog's memory-pressure state (its hysteresis decides `high`):
    /// a rising edge alerts at most once per hour.
    pub fn health_memory(&mut self, high: bool, now: Now) -> Vec<Effect> {
        let rising = high && !self.memory_high;
        self.memory_high = high;
        if !rising {
            return Vec::new();
        }
        if self.last_memory_alert_up.is_some_and(|at| now.up < at + MEMORY_ALERT_GAP_MS) {
            return Vec::new();
        }
        if self.asleep || !self.authorization.allows_delivery() || self.paused(now) || self.typing(now) {
            return Vec::new();
        }
        self.last_memory_alert_up = Some(now.up);
        self.targets.insert(MEMORY_ID.to_string(), FocusTarget::Home);
        vec![Effect::Deliver(Planned {
            id: MEMORY_ID.to_string(),
            kind: PlanKind::Health(HealthKind::MemoryPressure),
            sound: false,
            target: FocusTarget::Home,
        })]
    }

    /// The Settings test notification (explicit user action: no gate but the
    /// permission, which the caller checks).
    pub fn test_notification(&mut self) -> Planned {
        self.targets.insert(TEST_ID.to_string(), FocusTarget::Home);
        Planned {
            id: TEST_ID.to_string(),
            kind: PlanKind::Test,
            sound: super::sound::wants_sound_for_test(&self.settings),
            target: FocusTarget::Home,
        }
    }

    // -- internals -----------------------------------------------------------------

    fn is_settled(&self, now: Now) -> bool {
        self.settled || now.up >= self.launch.up + BOOT_SETTLE_FALLBACK_MS
    }

    fn paused(&self, now: Now) -> bool {
        self.paused_until.is_some_and(|until| now.wall < until)
    }

    fn typing(&self, now: Now) -> bool {
        self.last_input_up.is_some_and(|at| now.up < at.saturating_add(TYPING_QUIET_MS))
    }

    fn visible(&self, t: &Track) -> bool {
        let Some(view) = &self.view else { return false };
        let on_screen = view.document_visible && self.frontmost && !self.occluded && !self.asleep;
        if !on_screen {
            return false;
        }
        view.visible_uids.contains(&t.key.uid)
            || t.answer_in.as_ref().is_some_and(|other| view.visible_uids.contains(other))
    }

    fn bursting(&mut self, uid: &SessionUid, now: Now) -> bool {
        let Some(entries) = self.entries.get_mut(uid) else { return false };
        while entries.front().is_some_and(|&at| now.up >= at + BURST_WINDOW_MS) {
            entries.pop_front();
        }
        entries.len() > BURST_ENTRIES
    }

    fn prune(&mut self, now: Now) {
        self.entries.retain(|_, e| {
            while e.front().is_some_and(|&at| now.up >= at + BURST_WINDOW_MS) {
                e.pop_front();
            }
            !e.is_empty()
        });
        self.last_notified.retain(|_, at| now.up < *at + BURST_MIN_GAP_MS);
        if self.window_until.is_some_and(|until| now.up >= until) && self.held.is_empty() {
            self.window_until = None;
            self.window_lead = None;
        }
    }

    fn rekey(&mut self, old: &EpisodeKey, new: &EpisodeKey) {
        for k in self.held.iter_mut().chain(self.posted.iter_mut().flat_map(|p| p.keys.iter_mut())) {
            if k == old {
                *k = new.clone();
            }
        }
    }

    fn evaluate(&mut self, now: Now) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(until) = self.paused_until {
            if now.wall >= until {
                self.paused_until = None;
            }
        }
        if self.asleep {
            return effects;
        }
        if self.window_until.is_some_and(|until| now.up >= until) {
            self.flush(now, &mut effects);
        }
        for i in 0..self.tracks.len() {
            match self.tracks[i].stage {
                Stage::Waiting if now.up >= self.tracks[i].decide_at => self.decide(i, now, &mut effects),
                Stage::Delivered => {
                    let t = &self.tracks[i];
                    if !t.reminded && t.band <= 2 && t.reminder_at.is_some_and(|at| now.up >= at) {
                        self.remind(i, now, &mut effects);
                    }
                }
                _ => {}
            }
        }
        effects
    }

    /// The first (and only) decision for a key.
    fn decide(&mut self, i: usize, now: Now, effects: &mut Vec<Effect>) {
        let verdict = {
            let t = &self.tracks[i];
            if t.watched && !self.settings.watched_sessions {
                Some(Suppressed::WatchedOff)
            } else if t.later {
                Some(Suppressed::Later)
            } else if !self.authorization.allows_delivery() {
                Some(Suppressed::Denied)
            } else if self.paused(now) {
                Some(Suppressed::Paused)
            } else if self.typing(now) {
                Some(Suppressed::Typing)
            } else if self.visible(t) {
                Some(Suppressed::Visible)
            } else {
                None
            }
        };
        if let Some(why) = verdict {
            self.tracks[i].stage = Stage::Suppressed(why);
            return;
        }
        let uid = self.tracks[i].key.uid.clone();
        let mut kind = HeldKind::Initial;
        if self.tracks[i].band <= 2 && self.bursting(&uid, now) {
            if self.last_notified.get(&uid).is_some_and(|&at| now.up < at + BURST_MIN_GAP_MS) {
                self.tracks[i].stage = Stage::Suppressed(Suppressed::Burst);
                return;
            }
            kind = HeldKind::Repeat;
        }
        self.emit(i, kind, now, effects);
    }

    /// The one reminder of a delivered band 1–2 key.
    fn remind(&mut self, i: usize, now: Now, effects: &mut Vec<Effect>) {
        self.tracks[i].reminded = true;
        let t = &self.tracks[i];
        if t.later || !self.authorization.allows_delivery() || self.paused(now) || self.typing(now) || self.visible(t) {
            return;
        }
        self.emit(i, HeldKind::Reminder, now, effects);
    }

    /// Posts a key's notification now, or holds it in an open window.
    fn emit(&mut self, i: usize, kind: HeldKind, now: Now, effects: &mut Vec<Effect>) {
        if self.window_until.is_some_and(|until| now.up < until) {
            let key = self.tracks[i].key.clone();
            if !self.held.contains(&key) {
                self.held.push(key);
            }
            self.tracks[i].stage = Stage::Held(kind);
            return;
        }
        let planned = self.plan_single(i, kind, now);
        let id = planned.id.clone();
        self.mark_delivered(i, kind, now);
        self.upsert_posted(&id, vec![self.tracks[i].key.clone()]);
        self.targets.insert(id.clone(), planned.target.clone());
        self.window_until = Some(now.up + COALESCE_MS);
        self.window_lead = Some(id);
        effects.push(Effect::Deliver(planned));
    }

    fn plan_single(&self, i: usize, kind: HeldKind, now: Now) -> Planned {
        let t = &self.tracks[i];
        let key = t.key.clone();
        let plan = match kind {
            HeldKind::Initial => PlanKind::Single { key: key.clone(), band: t.band, reason: t.reason.clone() },
            HeldKind::Repeat => PlanKind::Repeat { key: key.clone(), band: t.band, reason: t.reason.clone() },
            HeldKind::Reminder => PlanKind::Reminder {
                key: key.clone(),
                band: t.band,
                reason: t.reason.clone(),
                waited_ms: now.up.saturating_sub(t.start_up),
            },
        };
        Planned {
            id: episode_id_of(&key),
            sound: kind != HeldKind::Reminder && super::sound::wants_sound(t.band, &self.settings),
            kind: plan,
            target: FocusTarget::Session { uid: key.uid, focus_card: true },
        }
    }

    fn mark_delivered(&mut self, i: usize, kind: HeldKind, now: Now) {
        let t = &mut self.tracks[i];
        t.stage = Stage::Delivered;
        if kind != HeldKind::Reminder && t.reminder_at.is_none() && t.band <= 2 {
            t.reminder_at = Some(now.up + REMINDER_AFTER_MS);
        }
        self.last_notified.insert(t.key.uid.clone(), now.up);
    }

    fn upsert_posted(&mut self, id: &str, keys: Vec<EpisodeKey>) {
        if let Some(p) = self.posted.iter_mut().find(|p| p.id == id) {
            p.keys = keys;
        } else {
            self.posted.push(Posted { id: id.to_string(), keys });
        }
    }

    /// Closes the coalescing window: merges what it held.
    fn flush(&mut self, now: Now, effects: &mut Vec<Effect>) {
        let held: Vec<(usize, HeldKind)> = std::mem::take(&mut self.held)
            .into_iter()
            .filter_map(|key| {
                let i = self.tracks.iter().position(|t| t.key == key)?;
                match self.tracks[i].stage {
                    Stage::Held(kind) => Some((i, kind)),
                    _ => None,
                }
            })
            .collect();
        self.window_until = None;
        let lead = self.window_lead.take();
        if held.is_empty() {
            return;
        }
        // global gates, re-checked at the flush
        let blocked = if !self.authorization.allows_delivery() {
            Some(Suppressed::Denied)
        } else if self.paused(now) {
            Some(Suppressed::Paused)
        } else if self.typing(now) {
            Some(Suppressed::Typing)
        } else {
            None
        };
        let mut keep: Vec<(usize, HeldKind)> = Vec::new();
        for (i, kind) in held {
            let why = blocked.or_else(|| self.visible(&self.tracks[i]).then_some(Suppressed::Visible));
            match (why, kind) {
                // a reminder that cannot go out is consumed; the key stays notified
                (Some(_), HeldKind::Reminder) => self.tracks[i].stage = Stage::Delivered,
                (Some(why), _) => self.tracks[i].stage = Stage::Suppressed(why),
                (None, _) => keep.push((i, kind)),
            }
        }
        if keep.is_empty() {
            return;
        }
        // everything still waiting under the window's notification and under
        // an earlier merged one joins the merge
        let mut keys: Vec<EpisodeKey> = Vec::new();
        let mut merged_ids: Vec<String> = Vec::new();
        for id in lead.iter().map(String::as_str).chain(std::iter::once(MANY_ID)) {
            if let Some(p) = self.posted.iter().find(|p| p.id == id) {
                if !p.keys.is_empty() {
                    merged_ids.push(p.id.clone());
                }
                for k in &p.keys {
                    if !keys.contains(k) {
                        keys.push(k.clone());
                    }
                }
            }
        }
        for (i, _) in &keep {
            let k = self.tracks[*i].key.clone();
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        let rank_of = |k: &EpisodeKey| self.tracks.iter().find(|t| &t.key == k).map_or(usize::MAX, |t| t.rank);
        keys.sort_by_key(rank_of);
        if keys.len() == 1 {
            // the window's own notification resolved: one key, sent alone
            let (i, kind) = keep[0];
            let planned = self.plan_single(i, kind, now);
            let id = planned.id.clone();
            self.mark_delivered(i, kind, now);
            self.upsert_posted(&id, vec![self.tracks[i].key.clone()]);
            self.targets.insert(id.clone(), planned.target.clone());
            self.window_until = Some(now.up + COALESCE_MS);
            self.window_lead = Some(id);
            effects.push(Effect::Deliver(planned));
            return;
        }
        let sound = keep
            .iter()
            .any(|(i, kind)| *kind != HeldKind::Reminder && super::sound::wants_sound(self.tracks[*i].band, &self.settings));
        for (i, kind) in &keep {
            self.mark_delivered(*i, *kind, now);
        }
        // the individual notifications merged into this one are withdrawn
        let superseded: Vec<String> = merged_ids.into_iter().filter(|id| id != MANY_ID).collect();
        self.posted.retain(|p| !superseded.contains(&p.id));
        for id in &superseded {
            self.targets.remove(id);
        }
        if !superseded.is_empty() {
            effects.push(Effect::Withdraw(superseded));
        }
        self.upsert_posted(MANY_ID, keys.clone());
        self.targets.insert(MANY_ID.to_string(), FocusTarget::Home);
        self.window_until = Some(now.up + COALESCE_MS);
        self.window_lead = Some(MANY_ID.to_string());
        effects.push(Effect::Deliver(Planned {
            id: MANY_ID.to_string(),
            kind: PlanKind::Many { keys },
            sound,
            target: FocusTarget::Home,
        }));
    }
}

fn wake_grace(watched: bool) -> u64 {
    if watched {
        WAKE_GRACE_WATCHED_MS
    } else {
        WAKE_GRACE_IN_GILDE_MS
    }
}
