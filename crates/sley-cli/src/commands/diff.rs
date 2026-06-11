//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

/// Peel a single revision string to the tree it names (commit/tag/tree all work).
fn diff_peel_rev_tree(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    rev: &str,
) -> Result<ObjectId> {
    let oid = resolve_revision(git_dir, format, rev)?;
    sley_rev::peel_to_tree(db, format, &oid)
}

fn diff_split_revisions(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    path_args: Vec<String>,
) -> Result<(Vec<ObjectId>, Vec<String>)> {
    let Some(first) = path_args.first() else {
        return Ok((Vec::new(), Vec::new()));
    };
    // Range forms name exactly two trees and consume only the first token. Check
    // `...` before `..` so `A...B` is not mis-split, and require both sides so a
    // relative path like `../x` (left side empty) is never taken as a range.
    // `A...B` (symmetric): diff merge-base(A,B)..B. An omitted side defaults to
    // HEAD. It is only a range when *both* endpoints resolve as revisions —
    // otherwise the token (e.g. a relative path `../x`) falls through to pathspec
    // handling, matching git's disambiguation.
    if let Some((left, right)) = first.split_once("...") {
        let left_spec = if left.is_empty() { "HEAD" } else { left };
        let right_spec = if right.is_empty() { "HEAD" } else { right };
        if let (Ok(left_oid), Ok(right_oid)) = (
            resolve_revision(git_dir, format, left_spec),
            resolve_revision(git_dir, format, right_spec),
        ) {
            let Some(base) = sley_rev::merge_bases(git_dir, format, db, &left_oid, &right_oid)?
                .into_iter()
                .next()
            else {
                eprintln!("fatal: {first}: no merge base");
                return Err(GitError::Exit(128));
            };
            let base_tree = sley_rev::peel_to_tree(db, format, &base)?;
            let right_tree = sley_rev::peel_to_tree(db, format, &right_oid)?;
            return Ok((vec![base_tree, right_tree], path_args[1..].to_vec()));
        }
    }
    // `A..B`: diff A..B. Omitted side defaults to HEAD; only a range when both
    // endpoints resolve.
    if let Some((left, right)) = first.split_once("..") {
        let left_spec = if left.is_empty() { "HEAD" } else { left };
        let right_spec = if right.is_empty() { "HEAD" } else { right };
        if let (Ok(left_tree), Ok(right_tree)) = (
            diff_peel_rev_tree(git_dir, format, db, left_spec),
            diff_peel_rev_tree(git_dir, format, db, right_spec),
        ) {
            return Ok((vec![left_tree, right_tree], path_args[1..].to_vec()));
        }
    }
    // Otherwise peel up to two leading args that each resolve as a revision.
    let mut trees = Vec::new();
    let mut rest = Vec::new();
    let mut iter = path_args.into_iter();
    for token in iter.by_ref() {
        if trees.len() < 2
            && let Ok(tree) = diff_peel_rev_tree(git_dir, format, db, &token)
        {
            trees.push(tree);
            continue;
        }
        rest.push(token);
        break;
    }
    rest.extend(iter);
    Ok((trees, rest))
}

pub(crate) fn cmd_diff(args: &[String]) -> Result<()> {
    let mut name_status = false;
    let mut name_only = false;
    let mut cached = false;
    let mut quiet = false;
    let mut exit_code = false;
    let mut summary = false;
    let mut raw = false;
    let mut stat = false;
    let mut compact_summary = false;
    let mut stat_count = None;
    // `git diff` is porcelain: scale --stat to the terminal and honour the
    // diff.stat*Width config (resolved after the repository is discovered).
    let mut stat_widths = DiffStatWidths::terminal();
    let mut numstat = false;
    let mut shortstat = false;
    let mut patch = false;
    let mut no_patch = false;
    let mut reverse = false;
    let mut pickaxe = None;
    let mut pickaxe_all = false;
    let mut pickaxe_regex = false;
    let mut find_object_values = Vec::new();
    let mut raw_abbrev = None;
    let mut patch_abbrev = None;
    let mut patch_full_index = false;
    let mut color_always = false;
    let mut diff_algorithm_control = false;
    let mut diff_driver_control = false;
    let mut diff_hunk_control = false;
    let mut diff_whitespace_control = false;
    let mut diff_output_indicator_control = false;
    let mut diff_patch_context_control = false;
    let mut diff_patch_output_control = false;
    let mut diff_rewrite_control = false;
    let mut diff_submodule_output_control = false;
    let mut diff_word_control = false;
    let mut diff_relative = DiffRelativeMode::Off;
    let mut src_prefix = "a/".to_string();
    let mut dst_prefix = "b/".to_string();
    let mut head = false;
    let mut z = false;
    let mut detect_renames = true;
    let mut detect_copies = false;
    let mut find_copies_harder = false;
    let mut rename_empty = true;
    // git enables rename detection by default (diff.renames defaults to true);
    // --no-renames turns it off. -M/-C select the similarity thresholds.
    let mut inexact_renames = true;
    let mut rename_threshold = sley_diff_merge::DEFAULT_RENAME_THRESHOLD;
    let mut copy_threshold = sley_diff_merge::DEFAULT_RENAME_THRESHOLD;
    let mut diff_filter = DiffFilter::default();
    let mut path_args = Vec::new();
    // Arguments after `--` are always pathspecs, never revisions; keep them apart
    // so the revision splitter only ever reinterprets the pre-`--` positionals.
    let mut explicit_paths: Vec<String> = Vec::new();
    let mut positional_only = false;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if positional_only {
            explicit_paths.push(arg.clone());
            idx += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--name-status" => {
                if no_patch {
                    return Err(GitError::Command(
                        "options '--name-only', '--name-status', and '-s' cannot be used together"
                            .into(),
                    ));
                }
                name_status = true;
            }
            "--name-only" => {
                if no_patch {
                    return Err(GitError::Command(
                        "options '--name-only', '--name-status', and '-s' cannot be used together"
                            .into(),
                    ));
                }
                name_only = true;
            }
            "--cached" | "--staged" => cached = true,
            "--quiet" => quiet = true,
            "--exit-code" => exit_code = true,
            "--summary" => {
                summary = true;
                no_patch = false;
            }
            "--raw" => {
                raw = true;
                no_patch = false;
            }
            "--stat" => {
                stat = true;
                no_patch = false;
            }
            "--compact-summary" => {
                compact_summary = true;
                no_patch = false;
            }
            "--numstat" => {
                numstat = true;
                no_patch = false;
            }
            "--shortstat" => {
                shortstat = true;
                no_patch = false;
            }
            "-p" | "-u" | "--patch" => {
                patch = true;
                no_patch = false;
            }
            "--patch-with-raw" => {
                raw = true;
                patch = true;
                no_patch = false;
            }
            "--patch-with-stat" => {
                stat = true;
                patch = true;
                no_patch = false;
            }
            "-s" | "--no-patch" => {
                name_status = false;
                name_only = false;
                summary = false;
                raw = false;
                stat = false;
                compact_summary = false;
                numstat = false;
                shortstat = false;
                patch = false;
                no_patch = true;
            }
            "-a" | "--text" | "--no-ext-diff" | "--no-textconv" => {}
            "-R" => reverse = true,
            "-S" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(diff_pickaxe_requires_value_error)?;
                if value.is_empty() {
                    return Err(diff_pickaxe_requires_non_empty_error());
                }
                pickaxe = Some(value.clone());
            }
            value if let Some(value) = value.strip_prefix("-S") => {
                pickaxe = Some(value.to_string());
            }
            "--pickaxe-all" => pickaxe_all = true,
            "--pickaxe-regex" => pickaxe_regex = true,
            value if value.starts_with("--pickaxe-all=") => {
                return log_option_takes_no_value_error("pickaxe-all");
            }
            value if value.starts_with("--pickaxe-regex=") => {
                return log_option_takes_no_value_error("pickaxe-regex");
            }
            "--find-object" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("find-object"))?;
                find_object_values.push(value.clone());
            }
            value if let Some(value) = value.strip_prefix("--find-object=") => {
                find_object_values.push(value.to_string());
            }
            "--ext-diff" | "--textconv" => diff_driver_control = true,
            "--minimal" | "--patience" | "--histogram" => diff_algorithm_control = true,
            "--anchored" => {
                idx += 1;
                args.get(idx)
                    .ok_or_else(|| log_option_requires_value_error("anchored"))?;
                diff_algorithm_control = true;
            }
            value if value.starts_with("--anchored=") => diff_algorithm_control = true,
            "--diff-algorithm" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("diff-algorithm"))?;
                log_validate_diff_algorithm(value)?;
                diff_algorithm_control = true;
            }
            value if let Some(value) = value.strip_prefix("--diff-algorithm=") => {
                log_validate_diff_algorithm(value)?;
                diff_algorithm_control = true;
            }
            value if value.starts_with("--ext-diff=") => {
                return log_option_takes_no_value_error("ext-diff");
            }
            value if value.starts_with("--textconv=") => {
                return log_option_takes_no_value_error("textconv");
            }
            "--inter-hunk-context" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("inter-hunk-context"))?;
                log_validate_inter_hunk_context(value)?;
                diff_hunk_control = true;
            }
            "--inter-hunk-context=" => {
                return log_inter_hunk_context_requires_number_error();
            }
            value if let Some(value) = value.strip_prefix("--inter-hunk-context=") => {
                log_validate_inter_hunk_context(value)?;
                diff_hunk_control = true;
            }
            "--ws-error-highlight" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("ws-error-highlight"))?;
                log_validate_ws_error_highlight(value)?;
                diff_whitespace_control = true;
            }
            value if let Some(value) = value.strip_prefix("--ws-error-highlight=") => {
                log_validate_ws_error_highlight(value)?;
                diff_whitespace_control = true;
            }
            "-b"
            | "-w"
            | "--ignore-space-at-eol"
            | "--ignore-cr-at-eol"
            | "--ignore-space-change"
            | "--ignore-all-space"
            | "--ignore-blank-lines" => diff_whitespace_control = true,
            value if value.starts_with("--ignore-space-at-eol=") => {
                return log_option_takes_no_value_error("ignore-space-at-eol");
            }
            value if value.starts_with("--ignore-cr-at-eol=") => {
                return log_option_takes_no_value_error("ignore-cr-at-eol");
            }
            value if value.starts_with("--ignore-space-change=") => {
                return log_option_takes_no_value_error("ignore-space-change");
            }
            value if value.starts_with("--ignore-all-space=") => {
                return log_option_takes_no_value_error("ignore-all-space");
            }
            value if value.starts_with("--ignore-blank-lines=") => {
                return log_option_takes_no_value_error("ignore-blank-lines");
            }
            "--submodule" => diff_submodule_output_control = true,
            value if let Some(value) = value.strip_prefix("--submodule=") => {
                log_validate_submodule_format(value)?;
                diff_submodule_output_control = true;
            }
            "--word-diff" => diff_word_control = true,
            value if let Some(value) = value.strip_prefix("--word-diff=") => {
                log_validate_word_diff(value)?;
                diff_word_control = true;
            }
            "--word-diff-regex" => {
                idx += 1;
                args.get(idx)
                    .ok_or_else(|| log_option_requires_value_error("word-diff-regex"))?;
                diff_word_control = true;
            }
            value if value.starts_with("--word-diff-regex=") => diff_word_control = true,
            "--color-words" => diff_word_control = true,
            value if value.starts_with("--color-words=") => diff_word_control = true,
            "--output-indicator-new" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-new"))?;
                log_validate_output_indicator("output-indicator-new", value)?;
                diff_output_indicator_control = true;
            }
            "--output-indicator-old" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-old"))?;
                log_validate_output_indicator("output-indicator-old", value)?;
                diff_output_indicator_control = true;
            }
            "--output-indicator-context" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("output-indicator-context"))?;
                log_validate_output_indicator("output-indicator-context", value)?;
                diff_output_indicator_control = true;
            }
            value if let Some(value) = value.strip_prefix("--output-indicator-new=") => {
                log_validate_output_indicator("output-indicator-new", value)?;
                diff_output_indicator_control = true;
            }
            value if let Some(value) = value.strip_prefix("--output-indicator-old=") => {
                log_validate_output_indicator("output-indicator-old", value)?;
                diff_output_indicator_control = true;
            }
            value if let Some(value) = value.strip_prefix("--output-indicator-context=") => {
                log_validate_output_indicator("output-indicator-context", value)?;
                diff_output_indicator_control = true;
            }
            "-W" | "--function-context" | "--indent-heuristic" | "--no-indent-heuristic" => {
                diff_patch_context_control = true;
            }
            "--full-diff"
            | "-D"
            | "--irreversible-delete"
            | "--ita-visible-in-index"
            | "--ita-invisible-in-index" => {
                diff_patch_output_control = true;
            }
            "-B" | "--break-rewrites" => diff_rewrite_control = true,
            value if let Some(value) = value.strip_prefix("-B") => {
                log_validate_break_rewrites_option(value)?;
                diff_rewrite_control = true;
            }
            value if let Some(value) = value.strip_prefix("--break-rewrites=") => {
                log_validate_break_rewrites_option(value)?;
                diff_rewrite_control = true;
            }
            value if value.starts_with("--function-context=") => {
                return log_option_takes_no_value_error("function-context");
            }
            value if value.starts_with("--indent-heuristic=") => {
                return log_option_takes_no_value_error("indent-heuristic");
            }
            value if value.starts_with("--no-indent-heuristic=") => {
                return log_option_takes_no_value_error("no-indent-heuristic");
            }
            value if value.starts_with("--full-diff=") => {
                return log_option_takes_no_value_error("full-diff");
            }
            value if value.starts_with("--irreversible-delete=") => {
                return log_option_takes_no_value_error("irreversible-delete");
            }
            value if value.starts_with("--ita-visible-in-index=") => {
                return log_option_takes_no_value_error("ita-visible-in-index");
            }
            value if value.starts_with("--ita-invisible-in-index=") => {
                return log_option_takes_no_value_error("ita-invisible-in-index");
            }
            "--relative" => diff_relative = DiffRelativeMode::Cwd,
            value if let Some(value) = value.strip_prefix("--relative=") => {
                diff_relative = DiffRelativeMode::Prefix(value.to_string());
            }
            "--no-relative" => diff_relative = DiffRelativeMode::Off,
            value if value.starts_with("--no-relative=") => {
                return log_option_takes_no_value_error("no-relative");
            }
            "--color" | "--color=always" => color_always = true,
            "--no-color" | "--color=never" | "--color=auto" => color_always = false,
            "--color-moved" | "--no-color-moved" | "--no-color-moved-ws" => {}
            value if let Some(value) = value.strip_prefix("--color-moved=") => {
                log_validate_color_moved(value)?;
            }
            "--color-moved-ws" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| log_option_requires_value_error("color-moved-ws"))?;
                log_validate_color_moved_ws(value)?;
            }
            value if let Some(value) = value.strip_prefix("--color-moved-ws=") => {
                log_validate_color_moved_ws(value)?;
            }
            value if value.starts_with("--no-color-moved-ws=") => {
                return log_option_takes_no_value_error("no-color-moved-ws");
            }
            "--ignore-submodules"
            | "--ignore-submodules=none"
            | "--ignore-submodules=untracked"
            | "--ignore-submodules=dirty"
            | "--ignore-submodules=all" => {}
            "--abbrev" => {
                raw_abbrev = Some(Some(7));
                patch_abbrev = Some(7);
            }
            "--no-abbrev" => raw_abbrev = Some(None),
            "--full-index" => patch_full_index = true,
            "--no-prefix" => {
                src_prefix.clear();
                dst_prefix.clear();
            }
            "--default-prefix" => {
                src_prefix = "a/".to_string();
                dst_prefix = "b/".to_string();
            }
            "--src-prefix" => {
                idx += 1;
                src_prefix = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--src-prefix requires a value".into()))?
                    .clone();
            }
            "--dst-prefix" => {
                idx += 1;
                dst_prefix = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--dst-prefix requires a value".into()))?
                    .clone();
            }
            "-z" => z = true,
            "-M" | "--find-renames" => {
                detect_renames = true;
                inexact_renames = true;
            }
            "-C" | "--find-copies" => {
                detect_copies = true;
                inexact_renames = true;
            }
            "--find-copies-harder" => {
                detect_copies = true;
                find_copies_harder = true;
                inexact_renames = true;
            }
            "--no-find-copies-harder" => {
                find_copies_harder = false;
            }
            "--no-renames" => {
                detect_renames = false;
                inexact_renames = false;
            }
            "--rename-empty" => rename_empty = true,
            "--no-rename-empty" => rename_empty = false,
            value if value.starts_with("-M") && value.len() > 2 => {
                log_validate_similarity_option(&value[2..], "find-renames")?;
                detect_renames = true;
                inexact_renames = true;
                rename_threshold = parse_similarity_threshold(&value[2..]);
            }
            value if let Some(value) = value.strip_prefix("--find-renames=") => {
                log_validate_similarity_option(value, "find-renames")?;
                detect_renames = true;
                inexact_renames = true;
                rename_threshold = parse_similarity_threshold(value);
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                log_validate_similarity_option(&value[2..], "find-copies")?;
                detect_copies = true;
                inexact_renames = true;
                copy_threshold = parse_similarity_threshold(&value[2..]);
            }
            value if let Some(value) = value.strip_prefix("--find-copies=") => {
                log_validate_similarity_option(value, "find-copies")?;
                detect_copies = true;
                inexact_renames = true;
                copy_threshold = parse_similarity_threshold(value);
            }
            value if value.starts_with("--find-copies-harder=") => {
                return log_option_takes_no_value_error("find-copies-harder");
            }
            value if value.starts_with("--no-find-copies-harder=") => {
                return log_option_takes_no_value_error("no-find-copies-harder");
            }
            value if value.starts_with("--rename-empty=") => {
                return log_option_takes_no_value_error("rename-empty");
            }
            value if value.starts_with("--no-rename-empty=") => {
                return log_option_takes_no_value_error("no-rename-empty");
            }
            "-l" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(diff_rename_limit_requires_integer_error)?;
                validate_diff_rename_limit(value)?;
            }
            value if let Some(value) = value.strip_prefix("-l") => {
                validate_diff_rename_limit(value)?;
            }
            "--diff-filter" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| GitError::Command("--diff-filter requires a value".into()))?;
                diff_filter = parse_diff_filter(value)?;
            }
            value if value.starts_with("--diff-filter=") => {
                let value = value
                    .strip_prefix("--diff-filter=")
                    .ok_or_else(|| GitError::Command("--diff-filter requires a value".into()))?;
                diff_filter = parse_diff_filter(value)?;
            }
            value
                if value.starts_with("--stat=")
                    || value.starts_with("--stat-width=")
                    || value.starts_with("--stat-name-width=")
                    || value.starts_with("--stat-graph-width=")
                    || value.starts_with("--stat-count=") =>
            {
                stat = true;
                no_patch = false;
                diff_stat_parse_width_option(value, &mut stat_widths)?;
                if let Some(count) = diff_stat_count_option(value)? {
                    stat_count = count;
                }
            }
            value if value.starts_with("--abbrev=") => {
                let value = value
                    .strip_prefix("--abbrev=")
                    .ok_or_else(|| GitError::Command("--abbrev requires a value".into()))?;
                raw_abbrev = Some(Some(parse_abbrev(value)?.max(4)));
                patch_abbrev = raw_abbrev.flatten();
            }
            value if value.starts_with("--src-prefix=") => {
                src_prefix = value
                    .strip_prefix("--src-prefix=")
                    .ok_or_else(|| GitError::Command("--src-prefix requires a value".into()))?
                    .to_string();
            }
            value if value.starts_with("--dst-prefix=") => {
                dst_prefix = value
                    .strip_prefix("--dst-prefix=")
                    .ok_or_else(|| GitError::Command("--dst-prefix requires a value".into()))?
                    .to_string();
            }
            value if value.starts_with("--default-prefix=") => {
                return Err(GitError::Command(format!(
                    "option `{}` takes no value",
                    value
                        .trim_start_matches('-')
                        .split_once('=')
                        .map(|(name, _)| name)
                        .unwrap_or("default-prefix")
                )));
            }
            "HEAD" if !head && path_args.is_empty() => head = true,
            value if !value.starts_with('-') => path_args.push(arg.clone()),
            value => {
                return Err(GitError::Command(format!(
                    "unsupported diff argument {value}"
                )));
            }
        }
        idx += 1;
    }
    if name_status && name_only {
        return Err(GitError::Command(
            "diff currently supports: diff [--cached] [-z] [-M|-C] [--diff-filter=<filter>] [--exit-code|--quiet] [--abbrev[=<n>]|--no-abbrev] [--src-prefix=<prefix>|--dst-prefix=<prefix>|--no-prefix|--default-prefix] [--raw|--stat|--compact-summary|--numstat|--shortstat|--summary|--name-status|--name-only|-p|-u|--patch|--patch-with-raw|--patch-with-stat|-s|--no-patch] [HEAD] [-- <path>...] and diff [--cached] [-z] --quiet [HEAD]".into(),
        ));
    }
    if color_always && !name_status && !name_only && !stat && !compact_summary && !shortstat {
        return Err(GitError::Unsupported(
            "diff colored output is not supported for this output mode".into(),
        ));
    }
    if diff_algorithm_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff algorithm controls are not supported for this output mode".into(),
        ));
    }
    if diff_driver_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff driver controls are not supported for this output mode".into(),
        ));
    }
    if diff_hunk_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff hunk context controls are not supported for this output mode".into(),
        ));
    }
    if diff_whitespace_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff whitespace controls are not supported for this output mode".into(),
        ));
    }
    if diff_output_indicator_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff output indicator controls are not supported for this output mode".into(),
        ));
    }
    if diff_patch_context_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff patch context controls are not supported for this output mode".into(),
        ));
    }
    if diff_patch_output_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff patch output controls are not supported for this output mode".into(),
        ));
    }
    if diff_rewrite_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff rewrite controls are not supported for this output mode".into(),
        ));
    }
    if diff_submodule_output_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff submodule output controls are not supported for this output mode".into(),
        ));
    }
    if diff_word_control && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff word controls are not supported for this output mode".into(),
        ));
    }
    if !matches!(diff_relative, DiffRelativeMode::Off) && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff relative output is not supported for this output mode".into(),
        ));
    }
    if reverse && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff reverse output is not supported for this output mode".into(),
        ));
    }
    if (pickaxe.is_some() || pickaxe_all || pickaxe_regex) && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff pickaxe controls are not supported for this output mode".into(),
        ));
    }
    if !find_object_values.is_empty() && !name_status && !name_only {
        return Err(GitError::Unsupported(
            "diff find-object output is not supported for this output mode".into(),
        ));
    }
    if pickaxe_all && !find_object_values.is_empty() {
        return diff_find_object_pickaxe_all_conflict_error();
    }
    if pickaxe.is_some() && pickaxe_regex {
        return Err(GitError::Unsupported(
            "diff pickaxe regex matching is not supported".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    // Pull any leading `<rev>` / `<rev> <rev>` / `<rev>..<rev>` / `<rev>...<rev>`
    // out of the positional arguments; the remainder are pathspecs. Without this,
    // `diff A B` was treated as two paths and silently fell back to an
    // index-vs-worktree diff (wrong output, and a full-worktree rescan on big
    // repos).
    // A bare `diff HEAD` keeps its dedicated head-vs-worktree path, but
    // `diff HEAD <rev>` / `diff HEAD HEAD` means the consumed HEAD is the first of
    // several revisions — hand it back to the splitter.
    if head && !path_args.is_empty() {
        path_args.insert(0, "HEAD".to_string());
        head = false;
    }
    let (diff_trees, mut path_args) = diff_split_revisions(&git_dir, format, &db, path_args)?;
    path_args.extend(explicit_paths);
    let find_objects = resolve_diff_find_objects(&git_dir, format, &find_object_values)?;
    let repository_abbrev = repository_abbrev(&git_dir, format)?;
    let raw_abbrev = match raw_abbrev {
        Some(abbrev) => abbrev.map(|width| width.min(format.hex_len())),
        None => repository_abbrev,
    };
    let patch_abbrev = if patch_full_index {
        format.hex_len()
    } else {
        patch_abbrev
            .or(repository_abbrev)
            .unwrap_or(7)
            .min(format.hex_len())
    };
    let worktree_root = if cached {
        None
    } else {
        Some(worktree_root_for_git_dir(&git_dir)?)
    };
    let pathspec = if path_args.is_empty() {
        DiffPathspec::default()
    } else {
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        DiffPathspec::new(&cwd, &worktree_root, &path_args)?
    };
    let name_status_options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies,
        find_copies_harder,
        rename_empty,
    };
    // The new-side oid is real (shown, not zeroed) when it comes from a tree or the
    // index; it is zeroed only when the new side is the worktree.
    let zero_worktree_oids = match diff_trees.len() {
        2 => false,
        1 => !cached,
        _ => !cached && !head,
    };
    // The new side's *content* comes from the worktree only when there is no second
    // tree and we're not diffing the index (`--cached`). A two-tree `diff A B` takes
    // its new content from tree B's blobs, never the worktree.
    let use_worktree_new = !cached && diff_trees.len() != 2;
    let rename_options = sley_diff_merge::RenameDetectionOptions {
        base: name_status_options,
        detect_inexact: true,
        rename_threshold,
        copy_threshold,
    };
    let entries = if !diff_trees.is_empty() {
        match diff_trees.as_slice() {
            // `diff <rev>`: that tree vs the worktree (or the index with --cached).
            [tree] => {
                if cached {
                    if inexact_renames {
                        sley_diff_merge::diff_name_status_tree_index_with_rename_options(
                            &git_dir,
                            format,
                            tree,
                            rename_options,
                        )?
                    } else {
                        sley_diff_merge::diff_name_status_tree_index_with_options(
                            &git_dir,
                            format,
                            tree,
                            name_status_options,
                        )?
                    }
                } else {
                    let worktree_root = worktree_root
                        .as_ref()
                        .expect("worktree root set for diff <rev>");
                    if inexact_renames {
                        sley_diff_merge::diff_name_status_tree_worktree_with_rename_options(
                            worktree_root,
                            &git_dir,
                            format,
                            tree,
                            rename_options,
                        )?
                    } else {
                        sley_diff_merge::diff_name_status_tree_worktree_with_options(
                            worktree_root,
                            &git_dir,
                            format,
                            tree,
                            name_status_options,
                        )?
                    }
                }
            }
            // `diff <rev> <rev>` / `<rev>..<rev>` / `<rev>...<rev>`: tree vs tree.
            [left, right] => {
                if inexact_renames {
                    sley_diff_merge::diff_name_status_trees_with_rename_options(
                        &db,
                        format,
                        left,
                        right,
                        rename_options,
                    )?
                } else {
                    sley_diff_merge::diff_name_status_trees_with_options(
                        &db,
                        format,
                        left,
                        right,
                        name_status_options,
                    )?
                }
            }
            _ => {
                return Err(GitError::Unsupported(
                    "diff accepts at most two revisions".into(),
                ));
            }
        }
    } else if cached {
        if inexact_renames {
            sley_diff_merge::diff_name_status_head_index_with_rename_options(
                &git_dir,
                format,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_head_index_with_options(
                &git_dir,
                format,
                name_status_options,
            )?
        }
    } else if head {
        let worktree_root = worktree_root
            .as_ref()
            .expect("worktree root set for diff HEAD");
        if inexact_renames {
            sley_diff_merge::diff_name_status_head_worktree_with_rename_options(
                worktree_root,
                &git_dir,
                format,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_head_worktree_with_options(
                worktree_root,
                &git_dir,
                format,
                name_status_options,
            )?
        }
    } else {
        let worktree_root = worktree_root.as_ref().expect("worktree root set for diff");
        if inexact_renames {
            sley_diff_merge::diff_name_status_index_worktree_with_rename_options(
                worktree_root,
                &git_dir,
                format,
                rename_options,
            )?
        } else {
            sley_diff_merge::diff_name_status_index_worktree_with_options(
                worktree_root,
                &git_dir,
                format,
                name_status_options,
            )?
        }
    };
    let entries = apply_diff_pathspec(entries, &pathspec);
    let entries = if let Some(needle) = pickaxe.as_deref() {
        apply_diff_pickaxe(
            entries,
            needle.as_bytes(),
            pickaxe_all,
            &db,
            worktree_root.as_deref(),
            use_worktree_new,
        )?
    } else if pickaxe_all || pickaxe_regex {
        sort_diff_entries_by_path(entries)
    } else {
        entries
    };
    let entries = apply_diff_find_objects(entries, &find_objects);
    let entries = if reverse {
        reverse_diff_entries(entries)
    } else {
        entries
    };
    let entries = if matches!(diff_relative, DiffRelativeMode::Off) {
        entries
    } else {
        let prefix = diff_relative_prefix(&diff_relative, &cwd, &git_dir)?;
        apply_diff_relative(entries, &prefix)
    };
    let entries: Vec<_> = if diff_filter.all_or_none {
        if !diff_filter.includes.is_empty()
            && entries.iter().any(|entry| {
                pathspec.matches(&entry.path) && diff_filter.matches_status(entry.status.code())
            })
        {
            entries
        } else {
            Vec::new()
        }
    } else {
        entries
            .into_iter()
            .filter(|entry| diff_filter.matches_status(entry.status.code()))
            .collect()
    };
    let has_differences = !entries.is_empty();
    if !quiet && !no_patch {
        let mut stdout = io::stdout();
        let show_raw = raw && !name_only && !name_status;
        let show_numstat = numstat && !name_only && !name_status;
        let show_stat = (stat || compact_summary) && !name_only && !name_status;
        let show_shortstat = shortstat && !name_only && !name_status;
        let no_output_mode = !raw
            && !stat
            && !compact_summary
            && !numstat
            && !shortstat
            && !summary
            && !name_status
            && !name_only;
        let show_patch = !name_only && !name_status && (patch || no_output_mode);
        let show_summary = summary && !name_only && !name_status;
        if show_raw {
            for entry in &entries {
                write_diff_raw_entry(
                    &mut stdout,
                    entry,
                    z,
                    zero_worktree_oids,
                    raw_abbrev,
                    format,
                )?;
            }
        }
        if show_numstat {
            for entry in &entries {
                write_diff_numstat_entry(
                    &mut stdout,
                    entry,
                    z,
                    &db,
                    worktree_root.as_deref(),
                    use_worktree_new,
                )?;
            }
        }
        if show_stat {
            let mut stat_widths = stat_widths;
            if let Ok(config) = read_repo_config(&git_dir) {
                stat_widths.resolve_config(&config);
            } else {
                stat_widths.resolve_config_defaults();
            }
            write_diff_stat_with_widths(
                &mut stdout,
                &entries,
                &db,
                worktree_root.as_deref(),
                use_worktree_new,
                DiffStatOptions {
                    compact_summary,
                    stat_count,
                    color: color_always,
                },
                stat_widths,
            )?;
        }
        if show_shortstat {
            write_diff_shortstat(
                &mut stdout,
                &entries,
                &db,
                worktree_root.as_deref(),
                use_worktree_new,
            )?;
        }
        if show_summary {
            for entry in &entries {
                write_diff_summary_entry(&mut stdout, entry)?;
            }
        }
        if show_patch {
            if show_raw || show_numstat || show_stat || show_shortstat || show_summary {
                writeln!(stdout)?;
            }
            for entry in &entries {
                let options = DiffPatchOptions {
                    db: &db,
                    worktree_root: worktree_root.as_deref(),
                    use_worktree_new,
                    format,
                    abbrev: patch_abbrev,
                    src_prefix: &src_prefix,
                    dst_prefix: &dst_prefix,
                };
                write_diff_patch_entry(&mut stdout, entry, options)?;
            }
        } else if !show_summary
            && (summary || (!show_stat && !show_shortstat))
            && !show_numstat
            && !show_raw
        {
            for entry in &entries {
                if z && (name_only || name_status) {
                    if name_only {
                        stdout.write_all(&entry.path)?;
                        stdout.write_all(b"\0")?;
                    } else {
                        stdout.write_all(entry.status.label().as_bytes())?;
                        stdout.write_all(b"\0")?;
                        if let Some(old_path) = &entry.old_path {
                            stdout.write_all(old_path)?;
                            stdout.write_all(b"\0")?;
                        }
                        stdout.write_all(&entry.path)?;
                        stdout.write_all(b"\0")?;
                    }
                } else if name_only {
                    let path = status_quote_path(&entry.path, false);
                    writeln!(stdout, "{path}")?;
                } else if !name_status && summary {
                    write_diff_summary_entry(&mut stdout, entry)?;
                } else {
                    write!(stdout, "{}", entry.status.label())?;
                    if let Some(old_path) = &entry.old_path {
                        let old_path = status_quote_path(old_path, false);
                        write!(stdout, "\t{old_path}")?;
                    }
                    let path = status_quote_path(&entry.path, false);
                    writeln!(stdout, "\t{path}")?;
                }
            }
        }
    }
    if (quiet || exit_code) && has_differences {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn apply_diff_pickaxe(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    needle: &[u8],
    pickaxe_all: bool,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut matches = Vec::new();
    for entry in &entries {
        if diff_entry_matches_pickaxe(entry, needle, db, worktree_root, use_worktree_new)? {
            matches.push(entry.clone());
        }
    }
    if pickaxe_all {
        if matches.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(sort_diff_entries_by_path(entries))
        }
    } else {
        Ok(sort_diff_entries_by_path(matches))
    }
}

fn diff_entry_matches_pickaxe(
    entry: &sley_diff_merge::NameStatusEntry,
    needle: &[u8],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
) -> Result<bool> {
    let old_content = diff_entry_old_content(entry, db)?;
    let new_content = diff_entry_new_content(entry, db, worktree_root, use_worktree_new)?;
    Ok(
        count_non_overlapping_occurrences(old_content.as_deref().unwrap_or_default(), needle)
            != count_non_overlapping_occurrences(
                new_content.as_deref().unwrap_or_default(),
                needle,
            ),
    )
}

fn resolve_diff_find_objects(
    git_dir: &Path,
    format: ObjectFormat,
    values: &[String],
) -> Result<Vec<ObjectId>> {
    values
        .iter()
        .map(|value| resolve_diff_find_object(git_dir, format, value))
        .collect()
}

fn resolve_diff_find_object(git_dir: &Path, format: ObjectFormat, value: &str) -> Result<ObjectId> {
    if value.len() == format.hex_len() && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return ObjectId::from_hex(format, value)
            .map_err(|_| diff_find_object_unable_to_resolve_error(value));
    }
    resolve_revision(git_dir, format, value)
        .map_err(|_| diff_find_object_unable_to_resolve_error(value))
}

fn apply_diff_find_objects(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    targets: &[ObjectId],
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if targets.is_empty() {
        return entries;
    }
    sort_diff_entries_by_path(
        entries
            .into_iter()
            .filter(|entry| {
                targets.iter().any(|target| {
                    entry.old_oid.as_ref() == Some(target) || entry.new_oid.as_ref() == Some(target)
                })
            })
            .collect(),
    )
}

fn count_non_overlapping_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut offset = 0;
    while offset + needle.len() <= haystack.len() {
        if &haystack[offset..offset + needle.len()] == needle {
            count += 1;
            offset += needle.len();
        } else {
            offset += 1;
        }
    }
    count
}

fn sort_diff_entries_by_path(
    mut entries: Vec<sley_diff_merge::NameStatusEntry>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    entries
}

fn diff_pickaxe_requires_value_error() -> GitError {
    eprintln!("error: switch `S' requires a value");
    GitError::Exit(129)
}

fn diff_pickaxe_requires_non_empty_error() -> GitError {
    eprintln!("error: -S requires a non-empty argument");
    GitError::Exit(129)
}

fn diff_find_object_unable_to_resolve_error(value: &str) -> GitError {
    eprintln!("error: unable to resolve '{value}'");
    GitError::Exit(129)
}

fn diff_find_object_pickaxe_all_conflict_error() -> Result<()> {
    eprintln!(
        "fatal: options '--pickaxe-all' and '--find-object' cannot be used together, use '--pickaxe-all' with '-G' and '-S'"
    );
    Err(GitError::Exit(128))
}

enum DiffRelativeMode {
    Off,
    Cwd,
    Prefix(String),
}

fn diff_relative_prefix(mode: &DiffRelativeMode, cwd: &Path, git_dir: &Path) -> Result<Vec<u8>> {
    match mode {
        DiffRelativeMode::Off => Ok(Vec::new()),
        DiffRelativeMode::Cwd => Ok(worktree_prefix(cwd, git_dir)?
            .trim_end_matches('/')
            .as_bytes()
            .to_vec()),
        DiffRelativeMode::Prefix(prefix) => Ok(diff_relative_prefix_arg(prefix).into_bytes()),
    }
}

fn diff_relative_prefix_arg(prefix: &str) -> String {
    if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        prefix.trim_end_matches('/').to_string()
    }
}

fn apply_diff_relative(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    prefix: &[u8],
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let mut filtered = Vec::new();
    for entry in entries {
        if let Some(old_path) = &entry.old_path {
            let old_display = diff_relative_display_path(old_path, prefix);
            let new_display = diff_relative_display_path(&entry.path, prefix);
            if matches!(entry.status, sley_diff_merge::NameStatus::Copied(_)) {
                match (old_display, new_display) {
                    (Some(old_path), Some(path)) => {
                        filtered.push(sley_diff_merge::NameStatusEntry {
                            path: BString::from(path),
                            old_path: Some(BString::from(old_path)),
                            ..entry
                        })
                    }
                    (None, Some(path)) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Added,
                        path: BString::from(path),
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (Some(_), None) | (None, None) => {}
                }
            } else {
                match (old_display, new_display) {
                    (Some(old_path), Some(path)) => {
                        filtered.push(sley_diff_merge::NameStatusEntry {
                            path: BString::from(path),
                            old_path: Some(BString::from(old_path)),
                            ..entry
                        });
                    }
                    (Some(path), None) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Deleted,
                        path: BString::from(path),
                        old_path: None,
                        old_mode: entry.old_mode,
                        new_mode: None,
                        old_oid: entry.old_oid,
                        new_oid: None,
                    }),
                    (None, Some(path)) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Added,
                        path: BString::from(path),
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (None, None) => {}
                }
            }
        } else if let Some(path) = diff_relative_display_path(&entry.path, prefix) {
            filtered.push(sley_diff_merge::NameStatusEntry {
                path: BString::from(path),
                ..entry
            });
        }
    }
    filtered.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    filtered
}

fn diff_relative_display_path(path: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    if prefix.is_empty() {
        return Some(path.to_vec());
    }
    if path == prefix {
        return Some(Vec::new());
    }
    path.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix(b"/"))
        .map(|rest| rest.to_vec())
}

fn log_validate_word_diff(value: &str) -> Result<()> {
    match value {
        "plain" | "color" | "porcelain" | "none" => Ok(()),
        _ => {
            eprintln!("error: bad --word-diff argument: {value}");
            Err(GitError::Exit(129))
        }
    }
}
