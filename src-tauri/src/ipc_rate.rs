//! The one rate limiter of the IPC path (contract §4.1 rule 6, A45.3).
//!
//! Every command takes its request as `Req<T>`; once the request has passed
//! §4.1 rule 3 (a) and (b) — the size guard, the shape and the value rules —
//! `Req<T>` asks [`admit`], and the command's manifest [`RateClass`] decides
//! the token bucket it draws from. An empty bucket answers
//! `BUSY{retryAfterMs}` and the command body never runs. A request refused at
//! (a)/(b) draws no token: nothing has happened before step (e) (rule 3),
//! exactly as in the owners' own buckets below.
//!
//! | class | refill | burst | bucket |
//! |---|---|---|---|
//! | `interactive` | 50/s | 100 | per PTY (`req.ptyId`) for `pty_*`, else one for the class |
//! | `turn` | 10/s | 10 | per session (`req.uid`, or `req.teamKey` for `team_send`) |
//! | `spawn` | 5 per 10 s | 5 | one for the class |
//! | `query` | 20/s | 20 | one for the class |
//! | `probe` | 1 per 60 s | 1 | per command |
//! | `default` | 10/s | 10 | per command |
//!
//! **One bucket per call, never two** ("don't double-count"): the commands
//! whose owners already enforce their class keep that enforcement and are
//! passed through here untouched ([`BESPOKE`]): the sessions manager's
//! buckets (`sessions/manager.rs` `Rates`), the history index's `query`
//! bucket (`discovery/history.rs`), `notify_test`'s 60 s probe
//! (`notify/mod.rs`) and `quota_refresh`, whose probe class answers the
//! cached quota with `stale: true` instead of `BUSY` (`accounts/mod.rs`).
//!
//! The buckets belong to one app: [`RateLimiter`] is managed state, managed
//! on first use. A per-target bucket is created on first use and dropped
//! only when it is full again (then it is indistinguishable from a new one),
//! so rotating targets can neither reset a busy bucket nor grow the table
//! past [`MAX_TARGETS`]; a new target while the table is full of busy
//! buckets is `BUSY` itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::Value;

use crate::contract::error::IpcError;
use crate::contract::ids::{PtyId, SessionUid, TeamKey};
use crate::contract::ipc_manifest::{self, CommandSpec, RateClass};

/// The most per-target buckets kept at once.
pub const MAX_TARGETS: usize = 4_096;

/// Commands whose class their owner enforces (see the module docs), with the
/// owner. The central limiter passes them through.
pub const BESPOKE: &[(&str, &str)] = &[
    ("newsession_options", "sessions/manager.rs"),
    ("session_start", "sessions/manager.rs"),
    ("session_continue", "sessions/manager.rs"),
    ("session_retry_as_terminal", "sessions/manager.rs"),
    ("session_stop_preview", "sessions/manager.rs"),
    ("session_stop", "sessions/manager.rs"),
    ("session_models", "sessions/manager.rs"),
    ("session_technical", "sessions/manager.rs"),
    ("session_set_model", "sessions/manager.rs"),
    ("session_set_permission_mode", "sessions/manager.rs"),
    ("turn_send", "sessions/manager.rs"),
    ("turn_cancel_queued", "sessions/manager.rs"),
    ("turn_interrupt", "sessions/manager.rs"),
    ("turn_force_stop", "sessions/manager.rs"),
    ("turn_retry", "sessions/manager.rs"),
    ("interaction_answer", "sessions/manager.rs"),
    ("screen_prompt_key", "sessions/manager.rs"),
    ("interaction_detail", "sessions/manager.rs"),
    ("account_sign_in", "sessions/start.rs"),
    ("history_query", "discovery/history.rs"),
    ("history_preview", "discovery/history.rs"),
    ("history_row", "discovery/history.rs"),
    ("notify_test", "notify/mod.rs"),
    ("quota_refresh", "accounts/mod.rs"),
];

/// How one command is limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Its owner enforces the class (the module that does).
    Bespoke(&'static str),
    /// This limiter, with the command's manifest class.
    Central(RateClass),
}

/// The policy of the manifest command `spec`.
pub fn policy(spec: &CommandSpec) -> Policy {
    match BESPOKE.iter().find(|(name, _)| *name == spec.name) {
        Some((_, owner)) => Policy::Bespoke(owner),
        None => Policy::Central(spec.rate),
    }
}

/// `(tokens per second, burst)` of a class (§4.1 rule 6).
pub const fn params(class: RateClass) -> (f64, f64) {
    match class {
        RateClass::Interactive => (50.0, 100.0),
        RateClass::Turn => (10.0, 10.0),
        RateClass::Spawn => (0.5, 5.0),
        RateClass::Query => (20.0, 20.0),
        RateClass::Probe => (1.0 / 60.0, 1.0),
        RateClass::Default => (10.0, 10.0),
    }
}

/// The bucket a call draws from: a class, a command, or a class's target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BucketKey {
    Class(&'static str),
    Command(&'static str),
    Target(&'static str, String),
}

/// The bucket of a call to `spec` whose raw `req` argument is `req`.
pub fn bucket_key(spec: &CommandSpec, req: Option<&Value>) -> BucketKey {
    let class = spec.rate.as_str();
    let field = |name: &str| req.and_then(|r| r.get(name)).and_then(Value::as_str);
    match spec.rate {
        RateClass::Interactive => match field("ptyId").filter(|s| PtyId::is_valid_str(s)) {
            Some(pty) => BucketKey::Target(class, pty.to_string()),
            None => BucketKey::Class(class),
        },
        RateClass::Turn => {
            let target = field("uid")
                .filter(|s| SessionUid::is_valid_str(s))
                .or_else(|| field("teamKey").filter(|s| TeamKey::is_valid_str(s)));
            match target {
                Some(t) => BucketKey::Target(class, t.to_string()),
                None => BucketKey::Class(class),
            }
        }
        RateClass::Spawn | RateClass::Query => BucketKey::Class(class),
        RateClass::Probe | RateClass::Default => BucketKey::Command(spec.name),
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn refill(&mut self, now: Instant, rate: f64, burst: f64) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * rate).min(burst);
        self.at = now;
    }
}

/// The ms until one token is back in a bucket holding `tokens` (≥ 1 ms).
fn wait_ms(tokens: f64, rate: f64) -> u64 {
    (((1.0 - tokens) / rate) * 1000.0).ceil().max(1.0) as u64
}

/// The token buckets of one app (one per invoke handler).
#[derive(Debug, Default)]
pub struct RateLimiter {
    tables: Mutex<Tables>,
}

#[derive(Debug, Default)]
struct Tables {
    /// Class and command buckets (a fixed, small set).
    shared: HashMap<BucketKey, Bucket>,
    /// Per-target buckets (≤ [`MAX_TARGETS`]).
    targets: HashMap<BucketKey, Bucket>,
}

impl RateLimiter {
    pub fn new() -> RateLimiter {
        RateLimiter::default()
    }

    /// Take one token for a call to `spec` at `now`: `Err(retryAfterMs)` when
    /// its bucket is empty. A bespoke command is never limited here.
    pub fn check(&self, spec: &CommandSpec, req: Option<&Value>, now: Instant) -> Result<(), u64> {
        let Policy::Central(class) = policy(spec) else {
            return Ok(());
        };
        let (rate, burst) = params(class);
        let key = bucket_key(spec, req);
        let mut tables = self.tables.lock();
        let map = if matches!(key, BucketKey::Target(..)) {
            let targets = &mut tables.targets;
            if !targets.contains_key(&key) && targets.len() >= MAX_TARGETS {
                prune_full(targets, now);
                if targets.len() >= MAX_TARGETS {
                    return Err(wait_ms(0.0, rate));
                }
            }
            targets
        } else {
            &mut tables.shared
        };
        let bucket = map.entry(key).or_insert(Bucket { tokens: burst, at: now });
        bucket.refill(now, rate, burst);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(wait_ms(bucket.tokens, rate))
        }
    }

    /// The number of buckets held (tests).
    pub fn len(&self) -> usize {
        let t = self.tables.lock();
        t.shared.len() + t.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drop every target bucket that is full at `now` (it carries no state).
fn prune_full(map: &mut HashMap<BucketKey, Bucket>, now: Instant) {
    map.retain(|key, b| {
        let BucketKey::Target(class, _) = key else { return true };
        let (rate, burst) = params(class_of(class));
        let dt = now.saturating_duration_since(b.at).as_secs_f64();
        b.tokens + dt * rate < burst
    });
}

fn class_of(name: &str) -> RateClass {
    match name {
        "interactive" => RateClass::Interactive,
        "turn" => RateClass::Turn,
        "spawn" => RateClass::Spawn,
        "query" => RateClass::Query,
        "probe" => RateClass::Probe,
        _ => RateClass::Default,
    }
}

/// The managed limiter of one app.
pub type RateLimiterState = Arc<RateLimiter>;

/// §4.1 rule 6 for a request of `name` that passed rule 3 (a)/(b): take a
/// token from its bucket, or answer `BUSY{retryAfterMs}`. A command that is
/// not in the manifest (never invocable) is not limited here.
pub fn admit<R: tauri::Runtime>(webview: &tauri::Webview<R>, name: &str, req: &Value) -> Result<(), IpcError> {
    use tauri::Manager as _;
    let Some(spec) = ipc_manifest::command(name) else {
        return Ok(());
    };
    if matches!(policy(spec), Policy::Bespoke(_)) {
        return Ok(());
    }
    let app = webview.app_handle();
    let limiter = match app.try_state::<RateLimiterState>() {
        Some(l) => l.inner().clone(),
        None => {
            // Two first calls at once: `manage` keeps the first, both use it.
            app.manage(Arc::new(RateLimiter::new()));
            app.state::<RateLimiterState>().inner().clone()
        }
    };
    limiter.check(spec, Some(req), Instant::now()).map_err(IpcError::busy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn spec(name: &str) -> &'static CommandSpec {
        ipc_manifest::command(name).unwrap()
    }

    fn burst(l: &RateLimiter, name: &str, req: Value, n: usize, now: Instant) -> (usize, Vec<u64>) {
        let mut ok = 0;
        let mut busy = Vec::new();
        for _ in 0..n {
            match l.check(spec(name), Some(&req), now) {
                Ok(()) => ok += 1,
                Err(ms) => busy.push(ms),
            }
        }
        (ok, busy)
    }

    #[test]
    fn every_manifest_command_has_exactly_one_limiter() {
        for (name, _) in BESPOKE {
            assert!(ipc_manifest::command(name).is_some(), "{name} is not a command");
        }
        for c in ipc_manifest::COMMANDS {
            match policy(c) {
                Policy::Bespoke(owner) => assert!(!owner.is_empty()),
                Policy::Central(class) => assert_eq!(class, c.rate, "{}", c.name),
            }
        }
        let n = BESPOKE.len();
        let mut names: Vec<&str> = BESPOKE.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "a bespoke command listed twice");
    }

    #[test]
    fn interactive_allows_a_burst_of_100_then_refills_at_50_per_second() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let (ok, busy) = burst(&l, "ui_state_set", json!({"key": "homeMode", "value": "focus"}), 200, t0);
        assert_eq!(ok, 100);
        assert_eq!(busy.len(), 100);
        assert!(busy.iter().all(|ms| *ms == 20), "one token every 20 ms: {:?}", &busy[..3]);
        // The class is shared by its non-PTY commands …
        assert!(l.check(spec("app_view_state"), Some(&json!({})), t0).is_err());
        // … and refills.
        let (ok, _) = burst(&l, "ui_state_set", json!({}), 60, t0 + ms(1_000));
        assert_eq!(ok, 50);
    }

    #[test]
    fn pty_commands_draw_from_their_own_ptys_bucket() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let a = json!({"ptyId": "y_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b", "b64": "eA=="});
        let b = json!({"ptyId": "y_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c", "b64": "eA=="});
        assert_eq!(burst(&l, "pty_write", a.clone(), 150, t0).0, 100);
        assert!(l.check(spec("pty_resize"), Some(&a), t0).is_err(), "write, paste and resize share the PTY's bucket");
        assert_eq!(burst(&l, "pty_write", b, 150, t0).0, 100, "another PTY is not starved");
        assert!(l.check(spec("ui_state_set"), Some(&json!({})), t0).is_ok(), "nor the class's other commands");
    }

    #[test]
    fn views_mount_and_unmount_in_the_interactive_class() {
        // A45.3 ruling: a screen flip subscribes and attaches in bursts.
        for name in ["convo_subscribe", "convo_unsubscribe", "pty_attach", "pty_detach"] {
            assert_eq!(spec(name).rate, RateClass::Interactive, "{name}");
            assert_eq!(policy(spec(name)), Policy::Central(RateClass::Interactive), "{name}");
        }
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let pty = json!({"ptyId": "y_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b", "viewId": "main-1"});
        assert_eq!(burst(&l, "pty_attach", pty.clone(), 30, t0).0, 30, "30 tiles at once");
        assert_eq!(burst(&l, "pty_detach", pty, 30, t0).0, 30);
        assert_eq!(burst(&l, "convo_subscribe", json!({"uid": "s_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"}), 40, t0).0, 40);
    }

    #[test]
    fn default_is_ten_per_second_per_command() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let (ok, busy) = burst(&l, "link_open", json!({"url": "https://example.com"}), 30, t0);
        assert_eq!((ok, busy.len()), (10, 20));
        assert!(busy.iter().all(|ms| *ms == 100));
        assert!(l.check(spec("ide_reveal"), Some(&json!({})), t0).is_ok(), "per command");
    }

    #[test]
    fn probe_is_one_per_minute_per_command() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let (ok, busy) = burst(&l, "agents_rescan", json!({}), 20, t0);
        assert_eq!((ok, busy.len()), (1, 19));
        assert!(busy.iter().all(|ms| *ms == 60_000));
        assert_eq!(l.check(spec("agents_rescan"), None, t0 + ms(30_000)), Err(30_000));
        assert!(l.check(spec("agents_rescan"), None, t0 + ms(60_000)).is_ok());
    }

    #[test]
    fn spawn_query_and_turn_classes() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let (ok, busy) = burst(&l, "continue_all", json!({}), 8, t0);
        assert_eq!((ok, busy.len()), (5, 3));
        assert_eq!(busy[0], 2_000, "5 per 10 s");
        // `query` is one bucket for its central commands.
        assert_eq!(burst(&l, "changes_get", json!({}), 15, t0).0, 15);
        assert_eq!(burst(&l, "convo_load_older", json!({}), 15, t0).0, 5);
        // `turn` per team for team_send.
        let t1 = json!({"teamKey": "t_0123456789abcdef"});
        let t2 = json!({"teamKey": "t_0123456789abcdee"});
        assert_eq!(burst(&l, "team_send", t1, 15, t0).0, 10);
        assert_eq!(burst(&l, "team_send", t2, 15, t0).0, 10);
    }

    #[test]
    fn bespoke_commands_are_never_counted_here() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        for name in ["notify_test", "history_query", "turn_send", "session_start", "quota_refresh"] {
            assert_eq!(burst(&l, name, json!({}), 50, t0).0, 50, "{name}");
        }
        assert!(l.is_empty(), "no bucket was even created");
    }

    #[test]
    fn rotating_targets_never_reset_a_busy_bucket_and_never_grow_the_table() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let mine = json!({"teamKey": "t_ffffffffffffffff"});
        assert_eq!(burst(&l, "team_send", mine.clone(), 10, t0).0, 10);
        // A flood of new targets at the same instant.
        let mut refused = 0;
        for i in 0..(MAX_TARGETS as u64 + 100) {
            let k = json!({"teamKey": format!("t_{i:016x}")});
            if l.check(spec("team_send"), Some(&k), t0).is_err() {
                refused += 1;
            }
        }
        assert!(l.len() <= MAX_TARGETS, "{}", l.len());
        assert!(refused >= 100, "new targets are refused while the table is full of busy buckets");
        assert!(l.check(spec("team_send"), Some(&mine), t0).is_err(), "my bucket is still empty");
        // Once the flood's buckets are full again they are dropped for new ones.
        assert!(l.check(spec("team_send"), Some(&json!({"teamKey": "t_eeeeeeeeeeeeeeee"})), t0 + ms(2_000)).is_ok());
    }

    #[test]
    fn malformed_targets_share_the_class_bucket() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        let mut ok = 0;
        for i in 0..300 {
            let r = json!({"ptyId": format!("not-a-pty-{i}")});
            if l.check(spec("pty_write"), Some(&r), t0).is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 100, "an invalid ptyId never mints a bucket");
        assert_eq!(l.len(), 1);
    }
}
