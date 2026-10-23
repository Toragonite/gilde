//! Sleep/wake and time-zone changes (contract §10.4 rows "Sleep / wake" and
//! "Clock change / timezone change", §8.5 rule 5).
//!
//! Owner: T12. [`SystemEvents`] is a small fan-out hub: the real observer
//! ([`install_observers`], called once by the composition root on the main
//! thread) publishes `NSWorkspaceWillSleepNotification`,
//! `NSWorkspaceDidWakeNotification` and `NSSystemTimeZoneDidChangeNotification`
//! into it, and every Rust consumer subscribes: the notification policy (wake
//! grace, §8.5 rule 5), the visibility hub (`asleep`), and — wired by T09b —
//! discovery (reconcile on wake, pause scans on sleep, §10.4), the watchdog
//! (reset hysteresis on wake) and World (the "ended today" boundary on a
//! time-zone change, T-world-endedToday). Tests publish into a hub directly;
//! nothing in a test registers for real system notifications.
//!
//! Durations never need a sleep correction here: every threshold runs on the
//! uptime clock (`CLOCK_UPTIME_RAW`, stops during sleep, §3.5.2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol,
    NSSystemTimeZoneDidChangeNotification, NSTimeZone,
};
use parking_lot::Mutex;

/// A system event every interested service hears about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemEvent {
    /// `NSWorkspaceWillSleepNotification`.
    WillSleep,
    /// `NSWorkspaceDidWakeNotification`.
    DidWake,
    /// `NSSystemTimeZoneDidChangeNotification` (libc's zone is already
    /// refreshed when listeners run).
    TimeZoneChanged,
}

/// A subscriber. Called on the publishing thread with no lock of the hub
/// held; it must not block (hand the event to a task or channel).
pub type SystemListener = Arc<dyn Fn(SystemEvent) + Send + Sync>;

/// The fan-out hub for [`SystemEvent`]s.
#[derive(Default)]
pub struct SystemEvents {
    listeners: Mutex<Vec<SystemListener>>,
    asleep: AtomicBool,
}

impl std::fmt::Debug for SystemEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemEvents")
            .field("listeners", &self.listeners.lock().len())
            .field("asleep", &self.asleep())
            .finish()
    }
}

impl SystemEvents {
    pub fn new() -> Arc<SystemEvents> {
        Arc::new(SystemEvents::default())
    }

    /// Adds a subscriber (kept for the life of the hub).
    pub fn subscribe(&self, listener: SystemListener) {
        self.listeners.lock().push(listener);
    }

    /// Records the event and calls every subscriber in subscription order.
    /// A repeated `WillSleep` or `DidWake` (macOS can post either twice) is
    /// delivered only on a change of the asleep state.
    pub fn publish(&self, event: SystemEvent) {
        match event {
            SystemEvent::WillSleep => {
                if self.asleep.swap(true, Ordering::SeqCst) {
                    return;
                }
            }
            SystemEvent::DidWake => {
                if !self.asleep.swap(false, Ordering::SeqCst) {
                    return;
                }
            }
            SystemEvent::TimeZoneChanged => {}
        }
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            listener(event);
        }
    }

    /// Between `WillSleep` and `DidWake`.
    pub fn asleep(&self) -> bool {
        self.asleep.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// the real observer (installed once, main thread; never in tests)
// ---------------------------------------------------------------------------

static HUB: OnceLock<Arc<SystemEvents>> = OnceLock::new();

extern "C" {
    /// POSIX `tzset` (libSystem; the `libc` crate does not declare it for Apple targets).
    fn tzset();
}

/// Refreshes the process time zone after a change: libc (`localtime_r`,
/// `mktime`) caches it until `tzset`, Foundation until
/// `resetSystemTimeZone`. Footgun pinned: without this the "tomorrow 09:00"
/// pause and session-label times would keep the old zone.
pub fn refresh_time_zone() {
    // SAFETY: `tzset` takes no arguments and only re-reads the zone rules.
    unsafe { tzset() };
    NSTimeZone::resetSystemTimeZone();
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "GildeSleepObserver"]
    struct SleepObserver;

    impl SleepObserver {
        #[unsafe(method(gildeSystemEvent:))]
        fn system_event(&self, note: &NSNotification) {
            let name = note.name();
            // SAFETY: the notification names are immutable framework statics.
            let event = unsafe {
                if name.isEqualToString(NSWorkspaceWillSleepNotification) {
                    Some(SystemEvent::WillSleep)
                } else if name.isEqualToString(NSWorkspaceDidWakeNotification) {
                    Some(SystemEvent::DidWake)
                } else if name.isEqualToString(NSSystemTimeZoneDidChangeNotification) {
                    Some(SystemEvent::TimeZoneChanged)
                } else {
                    None
                }
            };
            let Some(event) = event else { return };
            if event == SystemEvent::TimeZoneChanged {
                refresh_time_zone();
            }
            if let Some(hub) = HUB.get() {
                hub.publish(event);
            }
        }
    }

    unsafe impl NSObjectProtocol for SleepObserver {}
);

impl SleepObserver {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: plain `-[NSObject init]` on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Registers the sleep/wake (workspace notification center) and time-zone
/// (default center) observers that publish into `hub`. Once per process: a
/// second call returns `false` and changes nothing. Called by the composition
/// root (T09b) from `setup()` on the main thread; never by tests.
pub fn install_observers(_mtm: MainThreadMarker, hub: Arc<SystemEvents>) -> bool {
    if HUB.set(hub).is_err() {
        return false;
    }
    let observer = SleepObserver::new();
    let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
    let default = NSNotificationCenter::defaultCenter();
    // SAFETY: the observer implements `gildeSystemEvent:` with the
    // `(NSNotification *)` signature; the names are framework statics. The
    // observer is never deallocated (forgotten below), so the centers'
    // unretained references stay valid for the life of the process.
    unsafe {
        workspace.addObserver_selector_name_object(
            &observer,
            sel!(gildeSystemEvent:),
            Some(NSWorkspaceWillSleepNotification),
            None,
        );
        workspace.addObserver_selector_name_object(
            &observer,
            sel!(gildeSystemEvent:),
            Some(NSWorkspaceDidWakeNotification),
            None,
        );
        default.addObserver_selector_name_object(
            &observer,
            sel!(gildeSystemEvent:),
            Some(NSSystemTimeZoneDidChangeNotification),
            None,
        );
    }
    std::mem::forget(observer);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn recorder(hub: &SystemEvents) -> Arc<Mutex<Vec<SystemEvent>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        hub.subscribe(Arc::new(move |e| s.lock().push(e)));
        seen
    }

    #[test]
    fn publishes_to_every_subscriber_in_order() {
        let hub = SystemEvents::new();
        let a = recorder(&hub);
        let b = recorder(&hub);
        hub.publish(SystemEvent::WillSleep);
        hub.publish(SystemEvent::DidWake);
        hub.publish(SystemEvent::TimeZoneChanged);
        let want = vec![SystemEvent::WillSleep, SystemEvent::DidWake, SystemEvent::TimeZoneChanged];
        assert_eq!(*a.lock(), want);
        assert_eq!(*b.lock(), want);
    }

    #[test]
    fn duplicate_sleep_or_wake_is_delivered_once() {
        let hub = SystemEvents::new();
        let seen = recorder(&hub);
        hub.publish(SystemEvent::DidWake); // awake already: no change
        hub.publish(SystemEvent::WillSleep);
        hub.publish(SystemEvent::WillSleep);
        assert!(hub.asleep());
        hub.publish(SystemEvent::DidWake);
        hub.publish(SystemEvent::DidWake);
        assert!(!hub.asleep());
        assert_eq!(*seen.lock(), vec![SystemEvent::WillSleep, SystemEvent::DidWake]);
    }

    #[test]
    fn a_listener_may_subscribe_from_inside_a_callback_without_deadlock() {
        let hub = SystemEvents::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let h = hub.clone();
        let c = calls.clone();
        hub.subscribe(Arc::new(move |_| {
            let c2 = c.clone();
            h.subscribe(Arc::new(move |_| {
                c2.fetch_add(1, Ordering::SeqCst);
            }));
        }));
        hub.publish(SystemEvent::TimeZoneChanged);
        hub.publish(SystemEvent::TimeZoneChanged);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
