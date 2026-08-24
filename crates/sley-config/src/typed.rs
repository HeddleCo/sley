//! Typed config accessors: [`GitConfig::get_int`], [`GitConfig::get_size`],
//! [`GitConfig::get_bool_or_int`], and [`GitConfig::get_path`] (plus the
//! [`ConfigStack`] equivalents), retiring hand-rolled
//!"fetch-value-then-parse" compositions scattered across the CLI.
//!
//! The numeric grammar is upstream git's `git_parse_signed` /
//! `git_parse_unsigned` (parse.c): `strtoimax`/`strtoumax` with base auto
//! detection (decimal, `0x` hex, leading-`0` octal), an optional trailing
//! `k`/`m`/`g` unit suffix (case-insensitive, nothing else — `1kb` is an
//! error), and range checks against the target width. Parse failures carry
//! git's `errno == ERANGE` split so diagnostics read exactly like
//! `die_bad_number`: "invalid unit" for malformed values versus "out of
//! range" when the digits (or their product) exceed the target type.
//!
//! Diagnostics were pinned against oracle git 2.55 empirically:
//!
//! ```text
//! fatal: bad numeric config value '1x' for 'foo.bar': invalid unit
//! fatal: bad numeric config value '99999999999999999999999' for 'foo.bar': out of range
//! fatal: bad numeric config value '1x' for 'foo.bad' in file .git/config: invalid unit
//! fatal: bad boolean config value 'maybe2' for 'foo.bar'
//! fatal: failed to expand user dir in: '~user/x'
//! ```

use std::path::PathBuf;

use sley_core::{GitError, Result};

use crate::{
    ConfigBoolOrInt, ConfigOrigin, ConfigOriginKind, ConfigStack, GitConfig, eq_ignore_ascii_case,
    home_dir,
};

/// Why a numeric config value failed to parse — upstream's `errno` split in
/// `git_parse_signed` / `git_parse_unsigned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadNumericKind {
    /// `errno == EINVAL`: no digits, or a trailing fragment that is not
    /// exactly `k`/`m`/`g`. (For unsigned targets this also covers any value
    /// containing `-`, which `git_parse_unsigned` rejects outright.)
    InvalidUnit,
    /// `errno == ERANGE`: the digits do not fit the target width, or their
    /// product with the unit factor does.
    OutOfRange,
}

impl std::fmt::Display for BadNumericKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUnit => f.write_str("invalid unit"),
            Self::OutOfRange => f.write_str("out of range"),
        }
    }
}

impl BadNumericKind {
    /// Whether `value` failed the range check (`errno == ERANGE`) rather than
    /// the grammar check (`errno == EINVAL`).
    pub fn from_range_check(is_range: bool) -> Self {
        if is_range {
            Self::OutOfRange
        } else {
            Self::InvalidUnit
        }
    }
}

/// Everything upstream's `die_bad_number` reports for a bad numeric value:
/// the offending text, the config variable, the origin (which selects the
/// ` in file …` location clause), and the errno-derived kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadNumericValue {
    /// The raw config text (empty for a value-less bare key).
    pub value: String,
    /// The variable name in display form (`section.key` or
    /// `section.subsection.key`).
    pub name: String,
    /// Where the value came from, when known. File/blob/stdin origins add
    /// git's location clause; command-line origins render the plain form
    /// (matching observed oracle behaviour for `-c`, whose `filename` is
    /// unset).
    pub origin: Option<ConfigOrigin>,
    /// `invalid unit` versus `out of range`.
    pub kind: BadNumericKind,
}

impl BadNumericValue {
    /// The bare diagnostic (no `fatal:` prefix), byte-identical to oracle.
    pub fn diagnostic(&self) -> String {
        let location = match self.origin.as_ref() {
            Some(origin) => match origin.kind {
                ConfigOriginKind::File if !origin.name.is_empty() => {
                    format!(" in file {}", origin.name)
                }
                ConfigOriginKind::Blob if !origin.name.is_empty() => {
                    format!(" in blob {}", origin.name)
                }
                ConfigOriginKind::Stdin => " in standard input".to_string(),
                _ => String::new(),
            },
            None => String::new(),
        };
        format!(
            "bad numeric config value '{}' for '{}'{location}: {}",
            self.value, self.name, self.kind
        )
    }

    /// Print git's exact fatal line and return its exit status, matching how
    /// the CLI surfaces config fatals (`eprintln!` + exit 128).
    pub fn report(&self) -> GitError {
        eprintln!("fatal: {}", self.diagnostic());
        GitError::Exit(128)
    }
}

/// A failed boolean lookup, rendered like upstream's
/// `die(_("bad boolean config value '%s' for '%s'"))`. Note the absence of a
/// location clause: unlike numbers, bad booleans never name the origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadBooleanValue {
    pub value: String,
    pub name: String,
}

impl BadBooleanValue {
    /// The bare diagnostic (no `fatal:` prefix).
    pub fn diagnostic(&self) -> String {
        format!(
            "bad boolean config value '{}' for '{}'",
            self.value, self.name
        )
    }

    /// Print git's exact fatal line and return its exit status.
    pub fn report(&self) -> GitError {
        eprintln!("fatal: {}", self.diagnostic());
        GitError::Exit(128)
    }
}

/// A `~user`-style path config value whose user-directory expansion failed
/// (`git_config_pathname` → `interpolate_path` returning NULL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadPathValue {
    pub value: String,
}

impl BadPathValue {
    /// The bare diagnostic (no `fatal:` prefix).
    pub fn diagnostic(&self) -> String {
        format!("failed to expand user dir in: '{}'", self.value)
    }

    /// Print git's exact fatal line and return its exit status.
    pub fn report(&self) -> GitError {
        eprintln!("fatal: {}", self.diagnostic());
        GitError::Exit(128)
    }
}

/// A value-less bare key read through an accessor that requires a value;
/// upstream reports this from `git_config_pathname`'s nonbool check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingValueError {
    pub name: String,
}

impl MissingValueError {
    /// The bare diagnostic (no `fatal:` prefix).
    pub fn diagnostic(&self) -> String {
        format!("missing value for '{}'", self.name)
    }

    /// Print git's exact fatal line and return its exit status.
    pub fn report(&self) -> GitError {
        eprintln!("fatal: {}", self.diagnostic());
        GitError::Exit(128)
    }
}

/// Classify `value` under git's signed integer grammar (`git_parse_signed`
/// with the given inclusive maximum), distinguishing the two failure modes so
/// callers can reproduce `die_bad_number` verbatim.
pub fn classify_config_int(value: &str) -> std::result::Result<i64, BadNumericKind> {
    classify_signed(value, i64::MAX)
}

/// Classify under the `int`-width grammar parse-options uses for integer
/// options (`--window`, `--depth`, `--threads`): same units and bases as
/// [`classify_config_int`], but bounded to `[i32::MIN, i32::MAX]`.
pub fn classify_config_i32(value: &str) -> std::result::Result<i32, BadNumericKind> {
    classify_signed(value, i64::from(i32::MAX)).map(|parsed| parsed as i32)
}

/// Interpret a raw value with git's `--bool-or-int` typing (`true`/`yes`/`on`,
/// `false`/`no`/`off`, empty-as-false; otherwise the signed integer grammar),
/// classifying failures through [`classify_config_int`]'s errno split.
pub fn classify_config_bool_or_int(
    value: &str,
) -> std::result::Result<ConfigBoolOrInt, BadNumericKind> {
    interpret_bool_or_int(value)
}

/// Like [`classify_config_int`] but under the unsigned grammar
/// (`git_parse_unsigned`): any `-` anywhere is an invalid unit, and the
/// product must fit `u64`.
pub fn classify_config_size(value: &str) -> std::result::Result<u64, BadNumericKind> {
    classify_unsigned(value, u64::MAX)
}

fn strtonum_prefix(bytes: &[u8]) -> (u32, usize, bool, bool) {
    // strtoimax/strtoumax skip leading whitespace (exactly C `isspace`) and
    // take an optional sign before base detection.
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r'))
        .unwrap_or(bytes.len());
    let mut cursor = start;
    let mut negative = false;
    match bytes.get(cursor) {
        Some(b'-') => {
            negative = true;
            cursor += 1;
        }
        Some(b'+') => cursor += 1,
        _ => {}
    }
    let rest = &bytes[cursor..];
    // Base autodetection: `0x`/`0X` hex (only when a hex digit follows — a
    // bare `0x` parses the lone `0` and leaves `x` as the unit suffix),
    // leading-`0` octal, otherwise decimal. The trailing bool records the
    // lone-`0` conversions, which count towards C's "did any digit get
    // consumed" test even when no further digits follow.
    let (radix, digits, zero_digit) = match rest.first() {
        Some(b'0') => match rest.get(1) {
            Some(b'x' | b'X') if rest.len() > 2 && rest[2].is_ascii_hexdigit() => (16, 2, false),
            Some(b'x' | b'X') => (10, 1, true),
            _ => (8, 1, true),
        },
        _ => (10, 0, false),
    };
    (radix, cursor + digits, negative, zero_digit)
}

fn accumulate_digits(bytes: &[u8], radix: u32) -> (u64, bool, usize) {
    let mut magnitude: u64 = 0;
    let mut overflow = false;
    let mut consumed = 0usize;
    while let Some(&byte) = bytes.get(consumed) {
        let Some(digit) = char::from(byte).to_digit(radix) else {
            break;
        };
        match magnitude
            .checked_mul(u64::from(radix))
            .and_then(|magnitude| magnitude.checked_add(u64::from(digit)))
        {
            Some(next) => magnitude = next,
            None => {
                overflow = true;
                break;
            }
        }
        consumed += 1;
    }
    (magnitude, overflow, consumed)
}

fn unit_factor(suffix: &[u8]) -> Option<u64> {
    match suffix {
        b"" => Some(1),
        b"k" | b"K" => Some(1024),
        b"m" | b"M" => Some(1024 * 1024),
        b"g" | b"G" => Some(1024 * 1024 * 1024),
        _ => None,
    }
}

fn classify_signed(value: &str, max: i64) -> std::result::Result<i64, BadNumericKind> {
    const INVALID: BadNumericKind = BadNumericKind::InvalidUnit;
    if value.is_empty() {
        return Err(INVALID);
    }
    let bytes = value.as_bytes();
    let (radix, digits_start, negative, zero_digit) = strtonum_prefix(bytes);
    let (magnitude, overflow, consumed) = accumulate_digits(&bytes[digits_start..], radix);
    // C backtracks `endptr` to the original pointer when no digit was ever
    // consumed (whitespace-only strings, a lone sign, …): that is EINVAL,
    // not ERANGE.
    if !zero_digit && consumed == 0 {
        return Err(INVALID);
    }
    if overflow {
        return Err(BadNumericKind::OutOfRange);
    }
    let limit = if negative {
        1u64 << 63
    } else {
        i64::MAX as u64
    };
    if magnitude > limit {
        return Err(BadNumericKind::OutOfRange);
    }
    let Some(factor) = unit_factor(&bytes[digits_start + consumed..]) else {
        return Err(INVALID);
    };
    let val = if negative {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    let max128 = max as i128;
    let factor128 = factor as i128;
    // Upstream's pre-multiplication bounds check, in i128 so `i64::MIN`
    // magnitudes cannot trip intermediate overflow.
    if (val > 0 && max128 / factor128 < val) || (val < 0 && (-max128 - 1) / factor128 > val) {
        return Err(BadNumericKind::OutOfRange);
    }
    Ok((val * factor128) as i64)
}

fn classify_unsigned(value: &str, max: u64) -> std::result::Result<u64, BadNumericKind> {
    const INVALID: BadNumericKind = BadNumericKind::InvalidUnit;
    // Upstream rejects negatives up front because strtoumax would silently
    // negate them: *any* '-' anywhere invalidates the whole value.
    if value.is_empty() || value.contains('-') {
        return Err(INVALID);
    }
    let bytes = value.as_bytes();
    let (radix, digits_start, negative, zero_digit) = strtonum_prefix(bytes);
    debug_assert!(!negative, "'-' was rejected above");
    let (magnitude, overflow, consumed) = accumulate_digits(&bytes[digits_start..], radix);
    if !zero_digit && consumed == 0 {
        return Err(INVALID);
    }
    if overflow {
        return Err(BadNumericKind::OutOfRange);
    }
    let Some(factor) = unit_factor(&bytes[digits_start + consumed..]) else {
        return Err(INVALID);
    };
    let product = factor
        .checked_mul(magnitude)
        .ok_or(BadNumericKind::OutOfRange)?;
    if product > max {
        return Err(BadNumericKind::OutOfRange);
    }
    Ok(product)
}

/// The canonical display form for a lookup key, as git names variables in
/// diagnostics (`section.key`, or `section.subsection.key`).
fn config_display_name(section: &str, subsection: Option<&str>, key: &str) -> String {
    match subsection {
        Some(subsection) => format!("{section}.{subsection}.{key}"),
        None => format!("{section}.{key}"),
    }
}

/// Expand a path config value per `git_config_pathname`: a leading `~/` (or a
/// lone `~`) resolves against `$HOME`; other `~user…` forms require passwd
/// lookup, which sley does not perform, so they fail exactly like a missing
/// `HOME`. Everything else (including the empty string) passes through
/// verbatim.
fn expand_config_path_with_home(
    value: &str,
    home: Option<&str>,
) -> std::result::Result<PathBuf, BadPathValue> {
    let usable_home = home.filter(|home| !home.is_empty());
    if let Some(rest) = value.strip_prefix("~/") {
        return match usable_home {
            Some(home) => Ok(PathBuf::from(home).join(rest)),
            None => Err(BadPathValue {
                value: value.to_string(),
            }),
        };
    }
    if value == "~" {
        return match usable_home {
            Some(home) => Ok(PathBuf::from(home)),
            None => Err(BadPathValue {
                value: value.to_string(),
            }),
        };
    }
    if value.starts_with('~') {
        // `~user/…`: upstream consults the passwd database; without one the
        // expansion fails with the same diagnostic as a missing HOME.
        return Err(BadPathValue {
            value: value.to_string(),
        });
    }
    Ok(PathBuf::from(value))
}

fn bad_numeric(
    value: &str,
    name: String,
    origin: Option<ConfigOrigin>,
    kind: BadNumericKind,
) -> BadNumericValue {
    BadNumericValue {
        value: value.to_string(),
        name,
        origin,
        kind,
    }
}

/// Interpret a raw value with git's `--bool-or-int` typing, classifying a
/// failure through the signed grammar so the caller can render
/// `die_bad_number` verbatim.
fn interpret_bool_or_int(value: &str) -> std::result::Result<ConfigBoolOrInt, BadNumericKind> {
    if let Some(parsed) = crate::parse_config_bool_or_int(value) {
        return Ok(parsed);
    }
    // The shared interpreter rejected the value, meaning it is neither a
    // boolean keyword nor an integer under its (trailing-trim) grammar; run
    // the exact classifier so the failure carries the right errno kind. The
    // only values that classify successfully here are ones the shared
    // grammar misses (e.g. `i64::MIN`), which are integers.
    match classify_signed(value, i64::MAX) {
        Ok(parsed) => Ok(ConfigBoolOrInt::Int(parsed)),
        Err(kind) => Err(kind),
    }
}

/// Boolean keywords recognised by `git_parse_maybe_bool_text` (the strict,
/// integer-free typing used by `pack.allowPackReuse`). Returns `None` for
/// integers and garbage alike.
pub fn parse_strict_bool_keyword(value: &str) -> Option<bool> {
    if eq_ignore_ascii_case(value, "true")
        || eq_ignore_ascii_case(value, "yes")
        || eq_ignore_ascii_case(value, "on")
    {
        return Some(true);
    }
    if eq_ignore_ascii_case(value, "false")
        || eq_ignore_ascii_case(value, "no")
        || eq_ignore_ascii_case(value, "off")
        || value.is_empty()
    {
        return Some(false);
    }
    None
}

impl GitConfig {
    /// Read `section[.subsection].key` as a git integer (`int64_t` width).
    ///
    /// Implements `git_parse_signed`: optional sign, decimal/hex/octal bases,
    /// `k`/`m`/`g` unit suffixes, range-checked against `i64`. On failure the
    /// exact `die_bad_number` diagnostic is printed and `Err(Exit(128))` is
    /// returned; a value-less bare key fails the same way (upstream renders
    /// its value as the empty string). Unset keys yield `Ok(None)`.
    pub fn get_int(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<i64>> {
        let name = config_display_name(section, subsection, key);
        match self.get_entry(section, subsection, key) {
            None => Ok(None),
            Some(None) => Err(bad_numeric("", name, None, BadNumericKind::InvalidUnit).report()),
            Some(Some(value)) => match classify_signed(value, i64::MAX) {
                Ok(parsed) => Ok(Some(parsed)),
                Err(kind) => Err(bad_numeric(value, name, None, kind).report()),
            },
        }
    }

    /// Read `section[.subsection].key` as a size (`unsigned long` width):
    /// git's unsigned grammar — `k`/`m`/`g` units, no sign allowed anywhere —
    /// range-checked against `u64`. Diagnostics behave as in
    /// [`GitConfig::get_int`].
    pub fn get_size(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<u64>> {
        let name = config_display_name(section, subsection, key);
        match self.get_entry(section, subsection, key) {
            None => Ok(None),
            Some(None) => Err(bad_numeric("", name, None, BadNumericKind::InvalidUnit).report()),
            Some(Some(value)) => match classify_unsigned(value, u64::MAX) {
                Ok(parsed) => Ok(Some(parsed)),
                Err(kind) => Err(bad_numeric(value, name, None, kind).report()),
            },
        }
    }

    /// Read `section[.subsection].key` with git's `--bool-or-int` typing:
    /// boolean keywords (`true`/`yes`/`on`/`false`/`no`/`off`, case-
    /// insensitive), an empty value as `false`, a value-less bare key as
    /// `true`; anything else goes through the integer grammar and is returned
    /// as [`ConfigBoolOrInt::Int`]. Failures print `die_bad_number`'s
    /// diagnostic (bad booleans are reported by the integer machinery here
    /// because upstream's `git_config_bool_or_int` falls through to
    /// `git_config_int`).
    pub fn get_bool_or_int(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<ConfigBoolOrInt>> {
        let name = config_display_name(section, subsection, key);
        match self.get_entry(section, subsection, key) {
            None => Ok(None),
            Some(None) => Ok(Some(ConfigBoolOrInt::Bool(true))),
            Some(Some(value)) => match interpret_bool_or_int(value) {
                Ok(parsed) => Ok(Some(parsed)),
                Err(kind) => Err(bad_numeric(value, name, None, kind).report()),
            },
        }
    }

    /// Read `section[.subsection].key` as a pathname, expanding `~/` against
    /// `$HOME` like `git_config_pathname`. Unset keys yield `Ok(None)`; a
    /// failed expansion prints `failed to expand user dir in: '<value>'` and
    /// a value-less bare key prints `missing value for '<name>'`, both as
    /// fatal exit-128 errors.
    pub fn get_path(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<PathBuf>> {
        let name = config_display_name(section, subsection, key);
        match self.get_entry(section, subsection, key) {
            None => Ok(None),
            Some(None) => Err(MissingValueError { name }.report()),
            Some(Some(value)) => {
                let home = home_dir();
                expand_config_path_with_home(value, home.as_deref())
                    .map(Some)
                    .map_err(|error| error.report())
            }
        }
    }
}

impl ConfigStack {
    /// [`GitConfig::get_int`] over the flattened stack; failures carry the
    /// winning entry's origin, adding git's location clause (` in file …`,
    /// ` in blob …`, ` in standard input`) to the diagnostic.
    pub fn get_int(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<i64>> {
        let name = config_display_name(section, subsection, key);
        match self.get(section, subsection, key) {
            None => Ok(None),
            Some(entry) => match entry.value.as_deref() {
                None => Err(bad_numeric(
                    "",
                    name,
                    Some(entry.origin.clone()),
                    BadNumericKind::InvalidUnit,
                )
                .report()),
                Some(value) => match classify_signed(value, i64::MAX) {
                    Ok(parsed) => Ok(Some(parsed)),
                    Err(kind) => {
                        Err(bad_numeric(value, name, Some(entry.origin.clone()), kind).report())
                    }
                },
            },
        }
    }

    /// [`GitConfig::get_size`] over the flattened stack, with the same
    /// origin-attributed diagnostics as [`ConfigStack::get_int`].
    pub fn get_size(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<u64>> {
        let name = config_display_name(section, subsection, key);
        match self.get(section, subsection, key) {
            None => Ok(None),
            Some(entry) => match entry.value.as_deref() {
                None => Err(bad_numeric(
                    "",
                    name,
                    Some(entry.origin.clone()),
                    BadNumericKind::InvalidUnit,
                )
                .report()),
                Some(value) => match classify_unsigned(value, u64::MAX) {
                    Ok(parsed) => Ok(Some(parsed)),
                    Err(kind) => {
                        Err(bad_numeric(value, name, Some(entry.origin.clone()), kind).report())
                    }
                },
            },
        }
    }

    /// [`GitConfig::get_bool_or_int`] over the flattened stack.
    pub fn get_bool_or_int(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<ConfigBoolOrInt>> {
        match self.get(section, subsection, key) {
            None => Ok(None),
            Some(entry) => match entry.value.as_deref() {
                None => Ok(Some(ConfigBoolOrInt::Bool(true))),
                Some(value) => match interpret_bool_or_int(value) {
                    Ok(parsed) => Ok(Some(parsed)),
                    Err(kind) => Err(bad_numeric(
                        value,
                        config_display_name(section, subsection, key),
                        Some(entry.origin.clone()),
                        kind,
                    )
                    .report()),
                },
            },
        }
    }

    /// [`GitConfig::get_path`] over the flattened stack.
    pub fn get_path(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<PathBuf>> {
        match self.get(section, subsection, key) {
            None => Ok(None),
            Some(entry) => {
                let name = config_display_name(section, subsection, key);
                match entry.value.as_deref() {
                    None => Err(MissingValueError { name }.report()),
                    Some(value) => {
                        let home = home_dir();
                        expand_config_path_with_home(value, home.as_deref())
                            .map(Some)
                            .map_err(|error| error.report())
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(value: &str) -> std::result::Result<i64, BadNumericKind> {
        classify_config_int(value)
    }

    fn size(value: &str) -> std::result::Result<u64, BadNumericKind> {
        classify_config_size(value)
    }

    #[test]
    fn valid_units_and_bases() {
        assert_eq!(int("0"), Ok(0));
        assert_eq!(int("42"), Ok(42));
        assert_eq!(int("1k"), Ok(1024));
        assert_eq!(int("1K"), Ok(1024));
        assert_eq!(int("2m"), Ok(2 * 1024 * 1024));
        assert_eq!(int("3G"), Ok(3 * 1024 * 1024 * 1024));
        assert_eq!(int("0xff"), Ok(255));
        assert_eq!(int("0Xff"), Ok(255));
        assert_eq!(int("010"), Ok(8));
        assert_eq!(int("+7"), Ok(7));
        assert_eq!(int("9223372036854775807"), Ok(i64::MAX));
        assert_eq!(int("-9223372036854775808"), Ok(i64::MIN));
        assert_eq!(size("512"), Ok(512));
        assert_eq!(size("4k"), Ok(4096));
        assert_eq!(size("1g"), Ok(1024 * 1024 * 1024));
        assert_eq!(size("18446744073709551615"), Ok(u64::MAX));
    }

    #[test]
    fn negatives_are_signed_only() {
        assert_eq!(int("-1"), Ok(-1));
        assert_eq!(int("-2m"), Ok(-2 * 1024 * 1024));
        assert_eq!(int("-1k"), Ok(-1024));
        // Unsigned rejects any '-', anywhere in the value.
        assert_eq!(size("-1"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(size("10-20"), Err(BadNumericKind::InvalidUnit));
    }

    #[test]
    fn overflow_is_out_of_range() {
        assert_eq!(int("9223372036854775808"), Err(BadNumericKind::OutOfRange));
        assert_eq!(
            int("99999999999999999999999"),
            Err(BadNumericKind::OutOfRange)
        );
        assert_eq!(int("9223372036854775807k"), Err(BadNumericKind::OutOfRange));
        assert_eq!(int("-9223372036854775809"), Err(BadNumericKind::OutOfRange));
        assert_eq!(
            size("18446744073709551616"),
            Err(BadNumericKind::OutOfRange)
        );
        assert_eq!(
            size("18446744073709551615k"),
            Err(BadNumericKind::OutOfRange)
        );
    }

    #[test]
    fn malformed_values_are_invalid_units() {
        assert_eq!(int(""), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("1x"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("1kb"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("1 x"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("1 "), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("x5"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("foo"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("0x"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("018"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(size(""), Err(BadNumericKind::InvalidUnit));
        assert_eq!(size("1x"), Err(BadNumericKind::InvalidUnit));
    }

    #[test]
    fn leading_whitespace_is_skipped_like_strtol() {
        assert_eq!(int("\t\n 42"), Ok(42));
        assert_eq!(int(" 1x"), Err(BadNumericKind::InvalidUnit));
        assert_eq!(int("   "), Err(BadNumericKind::InvalidUnit));
    }

    fn config_with(entries: &[(&str, Option<&str>)]) -> GitConfig {
        let mut text = String::from("[foo]\n");
        for (key, value) in entries {
            match value {
                Some(value) => text.push_str(&format!("\t{key} = {value}\n")),
                None => text.push_str(&format!("\t{key}\n")),
            }
        }
        GitConfig::parse(text.as_bytes()).expect("valid config")
    }

    #[test]
    fn get_int_reads_last_value() {
        let config = config_with(&[("bar", Some("1")), ("bar", Some("2k"))]);
        assert_eq!(config.get_int("foo", None, "bar").ok(), Some(Some(2048)));
        assert_eq!(
            config.get_int("foo", None, "missing").ok(),
            Some::<Option<i64>>(None)
        );
    }

    #[test]
    fn get_int_diagnostics_match_oracle_wording() {
        let config = config_with(&[("bar", Some("1x"))]);
        let err = config
            .get_int("foo", None, "bar")
            .expect_err("bad value must be a fatal");
        assert!(matches!(err, GitError::Exit(128)));
        assert_eq!(err.cli_exit_code(), GitError::Exit(128).cli_exit_code());

        let overflowed = config_with(&[("bar", Some("99999999999999999999999"))]);
        assert!(overflowed.get_int("foo", None, "bar").is_err());

        let bad_number = BadNumericValue {
            value: "1x".into(),
            name: "foo.bar".into(),
            origin: None,
            kind: BadNumericKind::InvalidUnit,
        };
        assert_eq!(
            bad_number.diagnostic(),
            "bad numeric config value '1x' for 'foo.bar': invalid unit"
        );
        let ranged = BadNumericValue {
            origin: Some(ConfigOrigin::file(".git/config")),
            ..bad_number
        };
        assert_eq!(
            ranged.diagnostic(),
            "bad numeric config value '1x' for 'foo.bar' in file .git/config: invalid unit"
        );
        let overflow = BadNumericValue {
            value: "99999999999999999999999".into(),
            name: "foo.bar".into(),
            origin: None,
            kind: BadNumericKind::OutOfRange,
        };
        assert_eq!(
            overflow.diagnostic(),
            "bad numeric config value '99999999999999999999999' for 'foo.bar': out of range"
        );
    }

    #[test]
    fn bare_keys_fail_as_empty_numeric_values() {
        let config = config_with(&[("bar", None)]);
        assert!(config.get_int("foo", None, "bar").is_err());
        assert_eq!(
            BadNumericValue {
                value: String::new(),
                name: "foo.bar".into(),
                origin: None,
                kind: BadNumericKind::InvalidUnit,
            }
            .diagnostic(),
            "bad numeric config value '' for 'foo.bar': invalid unit"
        );
    }

    #[test]
    fn get_size_rejects_negatives_and_reports_units() {
        let config = config_with(&[("limit", Some("2g"))]);
        assert_eq!(
            config.get_size("foo", None, "limit").ok(),
            Some(Some(2 * 1024 * 1024 * 1024))
        );
        let negative = config_with(&[("limit", Some("-5"))]);
        assert!(negative.get_size("foo", None, "limit").is_err());
    }

    #[test]
    fn bool_int_duality() {
        // 'true'/'1' duality through the shared --bool-or-int interpretation.
        assert_eq!(
            interpret_bool_or_int("true"),
            Ok(ConfigBoolOrInt::Bool(true))
        );
        assert_eq!(
            interpret_bool_or_int("TRUE"),
            Ok(ConfigBoolOrInt::Bool(true))
        );
        assert_eq!(
            interpret_bool_or_int("yes"),
            Ok(ConfigBoolOrInt::Bool(true))
        );
        assert_eq!(
            interpret_bool_or_int("off"),
            Ok(ConfigBoolOrInt::Bool(false))
        );
        assert_eq!(interpret_bool_or_int(""), Ok(ConfigBoolOrInt::Bool(false)));
        assert_eq!(interpret_bool_or_int("1"), Ok(ConfigBoolOrInt::Int(1)));
        assert_eq!(interpret_bool_or_int("5k"), Ok(ConfigBoolOrInt::Int(5120)));
        assert_eq!(interpret_bool_or_int("-1"), Ok(ConfigBoolOrInt::Int(-1)));
        assert_eq!(
            interpret_bool_or_int("maybe2"),
            Err(BadNumericKind::InvalidUnit)
        );

        let config = config_with(&[
            ("flag", Some("true")),
            ("num", Some("3")),
            ("bare", None),
            ("blank", Some("")),
        ]);
        assert_eq!(
            config.get_bool_or_int("foo", None, "flag").ok(),
            Some(Some(ConfigBoolOrInt::Bool(true)))
        );
        assert_eq!(
            config.get_bool_or_int("foo", None, "num").ok(),
            Some(Some(ConfigBoolOrInt::Int(3)))
        );
        assert_eq!(
            config.get_bool_or_int("foo", None, "bare").ok(),
            Some(Some(ConfigBoolOrInt::Bool(true)))
        );
        assert_eq!(
            config.get_bool_or_int("foo", None, "blank").ok(),
            Some(Some(ConfigBoolOrInt::Bool(false)))
        );
        assert_eq!(
            config.get_bool_or_int("foo", None, "absent").ok(),
            Some::<Option<ConfigBoolOrInt>>(None)
        );
    }

    #[test]
    fn bad_boolean_diagnostic_has_no_location_clause() {
        let bad = BadBooleanValue {
            value: "maybe2".into(),
            name: "foo.bar".into(),
        };
        assert_eq!(
            bad.diagnostic(),
            "bad boolean config value 'maybe2' for 'foo.bar'"
        );
    }

    #[test]
    fn strict_bool_keywords_exclude_integers() {
        assert_eq!(parse_strict_bool_keyword("true"), Some(true));
        assert_eq!(parse_strict_bool_keyword("ON"), Some(true));
        assert_eq!(parse_strict_bool_keyword("no"), Some(false));
        assert_eq!(parse_strict_bool_keyword(""), Some(false));
        // Unlike the full bool grammar, bare integers are *not* booleans here
        // (oracle rejects `pack.allowPackReuse=1`).
        assert_eq!(parse_strict_bool_keyword("1"), None);
        assert_eq!(parse_strict_bool_keyword("0"), None);
        assert_eq!(parse_strict_bool_keyword("bogus"), None);
    }

    #[test]
    fn path_expansion_cases() {
        let expand = |value: &str, home: Option<&str>| {
            expand_config_path_with_home(value, home)
                .map(|path| path.to_string_lossy().into_owned())
        };
        assert_eq!(
            expand("~/templates", Some("/home/u")),
            Ok("/home/u/templates".into())
        );
        assert_eq!(expand("~", Some("/home/u")), Ok("/home/u".into()));
        assert_eq!(expand("", Some("/home/u")), Ok(String::new()));
        assert_eq!(expand("rel/path", Some("/home/u")), Ok("rel/path".into()));
        assert_eq!(expand("/abs", None), Ok("/abs".into()));
        // No usable HOME: tilde forms fail with git's diagnostic…
        let err = expand("~/templates", None).expect_err("fails");
        assert_eq!(
            err.diagnostic(),
            "failed to expand user dir in: '~/templates'"
        );
        // …and `~user` forms fail even with one (no passwd lookup in sley).
        let err = expand("~other/x", Some("/home/u")).expect_err("fails");
        assert_eq!(err.diagnostic(), "failed to expand user dir in: '~other/x'");
    }

    #[test]
    fn get_path_reports_missing_values_for_bare_keys() {
        let config = config_with(&[("home", None)]);
        assert!(config.get_path("foo", None, "home").is_err());
        assert_eq!(
            MissingValueError {
                name: "foo.home".into()
            }
            .diagnostic(),
            "missing value for 'foo.home'"
        );
    }
}
