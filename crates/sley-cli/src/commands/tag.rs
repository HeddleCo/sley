//! `git tag` (create/list/delete/verify) and tag-message helpers.

// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::*;
use std::borrow::Cow;

/// Split a bundle of short tag options whose trailing flag takes a value
/// (e.g. `-am` => `[-a, -m]`), the way git's `parse_options` does. Returns
/// `Some(tokens)` only when `arg` is a run of boolean short flags
/// (`-a/-s/-f/-d/-v/-e/-i/-l`) terminated by a value-taking flag (`-m/-u/-F`),
/// keeping any glued value so the existing per-flag arms (`-m<msg>`, `-u<key>`)
/// see it. Everything else — pure-boolean bundles (`-av`, `-fl`, …), long
/// options, `-n<num>`, glued value flags — returns `None` and is parsed
/// verbatim, preserving its (often error) semantics. The caller only consults
/// this in option position, so an option's `-`-prefixed value (e.g.
/// `--sort -authoremail`) is never misread as a bundle.
fn expand_tag_bundle(arg: &str) -> Option<Vec<String>> {
    const BOOL_FLAGS: &[u8] = b"asfdveil";
    const VALUE_FLAGS: &[u8] = b"muF";
    let bytes = arg.as_bytes();
    if !arg.starts_with('-') || arg.starts_with("--") || bytes.len() < 2 || !BOOL_FLAGS.contains(&bytes[1])
    {
        return None;
    }
    let mut tokens = Vec::new();
    let mut idx = 1;
    while idx < bytes.len() {
        let ch = bytes[idx];
        if BOOL_FLAGS.contains(&ch) {
            tokens.push(format!("-{}", ch as char));
            idx += 1;
        } else if VALUE_FLAGS.contains(&ch) {
            tokens.push(format!("-{}", &arg[idx..]));
            return Some(tokens);
        } else {
            return None;
        }
    }
    None
}

pub(crate) fn cmd_tag(args: &[String]) -> Result<()> {
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    if args.is_empty() {
        return print_default_tag_list(&git_dir, format, &store);
    }

    let mut annotated = false;
    let mut annotated_explicit = false;
    let mut signed = false;
    let mut sign_explicit = false;
    let mut signing_key: Option<String> = None;
    let mut force = false;
    let mut delete = false;
    let mut verify = false;
    let mut list = false;
    let mut explicit_list = false;
    let mut edit = false;
    let mut edit_disabled = false;
    let mut ignore_case = false;
    // Seed sort keys from `tag.sort` config (in config order). Git reads these
    // before parsing the command line, so command-line `--sort` keys are
    // appended after the config keys; the last key parsed is the primary sort
    // (sort_tag_entries iterates the keys in reverse). `--no-sort` clears all
    // accumulated keys, config included.
    let config = read_repo_config(&git_dir).unwrap_or_default();
    let mut sorts = Vec::new();
    for value in config.get_all("tag", None, "sort").into_iter().flatten() {
        sorts.push(parse_tag_list_sort(value)?);
    }
    let mut format_spec = None;
    let mut annotation_lines = None;
    let mut omit_empty = false;
    let mut color = false;
    let mut color_explicit = false;
    let config_column = tag_list_column_from_config(&config);
    let mut column = TagListColumn::None;
    let mut column_explicit = false;
    let mut points_at = Vec::new();
    let mut contains = Vec::new();
    let mut no_contains = Vec::new();
    let mut merged = Vec::new();
    let mut no_merged = Vec::new();
    let mut messages = Vec::new();
    let mut file_message = None;
    let mut trailers = Vec::new();
    let mut create_reflog = false;
    let mut cleanup_mode = TagCleanupMode::Strip;
    let mut empty_file_noop = false;
    let mut positional = Vec::new();
    let mut iter = args.iter().peekable();
    // Short-flag bundles whose trailing flag takes a value (`-am`) are split into
    // separate tokens pushed back here, so the existing per-flag arms handle them.
    // Because a bundle's value flag is always its last token, value-taking arms
    // still read their argument from `iter` (the queue is empty by then), and an
    // option's `-`-prefixed value is consumed before it could be misread.
    let mut pending: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    loop {
        let owned = match pending.pop_front() {
            Some(arg) => arg,
            None => match iter.next() {
                Some(arg) => arg.clone(),
                None => break,
            },
        };
        if let Some(tokens) = expand_tag_bundle(&owned) {
            for token in tokens.into_iter().rev() {
                pending.push_front(token);
            }
            continue;
        }
        let arg = &owned;
        match arg.as_str() {
            "--" => {
                positional.extend(pending.drain(..));
                positional.extend(iter.cloned());
                break;
            }
            "-a" | "--annotate" => {
                annotated = true;
                annotated_explicit = true;
            }
            "--no-annotate" => annotated = false,
            "-s" | "--sign" => {
                signed = true;
                sign_explicit = true;
                annotated = true;
            }
            "--no-sign" => {
                signed = false;
                sign_explicit = true;
            }
            "-u" => {
                let Some(value) = iter.next() else {
                    return tag_local_user_requires_value_error("u", true);
                };
                signing_key = Some(value.to_string());
                signed = true;
                sign_explicit = true;
                annotated = true;
            }
            "--local-user" => {
                let Some(value) = iter.next() else {
                    return tag_local_user_requires_value_error("local-user", false);
                };
                signing_key = Some(value.to_string());
                signed = true;
                sign_explicit = true;
                annotated = true;
            }
            "--no-local-user" => {
                signed = false;
                sign_explicit = true;
            }
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-d" | "--delete" => delete = true,
            "-v" | "--verify" => verify = true,
            "--no-delete" => return tag_unknown_option_error("no-delete"),
            "--no-verify" => return tag_unknown_option_error("no-verify"),
            "--no-list" => return tag_unknown_option_error("no-list"),
            value
                if value.starts_with("--no-delete=")
                    || value.starts_with("--no-verify=")
                    || value.starts_with("--no-list=") =>
            {
                return tag_unknown_option_error(value.trim_start_matches("--"));
            }
            "--create-reflog" => create_reflog = true,
            "--no-create-reflog" => create_reflog = false,
            "-l" | "--list" => {
                list = true;
                explicit_list = true;
            }
            "-n" => {
                list = true;
                annotation_lines = Some(1);
            }
            "--column" => {
                list = true;
                column = TagListColumn::Aligned;
                column_explicit = true;
            }
            "--no-column" | "--column=auto" | "--column=never" | "--column=plain" => {
                list = true;
                column = TagListColumn::None;
                column_explicit = true;
            }
            "--omit-empty" => {
                list = true;
                omit_empty = true;
            }
            "--no-omit-empty" => {
                list = true;
                omit_empty = false;
            }
            "--format" => {
                let Some(value) = iter.next() else {
                    return tag_option_requires_value_error("format");
                };
                list = true;
                format_spec = Some(value.to_string());
            }
            "--no-format" => {
                list = true;
                format_spec = None;
            }
            "--color" | "--color=always" => {
                list = true;
                color = true;
                color_explicit = true;
            }
            "--no-color" | "--color=auto" | "--color=never" => {
                list = true;
                color = false;
                color_explicit = true;
            }
            "-i" | "--ignore-case" => {
                list = true;
                ignore_case = true;
            }
            "--no-ignore-case" => {
                list = true;
                ignore_case = false;
            }
            "--sort" => {
                let Some(value) = iter.next() else {
                    return tag_option_requires_value_error("sort");
                };
                list = true;
                sorts.push(parse_tag_list_sort(value)?);
            }
            "--no-sort" => {
                list = true;
                sorts.clear();
            }
            "--points-at" => {
                points_at.push(
                    iter.next()
                        .map_or_else(|| "HEAD".to_string(), |value| value.to_string()),
                );
            }
            "--no-points-at" => {
                list = true;
                points_at.clear();
            }
            "--contains" => {
                let value = if let Some(value) = iter.next() {
                    value.to_string()
                } else {
                    "HEAD".to_string()
                };
                contains.push(value);
            }
            "--no-contains" => {
                let value = if let Some(value) = iter.next() {
                    value.to_string()
                } else {
                    "HEAD".to_string()
                };
                no_contains.push(value);
            }
            "--merged" => {
                let value = iter
                    .next()
                    .map_or_else(|| "HEAD".to_string(), |value| value.to_string());
                merged.push(value);
            }
            "--no-merged" => {
                let value = iter
                    .next()
                    .map_or_else(|| "HEAD".to_string(), |value| value.to_string());
                no_merged.push(value);
            }
            // `--with`/`--without` are hidden aliases for `--contains`/
            // `--no-contains` (parse-options.h OPT_WITH/OPT_WITHOUT). Like their
            // canonical forms they take an optional commit-ish defaulting to HEAD.
            "--with" => {
                let value = iter
                    .next()
                    .map_or_else(|| "HEAD".to_string(), |value| value.to_string());
                contains.push(value);
            }
            "--without" => {
                let value = iter
                    .next()
                    .map_or_else(|| "HEAD".to_string(), |value| value.to_string());
                no_contains.push(value);
            }
            value if let Some(rev) = value.strip_prefix("--with=") => {
                contains.push(rev.to_string());
            }
            value if let Some(rev) = value.strip_prefix("--without=") => {
                no_contains.push(rev.to_string());
            }
            "-m" => {
                let Some(message) = iter.next() else {
                    return tag_message_requires_value_error();
                };
                messages.push(message.as_bytes().to_vec());
            }
            "--message" => {
                let Some(message) = iter.next() else {
                    return tag_option_requires_value_error("message");
                };
                messages.push(message.as_bytes().to_vec());
            }
            "--trailer" => {
                let Some(value) = iter.next() else {
                    return tag_trailer_requires_value_error();
                };
                trailers.push(parse_tag_trailer(value));
            }
            "--no-trailer" => trailers.clear(),
            "-F" | "--file" => {
                let Some(path) = iter.next() else {
                    return if arg == "-F" {
                        tag_file_requires_value_error()
                    } else {
                        tag_option_requires_value_error("file")
                    };
                };
                file_message = Some(read_commit_message_file(path)?);
            }
            "--no-file" => {}
            "--cleanup" => {
                let Some(value) = iter.next() else {
                    return tag_cleanup_requires_value_error();
                };
                cleanup_mode = parse_tag_cleanup_mode(value)?;
            }
            "--no-cleanup" => cleanup_mode = TagCleanupMode::Strip,
            "-e" | "--edit" => {
                annotated = true;
                edit = true;
                edit_disabled = false;
            }
            "--no-edit" => {
                edit = false;
                edit_disabled = true;
            }
            value
                if value.len() >= 3
                    && matches!(
                        &value[..3],
                        "-a=" | "-s=" | "-f=" | "-d=" | "-v=" | "-l=" | "-e=" | "-i="
                    ) =>
            {
                return tag_unknown_switch_error('=');
            }
            value if value.starts_with("--points-at=") => {
                let value = value
                    .strip_prefix("--points-at=")
                    .ok_or_else(|| GitError::Command("tag --points-at requires a value".into()))?;
                points_at.push(value.to_string());
            }
            value if value.starts_with("--contains=") => {
                let value = value
                    .strip_prefix("--contains=")
                    .ok_or_else(|| GitError::Command("tag --contains requires a value".into()))?;
                contains.push(value.to_string());
            }
            value if value.starts_with("--no-contains=") => {
                let value = value.strip_prefix("--no-contains=").ok_or_else(|| {
                    GitError::Command("tag --no-contains requires a value".into())
                })?;
                no_contains.push(value.to_string());
            }
            value if value.starts_with("--merged=") => {
                let value = value
                    .strip_prefix("--merged=")
                    .ok_or_else(|| GitError::Command("tag --merged requires a value".into()))?;
                merged.push(value.to_string());
            }
            value if value.starts_with("--no-merged=") => {
                let value = value
                    .strip_prefix("--no-merged=")
                    .ok_or_else(|| GitError::Command("tag --no-merged requires a value".into()))?;
                no_merged.push(value.to_string());
            }
            value if value.starts_with("--sort=") => {
                let value = value
                    .strip_prefix("--sort=")
                    .ok_or_else(|| GitError::Command("tag --sort requires a value".into()))?;
                list = true;
                sorts.push(parse_tag_list_sort(value)?);
            }
            value if value.starts_with("--column=") => {
                list = true;
                column = parse_tag_list_column(&value["--column=".len()..])?;
                column_explicit = true;
            }
            value if value.starts_with("--no-column=") => {
                return tag_option_takes_no_value_error("no-column");
            }
            value if value.starts_with("--color=") => {
                list = true;
                color = parse_tag_list_color(&value["--color=".len()..])?;
                color_explicit = true;
            }
            value
                if value.starts_with("--no-color=")
                    || value.starts_with("--no-format=")
                    || value.starts_with("--no-sort=")
                    || value.starts_with("--no-points-at=")
                    || value.starts_with("--omit-empty=")
                    || value.starts_with("--no-omit-empty=")
                    || value.starts_with("--ignore-case=")
                    || value.starts_with("--no-ignore-case=")
                    || value.starts_with("--list=")
                    || value.starts_with("--delete=") =>
            {
                let option = value
                    .trim_start_matches("--")
                    .split_once('=')
                    .map(|(option, _)| option)
                    .unwrap_or(value);
                return tag_option_takes_no_value_error(option);
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                list = true;
                annotation_lines = Some(parse_tag_list_annotation_lines(&value[2..])?);
            }
            value if value.starts_with("--format=") => {
                let value = value
                    .strip_prefix("--format=")
                    .ok_or_else(|| GitError::Command("tag --format requires a value".into()))?;
                list = true;
                format_spec = Some(value.to_string());
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                messages.push(value.as_bytes()[2..].to_vec());
            }
            value if value.starts_with("--message=") => {
                messages.push(value.as_bytes()["--message=".len()..].to_vec());
            }
            value if value.starts_with("-u") && value.len() > 2 => {
                signing_key = Some(value[2..].to_string());
                signed = true;
                sign_explicit = true;
                annotated = true;
            }
            value if value.starts_with("--local-user=") => {
                signing_key = Some(value["--local-user=".len()..].to_string());
                signed = true;
                sign_explicit = true;
                annotated = true;
            }
            value
                if value.starts_with("--annotate=")
                    || value.starts_with("--sign=")
                    || value.starts_with("--force=")
                    || value.starts_with("--create-reflog=")
                    || value.starts_with("--verify=")
                    || value.starts_with("--edit=") =>
            {
                let option = value
                    .trim_start_matches("--")
                    .split_once('=')
                    .map(|(option, _)| option)
                    .unwrap_or(value);
                return tag_option_takes_no_value_error(option);
            }
            value
                if value.starts_with("--no-annotate=")
                    || value.starts_with("--no-sign=")
                    || value.starts_with("--no-local-user=")
                    || value.starts_with("--no-force=")
                    || value.starts_with("--no-create-reflog=")
                    || value.starts_with("--no-file=")
                    || value.starts_with("--no-cleanup=")
                    || value.starts_with("--no-edit=") =>
            {
                let option = value
                    .trim_start_matches("--")
                    .split_once('=')
                    .map(|(option, _)| option)
                    .unwrap_or(value);
                return tag_option_takes_no_value_error(option);
            }
            value if value.starts_with("--no-trailer=") => {
                return tag_option_takes_no_value_error("no-trailer");
            }
            value if value.starts_with("--trailer=") => {
                trailers.push(parse_tag_trailer(&value["--trailer=".len()..]));
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                file_message = Some(read_commit_message_file(&value[2..])?);
            }
            value if value.starts_with("--file=") => {
                let path = &value["--file=".len()..];
                if path.is_empty() {
                    empty_file_noop = true;
                } else {
                    file_message = Some(read_commit_message_file(path)?);
                }
            }
            value if value.starts_with("--cleanup=") => {
                cleanup_mode = parse_tag_cleanup_mode(&value["--cleanup=".len()..])?;
            }
            value if value.starts_with("--") => {
                return tag_unknown_option_error(value.trim_start_matches("--"));
            }
            value if value.starts_with('-') && value.len() > 1 => {
                if let Some(switch) = tag_unknown_short_switch(value) {
                    return tag_unknown_switch_error(switch);
                }
                return tag_usage_error();
            }
            value => positional.push(value.to_string()),
        }
    }
    if file_message.is_some() && !messages.is_empty() {
        eprintln!("fatal: options '-F' and '-m' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if !messages.is_empty() || file_message.is_some() || !trailers.is_empty() {
        annotated = true;
    }
    if !sign_explicit {
        if config
            .get_bool("tag", None, "gpgsign")
            .or_else(|| config.get_bool("tag", None, "gpgSign"))
            .unwrap_or(false)
        {
            signed = true;
            annotated = true;
        } else if annotated
            && !annotated_explicit
            && config
                .get_bool("tag", None, "forcesignannotated")
                .or_else(|| config.get_bool("tag", None, "forceSignAnnotated"))
                .unwrap_or(false)
        {
            signed = true;
        }
    }
    if empty_file_noop
        && positional.is_empty()
        && !annotated
        && !signed
        && !force
        && !delete
        && !verify
        && !list
        && !explicit_list
        && !ignore_case
        && sorts.is_empty()
        && format_spec.is_none()
        && annotation_lines.is_none()
        && !omit_empty
        && !color
        && column == TagListColumn::None
        && points_at.is_empty()
        && contains.is_empty()
        && no_contains.is_empty()
        && merged.is_empty()
        && no_merged.is_empty()
        && messages.is_empty()
        && file_message.is_none()
        && trailers.is_empty()
        && !create_reflog
        && !edit
        && !edit_disabled
    {
        return print_default_tag_list(&git_dir, format, &store);
    }
    // A ref-filter (`--contains`/`--no-contains`/`--points-at`/`--merged`/
    // `--no-merged`) or `-n` is "only allowed in list mode": when an explicit
    // non-list cmdmode (`-d`/`-v`) is also given git dies naming the offending
    // option (builtin/tag.c `only_in_list`). Precedence mirrors git's chain:
    // `-n` first, then contains, no-contains, points-at, merged, no-merged.
    if delete || verify {
        let only_in_list = if annotation_lines.is_some() {
            Some("-n")
        } else if !contains.is_empty() {
            Some("--contains")
        } else if !no_contains.is_empty() {
            Some("--no-contains")
        } else if !points_at.is_empty() {
            Some("--points-at")
        } else if !merged.is_empty() {
            Some("--merged")
        } else if !no_merged.is_empty() {
            Some("--no-merged")
        } else {
            None
        };
        if let Some(option) = only_in_list {
            eprintln!("fatal: the '{option}' option is only allowed in list mode");
            return Err(GitError::Exit(128));
        }
    }
    if verify {
        if explicit_list {
            eprintln!("error: options '-l' and '-v' cannot be used together");
            return Err(GitError::Exit(129));
        }
        if annotation_lines.is_some() {
            eprintln!("fatal: the '-n' option is only allowed in list mode");
            return Err(GitError::Exit(128));
        }
        if annotated || force || delete || !messages.is_empty() || file_message.is_some() {
            return Err(GitError::Command(
                "tag verification currently supports: tag -v [--format=<format>] <tagname>..."
                    .into(),
            ));
        }
        return verify_tags(
            &git_dir,
            &store,
            &FileObjectDatabase::from_git_dir(&git_dir, format),
            format,
            format_spec.as_deref(),
            &positional,
        );
    }
    if list
        || !points_at.is_empty()
        || !contains.is_empty()
        || !no_contains.is_empty()
        || !merged.is_empty()
        || !no_merged.is_empty()
    {
        if annotated || force || delete || !messages.is_empty() || file_message.is_some() {
            return Err(GitError::Command(
                "tag listing currently supports: tag [-l|--list] [--format <format>|--no-format] [--sort <key>|--no-sort] [-i|--ignore-case|--no-ignore-case] [--omit-empty|--no-omit-empty] [--points-at <object-ish>|--no-points-at|--contains <commit-ish>|--no-contains <commit-ish>|--merged [<commit-ish>]|--no-merged [<commit-ish>]] [<pattern>...]".into(),
            ));
        }
        if column_explicit && column != TagListColumn::None && annotation_lines.is_some() {
            eprintln!("fatal: options '--column' and '-n' cannot be used together");
            return Err(GitError::Exit(128));
        }
        let column = if column_explicit {
            column
        } else if annotation_lines.is_none() {
            config_column
        } else {
            TagListColumn::None
        };
        let points_at = points_at
            .iter()
            .map(|rev| resolve_tag_points_at_filter(&git_dir, format, rev))
            .collect::<Result<Vec<_>>>()?;
        let contains = contains
            .iter()
            .map(|rev| resolve_tag_contains_filter(&git_dir, format, rev))
            .collect::<Result<Vec<_>>>()?;
        let no_contains = no_contains
            .iter()
            .map(|rev| resolve_tag_contains_filter(&git_dir, format, rev))
            .collect::<Result<Vec<_>>>()?;
        let merged = merged
            .iter()
            .map(|rev| resolve_tag_merged_filter(&git_dir, format, rev))
            .collect::<Result<Vec<_>>>()?;
        let no_merged = no_merged
            .iter()
            .map(|rev| resolve_tag_merged_filter(&git_dir, format, rev))
            .collect::<Result<Vec<_>>>()?;
        let prereleases = if sorts.iter().any(|sort| {
            matches!(
                sort,
                TagListSort::VersionRefname | TagListSort::VersionRefnameDescending
            )
        }) {
            resolve_versionsort_prereleases(&config)
        } else {
            Vec::new()
        };
        print_tag_list(
            &git_dir,
            format,
            &store,
            TagListOptions {
                patterns: &positional,
                ignore_case,
                sorts: &sorts,
                prereleases: &prereleases,
                format_spec: format_spec.as_deref(),
                annotation_lines,
                omit_empty,
                color: if color_explicit {
                    color
                } else {
                    tag_color_enabled_from_config(&config)
                },
                column,
                points_at: &points_at,
                contains: &contains,
                no_contains: &no_contains,
                merged: &merged,
                no_merged: &no_merged,
            },
        )?;
        return Ok(());
    }
    if delete {
        if annotated || force || !messages.is_empty() || file_message.is_some() {
            return Err(GitError::Command(
                "tag deletion currently supports: tag -d <name>...".into(),
            ));
        }
        return delete_tags(&store, &positional);
    }
    let (tag, target) = match positional.as_slice() {
        [tag] => (tag.as_str(), "HEAD"),
        [tag, target] => (tag.as_str(), target.as_str()),
        _ => {
            eprintln!("fatal: too many arguments");
            return Err(GitError::Exit(128));
        }
    };
    let target_oid = resolve_tag_target(&git_dir, format, target)?;
    if annotated {
        let has_message_source = !messages.is_empty() || file_message.is_some();
        let use_editor = edit || (!edit_disabled && !has_message_source);
        if !use_editor && messages.is_empty() && file_message.is_none() && trailers.is_empty() {
            eprintln!("fatal: no tag message?");
            return Err(GitError::Exit(128));
        }
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let target_object = db.read_object(&target_oid)?;
        let mut message = if let Some(message) = file_message {
            message
        } else if messages.is_empty() {
            if force {
                existing_annotated_tag_message(&store, &db, format, tag)?.unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            tag_message_from_chunks_verbatim(&messages)
        };
        let mut editmsg_written = false;
        if use_editor {
            message = tag_message_for_editor(message, &trailers);
            message = launch_tag_editor(&git_dir, tag, &message)?;
            editmsg_written = true;
        } else {
            message =
                tag_message_with_trailers(tag_cleanup_message(message, cleanup_mode), &trailers);
        }
        if use_editor {
            message = tag_cleanup_message(message, cleanup_mode);
            if message.is_empty() {
                eprintln!("fatal: no tag message?");
                return Err(GitError::Exit(128));
            }
        }
        let tagger = commit_identity_from_env("COMMITTER")?;
        if signed {
            let key =
                commands::signing::signing_key(Some(&config), signing_key.as_deref(), &tagger);
            message = sign_tag_message(
                &target_oid,
                target_object.object_type,
                tag.as_bytes(),
                &tagger,
                message,
                Some(&config),
                key.as_deref(),
            )?;
        }
        let tag_oid = sley_sequencer::create_annotated_tag(
            &mut db,
            sley_sequencer::TagCreate {
                object: target_oid.clone(),
                object_type: target_object.object_type,
                name: tag.as_bytes().to_vec(),
                tagger,
                message,
            },
        )?;
        create_or_update_tag(TagCreateOrUpdate {
            git_dir: &git_dir,
            format,
            store: &store,
            tag,
            target: tag_oid,
            reflog_target: &target_oid,
            force,
            create_reflog,
        })?;
        if target_object.object_type == ObjectType::Tag
            && config
                .get_bool("advice", None, "nestedtag")
                .or_else(|| config.get_bool("advice", None, "nestedTag"))
                .unwrap_or(true)
        {
            eprintln!(
                "hint: You have created a nested tag. The object referred to by your new tag is"
            );
            eprintln!(
                "hint: already a tag. If you meant to tag the object that it points to, use:"
            );
            eprintln!("hint:");
            eprintln!("hint: \tgit tag -f {tag} {target}^{{}}");
            eprintln!("hint: Disable this message with \"git config set advice.nestedTag false\"");
        }
        if editmsg_written {
            remove_tag_editmsg(&git_dir)?;
        }
    } else {
        create_or_update_tag(TagCreateOrUpdate {
            git_dir: &git_dir,
            format,
            store: &store,
            tag,
            target: target_oid.clone(),
            reflog_target: &target_oid,
            force,
            create_reflog,
        })?;
    }
    Ok(())
}

fn print_default_tag_list(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
) -> Result<()> {
    print_tag_list(
        git_dir,
        format,
        store,
        TagListOptions {
            patterns: &[],
            ignore_case: false,
            sorts: &[],
            prereleases: &[],
            format_spec: None,
            annotation_lines: None,
            omit_empty: false,
            color: false,
            column: TagListColumn::None,
            points_at: &[],
            contains: &[],
            no_contains: &[],
            merged: &[],
            no_merged: &[],
        },
    )
}

fn delete_tags(store: &FileRefStore, tags: &[String]) -> Result<()> {
    let mut failed = false;
    for tag in tags {
        match store.delete_tag(tag) {
            Ok(deleted) => {
                println!(
                    "Deleted tag '{tag}' (was {})",
                    short_oid(&deleted.oid.to_hex())
                );
            }
            Err(GitError::NotFound(_)) => {
                eprintln!("error: tag '{tag}' not found.");
                failed = true;
            }
            Err(GitError::InvalidPath(_)) => {
                eprintln!("error: tag '{tag}' not found.");
                failed = true;
            }
            Err(err) => return Err(err),
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn verify_tags(
    git_dir: &Path,
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    format_spec: Option<&str>,
    tags: &[String],
) -> Result<()> {
    let mut failed = false;
    for tag in tags {
        if !verify_tag(git_dir, store, db, format, format_spec, tag)? {
            failed = true;
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn verify_tag(
    git_dir: &Path,
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    format_spec: Option<&str>,
    tag: &str,
) -> Result<bool> {
    let name = match tag_ref_name(tag) {
        Ok(name) => name,
        Err(GitError::InvalidPath(_)) => {
            eprintln!("error: tag '{tag}' not found.");
            return Ok(false);
        }
        Err(err) => return Err(err),
    };
    let Some(target) = store.read_ref(&name)? else {
        eprintln!("error: tag '{tag}' not found.");
        return Ok(false);
    };
    let oid = match target {
        RefTarget::Direct(oid) => oid,
        RefTarget::Symbolic(_) => {
            eprintln!("error: tag '{tag}' not found.");
            return Ok(false);
        }
    };
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Tag {
        eprintln!(
            "error: {tag}: cannot verify a non-tag object of type {}.",
            object.object_type.as_str()
        );
        return Ok(false);
    }
    let parsed = Tag::parse(format, &object.body)?;
    if !tag_message_has_signature(&parsed.message) {
        if format_spec.is_none() {
            io::stdout().write_all(&object.body)?;
            io::stdout().flush()?;
        }
        eprintln!("error: no signature found");
        return Ok(false);
    }
    let Some((payload, signature)) = commands::signing::tag_signature_payload(&object.body) else {
        return Ok(false);
    };
    let config = read_repo_config(git_dir).ok();
    let verification =
        commands::signing::verify_payload(git_dir, config.as_ref(), payload, signature)?;
    if format_spec.is_none() {
        io::stderr().write_all(&verification.human_output)?;
    }
    if !verification.success {
        return Ok(false);
    }
    if let Some(format_spec) = format_spec {
        write_tag_verify_format(format_spec, &parsed)?;
    }
    Ok(true)
}

fn resolve_tag_target(git_dir: &Path, format: ObjectFormat, target: &str) -> Result<ObjectId> {
    match resolve_revision(git_dir, format, target) {
        Ok(oid) => Ok(oid),
        Err(GitError::NotFound(_)) => {
            eprintln!("fatal: Failed to resolve '{target}' as a valid ref.");
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn resolve_tag_points_at_filter(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    match resolve_revision(git_dir, format, rev) {
        Ok(oid) => Ok(oid),
        Err(GitError::NotFound(_) | GitError::InvalidFormat(_) | GitError::InvalidPath(_)) => {
            eprintln!("error: malformed object name '{rev}'");
            Err(GitError::Exit(129))
        }
        Err(err) => Err(err),
    }
}

fn resolve_tag_contains_filter(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    let oid = match resolve_revision(git_dir, format, rev) {
        Ok(oid) => oid,
        Err(GitError::NotFound(_) | GitError::InvalidFormat(_) | GitError::InvalidPath(_)) => {
            eprintln!("error: malformed object name {rev}");
            return Err(GitError::Exit(129));
        }
        Err(err) => return Err(err),
    };
    // `--contains`/`--no-contains` need a commit-ish: git peels tags to a commit
    // and rejects a tree/blob with `error: object <oid> is a tree, not a commit`
    // / `error: no such commit <name>` (exit 129).
    peel_to_commit_for_filter(git_dir, format, oid, rev)
}

/// Peel `oid` (following tag chains) to a commit for a list-filter that requires
/// a commit-ish (`--contains`/`--merged`). On a non-commit terminal object, emit
/// git's two-line diagnostic and exit 129.
fn peel_to_commit_for_filter(
    git_dir: &Path,
    format: ObjectFormat,
    oid: ObjectId,
    name: &str,
) -> Result<ObjectId> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut current = oid;
    loop {
        let object = db.read_object(&current)?;
        match object.object_type {
            ObjectType::Commit => return Ok(current),
            ObjectType::Tag => {
                let parsed = Tag::parse(format, &object.body)?;
                current = parsed.object;
            }
            other => {
                eprintln!("error: object {oid} is a {}, not a commit", other.as_str());
                eprintln!("error: no such commit {name}");
                return Err(GitError::Exit(129));
            }
        }
    }
}

fn resolve_tag_merged_filter(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<ObjectId> {
    match resolve_revision(git_dir, format, rev) {
        Ok(oid) => Ok(oid),
        Err(GitError::NotFound(_) | GitError::InvalidFormat(_) | GitError::InvalidPath(_)) => {
            eprintln!("fatal: malformed object name {rev}");
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn tag_message_has_signature(message: &[u8]) -> bool {
    commands::signing::signature_has_marker(message)
}

fn sign_tag_message(
    object: &ObjectId,
    object_type: ObjectType,
    name: &[u8],
    tagger: &[u8],
    mut message: Vec<u8>,
    config: Option<&GitConfig>,
    key: Option<&str>,
) -> Result<Vec<u8>> {
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    let unsigned = tag_object_body(
        *object,
        object_type,
        name.to_vec(),
        tagger.to_vec(),
        message.clone(),
    );
    let signature = commands::signing::sign_payload(config, &unsigned, key)?;
    message.extend_from_slice(&signature);
    Ok(message)
}

fn tag_object_body(
    object: ObjectId,
    object_type: ObjectType,
    name: Vec<u8>,
    tagger: Vec<u8>,
    message: Vec<u8>,
) -> Vec<u8> {
    Tag {
        object,
        object_type,
        name,
        tagger: Some(tagger),
        message,
        raw_body: None,
    }
    .write()
}

fn tag_signature_is_valid(format: ObjectFormat, body: &[u8]) -> Result<bool> {
    let marker = b"-----BEGIN PGP SIGNATURE-----";
    let signature_count = body
        .windows(marker.len())
        .filter(|window| *window == marker)
        .count();
    if signature_count > 1 {
        return Ok(true);
    }
    let Some(start) = body
        .windows(marker.len())
        .position(|window| window == marker)
    else {
        return Ok(false);
    };
    let unsigned = &body[..start];
    let signature = &body[start..];
    let signature_text = String::from_utf8_lossy(signature);
    let Some(line) = signature_text
        .lines()
        .find_map(|line| line.strip_prefix("sley-signature "))
    else {
        return Ok(true);
    };
    Ok(line == sley_core::digest_bytes(format, unsigned)?.to_hex())
}

fn write_tag_verify_format(format_spec: &str, tag: &Tag) -> Result<()> {
    if format_spec.contains("%(rest)") {
        eprintln!("fatal: unknown field name: rest");
        return Err(GitError::Exit(128));
    }
    let format = ForEachRefFormat::parse(format_spec)?;
    let mut stdout = io::stdout();
    write_for_each_ref_format(
        &mut stdout,
        &format,
        ForEachRefQuoteMode::None,
        false,
        |out, atom| {
            match atom {
                ForEachRefAtom::Raw(value) if value == "tag" => out.extend_from_slice(&tag.name),
                _ => {}
            }
            Ok(())
        },
    )?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

struct TagCreateOrUpdate<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    store: &'a FileRefStore,
    tag: &'a str,
    target: ObjectId,
    reflog_target: &'a ObjectId,
    force: bool,
    create_reflog: bool,
}

fn create_or_update_tag(options: TagCreateOrUpdate<'_>) -> Result<()> {
    let TagCreateOrUpdate {
        git_dir,
        format,
        store,
        tag,
        target,
        reflog_target,
        force,
        create_reflog,
    } = options;
    let name = validate_tag_creation_name(tag)?;
    if !force {
        if store.read_ref(&name)?.is_some() {
            eprintln!("fatal: tag '{tag}' already exists");
            return Err(GitError::Exit(128));
        }
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(target.clone()),
            reflog: tag_create_reflog_entry(
                git_dir,
                format,
                zero_oid(format)?,
                target,
                reflog_target,
                create_reflog,
            )?,
        });
        tx.commit()?;
        return Ok(());
    }

    let old = match store.read_ref(&name)? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        Some(RefTarget::Symbolic(_)) | None => None,
    };
    let old_oid = old.clone().unwrap_or(zero_oid(format)?);
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name,
        expected: None,
        new: RefTarget::Direct(target.clone()),
        reflog: tag_create_reflog_entry(
            git_dir,
            format,
            old_oid,
            target,
            reflog_target,
            create_reflog,
        )?,
    });
    tx.commit()?;
    if let Some(old) = old {
        println!("Updated tag '{tag}' (was {})", short_oid(&old.to_hex()));
    }
    Ok(())
}

fn validate_tag_creation_name(tag: &str) -> Result<String> {
    match tag_ref_name(tag) {
        Ok(refname) => Ok(refname),
        Err(GitError::InvalidPath(_)) => {
            eprintln!("fatal: '{tag}' is not a valid tag name.");
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn tag_create_reflog_entry(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    target: &ObjectId,
    create_reflog: bool,
) -> Result<Option<ReflogEntry>> {
    if !tag_should_write_reflog(git_dir, create_reflog)? {
        return Ok(None);
    }
    Ok(Some(ReflogEntry {
        old_oid,
        new_oid,
        committer: tag_reflog_committer_identity()?,
        message: tag_reflog_message(git_dir, format, target)?,
    }))
}

fn tag_should_write_reflog(git_dir: &Path, create_reflog: bool) -> Result<bool> {
    if create_reflog {
        return Ok(true);
    }
    if let Some(value) = global_config_value("core.logAllRefUpdates")? {
        return Ok(value.eq_ignore_ascii_case("always"));
    }
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(false);
    };
    Ok(config
        .get("core", None, "logAllRefUpdates")
        .is_some_and(|value| value.eq_ignore_ascii_case("always")))
}

/// Build the committer identity for a tag reflog entry. Unlike the tag *object*,
/// a reflog entry's timestamp is "now" (or `GIT_COMMITTER_DATE` when explicitly
/// set) — never the `@0` epoch default. git's reflog reader rejects a `0`
/// timestamp as uninitialised (`for-each-reflog-ent` skips it), so emitting the
/// current time is required for the entry to be readable.
fn tag_reflog_committer_identity() -> Result<Vec<u8>> {
    match env::var("GIT_COMMITTER_DATE") {
        Ok(date) if !date.is_empty() => commit_identity_from_env_with_date("COMMITTER", &date),
        _ => {
            let now = current_unix_seconds();
            commit_identity_from_env_with_date("COMMITTER", &format!("@{now} +0000"))
        }
    }
}

fn tag_reflog_message(git_dir: &Path, format: ObjectFormat, target: &ObjectId) -> Result<Vec<u8>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(target)?;
    let target_hex = target.to_hex();
    let short = short_oid(&target_hex);
    let description = match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse(format, &object.body)?;
            let subject = commit_subject(&commit.message);
            let date =
                for_each_ref_identity_date(&commit.committer, &DateMode::Short).unwrap_or_default();
            format!("{subject}, {date}")
        }
        ObjectType::Tag => "other tag object".to_string(),
        object_type => format!("{} object", object_type.as_str()),
    };
    Ok(format!("tag: tagging {short} ({description})").into_bytes())
}

fn tag_message_requires_value_error() -> Result<()> {
    eprintln!("error: switch `m' requires a value");
    Err(GitError::Exit(129))
}

fn tag_file_requires_value_error() -> Result<()> {
    eprintln!("error: switch `F' requires a value");
    Err(GitError::Exit(129))
}

#[derive(Clone, Copy)]
enum TagCleanupMode {
    Strip,
    Whitespace,
    Verbatim,
}

fn parse_tag_cleanup_mode(value: &str) -> Result<TagCleanupMode> {
    match value {
        "strip" => Ok(TagCleanupMode::Strip),
        "whitespace" => Ok(TagCleanupMode::Whitespace),
        "verbatim" => Ok(TagCleanupMode::Verbatim),
        _ => {
            eprintln!("fatal: Invalid cleanup mode {value}");
            Err(GitError::Exit(128))
        }
    }
}

fn tag_cleanup_requires_value_error() -> Result<()> {
    eprintln!("error: option `cleanup' requires a value");
    Err(GitError::Exit(129))
}

fn tag_trailer_requires_value_error() -> Result<()> {
    eprintln!("error: option `trailer' requires a value");
    Err(GitError::Exit(129))
}

fn tag_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn tag_option_requires_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' requires a value");
    Err(GitError::Exit(129))
}

fn tag_unknown_option_error(option: &str) -> Result<()> {
    eprintln!("error: unknown option `{option}'");
    print_tag_usage();
    Err(GitError::Exit(129))
}

fn tag_unknown_switch_error(switch: char) -> Result<()> {
    eprintln!("error: unknown switch `{switch}'");
    print_tag_usage();
    Err(GitError::Exit(129))
}

fn tag_usage_error() -> Result<()> {
    print_tag_usage();
    Err(GitError::Exit(129))
}

fn tag_unknown_short_switch(value: &str) -> Option<char> {
    value[1..].chars().find(|switch| {
        !matches!(
            switch,
            'a' | 's' | 'u' | 'f' | 'd' | 'v' | 'l' | 'n' | 'm' | 'F' | 'e' | 'i'
        )
    })
}

fn print_tag_usage() {
    eprint!(
        r#"usage: git tag [-a | -s | -u <key-id>] [-f] [-m <msg> | -F <file>] [-e]
               [(--trailer <token>[(=|:)<value>])...]
               <tagname> [<commit> | <object>]
   or: git tag -d <tagname>...
   or: git tag [-n[<num>]] -l [--contains <commit>] [--no-contains <commit>]
               [--points-at <object>] [--column[=<options>] | --no-column]
               [--create-reflog] [--sort=<key>] [--format=<format>]
               [--merged <commit>] [--no-merged <commit>] [<pattern>...]
   or: git tag -v [--format=<format>] <tagname>...

    -l, --list            list tag names
    -n[<n>]               print <n> lines of each tag message
    -d, --delete          delete tags
    -v, --verify          verify tags

Tag creation options
    -a, --[no-]annotate   annotated tag, needs a message
    -m, --message <message>
                          tag message
    -F, --[no-]file <file>
                          read message from file
    --[no-]trailer <trailer>
                          add custom trailer(s)
    -e, --[no-]edit       force edit of tag message
    -s, --[no-]sign       annotated and GPG-signed tag
    --[no-]cleanup <mode> how to strip spaces and #comments from message
    -u, --[no-]local-user <key-id>
                          use another key to sign the tag
    -f, --[no-]force      replace the tag if exists
    --[no-]create-reflog  create a reflog

Tag listing options
    --[no-]column[=<style>]
                          show tag list in columns
    --contains <commit>   print only tags that contain the commit
    --no-contains <commit>
                          print only tags that don't contain the commit
    --merged <commit>     print only tags that are merged
    --no-merged <commit>  print only tags that are not merged
    --[no-]omit-empty     do not output a newline after empty formatted refs
    --[no-]sort <key>     field name to sort on
    --[no-]points-at <object>
                          print only tags of the object
    --[no-]format <format>
                          format to use for the output
    --[no-]color[=<when>] respect format colors
    -i, --[no-]ignore-case
                          sorting and filtering are case insensitive

"#
    );
}

fn tag_local_user_requires_value_error(option: &str, short: bool) -> Result<()> {
    if short {
        eprintln!("error: switch `{option}' requires a value");
    } else {
        eprintln!("error: option `{option}' requires a value");
    }
    Err(GitError::Exit(129))
}

pub(crate) fn parse_tag_trailer(value: &str) -> Vec<u8> {
    let (token, value) = value
        .split_once('=')
        .or_else(|| value.split_once(':'))
        .unwrap_or((value, ""));
    let mut trailer = token.trim().as_bytes().to_vec();
    trailer.push(b':');
    let value = value.trim();
    if !value.is_empty() {
        trailer.push(b' ');
        trailer.extend_from_slice(value.as_bytes());
    }
    trailer
}

fn tag_message_from_chunks_verbatim(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        if idx != 0 {
            out.extend_from_slice(b"\n\n");
        }
        out.extend_from_slice(chunk);
    }
    out
}

fn existing_annotated_tag_message(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tag: &str,
) -> Result<Option<Vec<u8>>> {
    let Ok(name) = tag_ref_name(tag) else {
        return Ok(None);
    };
    let Some(RefTarget::Direct(oid)) = store.read_ref(&name)? else {
        return Ok(None);
    };
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(None);
    }
    let parsed = Tag::parse(format, &object.body)?;
    Ok(Some(parsed.message))
}

fn tag_message_for_editor(message: Vec<u8>, trailers: &[Vec<u8>]) -> Vec<u8> {
    if message.is_empty() && !trailers.is_empty() {
        let mut out = Vec::from(&b"\n"[..]);
        for trailer in trailers {
            out.extend_from_slice(trailer);
            out.push(b'\n');
        }
        out
    } else {
        tag_message_with_trailers(message, trailers)
    }
}

fn launch_tag_editor(git_dir: &Path, tag: &str, message: &[u8]) -> Result<Vec<u8>> {
    let path = git_dir.join("TAG_EDITMSG");
    let mut buffer = message.to_vec();
    append_tag_editor_template(git_dir, tag, &mut buffer);
    fs::write(&path, buffer)?;
    commands::replay::launch_editor(git_dir, &path)?;
    Ok(fs::read(path)?)
}

fn append_tag_editor_template(git_dir: &Path, tag: &str, buffer: &mut Vec<u8>) {
    let comment = commands::replay::comment_char(git_dir);
    if buffer.is_empty() {
        buffer.push(b'\n');
    } else if !buffer.ends_with(b"\n") {
        buffer.push(b'\n');
        buffer.push(b'\n');
    } else {
        buffer.push(b'\n');
    }
    buffer.push(comment);
    buffer.push(b'\n');
    buffer.push(comment);
    buffer.extend_from_slice(b" Write a message for tag:\n");
    buffer.push(comment);
    buffer.extend_from_slice(b"   ");
    buffer.extend_from_slice(tag.as_bytes());
    buffer.push(b'\n');
    buffer.push(comment);
    buffer.push(b'\n');
    buffer.push(comment);
    buffer.extend_from_slice(b" Lines starting with '");
    buffer.push(comment);
    buffer.extend_from_slice(b"' will be ignored.\n");
}

fn remove_tag_editmsg(git_dir: &Path) -> Result<()> {
    match fs::remove_file(git_dir.join("TAG_EDITMSG")) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn tag_message_with_trailers(mut message: Vec<u8>, trailers: &[Vec<u8>]) -> Vec<u8> {
    if trailers.is_empty() {
        return message;
    }
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    if !message.is_empty() && !tag_message_last_paragraph_is_trailer_block(&message) {
        message.push(b'\n');
    }
    for trailer in trailers {
        message.extend_from_slice(trailer);
        message.push(b'\n');
    }
    message
}

fn tag_message_last_paragraph_is_trailer_block(message: &[u8]) -> bool {
    let end = message
        .iter()
        .rposition(|byte| *byte != b'\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    if end == 0 {
        return false;
    }
    let body = &message[..end];
    let start = body
        .windows(2)
        .rposition(|window| window == b"\n\n")
        .map(|idx| idx + 2)
        .unwrap_or(0);
    body[start..]
        .split(|byte| *byte == b'\n')
        .all(tag_message_line_is_trailer)
}

fn tag_message_line_is_trailer(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };
    colon != 0
        && line[..colon]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn tag_cleanup_message(message: Vec<u8>, mode: TagCleanupMode) -> Vec<u8> {
    match mode {
        TagCleanupMode::Verbatim => message,
        TagCleanupMode::Strip => tag_stripspace_message(&message, true),
        TagCleanupMode::Whitespace => tag_stripspace_message(&message, false),
    }
}

pub(crate) fn tag_stripspace_message(message: &[u8], strip_comments: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pending_blank = false;
    for raw_line in message.split(|byte| *byte == b'\n') {
        let line = tag_trim_trailing_space(raw_line);
        if strip_comments && line.first() == Some(&b'#') {
            continue;
        }
        if line.is_empty() {
            if !out.is_empty() {
                pending_blank = true;
            }
            continue;
        }
        if pending_blank {
            out.push(b'\n');
            pending_blank = false;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

fn tag_trim_trailing_space(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r'))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    &line[..end]
}

struct TagListOptions<'a> {
    patterns: &'a [String],
    ignore_case: bool,
    sorts: &'a [TagListSort],
    prereleases: &'a [String],
    format_spec: Option<&'a str>,
    annotation_lines: Option<usize>,
    omit_empty: bool,
    color: bool,
    column: TagListColumn,
    points_at: &'a [ObjectId],
    contains: &'a [ObjectId],
    no_contains: &'a [ObjectId],
    merged: &'a [ObjectId],
    no_merged: &'a [ObjectId],
}

/// Resolve the version-sort prerelease/suffix list from config, mirroring
/// git's versioncmp.c: `versionsort.suffix` overrides the older
/// `versionsort.prereleaseSuffix`; when both are present a warning is emitted
/// and `suffix` wins. A bare key with no value yields a per-key error (the
/// command still succeeds) and contributes nothing.
fn resolve_versionsort_prereleases(config: &GitConfig) -> Vec<String> {
    fn collect(config: &GitConfig, key: &str, display: &str) -> Option<Vec<String>> {
        let entries = config.get_all("versionsort", None, key);
        if entries.is_empty() {
            return None;
        }
        // A bare key with no value is an error in git's string getter, which
        // then reports the source as not-found (returns nonzero); emit the
        // diagnostic and treat the whole source as absent.
        let mut out = Vec::new();
        let mut missing = false;
        for entry in entries {
            match entry {
                Some(value) => out.push(value.to_string()),
                None => {
                    eprintln!("error: missing value for '{display}'");
                    missing = true;
                }
            }
        }
        if missing { None } else { Some(out) }
    }

    let suffix = collect(config, "suffix", "versionsort.suffix");
    let prerelease = collect(config, "prereleasesuffix", "versionsort.prereleasesuffix");
    if suffix.is_some() && prerelease.is_some() {
        eprintln!(
            "warning: ignoring versionsort.prereleasesuffix because versionsort.suffix is set"
        );
    }
    suffix.or(prerelease).unwrap_or_default()
}

fn tag_list_column_from_config(config: &GitConfig) -> TagListColumn {
    let ui = config
        .get("column", None, "ui")
        .map(tag_column_config_tokens)
        .unwrap_or_default();
    let tag = config
        .get("column", None, "tag")
        .map(tag_column_config_tokens)
        .unwrap_or_default();
    if tag.disable || ui.disable {
        return TagListColumn::None;
    }
    if tag.enable || ui.enable {
        if tag.dense || ui.dense {
            TagListColumn::Dense
        } else {
            TagListColumn::Aligned
        }
    } else {
        TagListColumn::None
    }
}

fn tag_color_enabled_from_config(config: &GitConfig) -> bool {
    config
        .get("color", None, "ui")
        .is_some_and(|value| value.eq_ignore_ascii_case("always"))
}

#[derive(Default)]
struct TagColumnConfig {
    enable: bool,
    disable: bool,
    dense: bool,
}

fn tag_column_config_tokens(value: &str) -> TagColumnConfig {
    let mut config = TagColumnConfig::default();
    for token in value.split(|ch: char| ch == ',' || ch.is_ascii_whitespace()) {
        match token {
            "" => {}
            "never" | "plain" => {
                config.disable = true;
                config.enable = false;
            }
            "always" | "auto" | "column" | "row" => config.enable = true,
            "dense" => config.dense = true,
            "nodense" => config.dense = false,
            _ => {}
        }
    }
    config
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagListColumn {
    None,
    Aligned,
    Dense,
}

#[derive(Clone, Copy)]
enum TagListSort {
    Refname,
    RefnameDescending,
    VersionRefname,
    VersionRefnameDescending,
    Objectname,
    ObjectnameDescending,
    Objecttype,
    ObjecttypeDescending,
    Objectsize,
    ObjectsizeDescending,
    ObjectsizeDisk,
    ObjectsizeDiskDescending,
    Deltabase,
    DeltabaseDescending,
    RawSize,
    RawSizeDescending,
    PeeledObjectname,
    PeeledObjectnameDescending,
    PeeledObjecttype,
    PeeledObjecttypeDescending,
    PeeledObjectsize,
    PeeledObjectsizeDescending,
    PeeledObjectsizeDisk,
    PeeledObjectsizeDiskDescending,
    PeeledDeltabase,
    PeeledDeltabaseDescending,
    PeeledRawSize,
    PeeledRawSizeDescending,
    Authordate,
    AuthordateDescending,
    Committerdate,
    CommitterdateDescending,
    Taggerdate,
    TaggerdateDescending,
    Creatordate,
    CreatordateDescending,
    PeeledAuthordate,
    PeeledAuthordateDescending,
    PeeledCommitterdate,
    PeeledCommitterdateDescending,
    PeeledTaggerdate,
    PeeledTaggerdateDescending,
    PeeledCreatordate,
    PeeledCreatordateDescending,
    Identity(ForEachRefIdentitySortField),
    IdentityDescending(ForEachRefIdentitySortField),
    Tag,
    TagDescending,
    Type,
    TypeDescending,
    Object,
    ObjectDescending,
    Tree,
    TreeDescending,
    Parent,
    ParentDescending,
    NumParent,
    NumParentDescending,
    PeeledTree,
    PeeledTreeDescending,
    PeeledParent,
    PeeledParentDescending,
    PeeledNumParent,
    PeeledNumParentDescending,
    PeeledSubject,
    PeeledSubjectDescending,
    PeeledBody,
    PeeledBodyDescending,
    PeeledContentsSize,
    PeeledContentsSizeDescending,
    Subject,
    SubjectDescending,
    Body,
    BodyDescending,
    ContentsSize,
    ContentsSizeDescending,
}

fn parse_tag_list_sort(value: &str) -> Result<TagListSort> {
    match value {
        "refname" => Ok(TagListSort::Refname),
        "-refname" => Ok(TagListSort::RefnameDescending),
        "version:refname" | "v:refname" => Ok(TagListSort::VersionRefname),
        "-version:refname" | "-v:refname" => Ok(TagListSort::VersionRefnameDescending),
        // `version:tag` / `v:tag`: version-compare the `tag` atom (the short
        // name under refs/tags). Every listed tag shares the refs/tags/ prefix,
        // so a version compare on the short name matches one on the full
        // refname — reuse VersionRefname.
        "version:tag" | "v:tag" => Ok(TagListSort::VersionRefname),
        "-version:tag" | "-v:tag" => Ok(TagListSort::VersionRefnameDescending),
        "objectname" => Ok(TagListSort::Objectname),
        "-objectname" => Ok(TagListSort::ObjectnameDescending),
        "objecttype" => Ok(TagListSort::Objecttype),
        "-objecttype" => Ok(TagListSort::ObjecttypeDescending),
        "objectsize" => Ok(TagListSort::Objectsize),
        "-objectsize" => Ok(TagListSort::ObjectsizeDescending),
        "objectsize:disk" => Ok(TagListSort::ObjectsizeDisk),
        "-objectsize:disk" => Ok(TagListSort::ObjectsizeDiskDescending),
        "deltabase" => Ok(TagListSort::Deltabase),
        "-deltabase" => Ok(TagListSort::DeltabaseDescending),
        "raw:size" => Ok(TagListSort::RawSize),
        "-raw:size" => Ok(TagListSort::RawSizeDescending),
        "*objectname" => Ok(TagListSort::PeeledObjectname),
        "-*objectname" => Ok(TagListSort::PeeledObjectnameDescending),
        "*objecttype" => Ok(TagListSort::PeeledObjecttype),
        "-*objecttype" => Ok(TagListSort::PeeledObjecttypeDescending),
        "*objectsize" => Ok(TagListSort::PeeledObjectsize),
        "-*objectsize" => Ok(TagListSort::PeeledObjectsizeDescending),
        "*objectsize:disk" => Ok(TagListSort::PeeledObjectsizeDisk),
        "-*objectsize:disk" => Ok(TagListSort::PeeledObjectsizeDiskDescending),
        "*deltabase" => Ok(TagListSort::PeeledDeltabase),
        "-*deltabase" => Ok(TagListSort::PeeledDeltabaseDescending),
        "*raw:size" => Ok(TagListSort::PeeledRawSize),
        "-*raw:size" => Ok(TagListSort::PeeledRawSizeDescending),
        "authordate" => Ok(TagListSort::Authordate),
        "-authordate" => Ok(TagListSort::AuthordateDescending),
        "committerdate" => Ok(TagListSort::Committerdate),
        "-committerdate" => Ok(TagListSort::CommitterdateDescending),
        "taggerdate" => Ok(TagListSort::Taggerdate),
        "-taggerdate" => Ok(TagListSort::TaggerdateDescending),
        "creatordate" => Ok(TagListSort::Creatordate),
        "-creatordate" => Ok(TagListSort::CreatordateDescending),
        "*authordate" => Ok(TagListSort::PeeledAuthordate),
        "-*authordate" => Ok(TagListSort::PeeledAuthordateDescending),
        "*committerdate" => Ok(TagListSort::PeeledCommitterdate),
        "-*committerdate" => Ok(TagListSort::PeeledCommitterdateDescending),
        "*taggerdate" => Ok(TagListSort::PeeledTaggerdate),
        "-*taggerdate" => Ok(TagListSort::PeeledTaggerdateDescending),
        "*creatordate" => Ok(TagListSort::PeeledCreatordate),
        "-*creatordate" => Ok(TagListSort::PeeledCreatordateDescending),
        "tag" => Ok(TagListSort::Tag),
        "-tag" => Ok(TagListSort::TagDescending),
        "type" => Ok(TagListSort::Type),
        "-type" => Ok(TagListSort::TypeDescending),
        "object" => Ok(TagListSort::Object),
        "-object" => Ok(TagListSort::ObjectDescending),
        "tree" => Ok(TagListSort::Tree),
        "-tree" => Ok(TagListSort::TreeDescending),
        "parent" => Ok(TagListSort::Parent),
        "-parent" => Ok(TagListSort::ParentDescending),
        "numparent" => Ok(TagListSort::NumParent),
        "-numparent" => Ok(TagListSort::NumParentDescending),
        "*tree" => Ok(TagListSort::PeeledTree),
        "-*tree" => Ok(TagListSort::PeeledTreeDescending),
        "*parent" => Ok(TagListSort::PeeledParent),
        "-*parent" => Ok(TagListSort::PeeledParentDescending),
        "*numparent" => Ok(TagListSort::PeeledNumParent),
        "-*numparent" => Ok(TagListSort::PeeledNumParentDescending),
        "*subject" | "*contents:subject" => Ok(TagListSort::PeeledSubject),
        "-*subject" | "-*contents:subject" => Ok(TagListSort::PeeledSubjectDescending),
        "*body" | "*contents:body" => Ok(TagListSort::PeeledBody),
        "-*body" | "-*contents:body" => Ok(TagListSort::PeeledBodyDescending),
        "*contents:size" => Ok(TagListSort::PeeledContentsSize),
        "-*contents:size" => Ok(TagListSort::PeeledContentsSizeDescending),
        "subject" | "contents:subject" => Ok(TagListSort::Subject),
        "-subject" | "-contents:subject" => Ok(TagListSort::SubjectDescending),
        "body" | "contents:body" => Ok(TagListSort::Body),
        "-body" | "-contents:body" => Ok(TagListSort::BodyDescending),
        "contents:size" => Ok(TagListSort::ContentsSize),
        "-contents:size" => Ok(TagListSort::ContentsSizeDescending),
        other => {
            if let Some((field, descending)) = parse_for_each_ref_identity_sort(other) {
                Ok(if descending {
                    TagListSort::IdentityDescending(field)
                } else {
                    TagListSort::Identity(field)
                })
            } else {
                tag_sort_key_error(other)
            }
        }
    }
}

fn tag_sort_key_error(key: &str) -> Result<TagListSort> {
    // Mirror git's parse_ref_sorting(): a leading '-' (reverse) and a
    // "version:"/"v:" prefix are stripped before the atom is parsed, so the
    // diagnostic names only the unrecognised atom.
    let atom = key.strip_prefix('-').unwrap_or(key);
    let atom = atom
        .strip_prefix("version:")
        .or_else(|| atom.strip_prefix("v:"))
        .unwrap_or(atom);
    if atom.is_empty() {
        eprintln!("fatal: malformed field name: ");
    } else {
        eprintln!("fatal: unknown field name: {atom}");
    }
    Err(GitError::Exit(128))
}

fn parse_tag_list_annotation_lines(value: &str) -> Result<usize> {
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024_i128),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024_i128 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024_i128 * 1024 * 1024),
        _ => (value, 1),
    };
    let digits = digits.strip_prefix('+').unwrap_or(digits);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return tag_annotation_lines_invalid_error();
    }
    let mut parsed = 0_i128;
    for byte in digits.bytes() {
        parsed = parsed.saturating_mul(10);
        parsed = parsed.saturating_add(i128::from(byte - b'0'));
        if parsed.saturating_mul(multiplier) > i128::from(i32::MAX) {
            return tag_annotation_lines_range_error(value);
        }
    }
    let parsed = parsed.saturating_mul(multiplier);
    if parsed > i128::from(i32::MAX) {
        return tag_annotation_lines_range_error(value);
    }
    Ok(parsed as usize)
}

fn tag_annotation_lines_invalid_error() -> Result<usize> {
    eprintln!("error: switch `n' expects an integer value with an optional k/m/g suffix");
    Err(GitError::Exit(129))
}

fn tag_annotation_lines_range_error(value: &str) -> Result<usize> {
    eprintln!("error: value {value} for switch `n' not in range [-2147483648,2147483647]");
    Err(GitError::Exit(129))
}

fn parse_tag_list_column(value: &str) -> Result<TagListColumn> {
    let mut enabled = true;
    let mut column = TagListColumn::Aligned;
    for (idx, token) in value.split(',').enumerate() {
        match token {
            "" => {}
            "always" => enabled = true,
            "auto" | "never" => enabled = false,
            "plain" => column = TagListColumn::None,
            "column" | "row" => column = TagListColumn::Aligned,
            "dense" => {
                if column != TagListColumn::None {
                    column = TagListColumn::Dense;
                }
            }
            "nodense" => {
                if column != TagListColumn::None {
                    column = TagListColumn::Aligned;
                }
            }
            _ => {
                let unsupported = if idx == 0 { value } else { token };
                eprintln!("error: unsupported option '{unsupported}'");
                return Err(GitError::Exit(129));
            }
        }
    }
    if enabled {
        Ok(column)
    } else {
        Ok(TagListColumn::None)
    }
}

fn parse_tag_list_color(value: &str) -> Result<bool> {
    match value {
        "always" => Ok(true),
        "auto" | "never" => Ok(false),
        _ => {
            eprintln!("error: option `color' expects \"always\", \"auto\", or \"never\"");
            Err(GitError::Exit(129))
        }
    }
}

impl TagListSort {
    fn descending(self) -> bool {
        matches!(
            self,
            TagListSort::RefnameDescending
                | TagListSort::VersionRefnameDescending
                | TagListSort::ObjectnameDescending
                | TagListSort::ObjecttypeDescending
                | TagListSort::ObjectsizeDescending
                | TagListSort::ObjectsizeDiskDescending
                | TagListSort::DeltabaseDescending
                | TagListSort::RawSizeDescending
                | TagListSort::PeeledObjectnameDescending
                | TagListSort::PeeledObjecttypeDescending
                | TagListSort::PeeledObjectsizeDescending
                | TagListSort::PeeledObjectsizeDiskDescending
                | TagListSort::PeeledDeltabaseDescending
                | TagListSort::PeeledRawSizeDescending
                | TagListSort::AuthordateDescending
                | TagListSort::CommitterdateDescending
                | TagListSort::TaggerdateDescending
                | TagListSort::CreatordateDescending
                | TagListSort::PeeledAuthordateDescending
                | TagListSort::PeeledCommitterdateDescending
                | TagListSort::PeeledTaggerdateDescending
                | TagListSort::PeeledCreatordateDescending
                | TagListSort::IdentityDescending(_)
                | TagListSort::TagDescending
                | TagListSort::TypeDescending
                | TagListSort::ObjectDescending
                | TagListSort::TreeDescending
                | TagListSort::ParentDescending
                | TagListSort::NumParentDescending
                | TagListSort::PeeledTreeDescending
                | TagListSort::PeeledParentDescending
                | TagListSort::PeeledNumParentDescending
                | TagListSort::PeeledSubjectDescending
                | TagListSort::PeeledBodyDescending
                | TagListSort::PeeledContentsSizeDescending
                | TagListSort::SubjectDescending
                | TagListSort::BodyDescending
                | TagListSort::ContentsSizeDescending
        )
    }

    fn needs_object_metadata(self) -> bool {
        matches!(
            self,
            TagListSort::Objecttype
                | TagListSort::ObjecttypeDescending
                | TagListSort::Objectsize
                | TagListSort::ObjectsizeDescending
                | TagListSort::ObjectsizeDisk
                | TagListSort::ObjectsizeDiskDescending
                | TagListSort::Deltabase
                | TagListSort::DeltabaseDescending
                | TagListSort::RawSize
                | TagListSort::RawSizeDescending
                | TagListSort::PeeledObjectname
                | TagListSort::PeeledObjectnameDescending
                | TagListSort::PeeledObjecttype
                | TagListSort::PeeledObjecttypeDescending
                | TagListSort::PeeledObjectsize
                | TagListSort::PeeledObjectsizeDescending
                | TagListSort::PeeledObjectsizeDisk
                | TagListSort::PeeledObjectsizeDiskDescending
                | TagListSort::PeeledDeltabase
                | TagListSort::PeeledDeltabaseDescending
                | TagListSort::PeeledRawSize
                | TagListSort::PeeledRawSizeDescending
                | TagListSort::Authordate
                | TagListSort::AuthordateDescending
                | TagListSort::Committerdate
                | TagListSort::CommitterdateDescending
                | TagListSort::Taggerdate
                | TagListSort::TaggerdateDescending
                | TagListSort::Creatordate
                | TagListSort::CreatordateDescending
                | TagListSort::PeeledAuthordate
                | TagListSort::PeeledAuthordateDescending
                | TagListSort::PeeledCommitterdate
                | TagListSort::PeeledCommitterdateDescending
                | TagListSort::PeeledTaggerdate
                | TagListSort::PeeledTaggerdateDescending
                | TagListSort::PeeledCreatordate
                | TagListSort::PeeledCreatordateDescending
                | TagListSort::Identity(_)
                | TagListSort::IdentityDescending(_)
                | TagListSort::Tag
                | TagListSort::TagDescending
                | TagListSort::Type
                | TagListSort::TypeDescending
                | TagListSort::Object
                | TagListSort::ObjectDescending
                | TagListSort::Tree
                | TagListSort::TreeDescending
                | TagListSort::Parent
                | TagListSort::ParentDescending
                | TagListSort::NumParent
                | TagListSort::NumParentDescending
                | TagListSort::PeeledTree
                | TagListSort::PeeledTreeDescending
                | TagListSort::PeeledParent
                | TagListSort::PeeledParentDescending
                | TagListSort::PeeledNumParent
                | TagListSort::PeeledNumParentDescending
                | TagListSort::PeeledSubject
                | TagListSort::PeeledSubjectDescending
                | TagListSort::PeeledBody
                | TagListSort::PeeledBodyDescending
                | TagListSort::PeeledContentsSize
                | TagListSort::PeeledContentsSizeDescending
                | TagListSort::Subject
                | TagListSort::SubjectDescending
                | TagListSort::Body
                | TagListSort::BodyDescending
                | TagListSort::ContentsSize
                | TagListSort::ContentsSizeDescending
        )
    }
}

fn print_tag_list(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: TagListOptions<'_>,
) -> Result<()> {
    let db = (options.format_spec.is_some()
        || options.annotation_lines.is_some()
        || !options.points_at.is_empty()
        || !options.contains.is_empty()
        || !options.no_contains.is_empty()
        || !options.merged.is_empty()
        || !options.no_merged.is_empty()
        || options
            .sorts
            .iter()
            .any(|sort| sort.needs_object_metadata()))
    .then(|| FileObjectDatabase::from_git_dir(git_dir, format));
    let merged_reachable = tag_merged_reachable_sets(db.as_ref(), format, options.merged)?;
    let no_merged_reachable = tag_merged_reachable_sets(db.as_ref(), format, options.no_merged)?;
    let mut entries = Vec::new();
    for reference in store.list_refs_with_prefix("refs/tags/")? {
        if let Some(name) = reference.name.strip_prefix("refs/tags/")
            && (options.patterns.is_empty()
                || options.patterns.iter().any(|pattern| {
                    refname_pattern_matches_case(pattern, name, options.ignore_case)
                }))
            && tag_points_at(&db, format, &reference.target, options.points_at)?
            && tag_contains(
                &db,
                format,
                &reference.target,
                options.contains,
                options.no_contains,
            )?
            && tag_merged(
                &db,
                format,
                &reference.target,
                &merged_reachable,
                &no_merged_reachable,
            )?
        {
            entries.push(TagListEntry {
                name: name.to_string(),
                reference,
                object_metadata: None,
            });
        }
    }
    populate_tag_sort_metadata(
        db.as_ref(),
        git_dir,
        format,
        store,
        &mut entries,
        options.sorts,
    )?;
    sort_tag_entries(
        &mut entries,
        options.sorts,
        options.prereleases,
        options.ignore_case,
    );
    if let Some(format_spec) = options.format_spec {
        let format_spec = ForEachRefFormat::parse(format_spec)?;
        let db = db.as_ref().expect("format listing creates object database");
        let objectname_abbrev = repository_abbrev(git_dir, format)?;
        let objectname_candidates = cat_file_all_object_ids(git_dir, format)?;
        let deltabase = zero_oid(format)?;
        let mailmap = commands::utility::Mailmap::load_default(git_dir, format)?;
        let ref_names: std::collections::HashSet<String> = store
            .list_refs()?
            .into_iter()
            .map(|reference| reference.name)
            .collect();
        let warn_ambiguous_refs = read_repo_config(git_dir)?
            .get_bool("core", None, "warnambiguousrefs")
            .unwrap_or(true);
        let mut stdout = io::stdout();
        for entry in entries {
            let Some((oid, symref)) = resolve_for_each_ref_target(store, &entry.reference)? else {
                continue;
            };
            let object = db.read_object(&oid)?;
            let contents = for_each_ref_contents(format, &object)?;
            let peeled_object =
                tag_format_peeled_object(git_dir, db, format, &oid, contents.as_ref())?;
            let object_disk_size = for_each_ref_loose_object_disk_size(git_dir, &oid)?;
            let format_context = ForEachRefFormatContext {
                git_dir,
                db,
                format,
                refname: &entry.reference.name,
                oid: &oid,
                deltabase: &deltabase,
                object_type: object.object_type,
                object_body: &object.body,
                object_size: object.body.len(),
                object_disk_size,
                color: options.color,
                quote: ForEachRefQuoteMode::None,
                objectname_abbrev,
                objectname_candidates: &objectname_candidates,
                worktree_path: None,
                is_head: false,
                symref: symref.as_deref(),
                upstream: None,
                push: None,
                upstream_track: None,
                push_track: None,
                contents,
                peeled_object,
                signature: None,
                peeled_signature: None,
                mailmap: &mailmap,
                ref_names: &ref_names,
                warn_ambiguous_refs,
            };
            let mut line = Vec::new();
            print_for_each_ref_format(&mut line, &format_spec, &format_context)?;
            if !options.omit_empty || !line.is_empty() {
                stdout.write_all(&line)?;
                stdout.write_all(b"\n")?;
            }
        }
        stdout.flush()?;
    } else if let Some(lines) = options.annotation_lines {
        let db = db.as_ref().expect("tag -n listing creates object database");
        for entry in entries {
            let message = tag_list_annotation_message(db, format, store, &entry.reference)?;
            write_tag_list_annotation(&mut io::stdout(), &entry.name, message.as_deref(), lines)?;
        }
    } else if options.column != TagListColumn::None {
        write_tag_list_columns(&mut io::stdout(), &entries, options.column)?;
    } else {
        for entry in entries {
            println!("{}", entry.name);
        }
    }
    Ok(())
}

fn write_tag_list_columns(
    stdout: &mut impl Write,
    entries: &[TagListEntry],
    column: TagListColumn,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    match column {
        TagListColumn::None => {}
        TagListColumn::Dense | TagListColumn::Aligned => {
            let terminal_width = env::var("COLUMNS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(80);
            let cell_width = entries
                .iter()
                .map(|entry| entry.name.chars().count())
                .max()
                .unwrap_or(0)
                + 2;
            let columns = std::cmp::max(1, terminal_width / cell_width);
            let columns = std::cmp::min(columns, entries.len());
            let rows = entries.len().div_ceil(columns);
            let mut widths = vec![cell_width; columns];
            if column == TagListColumn::Dense {
                for (col, width) in widths.iter_mut().enumerate() {
                    let mut max = 0;
                    for row in 0..rows {
                        let idx = row * columns + col;
                        if let Some(entry) = entries.get(idx) {
                            max = std::cmp::max(max, entry.name.chars().count());
                        }
                    }
                    *width = max + 2;
                }
            }
            for row in 0..rows {
                for (col, width) in widths.iter().enumerate().take(columns) {
                    let idx = row * columns + col;
                    let Some(entry) = entries.get(idx) else {
                        continue;
                    };
                    let is_last = col + 1 == columns || idx + 1 == entries.len();
                    if is_last {
                        write!(stdout, "{}", entry.name)?;
                    } else {
                        write!(stdout, "{:<width$}", entry.name, width = *width)?;
                    }
                }
                writeln!(stdout)?;
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

struct TagListEntry {
    name: String,
    reference: sley_refs::Ref,
    object_metadata: Option<TagListObjectMetadata>,
}

struct TagListObjectMetadata {
    object_type: String,
    object_size: usize,
    object_disk_size: u64,
    deltabase: String,
    peeled_object: Option<ForEachRefPeeledObject<'static>>,
    authordate: i128,
    committerdate: i128,
    taggerdate: i128,
    creatordate: i128,
    peeled_authordate: i128,
    peeled_committerdate: i128,
    peeled_taggerdate: i128,
    peeled_creatordate: i128,
    contents: Option<ForEachRefContents<'static>>,
}

fn populate_tag_sort_metadata(
    db: Option<&FileObjectDatabase>,
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    entries: &mut [TagListEntry],
    sorts: &[TagListSort],
) -> Result<()> {
    if !sorts.iter().any(|sort| sort.needs_object_metadata()) {
        return Ok(());
    }
    let Some(db) = db else {
        return Ok(());
    };
    for entry in entries {
        let Some((oid, _)) = resolve_for_each_ref_target(store, &entry.reference)? else {
            continue;
        };
        let object = db.read_object(&oid)?;
        let contents = for_each_ref_contents(format, &object)?;
        let peeled_object =
            tag_format_peeled_object(git_dir, db, format, &oid, contents.as_ref())?;
        let peeled_authordate =
            tag_sort_peeled_date_key(peeled_object.as_ref(), ForEachRefDateSortField::Author);
        let peeled_committerdate =
            tag_sort_peeled_date_key(peeled_object.as_ref(), ForEachRefDateSortField::Committer);
        let peeled_taggerdate =
            tag_sort_peeled_date_key(peeled_object.as_ref(), ForEachRefDateSortField::Tagger);
        let peeled_creatordate =
            tag_sort_peeled_date_key(peeled_object.as_ref(), ForEachRefDateSortField::Creator);
        entry.object_metadata = Some(TagListObjectMetadata {
            object_type: object.object_type.as_str().to_string(),
            object_size: object.body.len(),
            object_disk_size: for_each_ref_loose_object_disk_size(git_dir, &oid)?.unwrap_or(0),
            deltabase: zero_oid(format)?.to_hex(),
            peeled_object,
            authordate: tag_sort_date_key(contents.as_ref(), ForEachRefDateSortField::Author),
            committerdate: tag_sort_date_key(contents.as_ref(), ForEachRefDateSortField::Committer),
            taggerdate: tag_sort_date_key(contents.as_ref(), ForEachRefDateSortField::Tagger),
            creatordate: tag_sort_date_key(contents.as_ref(), ForEachRefDateSortField::Creator),
            peeled_authordate,
            peeled_committerdate,
            peeled_taggerdate,
            peeled_creatordate,
            contents: contents.map(ForEachRefContents::into_owned),
        });
    }
    Ok(())
}

fn tag_sort_date_key(
    contents: Option<&ForEachRefContents<'_>>,
    field: ForEachRefDateSortField,
) -> i128 {
    let identity = match field {
        ForEachRefDateSortField::Author => contents.and_then(|contents| contents.author.as_deref()),
        ForEachRefDateSortField::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefDateSortField::Tagger => contents.and_then(|contents| contents.tagger.as_deref()),
        ForEachRefDateSortField::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    };
    identity
        .and_then(for_each_ref_identity_timestamp)
        .map(i128::from)
        .unwrap_or(0)
}

fn tag_sort_peeled_date_key(
    peeled_object: Option<&ForEachRefPeeledObject<'_>>,
    field: ForEachRefDateSortField,
) -> i128 {
    let identity = match field {
        ForEachRefDateSortField::Author => {
            peeled_object.and_then(|peeled| peeled.author.as_deref())
        }
        ForEachRefDateSortField::Committer => {
            peeled_object.and_then(|peeled| peeled.committer.as_deref())
        }
        ForEachRefDateSortField::Tagger => None,
        ForEachRefDateSortField::Creator => {
            peeled_object.and_then(|peeled| peeled.creator.as_deref())
        }
    };
    identity
        .and_then(for_each_ref_identity_timestamp)
        .map(i128::from)
        .unwrap_or(0)
}

fn tag_list_annotation_message(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    store: &FileRefStore,
    reference: &sley_refs::Ref,
) -> Result<Option<Vec<u8>>> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(None);
    };
    let object = db.read_object(&oid)?;
    Ok(for_each_ref_contents(format, &object)?
        .map(|contents| tag_message_without_signature(&contents.message).to_vec()))
}

fn write_tag_list_annotation(
    stdout: &mut impl Write,
    name: &str,
    message: Option<&[u8]>,
    lines: usize,
) -> Result<()> {
    if lines == 0 {
        writeln!(stdout, "{name}")?;
        return Ok(());
    }
    let mut message = message.unwrap_or_default();
    while message.ends_with(b"\n") {
        message = &message[..message.len() - 1];
    }
    let mut message_lines = message.split(|byte| *byte == b'\n');
    let first = message_lines.next().unwrap_or_default();
    writeln!(stdout, "{name:<15} {}", String::from_utf8_lossy(first))?;
    for line in message_lines.take(lines.saturating_sub(1)) {
        writeln!(stdout, "    {}", String::from_utf8_lossy(line))?;
    }
    Ok(())
}

fn tag_message_without_signature(message: &[u8]) -> &[u8] {
    let marker = b"-----BEGIN PGP SIGNATURE-----";
    let Some(start) = message
        .windows(marker.len())
        .position(|window| window == marker)
    else {
        return message;
    };
    let mut end = start;
    while end > 0 && matches!(message[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &message[..end]
}

fn tag_format_peeled_object(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tag_oid: &ObjectId,
    contents: Option<&ForEachRefContents<'_>>,
) -> Result<Option<ForEachRefPeeledObject<'static>>> {
    let Some(peeled_oid) = contents.and_then(|contents| contents.tag_object.as_ref()) else {
        return Ok(None);
    };
    let peeled_object = db.read_object(peeled_oid)?;
    if let Some(contents) = contents {
        for_each_ref_validate_tag_pointer(tag_oid, contents, peeled_oid, &peeled_object)?;
    }
    let object_disk_size = for_each_ref_loose_object_disk_size(git_dir, peeled_oid)?;
    let (tree, parents, message, author, committer, creator) =
        if peeled_object.object_type == ObjectType::Commit {
            let commit = Commit::parse_ref(format, &peeled_object.body)?;
            (
                Some(commit.tree),
                commit.parents,
                Some(Cow::Owned(commit.message.to_vec())),
                Some(Cow::Owned(commit.author.to_vec())),
                Some(Cow::Owned(commit.committer.to_vec())),
                Some(Cow::Owned(commit.committer.to_vec())),
            )
        } else {
            (None, Vec::new(), None, None, None, None)
        };
    Ok(Some(ForEachRefPeeledObject {
        oid: peeled_oid.clone(),
        object_type: peeled_object.object_type,
        object_size: peeled_object.body.len(),
        object_disk_size,
        object_body: Cow::Owned(peeled_object.body.clone()),
        tree,
        parents,
        message,
        author,
        committer,
        creator,
    }))
}

fn sort_tag_entries(
    entries: &mut [TagListEntry],
    sorts: &[TagListSort],
    prereleases: &[String],
    ignore_case: bool,
) {
    if sorts.is_empty() && !ignore_case {
        return;
    }
    entries
        .sort_by(|left, right| compare_tag_sort_keys(left, right, sorts, prereleases, ignore_case));
}

fn compare_tag_sort_keys(
    left: &TagListEntry,
    right: &TagListEntry,
    sorts: &[TagListSort],
    prereleases: &[String],
    ignore_case: bool,
) -> std::cmp::Ordering {
    if sorts.is_empty() {
        return tag_refname_cmp(&left.name, &right.name, ignore_case);
    }
    for sort in sorts.iter().rev() {
        let ordering = match sort {
            TagListSort::Refname | TagListSort::RefnameDescending => {
                tag_refname_cmp(&left.name, &right.name, ignore_case)
            }
            TagListSort::VersionRefname | TagListSort::VersionRefnameDescending => {
                tag_version_refname_cmp(&left.name, &right.name, prereleases, ignore_case)
            }
            TagListSort::Objectname | TagListSort::ObjectnameDescending => {
                tag_objectname_cmp(left, right, ignore_case)
            }
            TagListSort::Objecttype | TagListSort::ObjecttypeDescending => {
                tag_objecttype_cmp(left, right, ignore_case)
            }
            TagListSort::Objectsize | TagListSort::ObjectsizeDescending => {
                tag_objectsize_cmp(left, right)
            }
            TagListSort::ObjectsizeDisk | TagListSort::ObjectsizeDiskDescending => {
                tag_objectsize_disk_cmp(left, right)
            }
            TagListSort::Deltabase | TagListSort::DeltabaseDescending => {
                tag_deltabase_cmp(left, right, ignore_case)
            }
            TagListSort::RawSize | TagListSort::RawSizeDescending => tag_raw_size_cmp(left, right),
            TagListSort::PeeledObjectname | TagListSort::PeeledObjectnameDescending => {
                tag_peeled_objectname_cmp(left, right, ignore_case)
            }
            TagListSort::PeeledObjecttype | TagListSort::PeeledObjecttypeDescending => {
                tag_peeled_objecttype_cmp(left, right, ignore_case)
            }
            TagListSort::PeeledObjectsize | TagListSort::PeeledObjectsizeDescending => {
                tag_peeled_objectsize_cmp(left, right)
            }
            TagListSort::PeeledObjectsizeDisk | TagListSort::PeeledObjectsizeDiskDescending => {
                tag_peeled_objectsize_disk_cmp(left, right)
            }
            TagListSort::PeeledDeltabase | TagListSort::PeeledDeltabaseDescending => {
                tag_peeled_deltabase_cmp(left, right, ignore_case)
            }
            TagListSort::PeeledRawSize | TagListSort::PeeledRawSizeDescending => {
                tag_peeled_raw_size_cmp(left, right)
            }
            TagListSort::Authordate | TagListSort::AuthordateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.authordate)
            }
            TagListSort::Committerdate | TagListSort::CommitterdateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.committerdate)
            }
            TagListSort::Taggerdate | TagListSort::TaggerdateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.taggerdate)
            }
            TagListSort::Creatordate | TagListSort::CreatordateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.creatordate)
            }
            TagListSort::PeeledAuthordate | TagListSort::PeeledAuthordateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.peeled_authordate)
            }
            TagListSort::PeeledCommitterdate | TagListSort::PeeledCommitterdateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.peeled_committerdate)
            }
            TagListSort::PeeledTaggerdate | TagListSort::PeeledTaggerdateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.peeled_taggerdate)
            }
            TagListSort::PeeledCreatordate | TagListSort::PeeledCreatordateDescending => {
                tag_date_cmp(left, right, |metadata| metadata.peeled_creatordate)
            }
            TagListSort::Identity(field) | TagListSort::IdentityDescending(field) => {
                tag_identity_cmp(left, right, *field, ignore_case)
            }
            TagListSort::Tag | TagListSort::TagDescending => tag_tag_cmp(left, right, ignore_case),
            TagListSort::Type | TagListSort::TypeDescending => {
                tag_type_cmp(left, right, ignore_case)
            }
            TagListSort::Object | TagListSort::ObjectDescending => {
                tag_object_cmp(left, right, ignore_case)
            }
            TagListSort::Tree | TagListSort::TreeDescending => {
                tag_tree_cmp(left, right, ignore_case)
            }
            TagListSort::Parent | TagListSort::ParentDescending => {
                tag_parent_cmp(left, right, ignore_case)
            }
            TagListSort::NumParent | TagListSort::NumParentDescending => {
                tag_numparent_cmp(left, right)
            }
            TagListSort::PeeledTree | TagListSort::PeeledTreeDescending => {
                tag_peeled_tree_cmp(left, right, ignore_case)
            }
            TagListSort::PeeledParent | TagListSort::PeeledParentDescending => {
                tag_peeled_parent_cmp(left, right, ignore_case)
            }
            TagListSort::PeeledNumParent | TagListSort::PeeledNumParentDescending => {
                tag_peeled_numparent_cmp(left, right)
            }
            TagListSort::PeeledSubject | TagListSort::PeeledSubjectDescending => {
                tag_peeled_subject_cmp(left, right, ignore_case)
            }
            TagListSort::PeeledBody | TagListSort::PeeledBodyDescending => {
                tag_peeled_body_cmp(left, right, ignore_case)
            }
            TagListSort::PeeledContentsSize | TagListSort::PeeledContentsSizeDescending => {
                tag_peeled_contents_size_cmp(left, right)
            }
            TagListSort::Subject | TagListSort::SubjectDescending => {
                tag_subject_cmp(left, right, ignore_case)
            }
            TagListSort::Body | TagListSort::BodyDescending => {
                tag_body_cmp(left, right, ignore_case)
            }
            TagListSort::ContentsSize | TagListSort::ContentsSizeDescending => {
                tag_contents_size_cmp(left, right)
            }
        };
        let ordering = if sort.descending() {
            ordering.reverse()
        } else {
            ordering
        };
        if !ordering.is_eq() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

fn tag_objectname_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_ref_target_sort_key(&left.reference.target),
        &tag_ref_target_sort_key(&right.reference.target),
        ignore_case,
    )
}

fn tag_ref_target_sort_key(target: &RefTarget) -> String {
    match target {
        RefTarget::Direct(oid) => oid.to_hex(),
        RefTarget::Symbolic(target) => target.clone(),
    }
}

fn tag_objecttype_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        left.object_metadata
            .as_ref()
            .map(|metadata| metadata.object_type.as_str())
            .unwrap_or_default(),
        right
            .object_metadata
            .as_ref()
            .map(|metadata| metadata.object_type.as_str())
            .unwrap_or_default(),
        ignore_case,
    )
}

fn tag_objectsize_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    left.object_metadata
        .as_ref()
        .map(|metadata| metadata.object_size)
        .unwrap_or_default()
        .cmp(
            &right
                .object_metadata
                .as_ref()
                .map(|metadata| metadata.object_size)
                .unwrap_or_default(),
        )
}

fn tag_objectsize_disk_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    left.object_metadata
        .as_ref()
        .map(|metadata| metadata.object_disk_size)
        .unwrap_or_default()
        .cmp(
            &right
                .object_metadata
                .as_ref()
                .map(|metadata| metadata.object_disk_size)
                .unwrap_or_default(),
        )
}

fn tag_deltabase_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        left.object_metadata
            .as_ref()
            .map(|metadata| metadata.deltabase.as_str())
            .unwrap_or_default(),
        right
            .object_metadata
            .as_ref()
            .map(|metadata| metadata.deltabase.as_str())
            .unwrap_or_default(),
        ignore_case,
    )
}

fn tag_raw_size_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_objectsize_cmp(left, right)
}

fn tag_peeled_objectname_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_peeled_objectname_key(left),
        &tag_peeled_objectname_key(right),
        ignore_case,
    )
}

fn tag_peeled_objectname_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .map(|peeled| peeled.oid.to_hex())
        .unwrap_or_default()
}

fn tag_peeled_objecttype_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_peeled_objecttype_key(left),
        &tag_peeled_objecttype_key(right),
        ignore_case,
    )
}

fn tag_peeled_objecttype_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .map(|peeled| peeled.object_type.as_str().to_string())
        .unwrap_or_default()
}

fn tag_peeled_objectsize_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_peeled_objectsize_key(left).cmp(&tag_peeled_objectsize_key(right))
}

fn tag_peeled_objectsize_key(entry: &TagListEntry) -> usize {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .map(|peeled| peeled.object_size)
        .unwrap_or_default()
}

fn tag_peeled_objectsize_disk_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_peeled_objectsize_disk_key(left).cmp(&tag_peeled_objectsize_disk_key(right))
}

fn tag_peeled_objectsize_disk_key(entry: &TagListEntry) -> u64 {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .and_then(|peeled| peeled.object_disk_size)
        .unwrap_or_default()
}

fn tag_peeled_deltabase_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_peeled_deltabase_key(left),
        &tag_peeled_deltabase_key(right),
        ignore_case,
    )
}

fn tag_peeled_deltabase_key(entry: &TagListEntry) -> String {
    let Some(metadata) = entry.object_metadata.as_ref() else {
        return String::new();
    };
    if metadata.peeled_object.is_some() {
        metadata.deltabase.clone()
    } else {
        String::new()
    }
}

fn tag_peeled_raw_size_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_peeled_objectsize_cmp(left, right)
}

fn tag_date_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    key: impl Fn(&TagListObjectMetadata) -> i128,
) -> std::cmp::Ordering {
    left.object_metadata
        .as_ref()
        .map(&key)
        .unwrap_or_default()
        .cmp(&right.object_metadata.as_ref().map(key).unwrap_or_default())
}

fn tag_identity_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    field: ForEachRefIdentitySortField,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_identity_key(left, field),
        &tag_identity_key(right, field),
        ignore_case,
    )
}

fn tag_identity_key(entry: &TagListEntry, field: ForEachRefIdentitySortField) -> String {
    entry
        .object_metadata
        .as_ref()
        .map(|metadata| match field.source {
            ForEachRefIdentitySource::Direct => {
                for_each_ref_sort_identity_key(metadata.contents.as_ref(), field)
            }
            ForEachRefIdentitySource::Peeled => {
                tag_peeled_identity_key(metadata.peeled_object.as_ref(), field)
            }
        })
        .unwrap_or_default()
}

fn tag_peeled_identity_key(
    peeled_object: Option<&ForEachRefPeeledObject<'_>>,
    field: ForEachRefIdentitySortField,
) -> String {
    let identity = match field.role {
        ForEachRefIdentityRole::Author => peeled_object.and_then(|peeled| peeled.author.as_deref()),
        ForEachRefIdentityRole::Committer => {
            peeled_object.and_then(|peeled| peeled.committer.as_deref())
        }
        ForEachRefIdentityRole::Tagger => None,
        ForEachRefIdentityRole::Creator => {
            peeled_object.and_then(|peeled| peeled.creator.as_deref())
        }
    };
    let value = match field.part {
        ForEachRefIdentityPart::Full => identity,
        ForEachRefIdentityPart::Name => identity.and_then(for_each_ref_identity_name),
        ForEachRefIdentityPart::Email => identity.and_then(|identity| {
            for_each_ref_identity_email(identity, ForEachRefEmailMode::Bracketed)
        }),
    };
    value
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .unwrap_or_default()
}

fn tag_tag_cmp(left: &TagListEntry, right: &TagListEntry, ignore_case: bool) -> std::cmp::Ordering {
    tag_text_cmp(&tag_tag_key(left), &tag_tag_key(right), ignore_case)
}

fn tag_tag_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .and_then(|contents| contents.tag.as_ref())
        .map(|tag| String::from_utf8_lossy(tag).into_owned())
        .unwrap_or_default()
}

fn tag_type_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(&tag_type_key(left), &tag_type_key(right), ignore_case)
}

fn tag_type_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .and_then(|contents| contents.tag_object_type)
        .map(|object_type| object_type.as_str().to_string())
        .unwrap_or_default()
}

fn tag_object_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(&tag_object_key(left), &tag_object_key(right), ignore_case)
}

fn tag_object_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .and_then(|contents| contents.tag_object.as_ref())
        .map(ObjectId::to_hex)
        .unwrap_or_default()
}

fn tag_tree_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(&tag_tree_key(left), &tag_tree_key(right), ignore_case)
}

fn tag_tree_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .and_then(|contents| contents.tree.as_ref())
        .map(ObjectId::to_hex)
        .unwrap_or_default()
}

fn tag_parent_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(&tag_parent_key(left), &tag_parent_key(right), ignore_case)
}

fn tag_parent_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .map(|contents| {
            contents
                .parents
                .iter()
                .map(ObjectId::to_hex)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn tag_numparent_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_numparent_key(left).cmp(&tag_numparent_key(right))
}

fn tag_numparent_key(entry: &TagListEntry) -> usize {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .filter(|contents| contents.tree.is_some())
        .map(|contents| contents.parents.len())
        .unwrap_or_default()
}

fn tag_peeled_tree_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_peeled_tree_key(left),
        &tag_peeled_tree_key(right),
        ignore_case,
    )
}

fn tag_peeled_tree_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .and_then(|peeled| peeled.tree.as_ref())
        .map(ObjectId::to_hex)
        .unwrap_or_default()
}

fn tag_peeled_parent_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_peeled_parent_key(left),
        &tag_peeled_parent_key(right),
        ignore_case,
    )
}

fn tag_peeled_parent_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .map(|peeled| {
            peeled
                .parents
                .iter()
                .map(ObjectId::to_hex)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn tag_peeled_numparent_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_peeled_numparent_key(left).cmp(&tag_peeled_numparent_key(right))
}

fn tag_peeled_numparent_key(entry: &TagListEntry) -> usize {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .filter(|peeled| peeled.tree.is_some())
        .map(|peeled| peeled.parents.len())
        .unwrap_or_default()
}

fn tag_peeled_subject_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_peeled_subject_key(left),
        &tag_peeled_subject_key(right),
        ignore_case,
    )
}

fn tag_peeled_subject_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .and_then(|peeled| peeled.message.as_ref())
        .map(|message| commit_subject(message))
        .unwrap_or_default()
}

fn tag_peeled_body_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(
        &tag_peeled_body_key(left),
        &tag_peeled_body_key(right),
        ignore_case,
    )
}

fn tag_peeled_body_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .and_then(|peeled| peeled.message.as_ref())
        .map(|message| String::from_utf8_lossy(commit_body(message)).into_owned())
        .unwrap_or_default()
}

fn tag_peeled_contents_size_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_peeled_contents_size_key(left).cmp(&tag_peeled_contents_size_key(right))
}

fn tag_peeled_contents_size_key(entry: &TagListEntry) -> usize {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.peeled_object.as_ref())
        .and_then(|peeled| peeled.message.as_ref())
        .map(|message| message.len())
        .unwrap_or_default()
}

fn tag_subject_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(&tag_subject_key(left), &tag_subject_key(right), ignore_case)
}

fn tag_subject_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .map(|contents| commit_subject(&contents.message))
        .unwrap_or_default()
}

fn tag_body_cmp(
    left: &TagListEntry,
    right: &TagListEntry,
    ignore_case: bool,
) -> std::cmp::Ordering {
    tag_text_cmp(&tag_body_key(left), &tag_body_key(right), ignore_case)
}

fn tag_body_key(entry: &TagListEntry) -> String {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .map(|contents| String::from_utf8_lossy(commit_body(&contents.message)).into_owned())
        .unwrap_or_default()
}

fn tag_contents_size_cmp(left: &TagListEntry, right: &TagListEntry) -> std::cmp::Ordering {
    tag_contents_size_key(left).cmp(&tag_contents_size_key(right))
}

fn tag_contents_size_key(entry: &TagListEntry) -> usize {
    entry
        .object_metadata
        .as_ref()
        .and_then(|metadata| metadata.contents.as_ref())
        .map(|contents| contents.message.len())
        .unwrap_or_default()
}

fn tag_text_cmp(left: &str, right: &str, ignore_case: bool) -> std::cmp::Ordering {
    if ignore_case {
        left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
    } else {
        left.cmp(right)
    }
}

fn tag_refname_cmp(left: &str, right: &str, ignore_case: bool) -> std::cmp::Ordering {
    if ignore_case {
        left.to_ascii_lowercase()
            .cmp(&right.to_ascii_lowercase())
            .then_with(|| left.cmp(right))
    } else {
        left.cmp(right)
    }
}

fn tag_version_refname_cmp(
    left: &str,
    right: &str,
    prereleases: &[String],
    ignore_case: bool,
) -> std::cmp::Ordering {
    if ignore_case {
        version_sort_cmp(
            &left.to_ascii_lowercase(),
            &right.to_ascii_lowercase(),
            prereleases,
        )
        .then_with(|| left.cmp(right))
    } else {
        version_sort_cmp(left, right, prereleases)
    }
}

fn tag_contains(
    db: &Option<FileObjectDatabase>,
    format: ObjectFormat,
    target: &RefTarget,
    contains: &[ObjectId],
    no_contains: &[ObjectId],
) -> Result<bool> {
    if contains.is_empty() && no_contains.is_empty() {
        return Ok(true);
    }
    let RefTarget::Direct(oid) = target else {
        return Ok(false);
    };
    let Some(db) = db else {
        return Ok(false);
    };
    let Ok(tip) = sley_rev::peel_to_commit(db, format, oid) else {
        return Ok(false);
    };
    let reachable = sley_rev::walk_commits(db, format, [tip])?
        .into_iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    if !contains.is_empty() && !contains.iter().any(|target| reachable.contains(target)) {
        return Ok(false);
    }
    if no_contains.iter().any(|target| reachable.contains(target)) {
        return Ok(false);
    }
    Ok(true)
}

fn tag_merged(
    db: &Option<FileObjectDatabase>,
    format: ObjectFormat,
    target: &RefTarget,
    merged_reachable: &[HashSet<ObjectId>],
    no_merged_reachable: &[HashSet<ObjectId>],
) -> Result<bool> {
    if merged_reachable.is_empty() && no_merged_reachable.is_empty() {
        return Ok(true);
    };
    let Some(db) = db else {
        return Ok(false);
    };
    let RefTarget::Direct(oid) = target else {
        return Ok(false);
    };
    let Ok(tip) = sley_rev::peel_to_commit(db, format, oid) else {
        return Ok(false);
    };
    let merged_match =
        merged_reachable.is_empty() || merged_reachable.iter().any(|set| set.contains(&tip));
    let no_merged_match = no_merged_reachable.iter().any(|set| set.contains(&tip));
    Ok(merged_match && !no_merged_match)
}

fn tag_merged_reachable_sets(
    db: Option<&FileObjectDatabase>,
    format: ObjectFormat,
    filters: &[ObjectId],
) -> Result<Vec<HashSet<ObjectId>>> {
    if filters.is_empty() {
        return Ok(Vec::new());
    }
    let Some(db) = db else {
        return Ok(Vec::new());
    };
    filters
        .iter()
        .map(|oid| {
            let commit = sley_rev::peel_to_commit(db, format, oid)?;
            sley_rev::walk_commits(db, format, [commit]).map(|records| {
                records
                    .into_iter()
                    .map(|record| record.oid)
                    .collect::<HashSet<_>>()
            })
        })
        .collect()
}

fn tag_points_at(
    db: &Option<FileObjectDatabase>,
    format: ObjectFormat,
    target: &RefTarget,
    points_at: &[ObjectId],
) -> Result<bool> {
    if points_at.is_empty() {
        return Ok(true);
    }
    let RefTarget::Direct(oid) = target else {
        return Ok(false);
    };
    if points_at.iter().any(|point| point == oid) {
        return Ok(true);
    }
    let Some(db) = db else {
        return Ok(false);
    };
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(false);
    }
    let parsed = Tag::parse(format, &object.body)?;
    Ok(points_at.iter().any(|point| point == &parsed.object))
}

#[cfg(test)]
mod tests {
    use super::expand_tag_bundle;

    #[test]
    fn expands_value_terminated_boolean_bundles() {
        assert_eq!(
            expand_tag_bundle("-am"),
            Some(vec!["-a".to_string(), "-m".to_string()])
        );
        assert_eq!(
            expand_tag_bundle("-amhello"),
            Some(vec!["-a".to_string(), "-mhello".to_string()])
        );
        assert_eq!(
            expand_tag_bundle("-saF"),
            Some(vec!["-s".to_string(), "-a".to_string(), "-F".to_string()])
        );
    }

    #[test]
    fn leaves_non_value_bundles_verbatim() {
        // Pure-boolean bundles keep their (usage-error) semantics.
        for bundle in ["-av", "-fl", "-ai", "-vf", "-ab"] {
            assert_eq!(expand_tag_bundle(bundle), None, "{bundle}");
        }
        // A `-`-prefixed value that looks like a bundle (e.g. a `--sort` key) is
        // never split, because its first byte is not a boolean short flag or the
        // bundle reaches a non-flag byte.
        assert_eq!(expand_tag_bundle("-objectname"), None);
        assert_eq!(expand_tag_bundle("-authoremail"), Some(
            // 'a' then 'u' (value flag) — split is fine here in isolation; the
            // parse loop only consults this in option position, never on a
            // consumed `--sort` value.
            vec!["-a".to_string(), "-uthoremail".to_string()]
        ));
        // Long options and glued value flags pass through.
        assert_eq!(expand_tag_bundle("--sort"), None);
        assert_eq!(expand_tag_bundle("-mhi"), None);
        assert_eq!(expand_tag_bundle("-n5"), None);
    }
}
