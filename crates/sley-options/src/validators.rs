//! Shared CLI option value validators.

use sley_core::{GitError, Result};

pub fn log_inter_hunk_context_requires_number_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' expects a numerical value");
    Err(GitError::Exit(129))
}

pub fn log_validate_similarity_option(value: &str, option: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let digits = value.strip_suffix('%').unwrap_or(value);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(());
    }
    eprintln!("error: invalid argument to {option}");
    Err(GitError::Exit(129))
}

pub fn log_validate_break_rewrites_option(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let mut parts = value.split('/');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some() {
        return log_break_rewrites_form_error();
    }
    if log_valid_break_rewrites_part(first) && second.is_none_or(log_valid_break_rewrites_part) {
        return Ok(());
    }
    log_break_rewrites_form_error()
}

pub fn log_valid_break_rewrites_part(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let digits = value.strip_suffix('%').unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn log_break_rewrites_form_error() -> Result<()> {
    eprintln!("error: break-rewrites expects <n>/<m> form");
    Err(GitError::Exit(129))
}

pub fn log_validate_diff_merges(value: &str) -> Result<()> {
    match value {
        "off" | "none" => Ok(()),
        "" => log_diff_merges_invalid_value(value),
        "on" | "first-parent" | "1" | "separate" | "m" | "combined" | "c" | "dense-combined"
        | "cc" | "remerge" | "r" => Err(GitError::Command(format!(
            "unsupported log option --diff-merges={value}"
        ))),
        _ => log_diff_merges_invalid_value(value),
    }
}

pub fn log_diff_merges_invalid_value(value: &str) -> Result<()> {
    eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
    Err(GitError::Exit(128))
}
pub fn log_validate_diff_algorithm(value: &str) -> Result<()> {
    match value {
        "myers" | "minimal" | "patience" | "histogram" | "default" => Ok(()),
        _ => {
            eprintln!(
                "error: option diff-algorithm accepts \"myers\", \"minimal\", \"patience\" and \"histogram\""
            );
            Err(GitError::Exit(129))
        }
    }
}
pub fn log_validate_inter_hunk_context(value: &str) -> Result<()> {
    let number = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    let digits = match number.as_bytes().first() {
        Some(b'+') if number.len() > 1 => &number[1..],
        _ => number,
    };
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(());
    }
    eprintln!(
        "error: option `inter-hunk-context' expects a non-negative integer value with an optional k/m/g suffix"
    );
    Err(GitError::Exit(129))
}

pub fn log_validate_output_indicator(option: &str, value: &str) -> Result<()> {
    // git's diff_opt_char (diff.c) requires exactly one byte; empty and multibyte
    // values are rejected.
    if value.len() == 1 {
        return Ok(());
    }
    if value.is_empty() {
        eprintln!("error: {option} expects a character, got ''");
    } else {
        eprintln!("error: {option} expects a character, got '{value}'");
    }
    Err(GitError::Exit(129))
}

pub fn log_validate_output_indicator_for_log(option: &str, value: &str) -> Result<()> {
    log_validate_output_indicator(option, value)
}

pub fn log_validate_submodule_format(value: &str) -> Result<()> {
    match value {
        "short" | "log" | "diff" => Ok(()),
        _ => {
            eprintln!("error: failed to parse --submodule option parameter: '{value}'");
            Err(GitError::Exit(129))
        }
    }
}

pub fn log_validate_ignore_submodules(value: &str) -> Result<()> {
    match value {
        "none" | "untracked" | "dirty" | "all" => Ok(()),
        _ => {
            eprintln!("fatal: bad --ignore-submodules argument: {value}");
            Err(GitError::Exit(128))
        }
    }
}

pub fn log_validate_color_moved(value: &str) -> Result<()> {
    match value {
        "" | "no" | "default" | "blocks" | "zebra" | "dimmed-zebra" | "plain" | "true" | "1"
        | "on" | "yes" | "false" | "0" | "off" => Ok(()),
        _ => {
            eprintln!(
                "error: color moved setting must be one of 'no', 'default', 'blocks', 'zebra', 'dimmed-zebra', 'plain'"
            );
            eprintln!("error: bad --color-moved argument: {value}");
            Err(GitError::Exit(129))
        }
    }
}

pub fn log_validate_color(value: &str) -> Result<()> {
    match value {
        "always" | "auto" | "never" => Ok(()),
        _ => {
            eprintln!("error: option `color' expects \"always\", \"auto\", or \"never\"");
            Err(GitError::Exit(129))
        }
    }
}

pub fn log_validate_color_moved_ws(value: &str) -> Result<()> {
    let mut has_allow_indentation_change = false;
    let mut mode_count = 0usize;
    for mode in value.split(',') {
        mode_count += 1;
        match mode {
            "no" | "ignore-space-change" | "ignore-space-at-eol" | "ignore-all-space" => {}
            "allow-indentation-change" => has_allow_indentation_change = true,
            _ => return log_color_moved_ws_invalid_mode(value, mode),
        }
    }
    if has_allow_indentation_change && mode_count > 1 {
        eprintln!(
            "error: color-moved-ws: allow-indentation-change cannot be combined with other whitespace modes"
        );
        eprintln!("error: invalid mode '{value}' in --color-moved-ws");
        return Err(GitError::Exit(129));
    }
    Ok(())
}

pub fn log_color_moved_ws_invalid_mode(value: &str, mode: &str) -> Result<()> {
    eprintln!(
        "error: unknown color-moved-ws mode '{mode}', possible values are 'ignore-space-change', 'ignore-space-at-eol', 'ignore-all-space', 'allow-indentation-change'"
    );
    eprintln!("error: invalid mode '{value}' in --color-moved-ws");
    Err(GitError::Exit(129))
}

pub fn log_validate_ws_error_highlight(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let mut valid_prefix = String::new();
    for mode in value.split(',') {
        match mode {
            "old" | "new" | "context" | "all" | "none" | "default" => {
                valid_prefix.push_str(mode);
                valid_prefix.push(',');
            }
            _ => {
                eprintln!("error: unknown value after ws-error-highlight={valid_prefix}");
                return Err(GitError::Exit(129));
            }
        }
    }
    Ok(())
}
