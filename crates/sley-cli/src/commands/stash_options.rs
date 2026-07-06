use super::{StashApplyOptions, StashListFormat, StashListOptions};
use crate::*;
use sley_grep::PatternKind;

pub(super) fn setup_stash_apply_options(args: &[String], command: &str) -> Result<StashApplyOptions> {
    let mut quiet = false;
    let mut reinstate_index = None;
    let mut specs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-h" | "--help" => {
                super::stash_push_usage_stdout();
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("-q") && value.len() > 2 => {
                super::stash_apply_parse_combined_quiet(value, command)?;
                quiet = true;
            }
            "--index" => reinstate_index = Some(true),
            "--no-index" => reinstate_index = Some(false),
            "--label-ours" | "--label-theirs" | "--label-base" if command == "apply" => {
                if index + 1 >= args.len() {
                    eprintln!("error: option `{}` requires a value", &arg[2..]);
                    super::stash_apply_usage(command);
                    return Err(GitError::Exit(129));
                }
                index += 1;
            }
            "--no-label-ours" | "--no-label-theirs" | "--no-label-base" if command == "apply" => {}
            value
                if command == "apply"
                    && (value.starts_with("--label-ours=")
                        || value.starts_with("--label-theirs=")
                        || value.starts_with("--label-base=")) => {}
            value
                if command == "apply"
                    && (value.starts_with("--no-label-ours=")
                        || value.starts_with("--no-label-theirs=")
                        || value.starts_with("--no-label-base=")) =>
            {
                return super::stash_option_takes_no_value_error(&value[5..value.find('=').unwrap()]);
            }
            value if value.starts_with("--quiet=") => {
                return super::stash_option_takes_no_value_error("quiet");
            }
            value if value.starts_with("--no-quiet=") => {
                return super::stash_option_takes_no_value_error("no-quiet");
            }
            value if value.starts_with("--index=") => {
                return super::stash_option_takes_no_value_error("index");
            }
            value if value.starts_with("--no-index=") => {
                return super::stash_option_takes_no_value_error("no-index");
            }
            "--" => {
                specs.extend(args[index + 1..].iter().cloned());
                break;
            }
            value if value.starts_with('-') => {
                return super::stash_apply_unknown_option_error(command, value);
            }
            value => specs.push(value.to_string()),
        }
        index += 1;
    }
    if specs.len() > 1 {
        eprintln!(
            "Too many revisions specified: '{}' '{}'",
            specs[0], specs[1]
        );
        return Err(GitError::Exit(1));
    }
    let display = specs
        .first()
        .cloned()
        .unwrap_or_else(|| "refs/stash@{0}".to_string());
    let selector = match specs.first() {
        Some(spec) if let Some(selector) = super::stash_numeric_selector(spec) => selector?,
        Some(spec) if super::stash_argument_names_stash_ref(spec) => 0,
        Some(spec) => return Err(super::stash_invalid_reference_error(spec)),
        None => 0,
    };
    Ok(StashApplyOptions {
        quiet,
        reinstate_index,
        explicit_selector: !specs.is_empty(),
        selector,
        spec: specs.first().cloned(),
        display,
        direct_oid: None,
    })
}

pub(super) fn setup_stash_list_options(args: &[String]) -> Result<StashListOptions> {
    let mut format = StashListFormat::Default;
    let mut max_count = None;
    let mut skip_count = 0;
    let mut max_age = None;
    let mut min_age = None;
    let mut min_parents = None;
    let mut max_parents = None;
    let mut abbrev_len = Some(7);
    let mut date_mode = DateMode::Default;
    let mut date_explicit = false;
    let mut author_patterns = Vec::new();
    let mut committer_patterns = Vec::new();
    let mut reflog_patterns = Vec::new();
    let mut grep_patterns = Vec::new();
    let mut grep_all_match = false;
    let mut invert_grep = false;
    let mut regexp_ignore_case = false;
    let mut regexp_mode = SimpleLogRegexMode::Basic;
    let mut note_refs = Vec::new();
    let mut show_patch = false;
    let mut combined_patch = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--" => {
                if index + 1 == args.len() {
                    break;
                }
                return Err(GitError::Unsupported(
                    "stash list currently does not support revisions or pathspecs".into(),
                ));
            }
            "--oneline" => format = StashListFormat::Oneline,
            "-p" | "--patch" => show_patch = true,
            "--cc" => {
                show_patch = true;
                combined_patch = true;
            }
            "--reverse" => {
                eprintln!(
                    "fatal: options '--reverse' and '--walk-reflogs' cannot be used together"
                );
                return Err(GitError::Exit(1));
            }
            "-q"
            | "--quiet"
            | "--no-quiet"
            | "--no-graph"
            | "--expand-tabs"
            | "--no-expand-tabs"
            | "--no-decorate"
            | "--walk-reflogs"
            | "--no-walk"
            | "--do-walk"
            | "--first-parent"
            | "--parents"
            | "--full-history"
            | "--dense"
            | "--sparse"
            | "--remove-empty"
            | "--left-right"
            | "--no-notes"
            | "--notes"
            | "--show-notes"
            | "--standard-notes"
            | "--no-standard-notes"
            | "--show-signature"
            | "--no-show-signature"
            | "--source"
            | "--no-source"
            | "--use-mailmap"
            | "--no-use-mailmap"
            | "--mailmap"
            | "--no-mailmap"
            | "--no-patch"
            | "--color"
            | "--no-color"
            | "--color-moved"
            | "--no-color-moved"
            | "--clear-decorations"
            | "--no-decorate-refs"
            | "--no-decorate-refs-exclude"
            | "--no-diff-merges"
            | "--full-diff"
            | "--relative"
            | "--no-relative"
            | "--ext-diff"
            | "--no-ext-diff"
            | "--no-renames"
            | "--find-renames"
            | "--find-copies"
            | "--find-copies-harder"
            | "--no-find-copies-harder"
            | "--textconv"
            | "--no-textconv"
            | "--minimal"
            | "--patience"
            | "--histogram"
            | "--indent-heuristic"
            | "--no-indent-heuristic"
            | "--ignore-space-at-eol"
            | "--ignore-cr-at-eol"
            | "--ignore-space-change"
            | "--ignore-all-space"
            | "--ignore-blank-lines"
            | "--function-context"
            | "--no-prefix"
            | "--default-prefix"
            | "--full-index"
            | "--break-rewrites"
            | "--irreversible-delete"
            | "--submodule"
            | "--ignore-submodules"
            | "--ita-visible-in-index"
            | "--ita-invisible-in-index"
            | "--pickaxe-all"
            | "--pickaxe-regex"
            | "-M"
            | "-C"
            | "-B"
            | "-D"
            | "-m"
            | "-s"
            | "-b"
            | "-w"
            | "-W" => {}
            "--encoding" => {
                if index + 1 < args.len() {
                    index += 1;
                }
            }
            value if value.starts_with("--encoding=") => {}
            value if value.starts_with("--no-encoding") => {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            "--merges" => min_parents = Some(2),
            "--no-merges" => max_parents = Some(1),
            "--no-min-parents" => min_parents = None,
            "--no-max-parents" => max_parents = None,
            "--graph"
            | "--children"
            | "--cherry-pick"
            | "--ancestry-path"
            | "--topo-order"
            | "--date-order"
            | "--author-date-order"
            | "--simplify-by-decoration"
            | "--simplify-merges" => {
                eprintln!("fatal: cannot combine --walk-reflogs with history-limiting options");
                return Err(GitError::Exit(1));
            }
            value if value.starts_with("--no-decorate=") => {
                eprintln!("error: option `no-decorate' takes no value");
                return Err(GitError::Exit(1));
            }
            value if let Some(value) = value.strip_prefix("--expand-tabs=") => {
                super::stash_list_validate_non_negative_integer(value)?;
            }
            value if value.starts_with("--quiet=") => {
                super::stash_list_option_takes_no_value_error("quiet")?;
            }
            value if value.starts_with("--no-quiet=") => {
                super::stash_list_option_takes_no_value_error("no-quiet")?;
            }
            value if value.starts_with("--clear-decorations=") => {
                super::stash_list_option_takes_no_value_error("clear-decorations")?;
            }
            value if value.starts_with("--no-decorate-refs=") => {
                super::stash_list_option_takes_no_value_error("no-decorate-refs")?;
            }
            value if value.starts_with("--no-decorate-refs-exclude=") => {
                super::stash_list_option_takes_no_value_error("no-decorate-refs-exclude")?;
            }
            value if value.starts_with("--use-mailmap=") => {
                super::stash_list_option_takes_no_value_error("use-mailmap")?;
            }
            value if value.starts_with("--no-use-mailmap=") => {
                super::stash_list_option_takes_no_value_error("no-use-mailmap")?;
            }
            value if value.starts_with("--mailmap=") => {
                super::stash_list_option_takes_no_value_error("mailmap")?;
            }
            value if value.starts_with("--no-mailmap=") => {
                super::stash_list_option_takes_no_value_error("no-mailmap")?;
            }
            value if value.starts_with("--source=") => {
                super::stash_list_option_takes_no_value_error("source")?;
            }
            value if value.starts_with("--no-source=") => {
                super::stash_list_option_takes_no_value_error("no-source")?;
            }
            value if let Some(value) = value.strip_prefix("--notes=") => {
                note_refs.push(super::stash_list_note_ref(value));
            }
            value if let Some(value) = value.strip_prefix("--show-notes=") => {
                note_refs.push(super::stash_list_note_ref(value));
            }
            value if value.starts_with("--no-color-moved=") => {
                super::stash_list_option_takes_no_value_error("no-color-moved")?;
            }
            value if value.starts_with("--no-color=") => {
                super::stash_list_option_takes_no_value_error("no-color")?;
            }
            value
                if value.starts_with("--no-graph=")
                    || value.starts_with("--oneline=")
                    || value.starts_with("--no-expand-tabs=")
                    || value.starts_with("--show-signature=")
                    || value.starts_with("--no-show-signature=")
                    || value.starts_with("--full-diff=")
                    || value.starts_with("--no-notes=")
                    || value.starts_with("--standard-notes=")
                    || value.starts_with("--no-standard-notes=")
                    || value.starts_with("--no-diff-merges=")
                    || value.starts_with("--perl-regexp=")
                    || value.starts_with("--basic-regexp=")
                    || value.starts_with("--extended-regexp=")
                    || value.starts_with("--fixed-strings=")
                    || value.starts_with("--regexp-ignore-case=")
                    || value.starts_with("--all-match=")
                    || value.starts_with("--invert-grep=")
                    || value.starts_with("--no-perl-regexp")
                    || value.starts_with("--no-basic-regexp")
                    || value.starts_with("--no-extended-regexp")
                    || value.starts_with("--no-fixed-strings")
                    || value.starts_with("--no-regexp-ignore-case")
                    || value.starts_with("--no-all-match")
                    || value.starts_with("--no-invert-grep")
                    || value.starts_with("--no-grep")
                    || value.starts_with("--full-history=")
                    || value.starts_with("--dense=")
                    || value.starts_with("--sparse=")
                    || value.starts_with("--remove-empty=")
                    || value.starts_with("--left-right=")
                    || value.starts_with("--merges=")
                    || value.starts_with("--no-merges=")
                    || value.starts_with("--no-min-parents=")
                    || value.starts_with("--no-max-parents=")
                    || value.starts_with("--children=")
                    || value.starts_with("--cherry-pick=")
                    || value.starts_with("--topo-order=")
                    || value.starts_with("--date-order=")
                    || value.starts_with("--author-date-order=")
                    || value.starts_with("--simplify-by-decoration=")
                    || value.starts_with("--simplify-merges=") =>
            {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            value if let Some(value) = value.strip_prefix("--ancestry-path=") => {
                eprintln!("error: could not get commit for --ancestry-path argument {value}");
                return Err(GitError::Exit(1));
            }
            "--decorate" => {}
            value if let Some(value) = value.strip_prefix("--decorate=") => {
                if matches!(
                    value,
                    "no" | "auto"
                        | "short"
                        | "full"
                        | ""
                        | "false"
                        | "0"
                        | "off"
                        | "true"
                        | "1"
                        | "on"
                        | "yes"
                ) {
                    // Decorations are not shown in the covered stash-list formats.
                } else {
                    eprintln!("fatal: invalid --decorate option: {value}");
                    return Err(GitError::Exit(1));
                }
            }
            "--decorate-refs" | "--decorate-refs-exclude" => {
                index += 1;
                if args.get(index).is_none() {
                    return Err(log_option_requires_value_error(
                        arg.trim_start_matches("--"),
                    ));
                }
            }
            value if value.starts_with("--decorate-refs=") => {}
            value if value.starts_with("--decorate-refs-exclude=") => {}
            "--no-walk=sorted" | "--no-walk=unsorted" => {}
            value if value.starts_with("--no-walk=") => {
                super::stash_list_no_walk_invalid_argument(value)?;
            }
            value
                if value.starts_with("--walk-reflogs=")
                    || value.starts_with("--do-walk=")
                    || value.starts_with("--first-parent=")
                    || value.starts_with("--parents=") =>
            {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            "--abbrev" => abbrev_len = Some(7),
            "--no-abbrev" => abbrev_len = None,
            value if value.starts_with("--no-abbrev=") => {
                super::stash_list_option_takes_no_value_error("no-abbrev")?;
            }
            "--grep" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(log_grep_requires_value_error());
                };
                grep_patterns.push(LogFilterPattern::new(value, "command line"));
            }
            value if let Some(value) = value.strip_prefix("--grep=") => {
                grep_patterns.push(LogFilterPattern::new(value, "command line"));
            }
            "--grep-reflog" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                reflog_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value if let Some(value) = value.strip_prefix("--grep-reflog=") => {
                reflog_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value
                if value.starts_with("--no-grep-reflog")
                    || value.starts_with("--no-author")
                    || value.starts_with("--no-committer") =>
            {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            "--author" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                author_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value if let Some(value) = value.strip_prefix("--author=") => {
                author_patterns.push(LogFilterPattern::new(value, "header"));
            }
            "--committer" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                committer_patterns.push(LogFilterPattern::new(value, "header"));
            }
            value if let Some(value) = value.strip_prefix("--committer=") => {
                committer_patterns.push(LogFilterPattern::new(value, "header"));
            }
            "--all-match" => grep_all_match = true,
            "--invert-grep" => invert_grep = true,
            "-i" | "--regexp-ignore-case" => regexp_ignore_case = true,
            "-F" | "--fixed-strings" => regexp_mode = SimpleLogRegexMode::Fixed,
            "-E" | "-P" | "--basic-regexp" | "--extended-regexp" | "--perl-regexp" => {
                regexp_mode = SimpleLogRegexMode::Basic
            }
            value
                if (value.starts_with("-F")
                    || value.starts_with("-E")
                    || value.starts_with("-P")
                    || value.starts_with("-i"))
                    && value.len() > 2 =>
            {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            value if value.starts_with("-M") => {
                super::stash_list_validate_similarity_option(&value[2..], "find-renames")?;
            }
            value if value.starts_with("-C") => {
                super::stash_list_validate_similarity_option(&value[2..], "find-copies")?;
            }
            value if value.starts_with("-B") => {
                super::stash_list_validate_break_rewrites_option(&value[2..])?;
            }
            value if let Some(option) = super::stash_list_diff_option_with_value(value) => {
                super::stash_list_option_takes_no_value_error(option)?;
            }
            value
                if value.len() > 2
                    && (value.starts_with("-D")
                        || value.starts_with("-s")
                        || value.starts_with("-b")
                        || value.starts_with("-w")
                        || value.starts_with("-W")) =>
            {
                super::stash_list_fatal_unrecognized_argument(&format!("-{}", &value[2..]))?;
            }
            value if value.len() > 2 && value.starts_with("-m") => {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            value if value.starts_with("--no-relative=") => {
                super::stash_list_option_takes_no_value_error("no-relative")?;
            }
            value if value.starts_with("--relative=") => {}
            value if let Some(value) = value.strip_prefix("--find-renames=") => {
                super::stash_list_validate_similarity_option(value, "find-renames")?;
            }
            value if let Some(value) = value.strip_prefix("--find-copies=") => {
                super::stash_list_validate_similarity_option(value, "find-copies")?;
            }
            value if value.starts_with("--find-copies-harder=") => {
                super::stash_list_option_takes_no_value_error("find-copies-harder")?;
            }
            value if value.starts_with("--no-find-copies-harder=") => {
                super::stash_list_option_takes_no_value_error("no-find-copies-harder")?;
            }
            value if let Some(value) = value.strip_prefix("--break-rewrites=") => {
                super::stash_list_validate_break_rewrites_option(value)?;
            }
            value if value.starts_with("--no-patch=") => {
                super::stash_list_option_takes_no_value_error("no-patch")?;
            }
            value if value.starts_with("--ext-diff=") => {
                super::stash_list_option_takes_no_value_error("ext-diff")?;
            }
            value if value.starts_with("--no-ext-diff=") => {
                super::stash_list_option_takes_no_value_error("no-ext-diff")?;
            }
            value if value.starts_with("--textconv=") => {
                super::stash_list_option_takes_no_value_error("textconv")?;
            }
            value if value.starts_with("--no-textconv=") => {
                super::stash_list_option_takes_no_value_error("no-textconv")?;
            }
            value if value.starts_with("--no-renames=") => {
                super::stash_list_option_takes_no_value_error("no-renames")?;
            }
            value if value.starts_with("--full-index=") => {
                super::stash_list_option_takes_no_value_error("full-index")?;
            }
            value if value.starts_with("--irreversible-delete=") => {
                super::stash_list_option_takes_no_value_error("irreversible-delete")?;
            }
            "--diff-merges" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                super::stash_list_validate_diff_merges(value)?;
            }
            value if let Some(value) = value.strip_prefix("--diff-merges=") => {
                super::stash_list_validate_diff_merges(value)?;
            }
            value if let Some(value) = value.strip_prefix("--color=") => {
                super::stash_list_validate_color(value)?;
            }
            value if let Some(value) = value.strip_prefix("--color-moved=") => {
                super::stash_list_validate_color_moved(value)?;
            }
            "--color-moved-ws" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                super::stash_list_validate_color_moved_ws(value)?;
            }
            value if let Some(value) = value.strip_prefix("--color-moved-ws=") => {
                super::stash_list_validate_color_moved_ws(value)?;
            }
            "--src-prefix" | "--dst-prefix" => {
                index += 1;
                if args.get(index).is_none() {
                    return Err(log_option_requires_value_error(
                        arg.trim_start_matches("--"),
                    ));
                }
            }
            value if value.starts_with("--src-prefix=") => {}
            value if value.starts_with("--dst-prefix=") => {}
            "--output-indicator-new" | "--output-indicator-old" | "--output-indicator-context" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                super::stash_list_validate_output_indicator(arg.trim_start_matches("--"), value)?;
            }
            value if let Some(value) = value.strip_prefix("--output-indicator-new=") => {
                super::stash_list_validate_output_indicator("output-indicator-new", value)?;
            }
            value if let Some(value) = value.strip_prefix("--output-indicator-old=") => {
                super::stash_list_validate_output_indicator("output-indicator-old", value)?;
            }
            value if let Some(value) = value.strip_prefix("--output-indicator-context=") => {
                super::stash_list_validate_output_indicator("output-indicator-context", value)?;
            }
            "--ws-error-highlight" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                super::stash_list_validate_ws_error_highlight(value)?;
            }
            value if let Some(value) = value.strip_prefix("--ws-error-highlight=") => {
                super::stash_list_validate_ws_error_highlight(value)?;
            }
            value if let Some(value) = value.strip_prefix("--submodule=") => {
                super::stash_list_validate_submodule_format(value)?;
            }
            value if let Some(value) = value.strip_prefix("--ignore-submodules=") => {
                super::stash_list_validate_ignore_submodules(value)?;
            }
            "--format" | "--pretty" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Command(format!("{arg} requires a value")));
                };
                format = super::parse_stash_list_format(value)?;
            }
            value if let Some(value) = value.strip_prefix("--format=") => {
                format = super::parse_stash_list_format(value)?;
            }
            value if let Some(value) = value.strip_prefix("--pretty=") => {
                format = super::parse_stash_list_format(value)?;
            }
            "--date" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                date_mode = super::stash_list_date_mode(value)?;
                date_explicit = true;
            }
            value if let Some(value) = value.strip_prefix("--date=") => {
                date_mode = super::stash_list_date_mode(value)?;
                date_explicit = true;
            }
            value if value.starts_with("--no-date") => {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            value if let Some(count) = value.strip_prefix("--max-count=") => {
                max_count = Some(parse_reflog_count(count)?);
            }
            value if let Some(count) = value.strip_prefix("--skip=") => {
                skip_count = parse_reflog_skip_count(count)?;
            }
            value if let Some(age) = value.strip_prefix("--max-age=") => {
                max_age = Some(super::parse_stash_list_age(age)?);
            }
            value if let Some(age) = value.strip_prefix("--min-age=") => {
                min_age = Some(super::parse_stash_list_min_age(age)?);
            }
            value if let Some(date) = value.strip_prefix("--since=") => {
                max_age = Some(super::parse_stash_list_date_cutoff(date)?);
            }
            value if let Some(date) = value.strip_prefix("--after=") => {
                max_age = Some(super::parse_stash_list_date_cutoff(date)?);
            }
            value if let Some(date) = value.strip_prefix("--until=") => {
                min_age = Some(super::parse_stash_list_date_cutoff(date)?);
            }
            value if let Some(date) = value.strip_prefix("--before=") => {
                min_age = Some(super::parse_stash_list_date_cutoff(date)?);
            }
            value
                if value.starts_with("--no-max-count")
                    || value.starts_with("--no-skip")
                    || value.starts_with("--no-max-age")
                    || value.starts_with("--no-min-age")
                    || value.starts_with("--no-since")
                    || value.starts_with("--no-after")
                    || value.starts_with("--no-until")
                    || value.starts_with("--no-before") =>
            {
                super::stash_list_fatal_unrecognized_argument(value)?;
            }
            value if let Some(count) = value.strip_prefix("--min-parents=") => {
                min_parents = Some(parse_reflog_min_parent_count(count)?);
            }
            value if let Some(count) = value.strip_prefix("--max-parents=") => {
                max_parents = Some(parse_reflog_max_parent_count(count)?);
            }
            value if let Some(value) = value.strip_prefix("--abbrev=") => {
                abbrev_len = super::parse_stash_list_abbrev(value);
            }
            "--max-count" | "-n" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                max_count = Some(parse_reflog_count(value)?);
            }
            "--skip" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                skip_count = parse_reflog_skip_count(value)?;
            }
            "--max-age" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                max_age = Some(super::parse_stash_list_age(value)?);
            }
            "--min-age" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                min_age = Some(super::parse_stash_list_min_age(value)?);
            }
            "--since" | "--after" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                max_age = Some(super::parse_stash_list_date_cutoff(value)?);
            }
            "--until" | "--before" => {
                index += 1;
                let value = args.get(index).map_or("refs/stash", String::as_str);
                min_age = Some(super::parse_stash_list_date_cutoff(value)?);
            }
            value if value.starts_with("-n") && value.len() > 2 => {
                max_count = Some(parse_reflog_count(&value[2..])?);
            }
            value
                if value.starts_with('-')
                    && value[1..].bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                max_count = Some(parse_reflog_count(&value[1..])?);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Unsupported(format!(
                    "unsupported stash list option {value}"
                )));
            }
            _ => {
                return Err(GitError::Unsupported(
                    "stash list currently does not support revisions or pathspecs".into(),
                ));
            }
        }
        index += 1;
    }
    let author_filters = super::parse_stash_list_filter_patterns(&author_patterns, regexp_mode)?;
    let committer_filters = super::parse_stash_list_filter_patterns(&committer_patterns, regexp_mode)?;
    let reflog_filters = super::parse_stash_list_filter_patterns(&reflog_patterns, regexp_mode)?;
    let grep_filters = super::parse_stash_list_filter_patterns(&grep_patterns, regexp_mode)?;
    Ok(StashListOptions {
        format,
        max_count,
        skip_count,
        max_age,
        min_age,
        min_parents,
        max_parents,
        abbrev_len,
        date_mode,
        date_explicit,
        author_filters,
        committer_filters,
        reflog_filters,
        grep_filters,
        grep_all_match,
        invert_grep,
        regexp_ignore_case,
        note_refs,
        show_patch,
        combined_patch,
    })
}
