//! The band-1 sound (contract §8.3, ux-ia §6.2).
//!
//! Owner: T12. "Sound for approvals" is off by default; when on, **one system
//! sound for band 1 only**. The sound is not played separately: it rides on
//! the notification it belongs to (the default notification sound), so
//!
//! - paused notifications are silent too (nothing is posted while paused),
//! - macOS Focus modes and the user's per-app sound setting apply,
//! - nothing on screen, nothing while typing, nothing before launch makes a
//!   sound (those notifications are never posted, §8.5 rules 2, 3, 5),
//! - nothing celebrates (§8.5 rule 8): bands 2–6, finished turns, reminders,
//!   health alerts and merged notifications without a new band-1 key are
//!   silent.

use crate::contract::ipc::NotifySettings;

/// Whether a session notification of `band` carries the sound.
pub fn wants_sound(band: u8, settings: &NotifySettings) -> bool {
    settings.sound && band == 1
}

/// The Settings test notification plays the sound when the setting is on, so
/// the user hears what they turned on.
pub fn wants_sound_for_test(settings: &NotifySettings) -> bool {
    settings.sound
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(sound: bool) -> NotifySettings {
        NotifySettings { watched_sessions: true, sound, show_details: false }
    }

    #[test]
    fn band_one_only_and_only_when_on() {
        assert!(wants_sound(1, &settings(true)));
        for band in 2..=6 {
            assert!(!wants_sound(band, &settings(true)), "band {band} is silent");
        }
        assert!(!wants_sound(1, &settings(false)), "off by default");
        assert!(wants_sound_for_test(&settings(true)));
        assert!(!wants_sound_for_test(&settings(false)));
    }
}
