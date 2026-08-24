//! Reflog expiry/count parsing and reference-name resolution.

use sley::{GitError, ObjectFormat, Result};

use crate::sley_core::date;
use crate::sley_refs::{FileRefStore, branch_ref_name};
use crate::sley_rev;

pub(crate) fn parse_reflog_expire_time(value: &str, option: &str) -> Result<i64> {
    // git's `parse_expiry_date`: "never"/"false" never expire; "all"/"now" expire
    // everything (TIME_MAX — by definition a reflog records only the past, so
    // "now" means "drop it all").
    match value {
        "all" | "now" => return Ok(i64::MAX),
        "never" | "false" => return Ok(i64::MIN),
        _ => {}
    }
    // Try the strict explicit-timestamp parser first; fall back to git's fuzzy
    // approxidate so relative forms ("2.weeks.ago", "yesterday", ...) work.
    if let Some(ts) = parse_reflog_expire_date(value) {
        return Ok(ts);
    }
    if let Some(ts) = crate::commands::approxidate::parse_approxidate(value) {
        return Ok(ts);
    }
    eprintln!("fatal: invalid timestamp '{value}' given to '{option}'");
    Err(GitError::Exit(128))
}

pub(crate) fn parse_reflog_expire_date(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let first = parts.next()?;
    if let Some(timestamp) = first.strip_prefix('@') {
        let timezone = parts.next()?;
        if parts.next().is_some() || date::parse_tz_offset(timezone).is_none() {
            return None;
        }
        return timestamp.parse::<i64>().ok();
    }
    let (date_str, time) = if let Some((date, time)) = first.split_once('T') {
        (date, time)
    } else {
        (first, parts.next()?)
    };
    let timezone = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let (year, month, day) = date::parse_date_ymd(date_str)?;
    let (hour, minute, second) = date::parse_time_hms(time)?;
    let timezone_offset = date::parse_tz_offset(timezone)?;
    Some(
        date::days_from_civil(year, month, day)
            .saturating_mul(86_400)
            .saturating_add(i64::from(hour * 3_600 + minute * 60 + second))
            .saturating_sub(timezone_offset),
    )
}

pub(crate) fn parse_reflog_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(usize::MAX);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

pub(crate) fn parse_reflog_skip_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(0);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

pub(crate) fn parse_reflog_min_parent_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(0);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

pub(crate) fn parse_reflog_max_parent_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(usize::MAX);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

pub(crate) fn parse_reflog_integer(value: &str) -> Result<i128> {
    value
        .parse::<i128>()
        .map_err(|_| reflog_invalid_integer_error(value))
}

pub(crate) fn reflog_invalid_integer_error(value: &str) -> GitError {
    eprintln!("fatal: '{value}': not an integer");
    GitError::Exit(1)
}

pub(crate) fn reflog_reference_name(
    store: &FileRefStore,
    git_dir: &std::path::Path,
    format: ObjectFormat,
    value: Option<&str>,
) -> Result<String> {
    let Some(value) = value else {
        return Ok("HEAD".to_string());
    };
    if value == "HEAD" || value.starts_with("refs/") {
        return Ok(value.to_string());
    }
    if let Ok(Some(refname)) = sley_rev::resolve_revision_symbolic_full_name(git_dir, format, value)
    {
        return Ok(refname);
    }
    if store.read_ref(&format!("refs/{value}"))?.is_some() {
        return Ok(format!("refs/{value}"));
    }
    branch_ref_name(value)
}
