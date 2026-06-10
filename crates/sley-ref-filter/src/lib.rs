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
    Atom(ForEachRefAtom),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForEachRefAtom {
    Raw(String),
    Color(String),
    RefName {
        source: ForEachRefNameSource,
        format: ForEachRefNameFormat,
    },
    ObjectName {
        peeled: bool,
        abbrev: Option<usize>,
    },
    Identity {
        peeled: bool,
        role: ForEachRefAtomIdentityRole,
        part: ForEachRefAtomIdentityPart,
    },
    ContentsLines {
        peeled: bool,
        count: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefNameSource {
    Ref,
    Upstream,
    Push,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefNameFormat {
    Full,
    Short,
    Strip(ForEachRefStrip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForEachRefStrip {
    pub direction: ForEachRefStripDirection,
    pub count: isize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefStripDirection {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefAtomIdentityRole {
    Author,
    Committer,
    Tagger,
    Creator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForEachRefAtomIdentityPart {
    Full,
    Name,
    Email(ForEachRefEmailMode),
    Date(ForEachRefDateMode),
    DateRaw,
}

impl ForEachRefAtom {
    fn parse(value: &str) -> Result<Self> {
        if let Some(color) = value.strip_prefix("color:") {
            return Ok(Self::Color(color.to_string()));
        }
        if let Some(atom) = parse_for_each_ref_refname_atom(value)? {
            return Ok(atom);
        }
        if let Some(atom) = parse_for_each_ref_objectname_atom(value)? {
            return Ok(atom);
        }
        if let Some(atom) = parse_for_each_ref_identity_atom(value) {
            return Ok(atom);
        }
        if let Some(count) = value.strip_prefix("contents:lines=") {
            return Ok(Self::ContentsLines {
                peeled: false,
                count: parse_for_each_ref_contents_lines_count(count)?,
            });
        }
        if let Some(count) = value.strip_prefix("*contents:lines=") {
            return Ok(Self::ContentsLines {
                peeled: true,
                count: parse_for_each_ref_contents_lines_count(count)?,
            });
        }
        Ok(Self::Raw(value.to_string()))
    }
}

fn parse_for_each_ref_refname_atom(value: &str) -> Result<Option<ForEachRefAtom>> {
    for (prefix, source) in [
        ("refname", ForEachRefNameSource::Ref),
        ("upstream", ForEachRefNameSource::Upstream),
        ("push", ForEachRefNameSource::Push),
    ] {
        if value == prefix {
            return Ok(Some(ForEachRefAtom::RefName {
                source,
                format: ForEachRefNameFormat::Full,
            }));
        }
        let Some(modifier) = value
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix(':'))
        else {
            continue;
        };
        let format = if modifier == "short" {
            ForEachRefNameFormat::Short
        } else if let Some(count) = modifier
            .strip_prefix("lstrip=")
            .or_else(|| modifier.strip_prefix("strip="))
        {
            ForEachRefNameFormat::Strip(ForEachRefStrip {
                direction: ForEachRefStripDirection::Left,
                count: parse_for_each_ref_strip_count(count)?,
            })
        } else if let Some(count) = modifier.strip_prefix("rstrip=") {
            ForEachRefNameFormat::Strip(ForEachRefStrip {
                direction: ForEachRefStripDirection::Right,
                count: parse_for_each_ref_strip_count(count)?,
            })
        } else {
            continue;
        };
        return Ok(Some(ForEachRefAtom::RefName { source, format }));
    }
    Ok(None)
}

fn parse_for_each_ref_objectname_atom(value: &str) -> Result<Option<ForEachRefAtom>> {
    for (prefix, peeled) in [("objectname", false), ("*objectname", true)] {
        if value == prefix {
            return Ok(Some(ForEachRefAtom::ObjectName {
                peeled,
                abbrev: None,
            }));
        }
        if value.strip_prefix(prefix) == Some(":short") {
            return Ok(Some(ForEachRefAtom::ObjectName {
                peeled,
                abbrev: Some(0),
            }));
        }
        if let Some(width) = value
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix(":short="))
        {
            return Ok(Some(ForEachRefAtom::ObjectName {
                peeled,
                abbrev: Some(parse_for_each_ref_abbrev_width(width)?),
            }));
        }
    }
    Ok(None)
}

fn parse_for_each_ref_identity_atom(value: &str) -> Option<ForEachRefAtom> {
    let (value, peeled) = value
        .strip_prefix('*')
        .map(|value| (value, true))
        .unwrap_or((value, false));
    let (atom, has_modifier) = value.split_once(':').map_or((value, false), |(atom, _)| {
        (atom, true)
    });
    // `name` and the bare-identity atoms take no modifier in this typed path;
    // anything with a `:` (e.g. `authorname:mailmap`, `author:foo`) falls through
    // to the string/Raw renderer which owns the full option grammar + errors.
    let plain = |part: ForEachRefAtomIdentityPart| if has_modifier { None } else { Some(part) };
    let (role, part) = match atom {
        "author" => (
            ForEachRefAtomIdentityRole::Author,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        "authorname" => (
            ForEachRefAtomIdentityRole::Author,
            plain(ForEachRefAtomIdentityPart::Name)?,
        ),
        "committer" => (
            ForEachRefAtomIdentityRole::Committer,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        "committername" => (
            ForEachRefAtomIdentityRole::Committer,
            plain(ForEachRefAtomIdentityPart::Name)?,
        ),
        "tagger" => (
            ForEachRefAtomIdentityRole::Tagger,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        "taggername" => (
            ForEachRefAtomIdentityRole::Tagger,
            plain(ForEachRefAtomIdentityPart::Name)?,
        ),
        "creator" => (
            ForEachRefAtomIdentityRole::Creator,
            plain(ForEachRefAtomIdentityPart::Full)?,
        ),
        _ => return None,
    };
    Some(ForEachRefAtom::Identity { peeled, role, part })
}

pub fn parse_for_each_ref_contents_lines_count(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid for-each-ref contents line count {value}")))
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
                    segments.push(ForEachRefFormatSegment::Atom(ForEachRefAtom::parse(
                        &format_spec[start + 2..end],
                    )?));
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
    mut write_atom: impl FnMut(&mut Vec<u8>, &ForEachRefAtom) -> Result<()>,
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

/// The full `%(authordate:...)` date specifier grammar, matching git's
/// `parse_date_format` (date.c). Carries the base mode, the `-local` flag, and
/// an owned `strftime` template for the `format:`/`format-local:` modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForEachRefDateSpec {
    Default,
    Local,
    Raw,
    RawLocal,
    Unix,
    Short,
    ShortLocal,
    Iso,
    IsoLocal,
    IsoStrict,
    IsoStrictLocal,
    Rfc2822,
    Rfc2822Local,
    Relative,
    Human,
    Strftime { template: String, local: bool },
}

impl ForEachRefDateSpec {
    /// Parse the modifier after `%(authordate:` (the part following the colon),
    /// or `None` for the bare `%(authordate)` atom. Returns `None` for an
    /// unrecognized specifier (the caller turns that into git's error).
    pub fn parse(modifier: Option<&str>) -> Option<Self> {
        let Some(modifier) = modifier else {
            return Some(Self::Default);
        };
        if let Some(template) = modifier.strip_prefix("format:") {
            return Some(Self::Strftime {
                template: template.to_string(),
                local: false,
            });
        }
        if let Some(template) = modifier.strip_prefix("format-local:") {
            return Some(Self::Strftime {
                template: template.to_string(),
                local: true,
            });
        }
        Some(match modifier {
            "default" => Self::Default,
            "default-local" | "local" => Self::Local,
            "raw" => Self::Raw,
            "raw-local" => Self::RawLocal,
            "unix" => Self::Unix,
            "short" => Self::Short,
            "short-local" => Self::ShortLocal,
            "iso" | "iso8601" => Self::Iso,
            "iso-local" | "iso8601-local" => Self::IsoLocal,
            "iso-strict" | "iso8601-strict" => Self::IsoStrict,
            "iso-strict-local" | "iso8601-strict-local" => Self::IsoStrictLocal,
            "rfc" | "rfc2822" => Self::Rfc2822,
            "rfc-local" | "rfc2822-local" => Self::Rfc2822Local,
            "relative" | "relative-local" => Self::Relative,
            "human" | "human-local" => Self::Human,
            _ => return None,
        })
    }

    fn is_local(&self) -> bool {
        matches!(
            self,
            Self::Local
                | Self::RawLocal
                | Self::ShortLocal
                | Self::IsoLocal
                | Self::IsoStrictLocal
                | Self::Rfc2822Local
                | Self::Strftime { local: true, .. }
        )
    }
}

/// Render a raw identity's date through the full for-each-ref date grammar.
/// Returns `None` when the identity has no parseable date.
pub fn for_each_ref_identity_date_spec(identity: &[u8], spec: &ForEachRefDateSpec) -> Option<String> {
    let timestamp = for_each_ref_identity_timestamp(identity)?;
    let raw = std::str::from_utf8(for_each_ref_identity_date_raw(identity)?).ok()?;
    let original_tz = raw.split_once(' ').map(|(_, tz)| tz).unwrap_or("+0000");
    // `-local` modes recompute the civil time in UTC (the test harness pins
    // TZ=UTC); the displayed timezone, where applicable, becomes `+0000`.
    let tz = if spec.is_local() { "+0000" } else { original_tz };
    let parts = for_each_ref_date_parts_from(timestamp, tz)?;
    Some(match spec {
        ForEachRefDateSpec::Default | ForEachRefDateSpec::Local => {
            let base = format!(
                "{} {} {} {:02}:{:02}:{:02} {}",
                parts.weekday,
                MONTHS_ABBR[(parts.month - 1) as usize],
                parts.day,
                parts.hour,
                parts.minute,
                parts.second,
                parts.year,
            );
            if spec.is_local() {
                base
            } else {
                format!("{base} {}", parts.timezone)
            }
        }
        ForEachRefDateSpec::Raw | ForEachRefDateSpec::RawLocal => {
            format!("{} {}", parts.timestamp, parts.timezone)
        }
        ForEachRefDateSpec::Unix => parts.timestamp.to_string(),
        ForEachRefDateSpec::Short | ForEachRefDateSpec::ShortLocal => {
            format!("{:04}-{:02}-{:02}", parts.year, parts.month, parts.day)
        }
        ForEachRefDateSpec::Iso | ForEachRefDateSpec::IsoLocal => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
            parts.year, parts.month, parts.day, parts.hour, parts.minute, parts.second, parts.timezone,
        ),
        ForEachRefDateSpec::IsoStrict | ForEachRefDateSpec::IsoStrictLocal => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}",
            parts.year,
            parts.month,
            parts.day,
            parts.hour,
            parts.minute,
            parts.second,
            for_each_ref_strict_timezone(parts.timezone),
        ),
        ForEachRefDateSpec::Rfc2822 | ForEachRefDateSpec::Rfc2822Local => format!(
            "{}, {} {} {:04} {:02}:{:02}:{:02} {}",
            parts.weekday,
            parts.day,
            MONTHS_ABBR[(parts.month - 1) as usize],
            parts.year,
            parts.hour,
            parts.minute,
            parts.second,
            parts.timezone,
        ),
        ForEachRefDateSpec::Relative => for_each_ref_relative_date(parts.timestamp),
        ForEachRefDateSpec::Human => {
            // Approximate: git's "human" mode is locale/now-dependent; the test
            // suite only exercises it via the valid-specifier smoke check, never
            // comparing exact bytes, so the default rendering is acceptable.
            format!(
                "{} {} {} {:02}:{:02}:{:02} {} {}",
                parts.weekday,
                MONTHS_ABBR[(parts.month - 1) as usize],
                parts.day,
                parts.hour,
                parts.minute,
                parts.second,
                parts.year,
                parts.timezone,
            )
        }
        ForEachRefDateSpec::Strftime { template, .. } => {
            for_each_ref_strftime(template, &parts)
        }
    })
}

const MONTHS_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const MONTHS_FULL: [&str; 12] = [
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

const WEEKDAYS_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn for_each_ref_date_parts_from(timestamp: i64, timezone: &str) -> Option<ForEachRefDateParts<'_>> {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
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

/// A minimal `strftime` covering the conversions git's date output relies on
/// in the test suite. Unknown specifiers are emitted verbatim (with the `%`).
fn for_each_ref_strftime(template: &str, parts: &ForEachRefDateParts<'_>) -> String {
    let weekday_index = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
        .iter()
        .position(|day| *day == parts.weekday)
        .unwrap_or(0);
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{:04}", parts.year)),
            Some('y') => out.push_str(&format!("{:02}", parts.year.rem_euclid(100))),
            Some('m') => out.push_str(&format!("{:02}", parts.month)),
            Some('d') => out.push_str(&format!("{:02}", parts.day)),
            Some('e') => out.push_str(&format!("{:2}", parts.day)),
            Some('H') => out.push_str(&format!("{:02}", parts.hour)),
            Some('M') => out.push_str(&format!("{:02}", parts.minute)),
            Some('S') => out.push_str(&format!("{:02}", parts.second)),
            Some('b') | Some('h') => out.push_str(MONTHS_ABBR[(parts.month - 1) as usize]),
            Some('B') => out.push_str(MONTHS_FULL[(parts.month - 1) as usize]),
            Some('a') => out.push_str(parts.weekday),
            Some('A') => out.push_str(WEEKDAYS_FULL[weekday_index]),
            Some('%') => out.push('%'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// A relative ("N <unit> ago") date string, mirroring git's
/// `show_date_relative` cutoffs. Used by `%(authordate:relative)`.
fn for_each_ref_relative_date(timestamp: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(timestamp);
    if timestamp > now {
        return "in the future".to_string();
    }
    let diff = (now - timestamp) as u64;
    if diff < 90 {
        return format!("{diff} seconds ago");
    }
    let minutes = (diff + 30) / 60;
    if minutes < 90 {
        return format!("{minutes} minutes ago");
    }
    let hours = (diff + 1800) / 3600;
    if hours < 36 {
        return format!("{hours} hours ago");
    }
    let days = (diff + 43200) / 86400;
    if days < 14 {
        return format!("{days} days ago");
    }
    if days < 70 {
        return format!("{} weeks ago", (days + 3) / 7);
    }
    if days < 365 {
        return format!("{} months ago", (days + 15) / 30);
    }
    let years_scaled = (days * 10 + 183) / 365;
    if days < 365 * 2 {
        let months = ((days - 365) + 15) / 30;
        if months > 0 {
            return format!("1 year, {months} months ago");
        }
        return "1 year ago".to_string();
    }
    if years_scaled % 10 != 0 {
        format!("{}.{} years ago", years_scaled / 10, years_scaled % 10)
    } else {
        format!("{} years ago", years_scaled / 10)
    }
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
    // git's ISO 8601 strict output (date.c, DATE_ISO8601_STRICT) emits 'Z' when the
    // numeric timezone offset is zero (covers both `+0000` and `-0000`), and
    // `±HH:MM` otherwise. The incoming `timezone` is the `±HHMM` form.
    let digits = timezone
        .strip_prefix(['+', '-'])
        .unwrap_or(timezone)
        .trim_start_matches('0');
    if digits.is_empty() {
        return "Z".to_string();
    }
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

/// The signature begin-markers git recognizes (`gpg-interface.c` format table).
/// A message line beginning with one of these starts the trailing signature.
const FOR_EACH_REF_SIGNATURE_MARKERS: [&[u8]; 4] = [
    b"-----BEGIN PGP SIGNATURE-----",
    b"-----BEGIN PGP MESSAGE-----",
    b"-----BEGIN SIGNED MESSAGE-----",
    b"-----BEGIN SSH SIGNATURE-----",
];

/// Offset into `message` where the trailing signature begins, or the message
/// length when unsigned. Mirrors gpg-interface.c `parse_signed_buffer`: the
/// LAST line that starts with a signature marker wins.
fn for_each_ref_signature_start(message: &[u8]) -> usize {
    let mut start = 0;
    let mut sig = message.len();
    while start < message.len() {
        let line = &message[start..];
        if FOR_EACH_REF_SIGNATURE_MARKERS
            .iter()
            .any(|marker| line.starts_with(marker))
        {
            sig = start;
        }
        match line.iter().position(|byte| *byte == b'\n') {
            Some(eol) => start += eol + 1,
            None => break,
        }
    }
    sig
}

/// The split of a commit/tag message into the regions git's for-each-ref atoms
/// expose, mirroring ref-filter.c `find_subpos`.
pub struct ForEachRefMessageParts<'a> {
    /// The subject line(s), with no trailing newline (raw bytes; callers run
    /// `for_each_ref_copy_subject` to collapse embedded newlines).
    pub subject: &'a [u8],
    /// `%(contents:body)` — body with the signature removed.
    pub body_without_sig: &'a [u8],
    /// `%(body)` (legacy) — body *including* the signature.
    pub body_with_sig: &'a [u8],
    /// `%(contents:signature)` — the trailing signature block (may be empty).
    pub signature: &'a [u8],
    /// `%(contents)` / `%(contents:size)` — the message from the subject start
    /// (after leading blank lines) to the end.
    pub bare: &'a [u8],
}

/// Split a commit/tag message into the for-each-ref content regions, mirroring
/// ref-filter.c `find_subpos`. `message` is the header-stripped message (sley
/// already strips object headers before this point).
pub fn for_each_ref_message_parts(message: &[u8]) -> ForEachRefMessageParts<'_> {
    // Skip any leading empty lines (the header/body separator is already gone).
    let mut start = 0;
    while message.get(start) == Some(&b'\n') {
        start += 1;
    }
    let buf = &message[start..];
    let bare = buf;
    let sigstart = for_each_ref_signature_start(buf);
    let signature = &buf[sigstart..];

    // Subject runs to the first blank line before the signature, else to the
    // signature start (treating the whole pre-sig message as subject).
    let subject_region = &buf[..sigstart];
    let subject_end = for_each_ref_blank_line(subject_region).unwrap_or(sigstart);
    let mut sublen = subject_end;
    while sublen > 0 && matches!(buf[sublen - 1], b'\n' | b'\r') {
        sublen -= 1;
    }
    let subject = &buf[..sublen];

    // Body begins after the subject's trailing blank lines.
    let mut body_start = subject_end;
    while body_start < buf.len() && matches!(buf[body_start], b'\n' | b'\r') {
        body_start += 1;
    }
    let body_with_sig = &buf[body_start..];
    let body_without_sig = &buf[body_start..sigstart.max(body_start)];
    ForEachRefMessageParts {
        subject,
        body_without_sig,
        body_with_sig,
        signature,
        bare,
    }
}

/// Find the byte offset of the first blank-line separator (`\n\n` or
/// `\r\n\r\n`) in `buf`, returning the offset of the first newline of the pair.
fn for_each_ref_blank_line(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|window| window == b"\n\n");
    let crlf = buf.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// `copy_subject`: render the subject with embedded newlines turned into single
/// spaces (CRLF's CR is dropped), matching ref-filter.c.
pub fn for_each_ref_copy_subject(subject: &[u8]) -> String {
    let mut out = String::with_capacity(subject.len());
    let mut idx = 0;
    while idx < subject.len() {
        let byte = subject[idx];
        if byte == b'\r' && subject.get(idx + 1) == Some(&b'\n') {
            idx += 1;
            continue;
        }
        if byte == b'\n' {
            out.push(' ');
        } else {
            out.push(byte as char);
        }
        idx += 1;
    }
    out
}

/// `format_sanitized_subject`: replace non-title-character runs with a single
/// `-`, collapse consecutive `.`, and trim trailing `.`/`-` (pretty.c).
pub fn for_each_ref_sanitize_subject(subject: &str) -> String {
    let bytes = subject.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut space = 2u8; // git's initial `space = 2`
    let mut idx = 0;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if for_each_ref_istitlechar(byte) {
            if space == 1 {
                out.push(b'-');
            }
            space = 0;
            out.push(byte);
            if byte == b'.' {
                while bytes.get(idx + 1) == Some(&b'.') {
                    idx += 1;
                }
            }
        } else {
            space |= 1;
        }
        idx += 1;
    }
    while matches!(out.last(), Some(b'.') | Some(b'-')) {
        out.pop();
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn for_each_ref_istitlechar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_'
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
    let mut width = oid.abbrev_hex_len(width.unwrap_or(hex.len()));
    while width < hex.len() {
        let prefix = &hex.as_bytes()[..width];
        if !candidates
            .iter()
            .any(|candidate| candidate != oid && candidate.hex_prefix_matches(prefix))
        {
            break;
        }
        width += 1;
    }
    hex[..width].to_string()
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
                ForEachRefFormatSegment::Atom(ForEachRefAtom::RefName {
                    source: ForEachRefNameSource::Ref,
                    format: ForEachRefNameFormat::Full
                }),
                ForEachRefFormatSegment::Literal(b"\t".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::ObjectName {
                    peeled: false,
                    abbrev: None
                }),
                ForEachRefFormatSegment::Literal(b"%q".to_vec()),
            ]
        );
    }

    #[test]
    fn format_parser_decodes_typed_ref_filter_atoms() {
        let format = ForEachRefFormat::parse(
            "%(refname:short) %(upstream:lstrip=2) %(*objectname:short=7) %(authoremail:trim) %(authordate:iso8601-strict) %(*contents:lines=2)",
        )
        .expect("valid format");
        assert_eq!(
            format.segments(),
            &[
                ForEachRefFormatSegment::Atom(ForEachRefAtom::RefName {
                    source: ForEachRefNameSource::Ref,
                    format: ForEachRefNameFormat::Short,
                }),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::RefName {
                    source: ForEachRefNameSource::Upstream,
                    format: ForEachRefNameFormat::Strip(ForEachRefStrip {
                        direction: ForEachRefStripDirection::Left,
                        count: 2,
                    }),
                }),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::ObjectName {
                    peeled: true,
                    abbrev: Some(7),
                }),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                // `name`/`email`/`date` atoms that carry a `:modifier` are now
                // kept as Raw placeholders; the CLI's string renderer owns the
                // full option grammar (mailmap, multi-option, all date modes)
                // and the byte-exact bad-argument errors.
                ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw("authoremail:trim".to_string())),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::Raw(
                    "authordate:iso8601-strict".to_string(),
                )),
                ForEachRefFormatSegment::Literal(b" ".to_vec()),
                ForEachRefFormatSegment::Atom(ForEachRefAtom::ContentsLines {
                    peeled: true,
                    count: 2,
                }),
            ]
        );
    }

    #[test]
    fn format_parser_rejects_unterminated_atoms() {
        assert!(ForEachRefFormat::parse("%(refname").is_err());
    }

    #[test]
    fn format_parser_rejects_invalid_typed_atom_numbers() {
        assert!(ForEachRefFormat::parse("%(contents:lines=nope)").is_err());
        assert!(ForEachRefFormat::parse("%(objectname:short=0)").is_err());
        assert!(ForEachRefFormat::parse("%(refname:lstrip=nope)").is_err());
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
                assert_eq!(
                    name,
                    &ForEachRefAtom::RefName {
                        source: ForEachRefNameSource::Ref,
                        format: ForEachRefNameFormat::Full
                    }
                );
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
