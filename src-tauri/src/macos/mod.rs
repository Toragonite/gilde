//! macOS integration (contract §8.3, §9.5): dock, owner focus, visibility, sleep/wake.
//!
//! Owner: T12. The `mod` lines below are pre-declared by T01 and frozen
//! after wave 0 (contract §15.1, F-18): a new module is a contract change.
//!
//! Every OS boundary of this module sits behind a small trait with a fake in
//! the tests (`dock::DockSink`, `visibility::VisibilityEmitter`,
//! `owner::AppActivator`) or behind an observer installer the composition
//! root calls once on the main thread (`sleep::install_observers`,
//! `visibility::install_observers`). No test posts a notification, sets the
//! real badge, activates an app or registers for real system notifications.
//!
//! This file also holds the local-time helpers the notification layer needs
//! (the "Until tomorrow 09:00" pause and the `HH:MM` start time of a session
//! label). They go through libc's `localtime_r`/`mktime`; the sleep observer
//! calls `tzset` on `NSSystemTimeZoneDidChangeNotification`, so a time-zone
//! change is picked up by the next call.

pub mod dock;
pub mod owner;
pub mod sleep;
pub mod visibility;

use crate::contract::common::EpochMs;

/// The local calendar fields of an instant (what `localtime_r` returns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalParts {
    pub year: i32,
    /// 1..=12
    pub month: u32,
    /// 1..=31
    pub day: u32,
    /// 0..=23
    pub hour: u32,
    /// 0..=59
    pub minute: u32,
}

/// The local calendar fields of `at` (UTC ms), or `None` if libc refuses.
pub fn local_parts(at: EpochMs) -> Option<LocalParts> {
    let secs = at.div_euclid(1000) as libc::time_t;
    // SAFETY: a zeroed `tm` is a valid out-parameter; `localtime_r` is the
    // re-entrant form and touches only the two pointers it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return None;
    }
    Some(LocalParts {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    })
}

/// `HH:MM` (24 h) of `at` in local time; `None` if libc refuses.
pub fn local_hhmm(at: EpochMs) -> Option<String> {
    local_parts(at).map(|p| format!("{:02}:{:02}", p.hour, p.minute))
}

/// Before this local hour, "tomorrow 09:00" means this morning's 09:00: a
/// pause set at 00:30 is meant to end after the night, not 32 hours later
/// (the footer shows only "Paused until 09:00") — T12's reading of ux-ia §6.5
/// "Until tomorrow 09:00".
pub const EARLY_MORNING_END_HOUR: u32 = 5;

/// The local calendar day whose 09:00 ends a "tomorrow 09:00" pause started
/// at local time `now` (pure): the next day, or the same day before
/// [`EARLY_MORNING_END_HOUR`].
pub fn nine_am_day(now: LocalParts) -> (i32, u32, u32) {
    if now.hour < EARLY_MORNING_END_HOUR {
        return (now.year, now.month, now.day);
    }
    let (y, m, d) = (now.year, now.month, now.day);
    if d < days_in_month(y, m) {
        (y, m, d + 1)
    } else if m < 12 {
        (y, m + 1, 1)
    } else {
        (y + 1, 1, 1)
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
    }
}

/// The instant of 09:00 local time on the day [`nine_am_day`] picks for
/// `now_wall`. Falls back to `now_wall + 12 h` if libc refuses; never panics
/// and never returns a time at or before `now_wall`.
pub fn tomorrow_nine(now_wall: EpochMs) -> EpochMs {
    let fallback = now_wall.saturating_add(12 * 3_600_000);
    let Some(now) = local_parts(now_wall) else {
        return fallback;
    };
    let (y, m, d) = nine_am_day(now);
    // SAFETY: a zeroed `tm` is valid; `mktime` normalizes it in place.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = y - 1900;
    tm.tm_mon = m as i32 - 1;
    tm.tm_mday = d as i32;
    tm.tm_hour = 9;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    tm.tm_isdst = -1; // libc decides DST for that day
    let secs = unsafe { libc::mktime(&mut tm) };
    if secs == -1 {
        return fallback;
    }
    let at = (secs as i64).saturating_mul(1000);
    if at <= now_wall {
        fallback
    } else {
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(year: i32, month: u32, day: u32, hour: u32) -> LocalParts {
        LocalParts { year, month, day, hour, minute: 0 }
    }

    #[test]
    fn nine_am_day_rolls_over_days_months_years_and_leap_days() {
        assert_eq!(nine_am_day(parts(2026, 10, 23, 14)), (2026, 10, 24));
        assert_eq!(nine_am_day(parts(2026, 10, 31, 9)), (2026, 11, 1));
        assert_eq!(nine_am_day(parts(2026, 12, 31, 23)), (2027, 1, 1));
        assert_eq!(nine_am_day(parts(2028, 2, 28, 12)), (2028, 2, 29));
        assert_eq!(nine_am_day(parts(2027, 2, 28, 12)), (2027, 3, 1));
        assert_eq!(nine_am_day(parts(2100, 2, 28, 12)), (2100, 3, 1));
        assert_eq!(nine_am_day(parts(2000, 2, 28, 12)), (2000, 2, 29));
        assert_eq!(nine_am_day(parts(2026, 4, 30, 12)), (2026, 5, 1));
    }

    #[test]
    fn nine_am_day_before_five_is_this_morning() {
        assert_eq!(nine_am_day(parts(2026, 10, 23, 0)), (2026, 10, 23));
        assert_eq!(nine_am_day(parts(2026, 10, 23, 4)), (2026, 10, 23));
        assert_eq!(nine_am_day(parts(2026, 10, 23, 5)), (2026, 10, 24));
    }

    #[test]
    fn tomorrow_nine_is_a_future_nine_oclock_within_a_day_and_a_half() {
        // Whatever the machine's zone: in the future, at most 33 h away (DST
        // shifts included), and 09:00 locally.
        for offset_h in [0i64, 3, 7, 11, 15, 19, 23] {
            let now = 1_792_000_000_000 + offset_h * 3_600_000;
            let at = tomorrow_nine(now);
            assert!(at > now, "future");
            assert!(at - now <= 33 * 3_600_000, "within 33 h");
            let p = local_parts(at).expect("local parts");
            assert_eq!((p.hour, p.minute), (9, 0));
        }
    }

    #[test]
    fn local_hhmm_is_two_digit_24h() {
        let s = local_hhmm(1_792_000_000_000).unwrap();
        assert_eq!(s.len(), 5);
        assert_eq!(&s[2..3], ":");
    }
}
