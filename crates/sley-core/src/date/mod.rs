//! Canonical civil-calendar arithmetic and date-token parsing (git `date.c`
//! subset): the one home for the helpers previously duplicated across
//! cli/log_cli.rs, rev/setup.rs, rev/lib.rs, and commands/am.rs.
//!
//! All functions take borrowed inputs; allocation is limited to returned
//! values. The civil math is Howard Hinnant's proleptic-Gregorian pair
//! (`days_from_civil`/`civil_from_days`), which matches git's date
//! arithmetic over the full range.

pub mod approxidate;

/// True for Gregorian leap years (divisible by 4, except centuries not
/// divisible by 400).
pub fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in `month` (1-12) of `year`; 0 when the month index is out of range.
pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days between 1970-01-01 and the given civil date (Howard Hinnant's
/// algorithm). Month/day are not range-checked: callers that need validated
/// input use [`parse_date_ymd`] or check [`days_in_month`] first.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Inverse of [`days_from_civil`]: civil `(year, month, day)` for a day count.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Parse a strict `YYYY-MM-DD` calendar date, rejecting trailing components,
/// months outside 1-12, and days beyond the month's length.
pub fn parse_date_ymd(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = days_in_month(year, month);
    if !(1..=max_day).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// Parse a strict `HH:MM:SS` clock time, rejecting trailing components and
/// out-of-range fields (`second == 60` is rejected here; leap-second-tolerant
/// callers keep their own looser parser).
pub fn parse_time_hms(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    let second = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

/// Parse a timezone token into seconds east of UTC. Only git's canonical
/// `<+|->HHMM` form is accepted: exactly five bytes, sign first, then four
/// ASCII digits, with hours at most 23 and minutes at most 59 (the bounds of
/// git's date.c `match_tz`). Returns `None` otherwise.
pub fn parse_tz_offset(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes.first(), Some(b'+' | b'-'))
        || !bytes[1..].iter().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let hours = value[1..3].parse::<i64>().ok()?;
    let minutes = value[3..5].parse::<i64>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = hours * 3_600 + minutes * 60;
    if bytes[0] == b'-' {
        Some(-offset)
    } else {
        Some(offset)
    }
}

/// Split an ISO 8601 time portion (the part after `T`) into the bare time and
/// an optional embedded timezone suffix. A trailing `Z` normalises to the
/// static `"+0000"`; a trailing `±HHMM` is borrowed from the input. Otherwise
/// the whole string is the time and any timezone arrives separately.
pub fn split_embedded_timezone(rest: &str) -> (&str, Option<&str>) {
    if let Some(time) = rest.strip_suffix('Z') {
        return (time, Some("+0000"));
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 5 {
        let tz_start = bytes.len() - 5;
        if matches!(bytes[tz_start], b'+' | b'-')
            && bytes[tz_start + 1..]
                .iter()
                .all(|byte| byte.is_ascii_digit())
        {
            return (&rest[..tz_start], Some(&rest[tz_start..]));
        }
    }
    (rest, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // Leap-day handling around century boundaries.
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 13), 0);
        assert!(!is_leap_year(1900));
        assert!(is_leap_year(2000));
    }

    #[test]
    fn ymd_and_time_validation() {
        assert_eq!(parse_date_ymd("2024-02-29"), Some((2024, 2, 29)));
        assert_eq!(parse_date_ymd("2023-02-29"), None);
        assert_eq!(parse_date_ymd("2024-13-01"), None);
        assert_eq!(parse_date_ymd("2024-01-02T00:00:00"), None);
        assert_eq!(parse_time_hms("23:59:59"), Some((23, 59, 59)));
        assert_eq!(parse_time_hms("24:00:00"), None);
        assert_eq!(parse_time_hms("12:60:00"), None);
        assert_eq!(parse_time_hms("12:00"), None);
    }

    #[test]
    fn tz_offsets() {
        assert_eq!(parse_tz_offset("+0000"), Some(0));
        assert_eq!(parse_tz_offset("-0530"), Some(-19_800));
        assert_eq!(parse_tz_offset("+2400"), None);
        assert_eq!(parse_tz_offset("+0060"), None);
        assert_eq!(parse_tz_offset("+000a"), None);
        assert_eq!(parse_tz_offset("+000"), None);
    }

    #[test]
    fn embedded_timezone_suffixes() {
        assert_eq!(
            split_embedded_timezone("00:00:01Z"),
            ("00:00:01", Some("+0000"))
        );
        assert_eq!(
            split_embedded_timezone("03:04:05+0100"),
            ("03:04:05", Some("+0100"))
        );
        assert_eq!(split_embedded_timezone("03:04:05"), ("03:04:05", None));
    }
}
