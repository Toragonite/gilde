//! The plugin fallback without withdrawal (contract §8.4, LP-28 fallback).
//!
//! Owner: T12. While §1.3 P34 (LP-28, app flag `notificationCenter`) is open
//! — and in every unbundled process — notifications go through
//! `tauri-plugin-notification` from Rust: no identifiers that macOS keeps, so
//! no withdrawal, and no click routing (a click only activates Gilde, which
//! shows Home). The plugin reports permission as granted on desktop and never
//! prompts. The WebView has no notification permission (§9.2).
//!
//! [`DisabledSink`] is used when neither sink is available (no plugin
//! registered, no bundle): nothing is posted, the queue and the badge still
//! work.

use async_trait::async_trait;
use tauri::{AppHandle, Manager, Runtime};

use super::{Authorization, Notification, NotificationSink};

/// The default system sound name the plugin passes on.
const DEFAULT_SOUND: &str = "default";

/// `tauri-plugin-notification` (registered by the composition root).
pub struct PluginSink<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> PluginSink<R> {
    pub fn new(app: AppHandle<R>) -> PluginSink<R> {
        PluginSink { app }
    }
}

#[async_trait]
impl<R: Runtime> NotificationSink for PluginSink<R> {
    fn deliver(&self, n: &Notification) -> bool {
        // `try_state`: a missing plugin is a refusal, never a panic
        let Some(plugin) = self.app.try_state::<tauri_plugin_notification::Notification<R>>() else {
            return false;
        };
        let mut builder = plugin.builder().title(n.title.clone()).body(n.body.clone());
        if n.sound {
            builder = builder.sound(DEFAULT_SOUND);
        }
        // `show` hands the notification to a task of its own (non-blocking)
        builder.show().is_ok()
    }

    fn withdraw(&self, ids: &[String]) {
        let _ = ids; // not supported by the fallback
    }

    fn supports_withdrawal(&self) -> bool {
        false
    }

    fn authorization(&self) -> Authorization {
        if self.app.try_state::<tauri_plugin_notification::Notification<R>>().is_some() {
            Authorization::Granted
        } else {
            Authorization::Denied
        }
    }

    async fn request_authorization(&self) -> bool {
        self.authorization() == Authorization::Granted
    }
}

/// Posts nothing (no sink available in this process).
#[derive(Debug, Default, Clone, Copy)]
pub struct DisabledSink;

#[async_trait]
impl NotificationSink for DisabledSink {
    fn deliver(&self, _n: &Notification) -> bool {
        false
    }

    fn withdraw(&self, _ids: &[String]) {}

    fn supports_withdrawal(&self) -> bool {
        false
    }

    fn authorization(&self) -> Authorization {
        Authorization::Denied
    }

    async fn request_authorization(&self) -> bool {
        false
    }
}
