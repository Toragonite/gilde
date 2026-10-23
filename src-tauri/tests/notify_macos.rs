//! The real macOS paths, for the lead to run **by hand** in a bundled build
//! (every test here is `#[ignore]`; CI and the task gates never run them).
//!
//! They post a real notification, ask for notification permission (a dialog
//! on screen), set the real dock badge and activate an app — exactly what the
//! automated tests must never do. Run one at a time, deliberately:
//! `cargo test --test notify_macos -- --ignored --test-threads=1 <name>`.
//! In an unbundled test binary the notification-center tests skip themselves
//! (the notification center needs a bundled `.app`; see `notify::center`).

use gilde_code_lib::macos::owner::{Activation, AppActivator, MacActivator};
use gilde_code_lib::notify::center::{is_bundled_app, NotificationCenterSink};
use gilde_code_lib::notify::{Notification, NotificationSink};

#[tokio::test]
#[ignore = "manual: shows a permission dialog and a real notification"]
async fn manual_notification_center_round_trip() {
    if !is_bundled_app() {
        eprintln!("not a bundled app: the notification center is unavailable here");
        return;
    }
    let sink = NotificationCenterSink::install().expect("bundled");
    let granted = sink.request_authorization().await;
    eprintln!("granted: {granted}, authorization: {:?}", sink.authorization());
    let n = Notification { id: "gilde.test".into(), title: "Gilde".into(), body: "manual test".into(), sound: false };
    assert!(sink.deliver(&n));
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    sink.withdraw(&["gilde.test".to_string()]);
}

#[test]
#[ignore = "manual: brings the Finder forward"]
fn manual_activate_finder_by_pid() {
    // the pid of Finder must be passed in by the person running the test
    let Ok(pid) = std::env::var("GILDE_MANUAL_FINDER_PID").map(|s| s.parse::<i32>().unwrap_or(0)) else {
        eprintln!("set GILDE_MANUAL_FINDER_PID");
        return;
    };
    let outcome = MacActivator.activate(pid, "com.apple.finder");
    assert_eq!(outcome, Activation::Activated);
    // the pid-reuse fence: the same pid under another bundle id is refused
    assert_eq!(MacActivator.activate(pid, "com.example.other"), Activation::NotThatApp);
}
