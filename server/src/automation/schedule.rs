//! When a policy fires: every few minutes or hours, daily or weekly at a time
//! of day in a time zone, once at a date and time, or when a device enrolls.
//!
//! Times of day are wall-clock times in the policy's IANA time zone, so a
//! daily 03:00 stays 03:00 across daylight saving changes. A time the clocks
//! skip that day fires at the first minute after the change, and a time that
//! happens twice fires the first time.
use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::time::MINUTE_MS;

/// The shortest interval between a policy's runs.
pub const MIN_INTERVAL_MINUTES: i64 = 5;
/// The longest interval: four weeks.
pub const MAX_INTERVAL_MINUTES: i64 = 28 * 24 * 60;

/// The days of the week a weekly policy runs on, a bit per day from Monday
/// (1) to Sunday (64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weekdays(u8);

impl Weekdays {
    pub const ALL: Self = Self(0x7f);

    /// From ISO day numbers, Monday = 1 to Sunday = 7. `None` if one is out
    /// of range or there are none.
    pub fn from_days(days: &[u8]) -> Option<Self> {
        let mut bits = 0u8;
        for &day in days {
            if !(1..=7).contains(&day) {
                return None;
            }
            bits |= 1 << (day - 1);
        }
        (bits != 0).then_some(Self(bits))
    }

    pub fn from_bits(bits: i64) -> Option<Self> {
        u8::try_from(bits)
            .ok()
            .filter(|bits| *bits != 0 && *bits <= 0x7f)
            .map(Self)
    }

    pub fn bits(self) -> i64 {
        i64::from(self.0)
    }

    /// The ISO day numbers, Monday first.
    pub fn days(self) -> Vec<u8> {
        (1..=7)
            .filter(|day| self.0 & (1 << (day - 1)) != 0)
            .collect()
    }

    fn contains(self, date: NaiveDate) -> bool {
        let day = date.weekday().number_from_monday();
        self.0 & (1 << (day - 1)) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Every `minutes`, counted from the anchor (when the schedule was set).
    Interval {
        minutes: i64,
    },
    Daily {
        time: NaiveTime,
        zone: Tz,
    },
    Weekly {
        days: Weekdays,
        time: NaiveTime,
        zone: Tz,
    },
    Once {
        at: NaiveDateTime,
        zone: Tz,
    },
    /// When a device enrolls; never on a clock.
    Enroll,
}

impl Trigger {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Interval { .. } => "interval",
            Self::Daily { .. } => "daily",
            Self::Weekly { .. } => "weekly",
            Self::Once { .. } => "once",
            Self::Enroll => "enroll",
        }
    }

    /// Whether the policy fires on a clock, rather than on enrollment.
    pub fn scheduled(&self) -> bool {
        !matches!(self, Self::Enroll)
    }

    /// Whether a firing the server missed by a lot (it was down) still runs
    /// late. Intervals catch up once; times of day are skipped, since a
    /// script meant for 03:00 may disrupt people at noon.
    pub fn catches_up_late(&self) -> bool {
        matches!(self, Self::Interval { .. })
    }

    /// The first time after `after_ms` the policy fires, or `None` if it
    /// never does again. Interval schedules count from `anchor_ms`.
    pub fn next_after(&self, after_ms: i64, anchor_ms: i64) -> Option<i64> {
        match *self {
            Self::Interval { minutes } => {
                let step = minutes.max(1) * MINUTE_MS;
                let periods = if after_ms < anchor_ms {
                    1
                } else {
                    (after_ms - anchor_ms) / step + 1
                };
                Some(anchor_ms + periods * step)
            }
            Self::Daily { time, zone } => next_on_days(after_ms, time, zone, Weekdays::ALL),
            Self::Weekly { days, time, zone } => next_on_days(after_ms, time, zone, days),
            Self::Once { at, zone } => resolve(zone, at).filter(|at| *at > after_ms),
            Self::Enroll => None,
        }
    }
}

/// The first time of day `time` in `zone`, on one of `days`, after `after_ms`.
fn next_on_days(after_ms: i64, time: NaiveTime, zone: Tz, days: Weekdays) -> Option<i64> {
    let after = DateTime::<Utc>::from_timestamp_millis(after_ms)?;
    let mut date = after.with_timezone(&zone).date_naive();
    // A week and a day covers every weekday after today's time has passed.
    for _ in 0..9 {
        if days.contains(date)
            && let Some(at) = resolve(zone, date.and_time(time))
            && at > after_ms
        {
            return Some(at);
        }
        date = date.checked_add_days(Days::new(1))?;
    }
    None
}

/// The moment a wall-clock time in `zone` happens: the first of two when
/// clocks go back, and the first minute after the change when they go
/// forward past it.
pub fn resolve(zone: Tz, local: NaiveDateTime) -> Option<i64> {
    if let Some(at) = zone.from_local_datetime(&local).earliest() {
        return Some(at.timestamp_millis());
    }
    // Clock changes skip at most a few hours.
    (1..=24 * 60).find_map(|minute| {
        zone.from_local_datetime(&(local + chrono::Duration::minutes(minute)))
            .earliest()
            .map(|at| at.timestamp_millis())
    })
}

/// `HH:MM`, 24-hour.
pub fn parse_time_of_day(text: &str) -> Option<NaiveTime> {
    let (hours, minutes) = text.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    NaiveTime::from_hms_opt(hours.parse().ok()?, minutes.parse().ok()?, 0)
}

/// `YYYY-MM-DDTHH:MM`, a local date and time.
pub fn parse_local_date_time(text: &str) -> Option<NaiveDateTime> {
    let (date, time) = text.split_once('T')?;
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(date.and_time(parse_time_of_day(time)?))
}

pub fn format_time_of_day(time: NaiveTime) -> String {
    time.format("%H:%M").to_string()
}

pub fn format_local_date_time(at: NaiveDateTime) -> String {
    at.format("%Y-%m-%dT%H:%M").to_string()
}

pub fn parse_zone(name: &str) -> Option<Tz> {
    name.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(zone: Tz, text: &str) -> i64 {
        resolve(zone, parse_local_date_time(text).unwrap()).unwrap()
    }

    fn local(zone: Tz, at: i64) -> String {
        format_local_date_time(
            DateTime::<Utc>::from_timestamp_millis(at)
                .unwrap()
                .with_timezone(&zone)
                .naive_local(),
        )
    }

    #[test]
    fn intervals_count_from_their_anchor() {
        let trigger = Trigger::Interval { minutes: 15 };
        let anchor = 1_000_000_000;
        let step = 15 * MINUTE_MS;
        assert_eq!(trigger.next_after(anchor, anchor), Some(anchor + step));
        assert_eq!(trigger.next_after(anchor - 5, anchor), Some(anchor + step));
        assert_eq!(
            trigger.next_after(anchor + step, anchor),
            Some(anchor + 2 * step),
            "strictly after"
        );
        // Hours late, the next one is still on the grid, once.
        assert_eq!(
            trigger.next_after(anchor + 10 * step + 1, anchor),
            Some(anchor + 11 * step)
        );
    }

    #[test]
    fn daily_times_follow_the_zones_clock_across_daylight_saving() {
        let zone: Tz = "America/New_York".parse().unwrap();
        let time = parse_time_of_day("09:30").unwrap();
        let trigger = Trigger::Daily { time, zone };
        let before = ms(zone, "2026-03-07T10:00");
        let first = trigger.next_after(before, 0).unwrap();
        assert_eq!(local(zone, first), "2026-03-08T09:30");
        // Clocks went forward overnight: 09:30 is 13:30 UTC, not 14:30.
        assert_eq!(
            first - before,
            23 * 60 * MINUTE_MS + 30 * MINUTE_MS - 60 * MINUTE_MS
        );
        let second = trigger.next_after(first, 0).unwrap();
        assert_eq!(local(zone, second), "2026-03-09T09:30");
    }

    #[test]
    fn skipped_and_repeated_times_fire_once() {
        let zone: Tz = "Europe/Berlin".parse().unwrap();
        // 02:30 doesn't exist on 29 March 2026: clocks jump from 02:00 to 03:00.
        let skipped = Trigger::Daily {
            time: parse_time_of_day("02:30").unwrap(),
            zone,
        };
        let at = skipped.next_after(ms(zone, "2026-03-28T12:00"), 0).unwrap();
        assert_eq!(local(zone, at), "2026-03-29T03:00");
        // 02:30 happens twice on 25 October 2026; the first one counts.
        let repeated = skipped.next_after(ms(zone, "2026-10-24T12:00"), 0).unwrap();
        let next = skipped.next_after(repeated, 0).unwrap();
        assert_eq!(local(zone, next), "2026-10-26T02:30");
        assert_eq!(next - repeated, 25 * 60 * MINUTE_MS, "the day has 25 hours");
    }

    #[test]
    fn weekly_schedules_run_on_their_days_only() {
        let zone: Tz = "UTC".parse().unwrap();
        let trigger = Trigger::Weekly {
            // Monday and Friday.
            days: Weekdays::from_days(&[1, 5]).unwrap(),
            time: parse_time_of_day("08:00").unwrap(),
            zone,
        };
        // Wednesday 21 October 2026.
        let at = trigger.next_after(ms(zone, "2026-10-21T12:00"), 0).unwrap();
        assert_eq!(local(zone, at), "2026-10-23T08:00");
        let at = trigger.next_after(at, 0).unwrap();
        assert_eq!(local(zone, at), "2026-10-26T08:00");
        // Monday before 08:00 runs that day.
        let at = trigger.next_after(ms(zone, "2026-10-26T07:59"), 0).unwrap();
        assert_eq!(local(zone, at), "2026-10-26T08:00");
        assert_eq!(Weekdays::from_days(&[5, 1, 1]).unwrap().days(), vec![1, 5]);
        assert_eq!(Weekdays::from_days(&[]), None);
        assert_eq!(Weekdays::from_days(&[8]), None);
        assert_eq!(Weekdays::from_bits(0), None);
        assert_eq!(Weekdays::from_bits(128), None);
    }

    #[test]
    fn once_fires_only_while_ahead() {
        let zone: Tz = "Asia/Tokyo".parse().unwrap();
        let at = parse_local_date_time("2026-11-01T06:15").unwrap();
        let trigger = Trigger::Once { at, zone };
        let when = ms(zone, "2026-11-01T06:15");
        assert_eq!(trigger.next_after(when - 1, 0), Some(when));
        assert_eq!(trigger.next_after(when, 0), None);
        assert_eq!(Trigger::Enroll.next_after(0, 0), None);
        assert!(Trigger::Interval { minutes: 5 }.catches_up_late());
        assert!(!trigger.catches_up_late());
    }

    #[test]
    fn times_parse_strictly() {
        assert_eq!(parse_time_of_day("07:05"), NaiveTime::from_hms_opt(7, 5, 0));
        for invalid in ["7:05", "24:00", "07:60", "07-05", "", "07:05:00"] {
            assert_eq!(parse_time_of_day(invalid), None, "{invalid}");
        }
        assert!(parse_local_date_time("2026-02-30T10:00").is_none());
        assert!(parse_local_date_time("2026-02-03 10:00").is_none());
        assert_eq!(
            format_local_date_time(parse_local_date_time("2026-02-03T10:00").unwrap()),
            "2026-02-03T10:00"
        );
        assert!(parse_zone("Mars/Olympus").is_none());
        // The website offers current names; browsers' older ones work too.
        for zone in [
            "Europe/London",
            "Asia/Kolkata",
            "Asia/Calcutta",
            "Europe/Kyiv",
            "America/Nuuk",
            "Pacific/Kanton",
            "America/Argentina/Buenos_Aires",
        ] {
            assert!(parse_zone(zone).is_some(), "{zone}");
        }
    }
}
