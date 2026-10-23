//! Window occlusion and frontmost state (`gilde://visibility`, contract §4.3).
//!
//! Owner: T12. [`VisibilityHub`] holds `{frontmost, occluded, asleep}` and
//! emits `gilde://visibility` on every change through a [`VisibilityEmitter`]
//! (the Floor scheduler in the WebView listens, §11.2 rule 6), and calls Rust
//! subscribers (the notification policy, rule 2 of §8.4; discovery's scan
//! cadence via T09b). The real inputs come from [`install_observers`]
//! (application active/resign, window occlusion and close, called once on the
//! main thread by the composition root) and from [`VisibilityHub::follow`]
//! (sleep/wake from `sleep::SystemEvents`). Tests drive the setters with a
//! fake emitter.
//!
//! "On screen" for a session is decided by the notification policy from this
//! state **and** the WebView's `app_view_state` (F-04): a session is visible
//! only while Gilde is frontmost, its window is not occluded, the machine is
//! awake, the document is visible and the uid is among `visibleUids`.

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSApplicationDidBecomeActiveNotification, NSApplicationDidResignActiveNotification,
    NSWindow, NSWindowDidChangeOcclusionStateNotification, NSWindowOcclusionState,
    NSWindowWillCloseNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol};
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Runtime};

use crate::contract::store::{event_names, VisibilityEvent};
use crate::macos::sleep::{SystemEvent, SystemEvents};

/// Where `gilde://visibility` goes (the WebView in the app, a recorder in
/// tests).
pub trait VisibilityEmitter: Send + Sync {
    fn emit(&self, event: VisibilityEvent);
}

/// A Rust subscriber; called after the event was emitted, with no state lock
/// held. It must not call the hub's setters (that would re-enter the
/// emission lock) and must not block.
pub type VisibilityListener = Arc<dyn Fn(VisibilityEvent) + Send + Sync>;

/// The current visibility and its change fan-out.
pub struct VisibilityHub {
    state: Mutex<VisibilityEvent>,
    /// Serializes "change → emit → listeners" so the last emitted event is
    /// always the current state, even with setters racing on two threads.
    emission: Mutex<()>,
    emitter: Arc<dyn VisibilityEmitter>,
    listeners: Mutex<Vec<VisibilityListener>>,
}

impl std::fmt::Debug for VisibilityHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisibilityHub").field("state", &self.current()).finish()
    }
}

impl VisibilityHub {
    /// Starts as `{frontmost: false, occluded: false, asleep: false}` until
    /// the observers report the real state (not frontmost = a session is
    /// never treated as on screen before Gilde knows it is).
    pub fn new(emitter: Arc<dyn VisibilityEmitter>) -> Arc<VisibilityHub> {
        Arc::new(VisibilityHub {
            state: Mutex::new(VisibilityEvent { frontmost: false, occluded: false, asleep: false }),
            emission: Mutex::new(()),
            emitter,
            listeners: Mutex::new(Vec::new()),
        })
    }

    pub fn current(&self) -> VisibilityEvent {
        *self.state.lock()
    }

    pub fn subscribe(&self, listener: VisibilityListener) {
        self.listeners.lock().push(listener);
    }

    pub fn set_frontmost(&self, frontmost: bool) {
        self.update(|s| s.frontmost = frontmost);
    }

    pub fn set_occluded(&self, occluded: bool) {
        self.update(|s| s.occluded = occluded);
    }

    pub fn set_asleep(&self, asleep: bool) {
        self.update(|s| s.asleep = asleep);
    }

    /// Mirrors sleep/wake from the system hub into `asleep`.
    pub fn follow(self: &Arc<Self>, system: &SystemEvents) {
        let hub = Arc::downgrade(self);
        system.subscribe(Arc::new(move |event| {
            let Some(hub) = hub.upgrade() else { return };
            match event {
                SystemEvent::WillSleep => hub.set_asleep(true),
                SystemEvent::DidWake => hub.set_asleep(false),
                SystemEvent::TimeZoneChanged => {}
            }
        }));
    }

    fn update(&self, change: impl FnOnce(&mut VisibilityEvent)) {
        let _emission = self.emission.lock();
        let next = {
            let mut state = self.state.lock();
            let before = *state;
            change(&mut state);
            if *state == before {
                return;
            }
            *state
        };
        self.emitter.emit(next);
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            listener(next);
        }
    }
}

/// Emits `gilde://visibility` to every webview of the app.
pub struct TauriVisibilityEmitter<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> TauriVisibilityEmitter<R> {
    pub fn new(app: AppHandle<R>) -> TauriVisibilityEmitter<R> {
        TauriVisibilityEmitter { app }
    }
}

impl<R: Runtime> VisibilityEmitter for TauriVisibilityEmitter<R> {
    fn emit(&self, event: VisibilityEvent) {
        if self.app.emit(event_names::VISIBILITY, event).is_err() {
            tracing::debug!(target: "macos", "visibility event not delivered");
        }
    }
}

// ---------------------------------------------------------------------------
// the real observer (installed once, main thread; never in tests)
// ---------------------------------------------------------------------------

static HUB: OnceLock<Arc<VisibilityHub>> = OnceLock::new();

/// No visible, unoccluded window (except `closing`, which is about to go).
fn all_windows_occluded(mtm: MainThreadMarker, closing: Option<*const c_void>) -> bool {
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    !windows.iter().any(|w: Retained<NSWindow>| {
        let ptr = Retained::as_ptr(&w) as *const c_void;
        closing != Some(ptr) && w.isVisible() && w.occlusionState().contains(NSWindowOcclusionState::Visible)
    })
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "GildeVisibilityObserver"]
    struct VisibilityObserver;

    impl VisibilityObserver {
        #[unsafe(method(gildeVisibilityEvent:))]
        fn visibility_event(&self, note: &NSNotification) {
            // AppKit posts these on the main thread; anything else is ignored.
            let Some(mtm) = MainThreadMarker::new() else { return };
            let Some(hub) = HUB.get() else { return };
            let name = note.name();
            // SAFETY: the notification names are immutable framework statics.
            unsafe {
                if name.isEqualToString(NSApplicationDidBecomeActiveNotification) {
                    hub.set_frontmost(true);
                } else if name.isEqualToString(NSApplicationDidResignActiveNotification) {
                    hub.set_frontmost(false);
                } else if name.isEqualToString(NSWindowDidChangeOcclusionStateNotification) {
                    hub.set_occluded(all_windows_occluded(mtm, None));
                } else if name.isEqualToString(NSWindowWillCloseNotification) {
                    let closing = note.object().map(|o| Retained::as_ptr(&o) as *const c_void);
                    hub.set_occluded(all_windows_occluded(mtm, closing));
                }
            }
        }
    }

    unsafe impl NSObjectProtocol for VisibilityObserver {}
);

impl VisibilityObserver {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: plain `-[NSObject init]` on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Registers the application-active and window-occlusion observers that feed
/// `hub`, then syncs the hub to the current state. Once per process (a second
/// call returns `false`). Called by the composition root (T09b) from
/// `setup()` on the main thread; never by tests.
pub fn install_observers(mtm: MainThreadMarker, hub: Arc<VisibilityHub>) -> bool {
    if HUB.set(hub.clone()).is_err() {
        return false;
    }
    let observer = VisibilityObserver::new();
    let center = NSNotificationCenter::defaultCenter();
    // SAFETY: the observer implements `gildeVisibilityEvent:` with the
    // `(NSNotification *)` signature; the names are framework statics; the
    // observer is never deallocated (forgotten below).
    unsafe {
        for name in [
            NSApplicationDidBecomeActiveNotification,
            NSApplicationDidResignActiveNotification,
            NSWindowDidChangeOcclusionStateNotification,
            NSWindowWillCloseNotification,
        ] {
            center.addObserver_selector_name_object(&observer, sel!(gildeVisibilityEvent:), Some(name), None);
        }
    }
    std::mem::forget(observer);
    let app = NSApplication::sharedApplication(mtm);
    hub.set_frontmost(app.isActive());
    hub.set_occluded(all_windows_occluded(mtm, None));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<VisibilityEvent>>);
    impl VisibilityEmitter for Recorder {
        fn emit(&self, event: VisibilityEvent) {
            self.0.lock().push(event);
        }
    }

    fn ev(frontmost: bool, occluded: bool, asleep: bool) -> VisibilityEvent {
        VisibilityEvent { frontmost, occluded, asleep }
    }

    #[test]
    fn emits_only_on_change_and_reports_the_current_state() {
        let rec = Arc::new(Recorder::default());
        let hub = VisibilityHub::new(rec.clone());
        assert_eq!(hub.current(), ev(false, false, false));
        hub.set_frontmost(true);
        hub.set_frontmost(true);
        hub.set_occluded(true);
        hub.set_occluded(false);
        assert_eq!(*rec.0.lock(), vec![ev(true, false, false), ev(true, true, false), ev(true, false, false)]);
        assert_eq!(hub.current(), ev(true, false, false));
    }

    #[test]
    fn follows_sleep_and_wake_from_the_system_hub() {
        let rec = Arc::new(Recorder::default());
        let hub = VisibilityHub::new(rec.clone());
        let system = SystemEvents::new();
        hub.follow(&system);
        system.publish(SystemEvent::WillSleep);
        assert!(hub.current().asleep);
        system.publish(SystemEvent::TimeZoneChanged);
        system.publish(SystemEvent::DidWake);
        assert!(!hub.current().asleep);
        assert_eq!(*rec.0.lock(), vec![ev(false, false, true), ev(false, false, false)]);
    }

    #[test]
    fn listeners_see_each_change_after_the_emit() {
        let rec = Arc::new(Recorder::default());
        let hub = VisibilityHub::new(rec.clone());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let h = Arc::downgrade(&hub);
        hub.subscribe(Arc::new(move |e| {
            // reading the state from a listener is allowed
            let now = h.upgrade().unwrap().current();
            assert_eq!(now, e);
            s.lock().push(e);
        }));
        hub.set_asleep(true);
        assert_eq!(*seen.lock(), vec![ev(false, false, true)]);
    }

    #[test]
    fn concurrent_setters_leave_the_last_emitted_event_equal_to_the_state() {
        let rec = Arc::new(Recorder::default());
        let hub = VisibilityHub::new(rec.clone());
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let hub = hub.clone();
                std::thread::spawn(move || {
                    for j in 0..200 {
                        match (i + j) % 3 {
                            0 => hub.set_frontmost(j % 2 == 0),
                            1 => hub.set_occluded(j % 2 == 1),
                            _ => hub.set_asleep(j % 5 == 0),
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let last = *rec.0.lock().last().expect("some change was emitted");
        assert_eq!(last, hub.current());
    }
}
