//! Durations, times and days as the app writes them.

use chrono::{DateTime, Datelike, Local};

/// `45s`, `1m 10s`, `12m`, `5h 32m`, `2d 4h`: two units at most, the second dropped when
/// it is zero or too small to matter.
pub fn duration(secs: i64) -> String {
    let d = secs.max(0);
    let (big, small, units) = match d {
        0..60 => return format!("{d}s"),
        60..600 => (d / 60, d % 60, ("m", "s")),
        600..3600 => return format!("{}m", d / 60),
        3600..86_400 => (d / 3600, d % 3600 / 60, ("h", "m")),
        _ => (d / 86_400, d % 86_400 / 3600, ("d", "h")),
    };
    if small == 0 {
        format!("{big}{}", units.0)
    } else {
        format!("{big}{} {small}{}", units.0, units.1)
    }
}

/// How long ago, in its largest unit: `45s`, `12m`, `5h`, `3d`. For "checked … ago",
/// where a second unit would only flicker.
pub fn ago(secs: i64) -> String {
    let d = secs.max(0);
    match d {
        0..60 => format!("{d}s"),
        60..3600 => format!("{}m", d / 60),
        3600..86_400 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86_400),
    }
}

/// `Today`, `Yesterday`, `Thu 15 Oct`, or `Thu 15 Oct 2025` in another year.
pub fn day(at: DateTime<Local>, now: DateTime<Local>) -> String {
    let (d, today) = (at.date_naive(), now.date_naive());
    if d == today {
        "Today".to_owned()
    } else if today.pred_opt() == Some(d) {
        "Yesterday".to_owned()
    } else if d.year() == today.year() {
        at.format("%a %-d %b").to_string()
    } else {
        at.format("%a %-d %b %Y").to_string()
    }
}

/// `18:31` when `at` falls on the same day as `on`, else `Thu 15 Oct 18:31`.
pub fn time_on(at: DateTime<Local>, on: DateTime<Local>) -> String {
    if at.date_naive() == on.date_naive() {
        at.format("%H:%M").to_string()
    } else {
        format!("{} {}", day(at, on), at.format("%H:%M"))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    #[test]
    fn durations_keep_two_units() {
        let cases = [
            (-3, "0s"),
            (45, "45s"),
            (70, "1m 10s"),
            (120, "2m"),
            (750, "12m"),
            (5 * 3600 + 32 * 60 + 9, "5h 32m"),
            (3600, "1h"),
            (2 * 86_400 + 4 * 3600, "2d 4h"),
            (153 * 86_400, "153d"),
        ];
        for (secs, text) in cases {
            assert_eq!(duration(secs), text, "{secs}");
        }
        assert_eq!(
            [9, 130, 5 * 3600 + 59, 3 * 86_400].map(ago),
            ["9s", "2m", "5h", "3d"]
        );
    }

    #[test]
    fn days_read_relative_to_now() {
        let now = Local.with_ymd_and_hms(2026, 10, 19, 9, 0, 0).unwrap();
        assert_eq!(day(now - Duration::hours(2), now), "Today");
        assert_eq!(day(now - Duration::hours(10), now), "Yesterday");
        assert_eq!(day(now - Duration::days(4), now), "Thu 15 Oct");
        assert_eq!(day(now - Duration::days(365), now), "Sun 19 Oct 2025");
        assert_eq!(time_on(now - Duration::minutes(5), now), "08:55");
        assert_eq!(time_on(now - Duration::days(4), now), "Thu 15 Oct 09:00");
    }
}
