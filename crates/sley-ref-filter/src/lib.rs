//! Shared ref-filter formatting primitives.
//!
//! Git reuses the same identity/date/refname formatting language across
//! `for-each-ref`, `branch`, `tag`, `log`, `show`, `stash`, and status output.
//! This crate owns those semantic primitives so the CLI can remain an entry
//! point instead of a home for every command's formatting state.

use sley_core::{GitError, ObjectId, Result};
use std::io::Write;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForEachRefFormat {
    segments: Vec<ForEachRefFormatSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForEachRefFormatSegment {
    Literal(Vec<u8>),
    Atom(String),
}

impl ForEachRefFormat {
    pub fn parse(format_spec: &str) -> Result<Self> {
        let mut segments = Vec::new();
        let mut cursor = 0;
        while let Some(start) = format_spec[cursor..].find('%') {
            let start = cursor + start;
            push_for_each_ref_literal(
                &mut segments,
                format_spec.as_bytes()[cursor..start].to_vec(),
            );
            let bytes = format_spec.as_bytes();
            match bytes.get(start + 1).copied() {
                Some(b'%') => {
                    push_for_each_ref_literal(&mut segments, b"%".to_vec());
                    cursor = start + 2;
                }
                Some(b'(') => {
                    let Some(end) = format_spec[start + 2..].find(')') else {
                        return Err(GitError::Command(
                            "unterminated for-each-ref format placeholder".into(),
                        ));
                    };
                    let end = start + 2 + end;
                    segments.push(ForEachRefFormatSegment::Atom(
                        format_spec[start + 2..end].to_string(),
                    ));
                    cursor = end + 1;
                }
                Some(_) => {
                    if let Some(byte) = for_each_ref_hex_escape(bytes.get(start + 1..start + 3)) {
                        push_for_each_ref_literal(&mut segments, vec![byte]);
                        cursor = start + 3;
                    } else {
                        push_for_each_ref_literal(&mut segments, b"%".to_vec());
                        cursor = start + 1;
                    }
                }
                None => {
                    push_for_each_ref_literal(&mut segments, b"%".to_vec());
                    cursor = start + 1;
                }
            }
        }
        push_for_each_ref_literal(&mut segments, format_spec.as_bytes()[cursor..].to_vec());
        Ok(Self { segments })
    }

    pub fn segments(&self) -> &[ForEachRefFormatSegment] {
        &self.segments
    }
}

fn push_for_each_ref_literal(segments: &mut Vec<ForEachRefFormatSegment>, literal: Vec<u8>) {
    if literal.is_empty() {
        return;
    }
    if let Some(ForEachRefFormatSegment::Literal(previous)) = segments.last_mut() {
        previous.extend_from_slice(&literal);
    } else {
        segments.push(ForEachRefFormatSegment::Literal(literal));
    }
}

pub fn write_for_each_ref_format(
    stdout: &mut impl Write,
    format: &ForEachRefFormat,
    quote: ForEachRefQuoteMode,
    mut write_atom: impl FnMut(&mut Vec<u8>, &str) -> Result<()>,
) -> Result<()> {
    for segment in format.segments() {
        match segment {
            ForEachRefFormatSegment::Literal(literal) => stdout.write_all(literal)?,
            ForEachRefFormatSegment::Atom(atom) => {
                let mut value = Vec::new();
                write_atom(&mut value, atom)?;
                write_for_each_ref_quoted_atom(stdout, &value, quote)?;
            }
        }
    }
    Ok(())
}

fn for_each_ref_hex_escape(value: Option<&[u8]>) -> Option<u8> {
    let value = value?;
    let [high, low] = value else {
        return None;
    };
    Some(for_each_ref_hex_digit(*high)? << 4 | for_each_ref_hex_digit(*low)?)
}

fn for_each_ref_hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub enum ForEachRefQuoteMode {
    #[default]
    None,
    Shell,
    Python,
    Perl,
    Tcl,
}

pub fn write_for_each_ref_quoted_atom(
    stdout: &mut impl Write,
    value: &[u8],
    quote: ForEachRefQuoteMode,
) -> Result<()> {
    match quote {
        ForEachRefQuoteMode::None => stdout.write_all(value)?,
        ForEachRefQuoteMode::Shell => {
            stdout.write_all(b"'")?;
            for byte in value {
                if *byte == b'\'' {
                    stdout.write_all(br#"'\''"#)?;
                } else {
                    stdout.write_all(&[*byte])?;
                }
            }
            stdout.write_all(b"'")?;
        }
        ForEachRefQuoteMode::Python | ForEachRefQuoteMode::Perl => {
            stdout.write_all(b"'")?;
            for byte in value {
                match (*byte, quote) {
                    (b'\\', _) => stdout.write_all(br#"\\"#)?,
                    (b'\'', _) => stdout.write_all(br#"\'"#)?,
                    (b'\n', ForEachRefQuoteMode::Python) => stdout.write_all(br#"\n"#)?,
                    _ => stdout.write_all(&[*byte])?,
                }
            }
            stdout.write_all(b"'")?;
        }
        ForEachRefQuoteMode::Tcl => {
            stdout.write_all(b"\"")?;
            for byte in value {
                match *byte {
                    b'\\' => stdout.write_all(br#"\\"#)?,
                    b'"' => stdout.write_all(br#"\""#)?,
                    b'\n' => stdout.write_all(br#"\n"#)?,
                    _ => stdout.write_all(&[*byte])?,
                }
            }
            stdout.write_all(b"\"")?;
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ForEachRefTrack {
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefEmailMode {
    #[default]
    Bracketed,
    Trim,
    LocalPart,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefDateMode {
    #[default]
    Default,
    Raw,
    Unix,
    Short,
    Iso,
    IsoStrict,
    Rfc2822,
}

struct ForEachRefDateParts<'a> {
    timestamp: i64,
    timezone: &'a str,
    weekday: &'static str,
    year: i64,
    month: u32,
    day: u32,
    hour: i64,
    minute: i64,
    second: i64,
}

pub fn write_for_each_ref_track(
    stdout: &mut impl Write,
    track: ForEachRefTrack,
    bracketed: bool,
) -> Result<()> {
    if bracketed && (track.ahead > 0 || track.behind > 0) {
        stdout.write_all(b"[")?;
    }
    match (track.ahead, track.behind) {
        (0, _) => {}
        (ahead, 0) => write!(stdout, "ahead {ahead}")?,
        (ahead, behind) => write!(stdout, "ahead {ahead}, behind {behind}")?,
    }
    if track.ahead == 0 && track.behind > 0 {
        write!(stdout, "behind {}", track.behind)?;
    }
    if bracketed && (track.ahead > 0 || track.behind > 0) {
        stdout.write_all(b"]")?;
    }
    Ok(())
}

pub fn for_each_ref_track_short(track: ForEachRefTrack) -> &'static str {
    match (track.ahead, track.behind) {
        (0, 0) => "=",
        (_, 0) => ">",
        (0, _) => "<",
        (_, _) => "<>",
    }
}

pub fn write_for_each_ref_identity(stdout: &mut impl Write, identity: Option<&[u8]>) -> Result<()> {
    if let Some(identity) = identity {
        stdout.write_all(identity)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_name(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(name) = for_each_ref_identity_name(identity)
    {
        stdout.write_all(name)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_email(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    write_for_each_ref_identity_email_mode(stdout, identity, ForEachRefEmailMode::Bracketed)
}

pub fn write_for_each_ref_identity_email_mode(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
    mode: ForEachRefEmailMode,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(email) = for_each_ref_identity_email(identity, mode)
    {
        stdout.write_all(email)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_date_raw(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(date) = for_each_ref_identity_date_raw(identity)
    {
        stdout.write_all(date)?;
    }
    Ok(())
}

pub fn write_for_each_ref_identity_date(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
) -> Result<()> {
    write_for_each_ref_identity_date_mode(stdout, identity, ForEachRefDateMode::Default)
}

pub fn write_for_each_ref_identity_date_mode(
    stdout: &mut impl Write,
    identity: Option<&[u8]>,
    mode: ForEachRefDateMode,
) -> Result<()> {
    if let Some(identity) = identity
        && let Some(date) = for_each_ref_identity_date(identity, mode)
    {
        stdout.write_all(date.as_bytes())?;
    }
    Ok(())
}

pub fn for_each_ref_identity_name(identity: &[u8]) -> Option<&[u8]> {
    let marker = identity.windows(2).position(|window| window == b" <")?;
    Some(&identity[..marker])
}

pub fn for_each_ref_identity_email(identity: &[u8], mode: ForEachRefEmailMode) -> Option<&[u8]> {
    let start = identity.iter().position(|byte| *byte == b'<')?;
    let end = identity[start..].iter().position(|byte| *byte == b'>')?;
    let bracketed = &identity[start..=start + end];
    match mode {
        ForEachRefEmailMode::Bracketed => Some(bracketed),
        ForEachRefEmailMode::Trim => Some(&identity[start + 1..start + end]),
        ForEachRefEmailMode::LocalPart => {
            let trimmed = &identity[start + 1..start + end];
            let at = trimmed.iter().position(|byte| *byte == b'@')?;
            Some(&trimmed[..at])
        }
    }
}

pub fn for_each_ref_identity_date_raw(identity: &[u8]) -> Option<&[u8]> {
    let email_end = identity.iter().position(|byte| *byte == b'>')?;
    let rest = identity.get(email_end + 1..)?.strip_prefix(b" ")?;
    let timestamp_end = rest.iter().position(|byte| *byte == b' ')?;
    let timezone = rest.get(timestamp_end + 1..)?;
    if timezone.len() == 5
        && matches!(timezone[0], b'+' | b'-')
        && timezone[1..].iter().all(|byte| byte.is_ascii_digit())
    {
        Some(rest)
    } else {
        None
    }
}

pub fn for_each_ref_identity_date(identity: &[u8], mode: ForEachRefDateMode) -> Option<String> {
    let parts = for_each_ref_identity_date_parts(identity)?;
    Some(format_for_each_ref_date(parts, mode))
}

fn for_each_ref_identity_date_parts(identity: &[u8]) -> Option<ForEachRefDateParts<'_>> {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    let raw = std::str::from_utf8(for_each_ref_identity_date_raw(identity)?).ok()?;
    let (timestamp, timezone) = raw.split_once(' ')?;
    let timestamp = timestamp.parse::<i64>().ok()?;
    let offset_seconds = for_each_ref_timezone_offset_seconds(timezone)?;
    let local = timestamp + offset_seconds;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Some(ForEachRefDateParts {
        timestamp,
        timezone,
        weekday: WEEKDAYS[(days + 4).rem_euclid(7) as usize],
        year,
        month,
        day,
        hour: seconds / 3_600,
        minute: (seconds % 3_600) / 60,
        second: seconds % 60,
    })
}

pub fn for_each_ref_identity_timestamp(identity: &[u8]) -> Option<i64> {
    let raw = std::str::from_utf8(for_each_ref_identity_date_raw(identity)?).ok()?;
    let (timestamp, _) = raw.split_once(' ')?;
    timestamp.parse::<i64>().ok()
}

fn for_each_ref_timezone_offset_seconds(timezone: &str) -> Option<i64> {
    if timezone.len() != 5 {
        return None;
    }
    let sign = match timezone.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours = timezone[1..3].parse::<i64>().ok()?;
    let minutes = timezone[3..5].parse::<i64>().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

fn format_for_each_ref_date(parts: ForEachRefDateParts<'_>, mode: ForEachRefDateMode) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match mode {
        ForEachRefDateMode::Default => format!(
            "{} {} {} {:02}:{:02}:{:02} {} {}",
            parts.weekday,
            MONTHS[(parts.month - 1) as usize],
            parts.day,
            parts.hour,
            parts.minute,
            parts.second,
            parts.year,
            parts.timezone
        ),
        ForEachRefDateMode::Raw => format!("{} {}", parts.timestamp, parts.timezone),
        ForEachRefDateMode::Unix => parts.timestamp.to_string(),
        ForEachRefDateMode::Short => {
            format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day)
        }
        ForEachRefDateMode::Iso => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
            parts.year,
            parts.month,
            parts.day,
            parts.hour,
            parts.minute,
            parts.second,
            parts.timezone
        ),
        ForEachRefDateMode::IsoStrict => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}",
            parts.year,
            parts.month,
            parts.day,
            parts.hour,
            parts.minute,
            parts.second,
            for_each_ref_strict_timezone(parts.timezone)
        ),
        ForEachRefDateMode::Rfc2822 => format!(
            // RFC 2822 day-of-month is not zero-padded (e.g. "Wed, 3 Jun 2026"),
            // matching upstream git; only the time fields are zero-padded.
            "{}, {} {} {:04} {:02}:{:02}:{:02} {}",
            parts.weekday,
            parts.day,
            MONTHS[(parts.month - 1) as usize],
            parts.year,
            parts.hour,
            parts.minute,
            parts.second,
            parts.timezone
        ),
    }
}

fn for_each_ref_strict_timezone(timezone: &str) -> String {
    format!("{}:{}", &timezone[..3], &timezone[3..])
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

pub fn for_each_ref_short_name(refname: &str) -> &str {
    if let Some(remote) = refname.strip_prefix("refs/remotes/")
        && let Some(remote_name) = remote.strip_suffix("/HEAD")
    {
        return remote_name;
    }
    refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/tags/"))
        .or_else(|| refname.strip_prefix("refs/remotes/"))
        .unwrap_or(refname)
}

pub fn parse_for_each_ref_strip_count(value: &str) -> Result<isize> {
    value
        .parse::<isize>()
        .map_err(|_| GitError::Command(format!("invalid refname strip count {value}")))
}

pub fn for_each_ref_lstrip_name(refname: &str, count: isize) -> String {
    let components = refname.split('/').collect::<Vec<_>>();
    if count == 0 {
        return refname.to_string();
    }
    let start = if count > 0 {
        (count as usize).min(components.len())
    } else {
        components.len().saturating_sub(count.unsigned_abs())
    };
    components[start..].join("/")
}

pub fn for_each_ref_rstrip_name(refname: &str, count: isize) -> String {
    let components = refname.split('/').collect::<Vec<_>>();
    if count == 0 {
        return refname.to_string();
    }
    let end = if count > 0 {
        components.len().saturating_sub(count as usize)
    } else {
        count.unsigned_abs().min(components.len())
    };
    components[..end].join("/")
}

pub fn for_each_ref_abbrev_oid(
    oid: &ObjectId,
    width: Option<usize>,
    candidates: &[ObjectId],
) -> String {
    let hex = oid.to_hex();
    let mut width = width.unwrap_or(hex.len()).min(hex.len());
    while width < hex.len() {
        let prefix = &hex.as_bytes()[..width];
        if !candidates
            .iter()
            .any(|candidate| candidate != oid && object_id_hex_starts_with(candidate, prefix))
        {
            break;
        }
        width += 1;
    }
    hex[..width].to_string()
}

fn object_id_hex_starts_with(oid: &ObjectId, prefix: &[u8]) -> bool {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    if prefix.len() > oid.format().hex_len() {
        return false;
    }

    prefix.iter().enumerate().all(|(index, expected)| {
        let byte = oid.as_bytes()[index / 2];
        let nibble = if index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0f
        };
        HEX[nibble as usize] == *expected
    })
}

pub fn parse_for_each_ref_abbrev_width(value: &str) -> Result<usize> {
    let width = value
        .parse::<usize>()
        .ok()
        .filter(|width| *width > 0)
        .ok_or_else(|| {
            GitError::Command(format!(
                "positive value expected in for-each-ref objectname:short format: {value}"
            ))
        })?;
    Ok(width.max(4))
}

pub fn commit_identity_date(raw: &[u8], mode: ForEachRefDateMode) -> String {
    for_each_ref_identity_date(raw, mode).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;

    #[test]
    fn format_parser_decodes_literals_atoms_and_percent_escapes() {
        let format =
            ForEachRefFormat::parse("refs/%%/%(refname)%09%(objectname)%q").expect("valid format");
        assert_eq!(
            format.segments(),
            &[
                ForEachRefFormatSegment::Literal(b"refs/%/".to_vec()),
                ForEachRefFormatSegment::Atom("refname".to_string()),
                ForEachRefFormatSegment::Literal(b"\t".to_vec()),
                ForEachRefFormatSegment::Atom("objectname".to_string()),
                ForEachRefFormatSegment::Literal(b"%q".to_vec()),
            ]
        );
    }

    #[test]
    fn format_parser_rejects_unterminated_atoms() {
        assert!(ForEachRefFormat::parse("%(refname").is_err());
    }

    #[test]
    fn format_renderer_streams_literals_atoms_and_quotes() {
        let format = ForEachRefFormat::parse("branch=%(refname)").expect("valid format");
        let mut out = Vec::new();
        write_for_each_ref_format(
            &mut out,
            &format,
            ForEachRefQuoteMode::Shell,
            |atom, name| {
                assert_eq!(name, "refname");
                atom.extend_from_slice(b"main's");
                Ok(())
            },
        )
        .expect("writes to in-memory buffer");
        assert_eq!(out, b"branch='main'\\''s'");
    }

    #[test]
    fn identity_parts_match_git_identity_layout() {
        let ident = b"Ada Lovelace <ada@example.com> 1717430401 -0530";
        assert_eq!(
            for_each_ref_identity_name(ident),
            Some(&b"Ada Lovelace"[..])
        );
        assert_eq!(
            for_each_ref_identity_email(ident, ForEachRefEmailMode::Bracketed),
            Some(&b"<ada@example.com>"[..])
        );
        assert_eq!(
            for_each_ref_identity_email(ident, ForEachRefEmailMode::Trim),
            Some(&b"ada@example.com"[..])
        );
        assert_eq!(
            for_each_ref_identity_email(ident, ForEachRefEmailMode::LocalPart),
            Some(&b"ada"[..])
        );
        assert_eq!(for_each_ref_identity_timestamp(ident), Some(1717430401));
        assert_eq!(
            for_each_ref_identity_date(ident, ForEachRefDateMode::Raw).as_deref(),
            Some("1717430401 -0530")
        );
    }

    #[test]
    fn dates_use_identity_timezone() {
        let ident = b"Ada <ada@example.com> 1717430401 -0530";
        assert_eq!(
            for_each_ref_identity_date(ident, ForEachRefDateMode::Short).as_deref(),
            Some("2024-06-03")
        );
        assert_eq!(
            for_each_ref_identity_date(ident, ForEachRefDateMode::IsoStrict).as_deref(),
            Some("2024-06-03T10:30:01-05:30")
        );
    }

    #[test]
    fn tracking_formats_match_ref_filter_atoms() {
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 0,
                behind: 0
            }),
            "="
        );
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 1,
                behind: 0
            }),
            ">"
        );
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 0,
                behind: 1
            }),
            "<"
        );
        assert_eq!(
            for_each_ref_track_short(ForEachRefTrack {
                ahead: 1,
                behind: 1
            }),
            "<>"
        );

        let mut out = Vec::new();
        write_for_each_ref_track(
            &mut out,
            ForEachRefTrack {
                ahead: 2,
                behind: 3,
            },
            true,
        )
        .expect("writes to in-memory buffer");
        assert_eq!(out, b"[ahead 2, behind 3]");
    }

    #[test]
    fn refname_shortening_and_stripping_match_ref_filter_rules() {
        assert_eq!(for_each_ref_short_name("refs/heads/main"), "main");
        assert_eq!(for_each_ref_short_name("refs/tags/v1"), "v1");
        assert_eq!(
            for_each_ref_short_name("refs/remotes/origin/HEAD"),
            "origin"
        );
        assert_eq!(for_each_ref_lstrip_name("refs/heads/main", 2), "main");
        assert_eq!(for_each_ref_lstrip_name("refs/heads/main", -1), "main");
        assert_eq!(for_each_ref_rstrip_name("refs/heads/main", 1), "refs/heads");
        assert_eq!(
            for_each_ref_rstrip_name("refs/heads/main", -2),
            "refs/heads"
        );
    }

    #[test]
    fn abbreviations_extend_to_avoid_ambiguity() {
        let one = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("valid object id");
        let two = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111122222222222222222222222222222222222",
        )
        .expect("valid object id");
        assert_eq!(
            parse_for_each_ref_abbrev_width("2").expect("valid abbrev width"),
            4
        );
        assert_eq!(
            for_each_ref_abbrev_oid(&one, Some(4), &[one.clone(), two]),
            "111111"
        );
    }
}
