//! UNUserNotificationCenter via objc2 (contract §8.4, PROVISIONAL LP-28).
//!
//! Owner: T12. Used only when app flag `notificationCenter` is on **and** the
//! process is a bundled `.app` (`notify::choose_sink`): in an unbundled
//! process (`cargo test`, `tauri dev`) `+[UNUserNotificationCenter
//! currentNotificationCenter]` raises an Objective-C exception, so this module
//! is never reached there. No test posts through it, requests authorization
//! or installs the delegate; the `#[ignore]` test in `tests/notify_macos.rs`
//! is for the lead to run by hand in a bundled build.
//!
//! What it adds over the fallback: stable identifiers (a reminder replaces
//! its original, the merged notification replaces the previous one),
//! `removeDeliveredNotificationsWithIdentifiers` (withdrawal), and the
//! delegate's click callback (→ `gilde://focus-request`). Presentation while
//! Gilde is frontmost is allowed by the delegate, because the policy posts
//! only for sessions that are not on screen (rule 2).
//!
//! Authorization: the current state is read once at install
//! (`getNotificationSettingsWithCompletionHandler`, never prompts); the prompt
//! appears only from `request_authorization`, i.e. `notify_permission_request`.
//!
//! Blocks: every completion handler is a `block2::RcBlock` (A16.6); the
//! blocks the delegate receives are called through `block2::Block::call`.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_foundation::{NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions, UNNotificationRequest,
    UNNotificationResponse, UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};
use parking_lot::Mutex;
use tokio::sync::oneshot;

use super::{Authorization, ClickHandler, Notification, NotificationSink};

/// Whether this process runs from a bundled `.app` with a bundle identifier
/// (the precondition of the notification center). Read-only; safe anywhere.
pub fn is_bundled_app() -> bool {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
}

// ---------------------------------------------------------------------------
// process state
// ---------------------------------------------------------------------------

/// 0 unknown, 1 not determined, 2 denied, 3 granted.
static STATUS: AtomicU8 = AtomicU8::new(0);
static CLICK: Mutex<Option<ClickHandler>> = Mutex::new(None);
static INSTALLED: OnceLock<()> = OnceLock::new();

fn status_code(status: UNAuthorizationStatus) -> u8 {
    if status == UNAuthorizationStatus::NotDetermined {
        1
    } else if status == UNAuthorizationStatus::Denied {
        2
    } else {
        // Authorized, Provisional and Ephemeral all show notifications
        3
    }
}

fn authorization_of(code: u8) -> Authorization {
    match code {
        1 => Authorization::NotDetermined,
        2 => Authorization::Denied,
        3 => Authorization::Granted,
        _ => Authorization::Unknown,
    }
}

// ---------------------------------------------------------------------------
// the delegate
// ---------------------------------------------------------------------------

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "GildeNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            handler: &DynBlock<dyn Fn()>,
        ) {
            // SAFETY: an immutable framework static.
            let default_action = unsafe { UNNotificationDefaultActionIdentifier };
            if response.actionIdentifier().isEqualToString(default_action) {
                let id = response.notification().request().identifier().to_string();
                if id.starts_with("gilde.") {
                    let click = CLICK.lock().clone();
                    if let Some(click) = click {
                        click(id);
                    }
                }
            }
            // called exactly once, as the framework requires
            handler.call(());
        }
    }
);

impl NotificationDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: plain `-[NSObject init]` on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------
// the sink
// ---------------------------------------------------------------------------

/// `UNUserNotificationCenter` (bundled app, app flag `notificationCenter`).
#[derive(Debug)]
pub struct NotificationCenterSink {
    _private: (),
}

impl NotificationCenterSink {
    /// Installs the delegate (once per process) and reads the current
    /// authorization without prompting. `None` in an unbundled process.
    pub fn install() -> Option<NotificationCenterSink> {
        if !is_bundled_app() {
            return None;
        }
        INSTALLED.get_or_init(|| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = NotificationDelegate::new();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // the center keeps only a weak reference: the delegate lives for
            // the life of the process
            std::mem::forget(delegate);
            let read_status = RcBlock::new(|settings: NonNull<UNNotificationSettings>| {
                // SAFETY: the framework passes a valid settings object.
                let status = unsafe { settings.as_ref() }.authorizationStatus();
                STATUS.store(status_code(status), Ordering::SeqCst);
            });
            center.getNotificationSettingsWithCompletionHandler(&read_status);
        });
        Some(NotificationCenterSink { _private: () })
    }
}

#[async_trait]
impl NotificationSink for NotificationCenterSink {
    fn deliver(&self, n: &Notification) -> bool {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&n.title));
        content.setBody(&NSString::from_str(&n.body));
        if n.sound {
            content.setSound(Some(&UNNotificationSound::defaultSound()));
        }
        let request =
            UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&n.id), &content, None);
        UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, None);
        true
    }

    fn withdraw(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let strings: Vec<Retained<NSString>> = ids.iter().map(|id| NSString::from_str(id)).collect();
        let array = NSArray::from_retained_slice(&strings);
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.removeDeliveredNotificationsWithIdentifiers(&array);
        center.removePendingNotificationRequestsWithIdentifiers(&array);
    }

    fn supports_withdrawal(&self) -> bool {
        true
    }

    fn authorization(&self) -> Authorization {
        authorization_of(STATUS.load(Ordering::SeqCst))
    }

    async fn request_authorization(&self) -> bool {
        let (reply, answer) = oneshot::channel();
        {
            let reply = Mutex::new(Some(reply));
            let done = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
                let granted = granted.as_bool();
                STATUS.store(if granted { 3 } else { 2 }, Ordering::SeqCst);
                if let Some(reply) = reply.lock().take() {
                    let _ = reply.send(granted);
                }
            });
            let options = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge;
            // the center copies the block; ours is released at the end of the scope
            UNUserNotificationCenter::currentNotificationCenter().requestAuthorizationWithOptions_completionHandler(options, &done);
        }
        matches!(tokio::time::timeout(Duration::from_secs(600), answer).await, Ok(Ok(true)))
    }

    fn set_click_handler(&self, handler: ClickHandler) {
        *CLICK.lock() = Some(handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_binary_is_not_a_bundled_app() {
        // the guard that keeps every test away from the notification center
        assert!(!is_bundled_app());
        assert!(NotificationCenterSink::install().is_none());
    }

    #[test]
    fn status_codes_map_to_authorization() {
        assert_eq!(authorization_of(status_code(UNAuthorizationStatus::NotDetermined)), Authorization::NotDetermined);
        assert_eq!(authorization_of(status_code(UNAuthorizationStatus::Denied)), Authorization::Denied);
        assert_eq!(authorization_of(status_code(UNAuthorizationStatus::Authorized)), Authorization::Granted);
        assert_eq!(authorization_of(status_code(UNAuthorizationStatus::Provisional)), Authorization::Granted);
        assert_eq!(authorization_of(0), Authorization::Unknown);
    }
}
