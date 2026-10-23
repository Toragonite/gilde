//! Dock badge (= `need_you`, contract §8.3, Q7) and the LP-29 dock menu.
//!
//! Owner: T12. The badge shows the queue's `need_you` (bands 1–2, Later-ed
//! items excluded by World) exactly as delivered — this module never
//! recounts (D7) — and is empty at 0. It is set from Rust on every queue
//! change, also while notifications are paused or not permitted (the badge
//! never depends on the notification permission, T-notify-denied), and also
//! with no window open (it goes through `NSApp.dockTile`, not a window).
//!
//! **Dock menu: fallback.** The menu needs an `applicationDockMenu:` hook in
//! the application delegate, which the windowing layer owns; §1.3 P35
//! (LP-29, app flag `dockMenu`) is still open, so v1 ships the contract's
//! fallback — no dock menu; "Pause notifications" stays in the footer bell and
//! the palette (§8.3).

use std::sync::Arc;

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;
use parking_lot::Mutex;
use tauri::{AppHandle, Runtime};

/// Where the badge goes (the real dock tile in the app, a recorder in tests).
pub trait DockSink: Send + Sync {
    /// Shows `count` on the dock icon; 0 clears it. Must not block.
    fn set_badge(&self, count: u32);
}

/// The dock label for a count: none at 0, the decimal count otherwise.
pub fn badge_label(count: u32) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

/// Writes the badge only when the count changes.
pub struct DockBadge {
    sink: Arc<dyn DockSink>,
    /// The last count written (`None` = nothing written yet). Held across the
    /// sink call, so two racing callers can never leave an older count on the
    /// dock than the last one recorded (the sink only posts; it never blocks).
    last: Mutex<Option<u32>>,
}

impl std::fmt::Debug for DockBadge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockBadge").field("last", &*self.last.lock()).finish()
    }
}

impl DockBadge {
    pub fn new(sink: Arc<dyn DockSink>) -> DockBadge {
        DockBadge { sink, last: Mutex::new(None) }
    }

    /// Shows `need_you`; returns whether the sink was called (a repeat of the
    /// last count is skipped).
    pub fn show(&self, need_you: u32) -> bool {
        let mut last = self.last.lock();
        if *last == Some(need_you) {
            return false;
        }
        *last = Some(need_you);
        self.sink.set_badge(need_you);
        true
    }
}

/// The real dock tile: `NSApp.dockTile.badgeLabel`, set on the main thread.
pub struct MacDock<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> MacDock<R> {
    pub fn new(app: AppHandle<R>) -> MacDock<R> {
        MacDock { app }
    }
}

impl<R: Runtime> DockSink for MacDock<R> {
    fn set_badge(&self, count: u32) {
        let label = badge_label(count);
        let posted = self.app.run_on_main_thread(move || {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let tile = NSApplication::sharedApplication(mtm).dockTile();
            let text = label.as_deref().map(NSString::from_str);
            tile.setBadgeLabel(text.as_deref());
        });
        if posted.is_err() {
            tracing::debug!(target: "macos", count, "dock badge not posted");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<u32>>);
    impl DockSink for Recorder {
        fn set_badge(&self, count: u32) {
            self.0.lock().push(count);
        }
    }

    #[test]
    fn label_is_empty_at_zero_and_the_count_otherwise() {
        assert_eq!(badge_label(0), None);
        assert_eq!(badge_label(1).as_deref(), Some("1"));
        assert_eq!(badge_label(120).as_deref(), Some("120"));
    }

    #[test]
    fn the_badge_is_written_on_change_only_and_zero_is_written_once() {
        let rec = Arc::new(Recorder::default());
        let badge = DockBadge::new(rec.clone());
        assert!(badge.show(0), "the first value is always written (clears a stale badge)");
        assert!(!badge.show(0));
        assert!(badge.show(2));
        assert!(!badge.show(2));
        assert!(badge.show(0));
        assert_eq!(*rec.0.lock(), vec![0, 2, 0]);
    }
}
