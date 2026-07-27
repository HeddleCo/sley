use super::{ReflogFormat, ReflogShowOptions};
use crate::*;

pub(super) fn setup_reflog_show_options(
    store: &FileRefStore,
    git_dir: &Path,
    object_format: ObjectFormat,
    args: &[String],
) -> Result<ReflogShowOptions> {
    let mut args = args;
    if args.first().is_some_and(|arg| arg == "show") {
        args = &args[1..];
    }
    let mut format = ReflogFormat::Default;
    let mut max_count = None;
    let mut abbrev_commit = None;
    let mut abbrev_len = None;
    let mut date_mode = None;
    let mut refs = Vec::new();
    let mut pathspecs = Vec::new();
    let mut grep_patterns = Vec::new();
    let mut grep_pattern_kind = sley_grep::PatternKind::Basic;
    let mut grep_pattern_kind_explicit = false;
    let mut grep_ignore_case = false;
    let mut grep_all_match = false;
    let mut grep_invert = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--" => {
                pathspecs.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--oneline" => format = ReflogFormat::Default,
            "--abbrev-commit" => abbrev_commit = Some(true),
            "--no-abbrev-commit" => abbrev_commit = Some(false),
            "--abbrev" => {
                // Bare `--abbrev` restores the default short width.
                abbrev_len = Some(7);
                abbrev_commit = Some(true);
            }
            "--no-abbrev" => {
                abbrev_len = None;
                abbrev_commit = Some(false);
            }
            value if value.starts_with("--abbrev=") => {
                let width = value["--abbrev=".len()..]
                    .parse::<usize>()
                    .map_err(|_| GitError::Command(format!("invalid --abbrev value: {value}")))?;
                abbrev_len = Some(width);
                abbrev_commit = Some(true);
            }
            "--date" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Command("--date requires a value".into()));
                };
                date_mode = Some(crate::log_cli::log_date_mode(value)?);
            }
            value if value.starts_with("--date=") => {
                date_mode = Some(crate::log_cli::log_date_mode(&value["--date=".len()..])?);
            }
            "--format=%H" | "--pretty=%H" => {
                format = ReflogFormat::NewOid {
                    final_newline: true,
                };
            }
            "--format=%gs" | "--pretty=%gs" => {
                format = ReflogFormat::Message {
                    final_newline: true,
                };
            }
            "--pretty=format:%H" | "--format=format:%H" => {
                format = ReflogFormat::NewOid {
                    final_newline: false,
                };
            }
            "--pretty=format:%gs" | "--format=format:%gs" => {
                format = ReflogFormat::Message {
                    final_newline: false,
                };
            }
            "--format" | "--pretty" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Command(format!("{arg} requires a value")));
                };
                match value.as_str() {
                    "%H" => {
                        format = ReflogFormat::NewOid {
                            final_newline: true,
                        };
                    }
                    "%gs" => {
                        format = ReflogFormat::Message {
                            final_newline: true,
                        };
                    }
                    "format:%H" => {
                        format = ReflogFormat::NewOid {
                            final_newline: false,
                        };
                    }
                    "format:%gs" => {
                        format = ReflogFormat::Message {
                            final_newline: false,
                        };
                    }
                    "oneline" => format = ReflogFormat::Default,
                    _ => {
                        return Err(GitError::Unsupported(
                            "reflog currently supports only --format=%gs".into(),
                        ));
                    }
                }
            }
            value if let Some(count) = value.strip_prefix("--max-count=") => {
                max_count = Some(parse_reflog_count(count)?);
            }
            "--max-count" | "-n" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Command(format!("{arg} requires a value")));
                };
                max_count = Some(parse_reflog_count(value)?);
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                max_count = Some(parse_reflog_count(&value[2..])?);
            }
            "--grep" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Command("--grep requires a value".into()));
                };
                grep_patterns.push(value.clone());
            }
            value if let Some(pattern) = value.strip_prefix("--grep=") => {
                grep_patterns.push(pattern.to_string());
            }
            "--all-match" => grep_all_match = true,
            "--invert-grep" => grep_invert = true,
            "-i" | "--regexp-ignore-case" => grep_ignore_case = true,
            "-F" | "--fixed-strings" => {
                grep_pattern_kind = sley_grep::PatternKind::Fixed;
                grep_pattern_kind_explicit = true;
            }
            "--basic-regexp" => {
                grep_pattern_kind = sley_grep::PatternKind::Basic;
                grep_pattern_kind_explicit = true;
            }
            "-E" | "--extended-regexp" => {
                grep_pattern_kind = sley_grep::PatternKind::Extended;
                grep_pattern_kind_explicit = true;
            }
            "-P" | "--perl-regexp" => {
                grep_pattern_kind = sley_grep::PatternKind::Perl;
                grep_pattern_kind_explicit = true;
            }
            value
                if value.starts_with('-')
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                max_count = Some(parse_reflog_count(&value[1..])?);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Unsupported(format!(
                    "unsupported reflog option {value}"
                )));
            }
            value => refs.push(value.to_string()),
        }
        index += 1;
    }
    if refs.len() > 1 {
        return Err(GitError::Command(
            "reflog show currently accepts at most one ref".into(),
        ));
    }
    let display = refs.first().cloned().unwrap_or_else(|| "HEAD".to_string());
    let reference = reflog_reference_name(
        store,
        git_dir,
        object_format,
        refs.first().map(String::as_str),
    )?;
    Ok(ReflogShowOptions {
        reference,
        display,
        format,
        max_count,
        abbrev_commit,
        abbrev_len,
        date_mode,
        pathspecs,
        grep_patterns,
        grep_pattern_kind,
        grep_pattern_kind_explicit,
        grep_ignore_case,
        grep_all_match,
        grep_invert,
    })
}

pub(super) fn setup_show_ref_short_options(
    value: &str,
    quiet: &mut bool,
    hash_only: &mut bool,
    dereference: &mut bool,
    abbrev: &mut Option<usize>,
) -> Result<bool> {
    let Some(flags) = value.strip_prefix('-') else {
        return Ok(false);
    };
    if flags.is_empty() || flags.starts_with('s') {
        return Ok(false);
    }
    for (index, flag) in flags.char_indices() {
        match flag {
            'd' => *dereference = true,
            'q' => *quiet = true,
            's' => {
                *hash_only = true;
                let width = &flags[index + flag.len_utf8()..];
                if !width.is_empty() {
                    *abbrev = Some(parse_abbrev(width)?);
                }
                return Ok(true);
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}
