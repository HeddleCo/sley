//! A faithful port of the subset of git's `date.c` needed to canonicalise
//! `expiry-date` config values (`git config --type=expiry-date`).
//!
//! git parses an expiry-date string with `parse_expiry_date`, which special-cases
//! `never`/`false` (→ 0) and `all`/`now` (→ `TIME_MAX`) and otherwise hands the
//! string to `approxidate_careful`. That routine first tries the strict-ish
//! absolute parser (`parse_date_basic`) and, failing that, falls back to the
//! fuzzy relative/partial parser (`approxidate_str`). We port both paths so that
//! `--type=expiry-date` accepts exactly what git accepts and emits the same
//! canonical Unix timestamp.
//!
//! Timezone handling: git uses the process-local timezone (`mktime`/`localtime_r`)
//! when a date carries no explicit offset. sley's existing date code (e.g. the
//! RFC2822 parser in `am.rs` and the log `--since`/`--until` cutoff parser) does
//! pure UTC civil-date arithmetic; we follow that convention here. Under the
//! upstream test harness (which exports `TZ=UTC`) this matches git bit-for-bit.
//!
//! Several routines mirror git's C control flow line-for-line (explicit
//! `x < lo || x > hi` bound checks, nested `if isdigit(...)` guards inside a
//! separator `match`, `tm_hour % 12` assignments). Keeping that shape makes the
//! port auditable against `date.c`, so the corresponding readability lints are
//! allowed module-wide rather than rewritten away from the source.
#![allow(
    clippy::manual_range_contains,
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::assign_op_pattern
)]

/// git's broken-down time. Unset fields carry the sentinel `-1`, exactly as in
/// git's `struct tm` usage in `date.c`.
#[derive(Clone, Copy)]
struct Tm {
    sec: i64,
    min: i64,
    hour: i64,
    mday: i64,
    mon: i64,
    year: i64, // years since 1900, mirroring `tm_year`
    wday: i64,
}

impl Tm {
    fn unset() -> Self {
        Tm {
            sec: -1,
            min: -1,
            hour: -1,
            mday: -1,
            mon: -1,
            year: -1,
            wday: -1,
        }
    }
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const WEEKDAY_NAMES: [&str; 7] = [
    "Sundays",
    "Mondays",
    "Tuesdays",
    "Wednesdays",
    "Thursdays",
    "Fridays",
    "Saturdays",
];

/// (name, offset-hours-east, dst) — git's `timezone_names` table.
const TIMEZONE_NAMES: &[(&str, i64, i64)] = &[
    ("IDLW", -12, 0),
    ("NT", -11, 0),
    ("CAT", -10, 0),
    ("HST", -10, 0),
    ("YST", -9, 0),
    ("YDT", -9, 1),
    ("PST", -8, 0),
    ("PDT", -8, 1),
    ("MST", -7, 0),
    ("MDT", -7, 1),
    ("CST", -6, 0),
    ("CDT", -6, 1),
    ("EST", -5, 0),
    ("EDT", -5, 1),
    ("AST", -3, 0),
    ("ADT", -3, 1),
    ("WAT", -1, 0),
    ("GMT", 0, 0),
    ("UTC", 0, 0),
    ("Z", 0, 0),
    ("WET", 0, 0),
    ("BST", 0, 1),
    ("MET", 1, 0),
    ("MEST", 1, 1),
    ("MEZ", 1, 0),
    ("MESZ", 1, 1),
    ("CET", 1, 0),
    ("CEST", 1, 1),
    ("EET", 2, 0),
    ("EEST", 2, 1),
    ("MSK", 3, 0),
    ("MSD", 3, 1),
    ("CCT", 8, 0),
    ("JST", 9, 0),
    ("EAST", 10, 0),
    ("EADT", 10, 1),
    ("GST", 10, 0),
    ("NZT", 12, 0),
    ("NZST", 12, 0),
    ("NZDT", 12, 1),
    ("IDLE", 12, 0),
];

const SPECIAL_NAMES: [&str; 8] = [
    "yesterday", "noon", "midnight", "tea", "PM", "AM", "never", "now",
];

const NUMBER_NAMES: [&str; 11] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];

/// (type-name, seconds-per-unit) — git's `typelen` table.
const TYPELEN: [(&str, i64); 5] = [
    ("seconds", 1),
    ("minutes", 60),
    ("hours", 60 * 60),
    ("days", 24 * 60 * 60),
    ("weeks", 7 * 24 * 60 * 60),
];

const TIMESTAMP_MAX: i64 = (((2100 - 1970) * 365 + 32) * 24 * 60 * 60) - 1;

fn is_ascii_digit_byte(b: u8) -> bool {
    b.is_ascii_digit()
}

fn is_ascii_alpha_byte(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Inverse of `days_from_civil`: civil (year, month, day) from a day count.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year + i64::from(month <= 2);
    (year, month, day)
}

/// git's `tm_to_time_t`: like `mktime` without normalisation, treating the tm as
/// UTC. Returns `None` (git's `-1`) on out-of-range or unset time fields.
fn tm_to_time_t(tm: &Tm) -> Option<i64> {
    const MDAYS: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let year = tm.year - 70;
    let month = tm.mon;
    let mut day = tm.mday;
    if year < 0 || year > 129 {
        return None;
    }
    if month < 0 || month > 11 {
        return None;
    }
    if month < 2 || (year + 2) % 4 != 0 {
        day -= 1;
    }
    if tm.hour < 0 || tm.min < 0 || tm.sec < 0 {
        return None;
    }
    Some(
        (year * 365 + (year + 1) / 4 + MDAYS[month as usize] + day) * 24 * 60 * 60
            + tm.hour * 60 * 60
            + tm.min * 60
            + tm.sec,
    )
}

/// Break a UTC Unix time into a `Tm` (git's `localtime_r`/`gmtime_r` under UTC).
fn time_t_to_tm(time: i64) -> Tm {
    let days = time.div_euclid(86_400);
    let secs_of_day = time.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // Day of week: 1970-01-01 was a Thursday (wday 4).
    let wday = (days.rem_euclid(7) + 4).rem_euclid(7);
    Tm {
        sec: secs_of_day % 60,
        min: (secs_of_day / 60) % 60,
        hour: secs_of_day / 3600,
        mday: day,
        mon: month - 1,
        year: year - 1900,
        wday,
    }
}

/// Current UTC time as a Unix timestamp.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

/// git's `match_string`: case-insensitive prefix match; returns the number of
/// matched leading chars, or 0 on a non-alphanumeric mismatch.
fn match_string(date: &[u8], pat: &str) -> usize {
    let pat = pat.as_bytes();
    let mut i = 0;
    while i < date.len() {
        let d = date[i];
        let p = if i < pat.len() { pat[i] } else { 0 };
        if p != 0 && d.eq_ignore_ascii_case(&p) {
            i += 1;
            continue;
        }
        if !d.is_ascii_alphanumeric() {
            break;
        }
        // Mismatch on an alphanumeric char that doesn't continue the pattern.
        if i >= pat.len() {
            break;
        }
        return 0;
    }
    i
}

fn skip_alpha(date: &[u8]) -> usize {
    let mut i = 1;
    while i < date.len() && is_ascii_alpha_byte(date[i]) {
        i += 1;
    }
    i
}

/// git's `set_time`: validate and store H:M:S. Returns Ok on success.
fn set_time(hour: i64, minute: i64, second: i64, tm: &mut Tm) -> Result<(), ()> {
    if (0..=24).contains(&hour) && (0..60).contains(&minute) && (0..=60).contains(&second) {
        tm.hour = hour;
        tm.min = minute;
        tm.sec = second;
        Ok(())
    } else {
        Err(())
    }
}

/// git's `set_date` for the absolute path. `now_tm`/`now` enable the future-date
/// refusal heuristic; pass `None` to disable it (the `num > 70` branch).
fn set_date(
    year: i64,
    month: i64,
    day: i64,
    now_tm: Option<&Tm>,
    now: i64,
    tm: &mut Tm,
) -> Result<bool, ()> {
    // Returns Ok(true) when set_date returned git's `1` (year unknown, no now_tm),
    // Ok(false) on success (git's `0`), Err on git's `-1`.
    if (1..13).contains(&month) && (1..32).contains(&day) {
        let mut r = *tm;
        let use_check = now_tm.is_some();
        r.mon = month - 1;
        r.mday = day;
        if year == -1 {
            match now_tm {
                None => return Ok(true),
                Some(now_tm) => r.year = now_tm.year,
            }
        } else if (1970..2100).contains(&year) {
            r.year = year - 1900;
        } else if (71..100).contains(&year) {
            r.year = year;
        } else if year < 38 {
            r.year = year + 100;
        } else {
            return Err(());
        }
        if !use_check {
            tm.mon = r.mon;
            tm.mday = r.mday;
            if year != -1 {
                tm.year = r.year;
            }
            return Ok(false);
        }
        let specified = tm_to_time_t(&r);
        if let Some(specified) = specified {
            if now + 10 * 24 * 3600 < specified {
                return Err(());
            }
        }
        tm.mon = r.mon;
        tm.mday = r.mday;
        if year != -1 {
            tm.year = r.year;
        }
        Ok(false)
    } else {
        Err(())
    }
}

fn is_date_known(tm: &Tm) -> bool {
    tm.year != -1 && tm.mon != -1 && tm.mday != -1
}

/// git's `match_multi_number`. `date`/`end` are byte offsets into the full
/// string; returns the number of consumed bytes (0 = no match).
fn match_multi_number(num: i64, c: u8, full: &[u8], start_off: usize, tm: &mut Tm, now: i64) -> usize {
    // `start_off` is the index of the separator `c` in `full`; git's `end`
    // pointer sits there and num2 is parsed from end+1.
    let (num2, mut cursor) = parse_long(full, start_off + 1);
    let mut num3: i64 = -1;
    if cursor < full.len() && full[cursor] == c && cursor + 1 < full.len() && is_ascii_digit_byte(full[cursor + 1]) {
        let (n3, c3) = parse_long(full, cursor + 1);
        num3 = n3;
        cursor = c3;
    }

    let date_start = {
        // git computes `end - date`; `date` is the start of the first number.
        // We track that as the position right before the first digit run. The
        // caller passes the number's own start via `num_start` implicitly: we
        // recompute by scanning back is awkward, so the caller hands `full`
        // already sliced from the first digit. Here `0` is the first digit.
        0usize
    };
    let _ = date_start;

    match c {
        b':' => {
            let num3v = if num3 < 0 { 0 } else { num3 };
            if set_time(num, num2, num3v, tm).is_ok() {
                // Optional fractional seconds after %H:%M:%S when a date is known.
                if cursor < full.len()
                    && full[cursor] == b'.'
                    && cursor + 1 < full.len()
                    && is_ascii_digit_byte(full[cursor + 1])
                    && is_date_known(tm)
                {
                    let (_n, c4) = parse_long(full, cursor + 1);
                    cursor = c4;
                }
                cursor
            } else {
                0
            }
        }
        b'-' | b'/' | b'.' => {
            let now = if now == 0 { now_unix() } else { now };
            let now_tm = time_t_to_tm(now);
            let refuse_future = Some(&now_tm);

            if num > 70 {
                if set_date(num, num2, num3, None, now, tm).is_ok() {
                    return cursor;
                }
                if set_date(num, num3, num2, None, now, tm).is_ok() {
                    return cursor;
                }
            }
            if c != b'.' && set_date(num3, num, num2, refuse_future, now, tm).is_ok() {
                return cursor;
            }
            if set_date(num3, num2, num, refuse_future, now, tm).is_ok() {
                return cursor;
            }
            if c == b'.' && set_date(num3, num, num2, refuse_future, now, tm).is_ok() {
                return cursor;
            }
            0
        }
        _ => cursor,
    }
}

/// Parse a base-10 integer at `off`, returning (value, next-offset).
fn parse_long(s: &[u8], off: usize) -> (i64, usize) {
    let mut i = off;
    let neg = i < s.len() && s[i] == b'-';
    if neg {
        i += 1;
    }
    let mut val: i64 = 0;
    while i < s.len() && is_ascii_digit_byte(s[i]) {
        val = val.saturating_mul(10).saturating_add((s[i] - b'0') as i64);
        i += 1;
    }
    (if neg { -val } else { val }, i)
}

/// Parse an unsigned base-10 integer at `off`, returning (value, next-offset).
fn parse_uint(s: &[u8], off: usize) -> (i64, usize) {
    let mut i = off;
    let mut val: i64 = 0;
    while i < s.len() && is_ascii_digit_byte(s[i]) {
        val = val.saturating_mul(10).saturating_add((s[i] - b'0') as i64);
        i += 1;
    }
    (val, i)
}

fn nodate(tm: &Tm) -> bool {
    (tm.year & tm.mon & tm.mday & tm.hour & tm.min & tm.sec) < 0
}

fn maybeiso8601(tm: &Tm) -> bool {
    tm.hour == -1 && tm.min == 0 && tm.sec == 0
}

/// git's `match_alpha`. `date` is the remaining byte slice; returns bytes consumed.
fn match_alpha(date: &[u8], tm: &mut Tm, offset: &mut i64) -> usize {
    for (i, name) in MONTH_NAMES.iter().enumerate() {
        let m = match_string(date, name);
        if m >= 3 {
            tm.mon = i as i64;
            return m;
        }
    }
    for (i, name) in WEEKDAY_NAMES.iter().enumerate() {
        let m = match_string(date, name);
        if m >= 3 {
            tm.wday = i as i64;
            return m;
        }
    }
    for (name, off, dst) in TIMEZONE_NAMES.iter() {
        let m = match_string(date, name);
        if m >= 3 || m == name.len() {
            let off = off + dst;
            if *offset == -1 {
                *offset = 60 * off;
            }
            return m;
        }
    }
    if match_string(date, "PM") == 2 {
        tm.hour = (tm.hour % 12) + 12;
        return 2;
    }
    if match_string(date, "AM") == 2 {
        tm.hour = tm.hour % 12;
        return 2;
    }
    if date.first() == Some(&b'T') && date.len() > 1 && is_ascii_digit_byte(date[1]) && tm.hour == -1 {
        tm.min = 0;
        tm.sec = 0;
        return 1;
    }
    skip_alpha(date)
}

/// git's `match_digit`. `date` is the remaining byte slice; returns bytes consumed.
fn match_digit(date: &[u8], tm: &mut Tm, offset: &mut i64, tm_gmt: &mut bool) -> usize {
    let (num, end) = parse_uint(date, 0);

    // Seconds since 1970 for any number with more than 8 digits.
    if num >= 100_000_000 && nodate(tm) {
        *tm = time_t_to_tm(num);
        *tm_gmt = true;
        return end;
    }

    // num[-.:/]num[same]num
    if end < date.len() {
        match date[end] {
            b':' | b'.' | b'/' | b'-' => {
                if end + 1 < date.len() && is_ascii_digit_byte(date[end + 1]) {
                    let m = match_multi_number(num, date[end], date, end, tm, 0);
                    if m != 0 {
                        return m;
                    }
                }
            }
            _ => {}
        }
    }

    // Number of consecutive digits from the start.
    let mut n = 0usize;
    while n < date.len() && is_ascii_digit_byte(date[n]) {
        n += 1;
    }

    // 8-digit YYYYmmDD or 6-digit HHMMSS.
    if n == 8 || n == 6 {
        let num1 = num / 10000;
        let num2 = (num % 10000) / 100;
        let num3 = num % 100;
        let mut cursor = end;
        if n == 8 {
            let _ = set_date(num1, num2, num3, None, now_unix(), tm);
        } else if n == 6 && set_time(num1, num2, num3, tm).is_ok() {
            if cursor < date.len() && date[cursor] == b'.' && cursor + 1 < date.len() && is_ascii_digit_byte(date[cursor + 1]) {
                let (_v, c) = parse_uint(date, cursor + 1);
                cursor = c;
            }
        }
        return cursor;
    }

    // Reduced-precision ISO-8601 time: HHMM or HH.
    if maybeiso8601(tm) {
        let mut num1 = num;
        let mut num2 = 0;
        if n == 4 {
            num1 = num / 100;
            num2 = num % 100;
        }
        if (n == 4 || n == 2) && !nodate(tm) && set_time(num1, num2, 0, tm).is_ok() {
            return n;
        }
        tm.min = -1;
        tm.sec = -1;
    }

    // Four-digit year or timezone.
    if n == 4 {
        if num <= 1400 && *offset == -1 {
            let minutes = num % 100;
            let hours = num / 100;
            *offset = hours * 60 + minutes;
        } else if num > 1900 && num < 2100 {
            tm.year = num - 1900;
        }
        return n;
    }

    if n > 2 {
        return n;
    }

    // Day-of-month precedence for 1..31.
    if num > 0 && num < 32 && tm.mday < 0 {
        tm.mday = num;
        return n;
    }

    // Two-digit year.
    if n == 2 && tm.year < 0 {
        if num < 10 && tm.mday >= 0 {
            tm.year = num + 100;
            return n;
        }
        if num >= 70 {
            tm.year = num;
            return n;
        }
    }

    if num > 0 && num < 13 && tm.mon < 0 {
        tm.mon = num - 1;
    }

    n
}

/// git's `match_tz`. `date` starts at the sign; returns bytes consumed.
fn match_tz(date: &[u8], offp: &mut i64) -> usize {
    let (mut hour, end) = parse_uint(date, 1);
    let n = end - 1;
    let mut min = 0i64;
    let mut cursor = end;

    if n == 4 {
        min = hour % 100;
        hour /= 100;
    } else if n != 2 {
        min = 99;
    } else if cursor < date.len() && date[cursor] == b':' {
        let (m, c) = parse_uint(date, cursor + 1);
        min = m;
        if c - 1 != 5 {
            min = 99;
        }
        cursor = c;
    }

    if min < 60 && hour < 24 {
        let mut off = hour * 60 + min;
        if date[0] == b'-' {
            off = -off;
        }
        *offp = off;
    }
    cursor
}

/// git's `match_object_header_date`: parse "<stamp> +HHMM" only.
fn match_object_header_date(date: &[u8]) -> Option<(i64, i64)> {
    if date.is_empty() || !is_ascii_digit_byte(date[0]) {
        return None;
    }
    let (stamp, end) = parse_uint(date, 0);
    if end >= date.len() || date[end] != b' ' || stamp == i64::MAX {
        return None;
    }
    if end + 1 >= date.len() || (date[end + 1] != b'+' && date[end + 1] != b'-') {
        return None;
    }
    let sign_idx = end + 1;
    let (ofs_raw, ofs_end) = parse_uint(date, sign_idx + 1);
    // git requires exactly 4 digits and end at NUL or newline.
    if ofs_end - (sign_idx + 1) != 4 {
        return None;
    }
    if ofs_end != date.len() && date[ofs_end] != b'\n' {
        return None;
    }
    let mut ofs = (ofs_raw / 100) * 60 + (ofs_raw % 100);
    if date[sign_idx] == b'-' {
        ofs = -ofs;
    }
    Some((stamp, ofs))
}

/// git's `parse_date_basic`: the strict-ish absolute parser. Returns the Unix
/// timestamp on success, `None` on failure.
fn parse_date_basic(date: &[u8]) -> Option<i64> {
    parse_date_basic_full(date).map(|(timestamp, _offset)| timestamp)
}

/// git's `parse_date_basic`, returning both the absolute UTC `timestamp` and the
/// parsed timezone `offset` in minutes (git's `*offset`). Mirrors git's
/// `parse_date` shape, which a commit/tag author/committer date needs: the
/// object line stores `<utc_seconds> <+HHMM>` where the seconds are timezone-
/// normalised and the offset is carried alongside for display.
fn parse_date_basic_full(date: &[u8]) -> Option<(i64, i64)> {
    let mut tm = Tm::unset();
    let mut tm_gmt = false;
    let mut offset: i64 = -1;

    if date.first() == Some(&b'@') {
        if let Some((stamp, ofs)) = match_object_header_date(&date[1..]) {
            return Some((stamp, ofs));
        }
    }

    let mut i = 0usize;
    while i < date.len() {
        let c = date[i];
        if c == 0 || c == b'\n' {
            break;
        }
        let rest = &date[i..];
        let mut m = 0usize;
        if is_ascii_alpha_byte(c) {
            m = match_alpha(rest, &mut tm, &mut offset);
        } else if is_ascii_digit_byte(c) {
            m = match_digit(rest, &mut tm, &mut offset, &mut tm_gmt);
        } else if (c == b'-' || c == b'+') && i + 1 < date.len() && is_ascii_digit_byte(date[i + 1]) {
            m = match_tz(rest, &mut offset);
        }
        if m == 0 {
            m = 1;
        }
        i += m;
    }

    let mut timestamp = tm_to_time_t(&tm)?;

    if offset == -1 {
        // git falls back to mktime() to derive the local offset. Under our UTC
        // convention the local offset is 0, so the timestamp stands as-is.
        offset = 0;
    }

    if !tm_gmt {
        if offset > 0 && offset * 60 > timestamp {
            return None;
        }
        if offset < 0 && -offset * 60 > TIMESTAMP_MAX - timestamp {
            return None;
        }
        timestamp -= offset * 60;
    }

    Some((timestamp, offset))
}

/// git's `update_tm`: fill in unset date fields from `now`, then apply a relative
/// `sec` offset. Uses UTC civil arithmetic in place of mktime/localtime_r.
fn update_tm(tm: &mut Tm, now: &Tm, sec: i64) -> i64 {
    if tm.mday < 0 {
        tm.mday = now.mday;
    }
    if tm.mon < 0 {
        tm.mon = now.mon;
    }
    if tm.year < 0 {
        tm.year = now.year;
        if tm.mon > now.mon {
            tm.year -= 1;
        }
    }
    let days = days_from_civil(tm.year + 1900, tm.mon + 1, tm.mday);
    let base = days * 86_400 + tm.hour.max(0) * 3600 + tm.min.max(0) * 60 + tm.sec.max(0);
    let n = base - sec;
    *tm = time_t_to_tm(n);
    n
}

/// git's `pending_number`: assume a trailing number is a day, else month, else year.
fn pending_number(tm: &mut Tm, num: &mut i64) {
    let number = *num;
    if number != 0 {
        *num = 0;
        if tm.mday < 0 && number < 32 {
            tm.mday = number;
        } else if tm.mon < 0 && number < 13 {
            tm.mon = number - 1;
        } else if tm.year < 0 {
            if number > 1969 && number < 2100 {
                tm.year = number - 1900;
            } else if number > 69 && number < 100 {
                tm.year = number;
            } else if number < 38 {
                tm.year = 100 + number;
            }
        }
    }
}

/// git's specials dispatch (`date_yesterday`, `date_noon`, …).
fn apply_special(name: &str, tm: &mut Tm, now: &Tm, num: &mut i64) {
    let date_time = |tm: &mut Tm, now: &Tm, hour: i64| {
        if tm.hour < hour {
            update_tm(tm, now, 24 * 60 * 60);
        }
        tm.hour = hour;
        tm.min = 0;
        tm.sec = 0;
    };
    match name {
        "yesterday" => {
            *num = 0;
            update_tm(tm, now, 24 * 60 * 60);
        }
        "noon" => {
            pending_number(tm, num);
            date_time(tm, now, 12);
        }
        "midnight" => {
            pending_number(tm, num);
            date_time(tm, now, 0);
        }
        "tea" => {
            pending_number(tm, num);
            date_time(tm, now, 17);
        }
        "PM" => {
            let n = *num;
            *num = 0;
            let mut hour = tm.hour;
            if n != 0 {
                hour = n;
                tm.min = 0;
                tm.sec = 0;
            }
            tm.hour = (hour % 12) + 12;
        }
        "AM" => {
            let n = *num;
            *num = 0;
            let mut hour = tm.hour;
            if n != 0 {
                hour = n;
                tm.min = 0;
                tm.sec = 0;
            }
            tm.hour = hour % 12;
        }
        "never" => {
            *tm = time_t_to_tm(0);
            *num = 0;
        }
        "now" => {
            *num = 0;
            update_tm(tm, now, 0);
        }
        _ => {}
    }
}

/// git's `approxidate_alpha`. Returns bytes consumed.
fn approxidate_alpha(date: &[u8], tm: &mut Tm, now: &Tm, num: &mut i64, touched: &mut bool) -> usize {
    let mut end = 1usize;
    while end < date.len() && is_ascii_alpha_byte(date[end]) {
        end += 1;
    }

    for (i, name) in MONTH_NAMES.iter().enumerate() {
        if match_string(date, name) >= 3 {
            tm.mon = i as i64;
            *touched = true;
            return end;
        }
    }

    for name in SPECIAL_NAMES.iter() {
        if match_string(date, name) == name.len() {
            apply_special(name, tm, now, num);
            *touched = true;
            return end;
        }
    }

    if *num == 0 {
        for (i, name) in NUMBER_NAMES.iter().enumerate().skip(1) {
            if match_string(date, name) == name.len() {
                *num = i as i64;
                *touched = true;
                return end;
            }
        }
        if match_string(date, "last") == 4 {
            *num = 1;
            *touched = true;
        }
        return end;
    }

    for (name, length) in TYPELEN.iter() {
        let m = match_string(date, name);
        if m >= name.len() - 1 {
            update_tm(tm, now, length * *num);
            *num = 0;
            *touched = true;
            return end;
        }
    }

    for (i, name) in WEEKDAY_NAMES.iter().enumerate() {
        if match_string(date, name) >= 3 {
            let mut n = *num - 1;
            *num = 0;
            let mut diff = tm.wday - i as i64;
            if diff <= 0 {
                n += 1;
            }
            diff += 7 * n;
            update_tm(tm, now, diff * 24 * 60 * 60);
            *touched = true;
            return end;
        }
    }

    if match_string(date, "months") >= 5 {
        update_tm(tm, now, 0);
        let mut n = tm.mon - *num;
        *num = 0;
        while n < 0 {
            n += 12;
            tm.year -= 1;
        }
        tm.mon = n;
        *touched = true;
        return end;
    }

    if match_string(date, "years") >= 4 {
        update_tm(tm, now, 0);
        tm.year -= *num;
        *num = 0;
        *touched = true;
        return end;
    }

    end
}

/// git's `approxidate_digit`. `date` is the remaining slice; returns bytes consumed.
fn approxidate_digit(date: &[u8], tm: &mut Tm, num: &mut i64, now: i64) -> usize {
    let (number, end) = parse_uint(date, 0);

    if end < date.len() {
        match date[end] {
            b':' | b'.' | b'/' | b'-' => {
                if end + 1 < date.len() && is_ascii_digit_byte(date[end + 1]) {
                    let m = match_multi_number(number, date[end], date, end, tm, now);
                    if m != 0 {
                        return m;
                    }
                }
            }
            _ => {}
        }
    }

    // Accept zero-padding only for small numbers ("Dec 02", never "Dec 0002").
    if date.first() != Some(&b'0') || end <= 2 {
        *num = number;
    }
    end
}

/// git's `approxidate_str`: the fuzzy relative/partial parser.
fn approxidate_str(date: &[u8], now: i64, error_ret: &mut bool) -> i64 {
    let mut number: i64 = 0;
    let mut touched = false;
    let now_tm = time_t_to_tm(now);
    let mut tm = now_tm;
    tm.year = -1;
    tm.mon = -1;
    tm.mday = -1;

    let mut i = 0usize;
    while i < date.len() {
        let c = date[i];
        if c == 0 {
            break;
        }
        if is_ascii_digit_byte(c) {
            pending_number(&mut tm, &mut number);
            let consumed = approxidate_digit(&date[i..], &mut tm, &mut number, now);
            touched = true;
            i += consumed.max(1);
            continue;
        }
        if is_ascii_alpha_byte(c) {
            let consumed = approxidate_alpha(&date[i..], &mut tm, &now_tm, &mut number, &mut touched);
            i += consumed.max(1);
            continue;
        }
        i += 1;
    }
    pending_number(&mut tm, &mut number);
    if !touched {
        *error_ret = true;
    }
    update_tm(&mut tm, &now_tm, 0)
}

/// git's `approxidate_careful`: try the absolute parser, fall back to fuzzy.
/// Returns `(timestamp, had_error)`.
fn approxidate_careful(date: &[u8]) -> (i64, bool) {
    if let Some(ts) = parse_date_basic(date) {
        return (ts, false);
    }
    let mut error_ret = false;
    let ts = approxidate_str(date, now_unix(), &mut error_ret);
    (ts, error_ret)
}

/// Parse a commit/tag author or committer date the way git's `parse_date` does
/// for `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` and `--date=`, returning the
/// absolute UTC `seconds` and the canonical `+HHMM` timezone string git would
/// store on the object line.
///
/// The fast path is the already-canonical raw form (`<seconds> +HHMM`, optional
/// leading `@`), which round-trips without going through the civil-date parser;
/// otherwise git's `parse_date_basic` handles the human/ISO/RFC formats the test
/// suite (and users) feed in (`2005-04-07T22:13:13`, `Thu, 7 Apr 2005 ...`, etc).
pub(crate) fn parse_commit_date(date: &str) -> Option<(i64, String)> {
    let trimmed = date.trim();
    // Raw form `<seconds> <+HHMM>` (optionally `@`-prefixed) — preserve the tz
    // string verbatim so a caller-supplied offset survives untouched.
    if let Some((raw_secs, raw_tz)) = split_raw_seconds_tz(trimmed) {
        return Some((raw_secs, raw_tz));
    }
    let (seconds, offset) = parse_date_basic_full(trimmed.as_bytes())?;
    Some((seconds, format_tz_offset(offset)))
}

/// Recognise git's already-canonical `<seconds> <+HHMM>` raw date (with an
/// optional leading `@`), returning the integer seconds and the tz string as
/// written. Anything else returns `None` so the full parser runs.
fn split_raw_seconds_tz(date: &str) -> Option<(i64, String)> {
    let mut parts = date.split_whitespace();
    let secs = parts.next()?;
    let tz = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let secs = secs.strip_prefix('@').unwrap_or(secs);
    let seconds = secs.parse::<i64>().ok()?;
    let tz_bytes = tz.as_bytes();
    if tz_bytes.len() == 5
        && matches!(tz_bytes[0], b'+' | b'-')
        && tz_bytes[1..].iter().all(u8::is_ascii_digit)
    {
        Some((seconds, tz.to_string()))
    } else {
        None
    }
}

/// Render a tz offset in minutes as git's `+HHMM`/`-HHMM`.
fn format_tz_offset(offset_minutes: i64) -> String {
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let abs = offset_minutes.abs();
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

/// git's `parse_expiry_date`: the public entry point. Returns `Some(timestamp)`
/// when the value canonicalises, `None` when it does not (git's `errors != 0`).
pub(crate) fn parse_expiry_date(date: &str) -> Option<i64> {
    if date == "never" || date == "false" {
        return Some(0);
    }
    if date == "all" || date == "now" {
        // git's TIME_MAX; expiry-date is unsigned, so this is u64::MAX.
        return Some(u64::MAX as i64);
    }
    let (ts, had_error) = approxidate_careful(date.as_bytes());
    if had_error { None } else { Some(ts) }
}

/// Render a parsed expiry-date timestamp the way git prints it. git stores the
/// result in an unsigned `timestamp_t` and prints it with `%"PRItime"`, so the
/// "now"/"all" sentinel renders as `u64::MAX`.
pub(crate) fn format_expiry_date(date: &str) -> Option<String> {
    parse_expiry_date(date).map(|ts| {
        if ts == i64::MIN {
            // unreachable in practice; keep total.
            "0".to_string()
        } else if date == "all" || date == "now" {
            u64::MAX.to_string()
        } else {
            // Negative timestamps are not expected from expiry-date; clamp.
            (ts as i128).to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_asctime_form() {
        // Under the UTC convention this matches git's `1275666415`.
        assert_eq!(
            parse_expiry_date("Fri Jun 4 15:46:55 2010"),
            Some(1_275_666_415)
        );
    }

    #[test]
    fn slash_form_with_pm() {
        assert_eq!(
            parse_expiry_date("2017/11/11 11:11:11PM"),
            Some(1_510_441_871)
        );
    }

    #[test]
    fn slash_form_space_pm() {
        assert_eq!(
            parse_expiry_date("2017/11/10 09:08:07 PM"),
            Some(1_510_348_087)
        );
    }

    #[test]
    fn never_and_now() {
        assert_eq!(parse_expiry_date("never"), Some(0));
        assert_eq!(parse_expiry_date("false"), Some(0));
        assert_eq!(parse_expiry_date("now"), Some(u64::MAX as i64));
        assert_eq!(format_expiry_date("now").as_deref(), Some("18446744073709551615"));
    }

    #[test]
    fn relative_values_parse() {
        // `1M` and `10` are relative; we only require that they parse (git's
        // list --type=expiry-date shows them, value depends on "now").
        assert!(parse_expiry_date("1M").is_some());
        assert!(parse_expiry_date("10").is_some());
    }

    #[test]
    fn rejects_non_dates() {
        assert!(parse_expiry_date("abc").is_none());
        assert!(parse_expiry_date("True").is_none());
        assert!(parse_expiry_date("red").is_none());
        assert!(parse_expiry_date("Blue").is_none());
        assert!(parse_expiry_date("~/dir").is_none());
        assert!(parse_expiry_date(":(optional)no-such-path").is_none());
    }
}
