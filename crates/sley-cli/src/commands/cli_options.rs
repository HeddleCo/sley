//! Shared helpers for `sley-options` tables in CLI command modules.

use crate::GitError;
use sley_options::{OptFlags, OptValue, OptionSpec, Parsed, ParsedOption, ParsedValue, UsageError};

pub(crate) fn cli_usage_error(error: UsageError) -> GitError {
    eprint!("{}", error.render_stderr());
    GitError::Exit(error.exit_code())
}

pub(crate) fn cli_usage_error_with_code(error: UsageError, exit_code: i32) -> GitError {
    eprint!("{}", error.render_stderr());
    GitError::Exit(exit_code)
}

pub(crate) const fn opt_bool(
    short: Option<char>,
    long: Option<&'static str>,
    flags: OptFlags,
    help: &'static str,
) -> OptionSpec<'static> {
    OptionSpec {
        short,
        long,
        value: OptValue::Bool,
        flags,
        help,
    }
}

pub(crate) const fn opt_str(
    short: Option<char>,
    long: Option<&'static str>,
    metavar: &'static str,
    flags: OptFlags,
    help: &'static str,
) -> OptionSpec<'static> {
    OptionSpec {
        short,
        long,
        value: OptValue::Str(metavar),
        flags,
        help,
    }
}

pub(crate) fn option_bool(option: &ParsedOption<'_>) -> Option<bool> {
    match option.value {
        ParsedValue::Bool(value) => Some(value),
        _ => None,
    }
}

pub(crate) fn option_str<'a>(option: &'a ParsedOption<'a>) -> Option<&'a str> {
    match option.value {
        ParsedValue::Str(value) => Some(value),
        _ => None,
    }
}

pub(crate) fn last_tri_state_bool(parsed: &Parsed<'_>, long: &str) -> Option<bool> {
    parsed
        .options
        .iter()
        .filter(|option| option.long == Some(long))
        .filter_map(option_bool)
        .last()
}

pub(crate) fn count_force_occurrences(parsed: &Parsed<'_>) -> usize {
    let mut force = 0usize;
    for option in &parsed.options {
        if option.short == Some('f') || option.long == Some("force") {
            match option.value {
                ParsedValue::Bool(true) => force += 1,
                ParsedValue::Bool(false) => force = 0,
                _ => {}
            }
        }
    }
    force
}