//! `git log` / pretty-format CLI helpers (decoration, filters, signatures, dates).

use crate::commands;
use crate::{
    has_unescaped_trailing_dollar, sley_core, sley_diff_merge, sley_pretty, sley_rev,
    CompiledLogFormat, GitConfig, GitError, LogFormatContext, ObjectFormat, ObjectId, ReflogEntry,
    Result, StashFormatContext, emit_compiled_log_format, emit_compiled_stash_format,
    log_reencode_message,
};
use sley_grep;
use sley::ReferenceTarget as RefTarget;
use sley::plumbing::sley_core::DateMode;
use sley::plumbing::sley_object::{Commit, ObjectType};
use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectReader};
use sley::plumbing::sley_refs::FileRefStore;
use sley::plumbing::sley_rev::CommitRecord;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

pub(crate) fn log_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

pub(crate) fn log_option_requires_value_error(option: &str) -> GitError {
    eprintln!("error: option `{option}' requires a value");
    GitError::Exit(129)
}

pub(crate) fn log_parse_age(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("fatal: '{value}': not a number of seconds since epoch");
        GitError::Exit(128)
    })
}

pub(crate) fn log_parse_date_cutoff(value: &str) -> Result<i64> {
    let mut parts = value.split_whitespace();
    let Some(first) = parts.next() else {
        return log_invalid_date_format(value);
    };
    if let Some(timestamp) = first.strip_prefix('@') {
        let Some(timezone) = parts.next() else {
            return log_invalid_date_format(value);
        };
        if parts.next().is_some() || log_parse_timezone_offset_seconds(timezone).is_none() {
            return log_invalid_date_format(value);
        }
        return timestamp.parse::<i64>().map_err(|_| {
            eprintln!("fatal: invalid date format: {value}");
            GitError::Exit(128)
        });
    }
    // The timezone may be embedded directly after the time in the `T`-separated
    // ISO 8601 form (e.g. `1970-01-01T00:00:01Z` or `...01+0000`), in which case
    // there is no separate whitespace-delimited timezone token to consume.
    let (date, time, embedded_tz) = if let Some((date, rest)) = first.split_once('T') {
        let (time, tz) = log_split_embedded_timezone(rest);
        (date, time, tz)
    } else {
        let Some(time) = parts.next() else {
            return log_invalid_date_format(value);
        };
        (first, time, None)
    };
    let timezone = match embedded_tz {
        Some(tz) => tz,
        None => match parts.next() {
            Some(tz) => tz.to_string(),
            None => return log_invalid_date_format(value),
        },
    };
    if parts.next().is_some() {
        return log_invalid_date_format(value);
    }
    let Some((year, month, day)) = log_parse_date_ymd(date) else {
        return log_invalid_date_format(value);
    };
    let Some((hour, minute, second)) = log_parse_time_hms(time) else {
        return log_invalid_date_format(value);
    };
    let Some(timezone_offset) = log_parse_timezone_offset_seconds(&timezone) else {
        return log_invalid_date_format(value);
    };
    let days = log_days_from_civil(year, month, day);
    Ok(days * 86_400 + i64::from(hour * 3_600 + minute * 60 + second) - timezone_offset)
}

/// Split an ISO 8601 time portion (the part after `T`) into the bare time and an
/// optional embedded timezone. Recognises a trailing `Z` (UTC, normalised to
/// `+0000`) and a trailing `±HHMM` offset; otherwise the whole string is the time
/// and the timezone (if any) is supplied separately.
fn log_split_embedded_timezone(rest: &str) -> (&str, Option<String>) {
    if let Some(time) = rest.strip_suffix('Z') {
        return (time, Some("+0000".to_string()));
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 5 {
        let tz_start = bytes.len() - 5;
        if matches!(bytes[tz_start], b'+' | b'-')
            && bytes[tz_start + 1..]
                .iter()
                .all(|byte| byte.is_ascii_digit())
        {
            return (&rest[..tz_start], Some(rest[tz_start..].to_string()));
        }
    }
    (rest, None)
}

pub(crate) fn log_parse_date_ymd(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = log_days_in_month(year, month);
    if !(1..=max_day).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

pub(crate) fn log_parse_time_hms(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    let second = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

pub(crate) fn log_parse_timezone_offset_seconds(value: &str) -> Option<i64> {
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

fn log_days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if log_is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn log_is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub(crate) fn log_days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn log_invalid_date_format<T>(value: &str) -> Result<T> {
    eprintln!("fatal: invalid date format: {value}");
    Err(GitError::Exit(128))
}

pub(crate) fn log_date_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--date' requires a value");
    GitError::Exit(128)
}

pub(crate) fn log_author_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--author' requires a value");
    GitError::Exit(128)
}

pub(crate) fn log_committer_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--committer' requires a value");
    GitError::Exit(128)
}

pub(crate) fn log_grep_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--grep' requires a value");
    GitError::Exit(128)
}

/// `git log -S`/`-G` with no value: parse-options "switch requires a value"
/// (exit 129). `kind` is the single letter (`S`/`G`).
pub(crate) fn log_pickaxe_requires_value_error(kind: &str) -> GitError {
    eprintln!("error: switch `{kind}' requires a value");
    GitError::Exit(129)
}

/// `git log -S ""`/`-G ""` with an empty value (exit 129).
pub(crate) fn log_pickaxe_empty_error(kind: &str) -> GitError {
    eprintln!("error: -{kind} requires a non-empty argument");
    GitError::Exit(129)
}

/// Combining multiple pickaxe kinds (`-S`/`-G`/`--find-object`) — git rejects
/// with exit 128.
pub(crate) fn log_pickaxe_kinds_conflict_error() -> GitError {
    eprintln!("fatal: options '-G', '-S', and '--find-object' cannot be used together");
    GitError::Exit(128)
}

/// `-G` with `--pickaxe-regex` (exit 128).
pub(crate) fn log_pickaxe_g_regex_conflict_error() -> GitError {
    eprintln!(
        "fatal: options '-G' and '--pickaxe-regex' cannot be used together, use '--pickaxe-regex' with '-S'"
    );
    GitError::Exit(128)
}

/// `--pickaxe-all` with `--find-object` (exit 128).
pub(crate) fn log_pickaxe_all_objfind_conflict_error() -> GitError {
    eprintln!(
        "fatal: options '--pickaxe-all' and '--find-object' cannot be used together, use '--pickaxe-all' with '-G' and '-S'"
    );
    GitError::Exit(128)
}

pub(crate) fn log_date_mode(value: &str) -> Result<DateMode> {
    match DateMode::parse(value) {
        Some(mode) => Ok(mode),
        None => {
            log_unknown_date_format(value)?;
            unreachable!("log_unknown_date_format always returns an error")
        }
    }
}

fn log_unknown_date_format(value: &str) -> Result<()> {
    eprintln!("fatal: unknown date format {value}");
    Err(GitError::Exit(128))
}

pub(crate) fn log_parse_diff_algorithm(value: &str) -> sley_diff_merge::DiffAlgorithm {
    match value {
        "minimal" => sley_diff_merge::DiffAlgorithm::Minimal,
        "patience" => sley_diff_merge::DiffAlgorithm::Patience,
        "histogram" => sley_diff_merge::DiffAlgorithm::Histogram,
        _ => sley_diff_merge::DiffAlgorithm::Myers,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogDecorationMode {
    Off,
    Short,
    Full,
}

/// A single normalized decoration ref-filter pattern. git's
/// `normalize_glob_ref`: a pattern not starting with `refs/` (and not `HEAD`)
/// is prefixed with `refs/`; a trailing `/` is stripped; the pattern matches
/// either as a glob (`wildmatch`) or, when it has no glob metacharacters, as a
/// path-prefix (`refs/foo` matches `refs/foo` and `refs/foo/...`).
#[derive(Debug, Clone)]
struct DecorationPattern {
    normalized: String,
    is_glob: bool,
}

impl DecorationPattern {
    fn new(pattern: &str) -> Self {
        let mut normalized = String::new();
        if !pattern.starts_with("refs/") && pattern != "HEAD" {
            normalized.push_str("refs/");
        }
        normalized.push_str(pattern);
        while normalized.ends_with('/') {
            normalized.pop();
        }
        let is_glob = pattern.bytes().any(|b| matches!(b, b'*' | b'?' | b'['));
        DecorationPattern {
            normalized,
            is_glob,
        }
    }

    fn matches(&self, refname: &str) -> bool {
        if self.is_glob {
            sley_pathspec::wildmatch(self.normalized.as_bytes(), refname.as_bytes(), 0)
        } else {
            // Prefix match: refname == pattern, or refname starts with
            // "pattern/".
            match refname.strip_prefix(&self.normalized) {
                Some(rest) => rest.is_empty() || rest.starts_with('/'),
                None => false,
            }
        }
    }
}

/// Decoration ref filter mirroring git's `decoration_filter` / `ref_filter_match`.
#[derive(Debug, Clone, Default)]
pub(crate) struct DecorationFilter {
    include: Vec<DecorationPattern>,
    exclude: Vec<DecorationPattern>,
    exclude_config: Vec<DecorationPattern>,
}

impl DecorationFilter {
    pub(crate) fn new(include: &[String], exclude: &[String], exclude_config: &[String]) -> Self {
        DecorationFilter {
            include: include.iter().map(|p| DecorationPattern::new(p)).collect(),
            exclude: exclude.iter().map(|p| DecorationPattern::new(p)).collect(),
            exclude_config: exclude_config
                .iter()
                .map(|p| DecorationPattern::new(p))
                .collect(),
        }
    }

    /// Whether `refname` survives the filter (git `ref_filter_match`): explicit
    /// excludes first, then include-only (any include patterns ⇒ refname must
    /// match one), then config excludes, else keep.
    fn matches(&self, refname: &str) -> bool {
        if self.exclude.iter().any(|p| p.matches(refname)) {
            return false;
        }
        if !self.include.is_empty() {
            return self.include.iter().any(|p| p.matches(refname));
        }
        if self.exclude_config.iter().any(|p| p.matches(refname)) {
            return false;
        }
        true
    }
}

pub(crate) fn log_decoration_map(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    mode: LogDecorationMode,
    filter: &DecorationFilter,
) -> Result<HashMap<ObjectId, Vec<String>>> {
    let store = FileRefStore::new(git_dir, format);
    let head_ref = store.current_branch_ref()?;
    let mut decorations = HashMap::<ObjectId, Vec<String>>::new();
    // Git stores decorations in a per-object linked list by prepending each ref
    // as refs_for_each_ref() visits sorted refs; the rendered order is therefore
    // reverse ref iteration order. HEAD is loaded after refs, so it prepends over
    // all ordinary names and can collapse with the branch it points at.
    let mut head_decoration: Option<(ObjectId, String)> = None;
    let mut head_branch_shown_inline = false;
    if let Some(head_target) = store.read_ref("HEAD")? {
        let head_kept = filter.matches("HEAD");
        match head_target {
            RefTarget::Symbolic(name) => {
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&name)?
                    && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
                {
                    let branch_kept = filter.matches(&name);
                    if head_kept && branch_kept {
                        let label = log_decoration_ref_name(&name, mode);
                        head_decoration = Some((commit, format!("HEAD -> {label}")));
                        head_branch_shown_inline = true;
                    } else if head_kept {
                        head_decoration = Some((commit, "HEAD".to_string()));
                    }
                }
            }
            RefTarget::Direct(oid) => {
                if head_kept && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) {
                    head_decoration = Some((commit, "HEAD".to_string()));
                }
            }
        }
    }
    for reference in store.list_refs()? {
        if head_branch_shown_inline && head_ref.as_deref() == Some(reference.name.as_str()) {
            continue;
        }
        if !filter.matches(&reference.name) {
            continue;
        }
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) else {
            continue;
        };
        let label = log_decoration_label(&reference.name, mode);
        decorations.entry(commit).or_default().insert(0, label);
    }
    if let Some((commit, label)) = head_decoration {
        decorations.entry(commit).or_default().insert(0, label);
    }
    Ok(decorations)
}

fn log_decoration_label(refname: &str, mode: LogDecorationMode) -> String {
    if refname.starts_with("refs/tags/") {
        format!("tag: {}", log_decoration_ref_name(refname, mode))
    } else {
        log_decoration_ref_name(refname, mode)
    }
}

fn log_decoration_ref_name(refname: &str, mode: LogDecorationMode) -> String {
    if mode == LogDecorationMode::Full {
        return refname.to_string();
    }
    refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/tags/"))
        .or_else(|| refname.strip_prefix("refs/remotes/"))
        .unwrap_or(refname)
        .to_string()
}

pub(crate) fn print_log_decorations(oid: &ObjectId, decorations: &HashMap<ObjectId, Vec<String>>) {
    if let Some(labels) = decorations.get(oid)
        && !labels.is_empty()
    {
        print!(" ({})", labels.join(", "));
    }
}

pub(crate) fn commit_author_identity(raw: &[u8]) -> String {
    // Split the ident git's way (tolerant of broken emails / missing dates) and
    // re-join as `Name <email>`, exactly as pretty.c's pp_user_info renders the
    // Author:/Committer: line. A line with no `<…>` pair falls back to the raw
    // bytes.
    let Some(fields) = sley_core::split_ident_line(raw) else {
        return String::from_utf8_lossy(raw).into_owned();
    };
    let mut identity = String::new();
    identity.push_str(&String::from_utf8_lossy(fields.name));
    identity.push_str(" <");
    identity.push_str(&String::from_utf8_lossy(fields.email));
    identity.push('>');
    identity
}

/// `commit_author_identity` with an optional mailmap pass — the default/medium/
/// full pretty formats route the whole `Name <email>` through the mailmap when
/// `git log --use-mailmap`/`log.mailmap` is active (git's `pp_user_info`). When
/// `mailmap` is `None` (or empty) this is identical to `commit_author_identity`.
pub(crate) fn commit_identity_mailmapped(
    raw: &[u8],
    mailmap: Option<&commands::utility::Mailmap>,
) -> String {
    let identity = commit_author_identity(raw);
    let Some(mailmap) = mailmap.filter(|m| !m.is_empty()) else {
        return identity;
    };
    // Split `Name <email>` (commit_author_identity already trimmed the date).
    let (name, email) = match identity.rsplit_once(" <") {
        Some((name, rest)) => (name, rest.strip_suffix('>').unwrap_or(rest)),
        None => return identity,
    };
    let (name, email) = mailmap.map_user(name, email);
    format!("{name} <{email}>")
}

#[derive(Debug)]
pub(crate) struct SimpleLogRegex {
    alternatives: Vec<SimpleLogRegexAlternative>,
    /// `--perl-regexp` patterns compile through the full grep regex engine in
    /// PCRE mode instead of the simple BRE subset above.
    perl: Option<sley_grep::Regex>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SimpleLogRegexMode {
    Basic,
    Fixed,
    Perl,
}

#[derive(Debug)]
pub(crate) struct LogFilterPattern {
    pattern: String,
    error_context: &'static str,
}

impl LogFilterPattern {
    pub(crate) fn new(pattern: &str, error_context: &'static str) -> Self {
        Self {
            pattern: pattern.to_string(),
            error_context,
        }
    }
}

#[derive(Debug)]
struct SimpleLogRegexAlternative {
    anchor_start: bool,
    anchor_end: bool,
    tokens: Vec<SimpleLogRegexToken>,
}

#[derive(Debug)]
enum SimpleLogRegexToken {
    Literal(u8),
    Any,
    AnyString,
    Class(SimpleLogRegexClass),
}

#[derive(Debug)]
struct SimpleLogRegexClass {
    negated: bool,
    items: Vec<SimpleLogRegexClassItem>,
}

#[derive(Debug)]
enum SimpleLogRegexClassItem {
    Literal(u8),
    Range(u8, u8),
}

impl SimpleLogRegex {
    fn parse(pattern: &str, error_context: &'static str, mode: SimpleLogRegexMode) -> Result<Self> {
        Self::parse_with_diagnostic_verbosity(
            pattern,
            error_context,
            mode,
            sley_grep::RegexDiagnosticVerbosity::from_env(),
        )
    }

    fn parse_with_diagnostic_verbosity(
        pattern: &str,
        error_context: &'static str,
        mode: SimpleLogRegexMode,
        diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
    ) -> Result<Self> {
        if pattern.is_empty() {
            return Ok(Self {
                alternatives: vec![SimpleLogRegexAlternative {
                    anchor_start: false,
                    anchor_end: false,
                    tokens: Vec::new(),
                }],
                perl: None,
            });
        }
        if let SimpleLogRegexMode::Perl = mode {
            let regex =
                sley_grep::Regex::compile(pattern, sley_grep::RegexMode::Pcre, false, false)?;
            return Ok(Self {
                alternatives: Vec::new(),
                perl: Some(regex),
            });
        }
        let alternatives = match mode {
            SimpleLogRegexMode::Basic => split_log_regex_alternatives(pattern)
                .into_iter()
                .map(|alternative| {
                    SimpleLogRegexAlternative::parse(
                        alternative,
                        error_context,
                        diagnostic_verbosity,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
            SimpleLogRegexMode::Fixed => vec![SimpleLogRegexAlternative::parse_fixed(pattern)],
            SimpleLogRegexMode::Perl => unreachable!("handled above"),
        };
        Ok(Self {
            alternatives,
            perl: None,
        })
    }

    pub(crate) fn is_match(&self, value: &str, ignore_case: bool) -> bool {
        if let Some(perl) = &self.perl {
            return perl.is_match_with_case(value.as_bytes(), ignore_case);
        }
        self.alternatives
            .iter()
            .any(|alternative| alternative.is_match(value, ignore_case))
    }
}

impl SimpleLogRegexAlternative {
    fn parse(
        pattern: &str,
        error_context: &'static str,
        diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
    ) -> Result<Self> {
        let mut bytes = pattern.as_bytes();
        let anchor_start = bytes.first().copied() == Some(b'^');
        if anchor_start {
            bytes = &bytes[1..];
        }
        let anchor_end = has_unescaped_trailing_dollar(bytes);
        if anchor_end {
            bytes = &bytes[..bytes.len() - 1];
        }
        let mut tokens = Vec::new();
        let mut idx = 0;
        while idx < bytes.len() {
            match bytes[idx] {
                b'\\' if idx + 1 < bytes.len() => {
                    tokens.push(SimpleLogRegexToken::Literal(bytes[idx + 1]));
                    idx += 2;
                }
                b'.' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                    tokens.push(SimpleLogRegexToken::AnyString);
                    idx += 2;
                }
                b'.' => {
                    tokens.push(SimpleLogRegexToken::Any);
                    idx += 1;
                }
                b'[' => {
                    let (class, consumed) = parse_simple_log_regex_class(
                        &bytes[idx + 1..],
                        pattern,
                        error_context,
                        diagnostic_verbosity,
                    )?;
                    tokens.push(SimpleLogRegexToken::Class(class));
                    idx += consumed + 2;
                }
                byte => {
                    tokens.push(SimpleLogRegexToken::Literal(byte));
                    idx += 1;
                }
            }
        }
        Ok(Self {
            anchor_start,
            anchor_end,
            tokens,
        })
    }

    fn parse_fixed(pattern: &str) -> Self {
        Self {
            anchor_start: false,
            anchor_end: false,
            tokens: pattern
                .as_bytes()
                .iter()
                .copied()
                .map(SimpleLogRegexToken::Literal)
                .collect(),
        }
    }

    fn is_match(&self, value: &str, ignore_case: bool) -> bool {
        let bytes = value.as_bytes();
        if self.anchor_start {
            return self.match_from(bytes, 0, 0, ignore_case);
        }
        (0..=bytes.len()).any(|start| self.match_from(bytes, 0, start, ignore_case))
    }

    fn match_from(
        &self,
        bytes: &[u8],
        token_idx: usize,
        byte_idx: usize,
        ignore_case: bool,
    ) -> bool {
        let Some(token) = self.tokens.get(token_idx) else {
            return !self.anchor_end || byte_idx == bytes.len();
        };
        match token {
            SimpleLogRegexToken::Literal(expected) => {
                bytes
                    .get(byte_idx)
                    .is_some_and(|actual| log_regex_byte_eq(*actual, *expected, ignore_case))
                    && self.match_from(bytes, token_idx + 1, byte_idx + 1, ignore_case)
            }
            SimpleLogRegexToken::Any => {
                byte_idx < bytes.len()
                    && self.match_from(bytes, token_idx + 1, byte_idx + 1, ignore_case)
            }
            SimpleLogRegexToken::AnyString => (byte_idx..=bytes.len())
                .any(|idx| self.match_from(bytes, token_idx + 1, idx, ignore_case)),
            SimpleLogRegexToken::Class(class) => {
                bytes
                    .get(byte_idx)
                    .is_some_and(|actual| class.matches(*actual, ignore_case))
                    && self.match_from(bytes, token_idx + 1, byte_idx + 1, ignore_case)
            }
        }
    }
}

impl SimpleLogRegexClass {
    fn matches(&self, value: u8, ignore_case: bool) -> bool {
        let matched = self.items.iter().any(|item| match item {
            SimpleLogRegexClassItem::Literal(expected) => {
                log_regex_byte_eq(value, *expected, ignore_case)
            }
            SimpleLogRegexClassItem::Range(start, end) => {
                if ignore_case {
                    let value = value.to_ascii_lowercase();
                    let start = start.to_ascii_lowercase();
                    let end = end.to_ascii_lowercase();
                    start <= value && value <= end
                } else {
                    *start <= value && value <= *end
                }
            }
        });
        if self.negated { !matched } else { matched }
    }
}

fn log_regex_byte_eq(left: u8, right: u8, ignore_case: bool) -> bool {
    if ignore_case {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

pub(crate) fn parse_log_filter_patterns(
    patterns: &[LogFilterPattern],
    mode: SimpleLogRegexMode,
) -> Result<Vec<SimpleLogRegex>> {
    parse_log_filter_patterns_with_diagnostic_verbosity(
        patterns,
        mode,
        sley_grep::RegexDiagnosticVerbosity::Verbose,
    )
}

pub(crate) fn parse_log_filter_patterns_with_diagnostic_verbosity(
    patterns: &[LogFilterPattern],
    mode: SimpleLogRegexMode,
    diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
) -> Result<Vec<SimpleLogRegex>> {
    patterns
        .iter()
        .map(|pattern| {
            SimpleLogRegex::parse_with_diagnostic_verbosity(
                &pattern.pattern,
                pattern.error_context,
                mode,
                diagnostic_verbosity,
            )
        })
        .collect()
}

pub(crate) fn log_grep_pattern_kind_from_config(
    config: &GitConfig,
    current: sley_grep::PatternKind,
    explicit: bool,
) -> sley_grep::PatternKind {
    if explicit {
        return current;
    }
    match config
        .get("grep", None, "patterntype")
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("fixed") => sley_grep::PatternKind::Fixed,
        Some("basic") => sley_grep::PatternKind::Basic,
        Some("extended") => sley_grep::PatternKind::Extended,
        Some("perl") => sley_grep::PatternKind::Perl,
        _ => current,
    }
}

pub(crate) fn compile_log_message_grep_matcher(
    patterns: &[String],
    kind: sley_grep::PatternKind,
    ignore_case: bool,
) -> Result<Option<sley_grep::GrepMatcher>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    sley_grep::GrepMatcher::compile_with_error_context(
        sley_grep::GrepCompileConfig {
            patterns,
            kind,
            ignore_case,
            word: false,
            line_regexp: false,
            diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity::Verbose,
        },
        "command line",
    )
    .map(Some)
}

fn split_log_regex_alternatives(pattern: &str) -> Vec<&str> {
    let mut alternatives = Vec::new();
    let bytes = pattern.as_bytes();
    let mut start = 0;
    let mut idx = 0;
    while idx + 1 < bytes.len() {
        if bytes[idx] == b'\\' && bytes[idx + 1] == b'|' {
            alternatives.push(&pattern[start..idx]);
            idx += 2;
            start = idx;
        } else {
            idx += 1;
        }
    }
    alternatives.push(&pattern[start..]);
    alternatives
}

fn parse_simple_log_regex_class(
    bytes: &[u8],
    pattern: &str,
    error_context: &'static str,
    diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
) -> Result<(SimpleLogRegexClass, usize)> {
    let mut end = None;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b']' && idx > 0 {
            end = Some(idx);
            break;
        }
    }
    let Some(end) = end else {
        return log_regex_unterminated_class_error(
            bytes,
            pattern,
            error_context,
            diagnostic_verbosity,
        );
    };
    let mut class = &bytes[..end];
    let negated = class.first().copied().is_some_and(|byte| byte == b'^');
    if negated {
        class = &class[1..];
    }
    let mut items = Vec::new();
    let mut idx = 0;
    while idx < class.len() {
        if idx + 2 < class.len() && class[idx + 1] == b'-' {
            items.push(SimpleLogRegexClassItem::Range(class[idx], class[idx + 2]));
            idx += 3;
        } else {
            items.push(SimpleLogRegexClassItem::Literal(class[idx]));
            idx += 1;
        }
    }
    Ok((SimpleLogRegexClass { negated, items }, end))
}

fn log_regex_unterminated_class_error(
    _class_bytes: &[u8],
    pattern: &str,
    error_context: &str,
    diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
) -> Result<(SimpleLogRegexClass, usize)> {
    Err(sley_grep::report_regex_compile_error(
        error_context,
        pattern,
        diagnostic_verbosity,
        sley_grep::RegexDiagnosticDetail::UnbalancedBrackets,
    ))
}

pub(crate) fn log_author_filters_match(
    record: &sley_rev::CommitRecord,
    filters: &[SimpleLogRegex],
    ignore_case: bool,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let author = String::from_utf8_lossy(&record.commit.author);
    filters
        .iter()
        .any(|filter| filter.is_match(&author, ignore_case))
}

pub(crate) fn log_committer_filters_match(
    record: &sley_rev::CommitRecord,
    filters: &[SimpleLogRegex],
    ignore_case: bool,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let committer = String::from_utf8_lossy(&record.commit.committer);
    filters
        .iter()
        .any(|filter| filter.is_match(&committer, ignore_case))
}

pub(crate) fn log_grep_filters_match(
    record: &sley_rev::CommitRecord,
    filters: &[SimpleLogRegex],
    all_match: bool,
    invert: bool,
    ignore_case: bool,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let message = String::from_utf8_lossy(&record.commit.message);
    let matched = if all_match {
        filters
            .iter()
            .all(|filter| filter.is_match(&message, ignore_case))
    } else {
        filters
            .iter()
            .any(|filter| filter.is_match(&message, ignore_case))
    };
    matched != invert
}


// W31: CLI adapters for sley-pretty log format traits.
pub(crate) struct CliMailmapAdapter<'a>(pub(crate) &'a commands::utility::Mailmap);
impl sley_pretty::MailmapLookup for CliMailmapAdapter<'_> {
    fn map_user(&self, name: &str, email: &str) -> (String, String) {
        self.0.map_user(name, email)
    }
}

pub(crate) struct CliLogSignatureContext<'a> {
    pub git_dir: &'a Path,
    pub db: &'a FileObjectDatabase,
    pub config: &'a GitConfig,
    pub source_tag_signatures: &'a HashMap<ObjectId, commands::signing::GpgVerification>,
}

pub(crate) struct CliLogSignatureAdapter<'a>(pub(crate) &'a CliLogSignatureContext<'a>);
impl sley_pretty::LogSignatureLookup for CliLogSignatureAdapter<'_> {
    fn verification_for_oid(&self, oid: &ObjectId) -> Result<sley_pretty::LogSignatureView> {
        if let Some(v) = self.0.source_tag_signatures.get(oid) {
            return Ok(cli_log_signature_view(v));
        }
        let object = self.0.db.read_object(oid)?;
        let Some((payload, signature)) = commands::signing::commit_signature_payload(&object.body)
        else {
            return Ok(sley_pretty::LogSignatureView {
                trust: "undefined".into(),
                pretty_code: b'N',
                ..Default::default()
            });
        };
        Ok(cli_log_signature_view(&commands::signing::verify_payload(
            self.0.git_dir,
            Some(self.0.config),
            &payload,
            &signature,
        )?))
    }
}

fn cli_log_signature_view(v: &commands::signing::GpgVerification) -> sley_pretty::LogSignatureView {
    sley_pretty::LogSignatureView {
        trust: v.trust.clone(),
        signer: v.signer.clone(),
        key: v.key.clone(),
        fingerprint: v.fingerprint.clone(),
        primary_fingerprint: v.primary_fingerprint.clone(),
        pretty_code: v.pretty_code(),
        bare_output: commands::signing::bare_signature_output(v),
    }
}

pub(crate) struct CliLogDescribeContext<'a> {
    pub git_dir: &'a Path,
    pub db: &'a FileObjectDatabase,
    pub format: ObjectFormat,
}

pub(crate) struct CliLogDescribeAdapter<'a>(pub(crate) &'a CliLogDescribeContext<'a>);
impl sley_pretty::LogDescribeLookup for CliLogDescribeAdapter<'_> {
    fn describe_oid(&self, oid: &ObjectId, spec: &sley_pretty::DescribeSpec) -> Result<String> {
        Ok(commands::describe::describe_for_format(
            self.0.git_dir,
            self.0.format,
            self.0.db,
            oid,
            spec.tags,
            spec.abbrev,
            &spec.matches,
            &spec.excludes,
        )?
        .unwrap_or_default())
    }
}

pub(crate) fn source_tag_signatures_for_revision_tips(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    tips: &[sley_rev::RevisionTip],
) -> Result<HashMap<ObjectId, commands::signing::GpgVerification>> {
    let mut signatures = HashMap::new();
    for tip in tips {
        let object = db.read_object(&tip.oid)?;
        if object.object_type != ObjectType::Tag {
            continue;
        }
        let commit = match sley_rev::peel_to_commit(db, format, &tip.oid) {
            Ok(c) => c,
            Err(e) if tip.from_ref_selector => {
                let _ = e;
                continue;
            }
            Err(e) => return Err(e),
        };
        let Some((payload, signature)) = commands::signing::tag_signature_payload(&object.body)
        else {
            continue;
        };
        signatures.insert(
            commit,
            commands::signing::verify_payload(git_dir, Some(config), payload, signature)?,
        );
    }
    Ok(signatures)
}

pub(crate) fn print_log_format(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: LogFormatContext<'_>,
) -> Result<usize> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(
        record,
        compiled,
        &context,
        &mut line,
        0..compiled.tokens.len(),
    )?;
    let out = log_reencode_message(&line, "UTF-8", context.output_encoding);
    let emitted = out.len();
    io::stdout().write_all(&out)?;
    io::stdout().flush()?;
    Ok(emitted)
}

pub(crate) fn print_stash_compiled_format(
    entry: &ReflogEntry,
    index: usize,
    commit: &Commit,
    compiled: &CompiledLogFormat,
    abbrev_len: Option<usize>,
    date_mode: &DateMode,
    date_explicit: bool,
) -> Result<()> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_stash_format(
        compiled,
        &StashFormatContext {
            entry,
            index,
            commit,
            abbrev_len,
            date_mode,
            date_explicit,
        },
        &mut line,
    )?;
    io::stdout().write_all(&line)?;
    io::stdout().flush()?;
    Ok(())
}
