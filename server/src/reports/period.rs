//! A report's period: the last 7 or 30 days, the previous calendar month, or
//! a custom range of dates, judged in a time zone.
//!
//! Days are the zone's calendar days, so a period always starts and ends at
//! a local midnight, and a week across a daylight saving change is 167 or 169
//! hours long. "The last 7 days" are the seven whole days before the day the
//! report is made, so a report made on Monday morning covers Monday to Sunday
//! of the week before, and two reports a week apart don't overlap.
use chrono::{DateTime, Datelike, Days, Months, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::automation::schedule;

/// The longest custom range, in days.
pub const MAX_CUSTOM_DAYS: u64 = 366;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeriodKind {
    #[serde(rename = "last_7_days")]
    Last7Days,
    #[serde(rename = "last_30_days")]
    Last30Days,
    #[serde(rename = "previous_month")]
    PreviousMonth,
    #[serde(rename = "custom")]
    Custom,
}

impl PeriodKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Last7Days => "last_7_days",
            Self::Last30Days => "last_30_days",
            Self::PreviousMonth => "previous_month",
            Self::Custom => "custom",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "last_7_days" => Some(Self::Last7Days),
            "last_30_days" => Some(Self::Last30Days),
            "previous_month" => Some(Self::PreviousMonth),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Last7Days => "Last 7 days",
            Self::Last30Days => "Last 30 days",
            Self::PreviousMonth => "Previous month",
            Self::Custom => "Custom range",
        }
    }
}

/// A period resolved to times: from `start_ms` up to, not including,
/// `end_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub kind: PeriodKind,
    pub start_ms: i64,
    pub end_ms: i64,
    pub zone: Tz,
}

/// Why a period can't be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeriodError {
    #[error("choose the first and last day of the range")]
    MissingDates,
    #[error("the range's first day must not be after its last")]
    Reversed,
    #[error("the range can be at most {MAX_CUSTOM_DAYS} days")]
    TooLong,
    #[error("the range must start today or earlier")]
    Future,
}

impl Period {
    /// The period of `kind` (`custom` with its first and last day) for a
    /// report made at `now_ms`, in `zone`.
    pub fn resolve(
        kind: PeriodKind,
        custom: Option<(NaiveDate, NaiveDate)>,
        zone: Tz,
        now_ms: i64,
    ) -> Result<Self, PeriodError> {
        let today = local_date(zone, now_ms);
        let days_back = |days: u64| today.checked_sub_days(Days::new(days)).unwrap_or(today);
        let (start, end) = match kind {
            PeriodKind::Last7Days => (midnight(zone, days_back(7)), midnight(zone, today)),
            PeriodKind::Last30Days => (midnight(zone, days_back(30)), midnight(zone, today)),
            PeriodKind::PreviousMonth => {
                let this_month = today.with_day(1).unwrap_or(today);
                let previous = this_month
                    .checked_sub_months(Months::new(1))
                    .unwrap_or(this_month);
                (midnight(zone, previous), midnight(zone, this_month))
            }
            PeriodKind::Custom => {
                let (first, last) = custom.ok_or(PeriodError::MissingDates)?;
                if first > last {
                    return Err(PeriodError::Reversed);
                }
                if (last - first).num_days() >= MAX_CUSTOM_DAYS as i64 {
                    return Err(PeriodError::TooLong);
                }
                if first > today {
                    return Err(PeriodError::Future);
                }
                let after = last.checked_add_days(Days::new(1)).unwrap_or(last);
                // A range that runs into today ends now: nothing later exists.
                (midnight(zone, first), midnight(zone, after).min(now_ms))
            }
        };
        Ok(Self {
            kind,
            start_ms: start,
            end_ms: end.max(start),
            zone,
        })
    }

    /// The first and last day it covers, in its zone.
    pub fn days(&self) -> (NaiveDate, NaiveDate) {
        let first = local_date(self.zone, self.start_ms);
        let last = local_date(self.zone, (self.end_ms - 1).max(self.start_ms));
        (first, last)
    }

    /// Such as "1 Oct 2026 – 31 Oct 2026" or "11 Nov 2026".
    pub fn label(&self) -> String {
        let (first, last) = self.days();
        if first == last {
            format_date(first)
        } else {
            format!("{} – {}", format_date(first), format_date(last))
        }
    }
}

/// The date at `at_ms` in `zone`.
pub fn local_date(zone: Tz, at_ms: i64) -> NaiveDate {
    DateTime::<Utc>::from_timestamp_millis(at_ms)
        .unwrap_or_default()
        .with_timezone(&zone)
        .date_naive()
}

/// When `date` starts in `zone`: its midnight, or the first moment after it
/// on a day whose clocks skip it.
pub fn midnight(zone: Tz, date: NaiveDate) -> i64 {
    schedule::resolve(zone, date.and_time(chrono::NaiveTime::MIN)).unwrap_or_else(|| {
        date.and_time(chrono::NaiveTime::MIN)
            .and_utc()
            .timestamp_millis()
    })
}

pub fn format_date(date: NaiveDate) -> String {
    date.format("%-d %b %Y").to_string()
}

/// `YYYY-MM-DD`.
pub fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(zone: Tz, text: &str) -> i64 {
        schedule::resolve(zone, schedule::parse_local_date_time(text).unwrap()).unwrap()
    }

    #[test]
    fn the_last_days_are_whole_days_before_today_in_the_zone() {
        let zone: Tz = "America/New_York".parse().unwrap();
        // Tuesday 10 March 2026, after clocks went forward on Sunday the 8th.
        let now = at(zone, "2026-03-10T09:00");
        let week = Period::resolve(PeriodKind::Last7Days, None, zone, now).unwrap();
        assert_eq!(week.start_ms, at(zone, "2026-03-03T00:00"));
        assert_eq!(week.end_ms, at(zone, "2026-03-10T00:00"));
        assert_eq!(
            week.end_ms - week.start_ms,
            (7 * 24 - 1) * crate::time::HOUR_MS,
            "a day lost to daylight saving"
        );
        assert_eq!(week.label(), "3 Mar 2026 – 9 Mar 2026");
        let month = Period::resolve(PeriodKind::Last30Days, None, zone, now).unwrap();
        assert_eq!(month.start_ms, at(zone, "2026-02-08T00:00"));
        // The same moment is another day in Tokyo.
        let tokyo: Tz = "Asia/Tokyo".parse().unwrap();
        let week = Period::resolve(PeriodKind::Last7Days, None, tokyo, now).unwrap();
        assert_eq!(week.end_ms, at(tokyo, "2026-03-10T00:00"));
        assert_eq!(week.label(), "3 Mar 2026 – 9 Mar 2026");
        let late = at(zone, "2026-03-10T23:30");
        let week = Period::resolve(PeriodKind::Last7Days, None, tokyo, late).unwrap();
        assert_eq!(week.label(), "4 Mar 2026 – 10 Mar 2026");
    }

    #[test]
    fn the_previous_month_is_the_calendar_month() {
        let zone: Tz = "Europe/London".parse().unwrap();
        let now = at(zone, "2026-01-01T00:30");
        let month = Period::resolve(PeriodKind::PreviousMonth, None, zone, now).unwrap();
        assert_eq!(month.start_ms, at(zone, "2025-12-01T00:00"));
        assert_eq!(month.end_ms, at(zone, "2026-01-01T00:00"));
        assert_eq!(month.label(), "1 Dec 2025 – 31 Dec 2025");
        // October has 31 days and an hour more in London.
        let now = at(zone, "2026-11-11T12:00");
        let month = Period::resolve(PeriodKind::PreviousMonth, None, zone, now).unwrap();
        assert_eq!(
            month.end_ms - month.start_ms,
            (31 * 24 + 1) * crate::time::HOUR_MS
        );
    }

    #[test]
    fn custom_ranges_are_checked_and_end_now_at_the_latest() {
        let zone: Tz = "UTC".parse().unwrap();
        let now = at(zone, "2026-11-11T12:00");
        let day = |text: &str| parse_date(text).unwrap();
        let range = |first: &str, last: &str| {
            Period::resolve(PeriodKind::Custom, Some((day(first), day(last))), zone, now)
        };
        let october = range("2026-10-01", "2026-10-31").unwrap();
        assert_eq!(october.start_ms, at(zone, "2026-10-01T00:00"));
        assert_eq!(october.end_ms, at(zone, "2026-11-01T00:00"));
        let today = range("2026-11-11", "2026-11-11").unwrap();
        assert_eq!(today.end_ms, now);
        assert_eq!(today.label(), "11 Nov 2026");
        assert_eq!(range("2026-11-12", "2026-11-13"), Err(PeriodError::Future));
        assert_eq!(
            range("2026-10-02", "2026-10-01"),
            Err(PeriodError::Reversed)
        );
        assert_eq!(range("2024-01-01", "2026-01-01"), Err(PeriodError::TooLong));
        assert!(range("2025-11-11", "2026-11-11").is_ok());
        assert_eq!(
            Period::resolve(PeriodKind::Custom, None, zone, now),
            Err(PeriodError::MissingDates)
        );
        assert!(parse_date("2026-02-30").is_none());
    }
}
