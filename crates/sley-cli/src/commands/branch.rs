//! `git branch` and all its modes
//! (list/create/delete/rename/copy/set-upstream/edit-description).

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_options::{
    CallbackValue, OptFlags, OptValue, OptionName, OptionSpec, Parsed, ParsedOption, ParsedValue,
    UsageError, parse_options,
};

pub(crate) fn cmd_branch(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let repo = RepositoryContext::discover(&cwd)?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let store = repo.refs();
    // git validates branch.autosetuprebase up front, so even a plain listing
    // fails on a malformed value (t3200 #145/#146).
    validate_autosetuprebase(&read_repo_config(git_dir)?)?;
    if let Some(option) = args
        .iter()
        .find_map(|arg| matches!(arg.as_str(), "--no-remotes" | "--no-all").then_some(arg))
    {
        eprintln!("error: unknown option `{}`", option.trim_start_matches("--"));
        return Err(GitError::Exit(129));
    }
    if let Some(format_options) = parse_branch_format_list_options(git_dir, format, args)? {
        return run_branch_format_list_options(git_dir, format, store, format_options);
    }
    if let Some(show_current) = parse_branch_show_current_options(args)? {
        if show_current {
            if let Some(branch) = store.current_branch()? {
                println!("{branch}");
            }
            return Ok(());
        }
        return print_branch_list(store, BranchListMode::Local);
    }
    if let Some(move_options) = parse_branch_move_options(args)? {
        return run_branch_move_options(git_dir, store, move_options);
    }
    if let Some(upstream) = parse_branch_upstream_options(args)? {
        return run_branch_upstream_options(git_dir, store, upstream);
    }
    if branch_has_conflicting_action_modes(args) {
        eprintln!("fatal: options are incompatible");
        return Err(GitError::Exit(128));
    }
    if let Some(verbose) = parse_branch_verbose_list_options(args)? {
        return run_branch_verbose_list_options(git_dir, format, store, verbose);
    }
    if let Some(delete) = parse_branch_delete_options(args)? {
        let BranchDeleteOptions {
            force,
            quiet,
            mode,
            branches,
        } = delete;
        return if matches!(mode, BranchDeleteMode::Remote) {
            delete_remote_tracking_branches(store, &branches, quiet)
        } else if matches!(mode, BranchDeleteMode::All) {
            eprintln!("fatal: cannot use -a with -d");
            Err(GitError::Exit(128))
        } else if force {
            force_delete_branches(git_dir, store, &branches, quiet)
        } else {
            delete_merged_branches(git_dir, format, store, &branches, quiet)
        };
    }
    if let Some(create) = parse_branch_create_options(args)? {
        return run_branch_create_options(git_dir, format, store, create);
    }
    if let Some(list) = parse_branch_general_list_options(git_dir, args)? {
        return run_branch_general_list_options(git_dir, format, store, list);
    }
    match args {
        [] => print_branch_list(store, BranchListMode::Local),
        [flag] if flag == "--list" => print_branch_list(store, BranchListMode::Local),
        [flag] if flag == "-r" || flag == "--remotes" => {
            print_branch_list(store, BranchListMode::Remote)
        }
        [flag] if flag == "-a" || flag == "--all" => print_branch_list(store, BranchListMode::All),
        [flag] if flag == "--color" || flag == "--color=always" => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [color, no_color] if branch_color_always_flag(color) && no_color == "--no-color" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_color, color] if no_color == "--no-color" && branch_color_always_flag(color) => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [flag, color]
            if (flag == "-r" || flag == "--remotes")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::Remote)
        }
        [color, flag]
            if (flag == "-r" || flag == "--remotes")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::Remote)
        }
        [flag, color]
            if (flag == "-a" || flag == "--all")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::All)
        }
        [color, flag]
            if (flag == "-a" || flag == "--all")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::All)
        }
        [flag, color, no_color]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_color_always_flag(color)
                && no_color == "--no-color" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, no_color, color]
            if branch_remote_or_all_mode(flag).is_some()
                && no_color == "--no-color"
                && branch_color_always_flag(color) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_colored(git_dir, store, mode)
        }
        [flag, display_flag]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [display_flag, flag]
            if branch_list_noop_display_flag(display_flag)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [first, second, flag]
            if branch_column_noop_flag(first)
                && branch_column_noop_flag(second)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [first, second, flag]
            if branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [first, second, flag]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [sort, flag]
            if branch_version_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [sort, flag]
            if branch_objectname_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [sort, flag]
            if branch_objecttype_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_date_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some() && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [sort, flag]
            if branch_objectsize_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [sort, flag]
            if branch_date_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [sort, flag]
            if branch_upstream_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [sort, flag]
            if branch_push_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some() && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [sort, flag]
            if sort == "--sort=-refname" && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [sort, key, flag]
            if sort == "--sort"
                && key == "-refname"
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_date_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [sort, key, flag]
            if sort == "--sort"
                && key == "refname"
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag] if branch_ignore_case_flag(flag) => print_branch_list(store, BranchListMode::Local),
        [list, flag] if list == "--list" && branch_ignore_case_flag(flag) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, list] if branch_ignore_case_flag(flag) && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, ignore]
            if branch_remote_or_all_mode(flag).is_some() && branch_ignore_case_flag(ignore) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [ignore, flag]
            if branch_ignore_case_flag(ignore) && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag] if flag == "--no-points-at" => print_branch_list(store, BranchListMode::Local),
        [points_at, _rev, no_points_at]
            if points_at == "--points-at" && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_points_at, points_at, rev]
            if no_points_at == "--no-points-at" && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [points_at, no_points_at]
            if points_at.starts_with("--points-at=") && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_points_at, points_at]
            if no_points_at == "--no-points-at" && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [flag, color] if flag == "--list" && (color == "--color" || color == "--color=always") => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [color, flag] if flag == "--list" && (color == "--color" || color == "--color=always") => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [list, color, no_color]
            if list == "--list" && branch_color_always_flag(color) && no_color == "--no-color" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [color, no_color, list, patterns @ ..]
            if branch_color_always_flag(color) && no_color == "--no-color" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, color, no_color, patterns @ ..]
            if list == "--list" && branch_color_always_flag(color) && no_color == "--no-color" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, no_color, color]
            if list == "--list" && no_color == "--no-color" && branch_color_always_flag(color) =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [list, no_color, color, patterns @ ..]
            if list == "--list" && no_color == "--no-color" && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [no_color, color, list, patterns @ ..]
            if no_color == "--no-color" && branch_color_always_flag(color) && list == "--list" =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [list, color, patterns @ ..] if list == "--list" && branch_color_always_flag(color) => {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [color, list, patterns @ ..] if branch_color_always_flag(color) && list == "--list" => {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [list, color] if list == "--list" && branch_color_noop_flag(color) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, color, patterns @ ..] if list == "--list" && branch_color_noop_flag(color) => {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [color, list] if branch_color_noop_flag(color) && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [color, list, patterns @ ..] if branch_color_noop_flag(color) && list == "--list" => {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, points_at, _rev, no_points_at]
            if list == "--list" && points_at == "--points-at" && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, no_points_at, points_at, rev]
            if list == "--list" && no_points_at == "--no-points-at" && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [list, points_at, no_points_at]
            if list == "--list"
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, no_points_at, points_at]
            if list == "--list"
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [list, display_flag]
            if list == "--list" && branch_list_noop_display_flag(display_flag) =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, display_flag, patterns @ ..]
            if list == "--list" && branch_list_noop_display_flag(display_flag) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [display_flag, list]
            if branch_list_noop_display_flag(display_flag) && list == "--list" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [display_flag, list, patterns @ ..]
            if branch_list_noop_display_flag(display_flag) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list"
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key] if list == "--list" && sort == "--sort" && key == "refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, sort] if list == "--list" && branch_version_sort_value(sort).is_some() => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_objectname_sort_value(sort).is_some() => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_objecttype_sort_value(sort).is_some() => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_objectsize_sort_value(sort).is_some() => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_date_sort_value(sort).is_some() => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_upstream_sort_value(sort).is_some() => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_push_sort_value(sort).is_some() => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list" && sort == "--sort" && branch_push_sort_value(key).is_some() =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_objectname_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_objecttype_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_objectsize_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_date_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_upstream_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_push_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list"
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_version_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort] if list == "--list" && sort == "--sort=-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [list, sort, key] if list == "--list" && sort == "--sort" && key == "-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort=-refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, patterns @ ..] if list == "--list" && sort == "--sort=-refname" => {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "-refname" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "refname" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, list] if sort == "--sort" && key == "refname" && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, list] if branch_version_sort_value(sort).is_some() && list == "--list" => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_objectname_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_objecttype_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, list] if branch_objectsize_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, list] if branch_date_sort_value(sort).is_some() && list == "--list" => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list" =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [sort, list] if branch_upstream_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_push_sort_value(sort).is_some() && list == "--list" => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort" && branch_push_sort_value(key).is_some() && list == "--list" =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, list, patterns @ ..]
            if branch_objectname_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_objecttype_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_objectsize_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_date_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [sort, list, patterns @ ..]
            if branch_upstream_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_push_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_version_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_date_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_push_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list" =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_version_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list] if sort == "--sort=-refname" && list == "--list" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, key, list] if sort == "--sort" && key == "-refname" && list == "--list" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, list, patterns @ ..] if sort == "--sort=-refname" && list == "--list" => {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort" && key == "-refname" && list == "--list" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort=-refname" && list == "--list" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort" && key == "refname" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if sort == "--sort=refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if sort == "--sort=-refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort=refname" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort=refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort" && key == "refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort" && key == "-refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort" && key == "refname" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [flag, list, color, no_color, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_color_always_flag(color)
                && no_color == "--no-color" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, color, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Remote, patterns)
        }
        [flag, color, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Remote, patterns)
        }
        [flag, list, color, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(store, BranchListMode::All, patterns)
        }
        [flag, color, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            print_branch_list_matching_colored(store, BranchListMode::All, patterns)
        }
        [flag, color, no_color, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_color_always_flag(color)
                && no_color == "--no-color"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, no_color, color, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && no_color == "--no-color"
                && branch_color_always_flag(color) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_colored(store, mode, patterns)
        }
        [flag, no_color, color, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_color == "--no-color"
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_colored(store, mode, patterns)
        }
        [flag, rev] if flag == "--points-at" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [flag, rev] if flag == "--contains" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [contains, contains_rev, no_contains, no_contains_rev]
            if contains == "--contains" && no_contains == "--no-contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [no_contains, no_contains_rev, contains, contains_rev]
            if no_contains == "--no-contains" && contains == "--contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag] if flag == "--contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, rev] if flag == "--no-contains" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag == "--no-contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, rev] if flag == "--merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [merged, merged_rev, no_merged, no_merged_rev]
            if merged == "--merged" && no_merged == "--no-merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [no_merged, no_merged_rev, merged, merged_rev]
            if no_merged == "--no-merged" && merged == "--merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag] if flag == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, rev] if flag == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, points_at, rev, patterns @ ..] if flag == "--list" && points_at == "--points-at" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, BranchListMode::Local, &oid, patterns)
        }
        [flag, contains, rev, patterns @ ..]
            if flag == "--list"
                && contains == "--contains"
                && patterns
                    .first()
                    .is_none_or(|value| *value != "--no-contains") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [list, contains, contains_rev, no_contains, no_contains_rev, patterns @ ..]
            if list == "--list" && contains == "--contains" && no_contains == "--no-contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [list, no_contains, no_contains_rev, contains, contains_rev, patterns @ ..]
            if list == "--list" && no_contains == "--no-contains" && contains == "--contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, contains] if flag == "--list" && contains == "--contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, contains, rev, patterns @ ..]
            if flag == "--list"
                && contains == "--no-contains"
                && patterns.first().is_none_or(|value| *value != "--contains") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, contains] if flag == "--list" && contains == "--no-contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag, merged] if flag == "--list" && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, merged, rev, patterns @ ..]
            if flag == "--list"
                && merged == "--merged"
                && patterns.first().is_none_or(|value| *value != "--no-merged") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [list, merged, merged_rev, no_merged, no_merged_rev, patterns @ ..]
            if list == "--list" && merged == "--merged" && no_merged == "--no-merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [list, no_merged, no_merged_rev, merged, merged_rev, patterns @ ..]
            if list == "--list" && no_merged == "--no-merged" && merged == "--merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, merged] if flag == "--list" && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, merged, rev, patterns @ ..]
            if flag == "--list"
                && merged == "--no-merged"
                && patterns.first().is_none_or(|value| *value != "--merged") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, points_at, rev]
            if (flag == "-r" || flag == "--remotes") && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, points_at, _rev, no_points_at]
            if (flag == "-r" || flag == "--remotes")
                && points_at == "--points-at"
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Remote)
        }
        [flag, no_points_at, points_at, rev]
            if (flag == "-r" || flag == "--remotes")
                && no_points_at == "--no-points-at"
                && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, contains, rev]
            if (flag == "-r" || flag == "--remotes") && contains == "--contains" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                true,
            )
        }
        [flag, contains, contains_rev, no_contains, no_contains_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && contains == "--contains"
                && no_contains == "--no-contains" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, no_contains, no_contains_rev, contains, contains_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && no_contains == "--no-contains"
                && contains == "--contains" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains == "--contains" =>
        {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                true,
            )
        }
        [flag, contains, rev]
            if (flag == "-r" || flag == "--remotes") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, merged] if (flag == "-r" || flag == "--remotes") && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged, rev]
            if (flag == "-r" || flag == "--remotes") && merged == "--merged" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged, merged_rev, no_merged, no_merged_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && merged == "--merged"
                && no_merged == "--no-merged" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, no_merged, no_merged_rev, merged, merged_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && no_merged == "--no-merged"
                && merged == "--merged" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, merged] if (flag == "-r" || flag == "--remotes") && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, false)
        }
        [flag, merged, rev]
            if (flag == "-r" || flag == "--remotes") && merged == "--no-merged" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, false)
        }
        [flag, points_at, rev]
            if (flag == "-a" || flag == "--all") && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, points_at, _rev, no_points_at]
            if (flag == "-a" || flag == "--all")
                && points_at == "--points-at"
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::All)
        }
        [flag, no_points_at, points_at, rev]
            if (flag == "-a" || flag == "--all")
                && no_points_at == "--no-points-at"
                && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, contains, rev] if (flag == "-a" || flag == "--all") && contains == "--contains" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, contains] if (flag == "-a" || flag == "--all") && contains == "--contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, contains, rev]
            if (flag == "-a" || flag == "--all") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, merged] if (flag == "-a" || flag == "--all") && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, merged, rev] if (flag == "-a" || flag == "--all") && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, merged] if (flag == "-a" || flag == "--all") && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, false)
        }
        [flag, merged, rev] if (flag == "-a" || flag == "--all") && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, false)
        }
        [contains, no_contains]
            if branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [no_contains, contains]
            if branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [merged, no_merged]
            if branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [no_merged, merged]
            if branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag] if flag.starts_with("--points-at=") => {
            let rev = flag
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [flag] if flag.starts_with("--contains=") => {
            let rev = flag
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag] if flag.starts_with("--no-contains=") => {
            let rev = flag
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag.starts_with("--merged=") => {
            let rev = flag
                .strip_prefix("--merged=")
                .ok_or_else(|| GitError::Command("branch --merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag] if flag.starts_with("--no-merged=") => {
            let rev = flag
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, points_at, patterns @ ..] if flag == "--list" && points_at.starts_with("--points-at=") => {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, BranchListMode::Local, &oid, patterns)
        }
        [list, contains, no_contains, patterns @ ..]
            if list == "--list"
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [list, no_contains, contains, patterns @ ..]
            if list == "--list"
                && branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [list, merged, no_merged, patterns @ ..]
            if list == "--list"
                && branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [list, no_merged, merged, patterns @ ..]
            if list == "--list"
                && branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, contains, patterns @ ..] if flag == "--list" && contains.starts_with("--contains=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, contains, patterns @ ..] if flag == "--list" && contains.starts_with("--no-contains=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, merged, patterns @ ..] if flag == "--list" && merged.starts_with("--merged=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, merged, patterns @ ..] if flag == "--list" && merged.starts_with("--no-merged=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [format_flag, ignore, list, patterns @ ..]
            if format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [ignore, format_flag, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [list, ignore, format_flag, patterns @ ..]
            if list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [format_flag, ignore, reset, list, patterns @ ..]
            if format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [format_flag, format_spec, ignore, list, patterns @ ..]
            if format_flag == "--format" && branch_ignore_case_enabled_flag(ignore) && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [ignore, format_flag, format_spec, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore) && format_flag == "--format" && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [list, ignore, format_flag, format_spec, patterns @ ..]
            if list == "--list" && branch_ignore_case_enabled_flag(ignore) && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [format_flag, format_spec, ignore, reset, list, patterns @ ..]
            if format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, ignore, reset, patterns @ ..]
            if list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [ignore, list, reset, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && list == "--list"
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [ignore, reset, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [flag, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(flag) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, true)
        }
        [list, flag, reset, patterns @ ..]
            if list == "--list"
                && branch_ignore_case_enabled_flag(flag)
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, flag, patterns @ ..]
            if list == "--list" && branch_ignore_case_enabled_flag(flag) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, true)
        }
        [list, column] if list == "--list" && branch_column_noop_flag(column) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, column, patterns @ ..]
            if list == "--list" && branch_column_noop_flag(column) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [column, list] if branch_column_noop_flag(column) && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [column, list, patterns @ ..]
            if branch_column_noop_flag(column) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_column_noop_flag(first) && branch_column_noop_flag(second) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list" && branch_column_noop_flag(first) && branch_column_noop_flag(second) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list" && branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [format_flag, no_format, list, patterns @ ..]
            if format_flag.starts_with("--format=") && no_format == "--no-format" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [format_flag, format_spec, no_format, list, patterns @ ..]
            if format_flag == "--format" && no_format == "--no-format" && list == "--list" =>
        {
            let _ = format_spec;
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [no_format, format_flag, list, patterns @ ..]
            if no_format == "--no-format" && format_flag.starts_with("--format=") && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [no_format, format_flag, format_spec, list, patterns @ ..]
            if no_format == "--no-format" && format_flag == "--format" && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, format_flag, no_format, patterns @ ..]
            if list == "--list" && format_flag.starts_with("--format=") && no_format == "--no-format" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, format_flag, format_spec, no_format, patterns @ ..]
            if list == "--list" && format_flag == "--format" && no_format == "--no-format" =>
        {
            let _ = format_spec;
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [format_flag, omit_empty, list, patterns @ ..]
            if format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [list, format_flag, omit_empty, patterns @ ..]
            if list == "--list"
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [format_flag, format_spec, omit_empty, list, patterns @ ..]
            if format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [list, format_flag, format_spec, omit_empty, patterns @ ..]
            if list == "--list"
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [list, flag, patterns @ ..] if list == "--list" && flag.starts_with("--format=") => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_spec, list, patterns @ ..] if flag == "--format" && list == "--list" => {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, flag, format_spec, patterns @ ..] if list == "--list" && flag == "--format" => {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, patterns @ ..] if flag == "--list" => {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [flag, points_at]
            if (flag == "-r" || flag == "--remotes") && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, points_at, no_points_at]
            if (flag == "-r" || flag == "--remotes")
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Remote)
        }
        [flag, no_points_at, points_at]
            if (flag == "-r" || flag == "--remotes")
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, contains, no_contains]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, no_contains, contains]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, merged, no_merged]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, no_merged, merged]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains.starts_with("--contains=") =>
        {
            let rev = contains
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                true,
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains.starts_with("--no-contains=") =>
        {
            let rev = contains
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, merged]
            if (flag == "-r" || flag == "--remotes") && merged.starts_with("--merged=") =>
        {
            let rev = merged
                .strip_prefix("--merged=")
                .ok_or_else(|| GitError::Command("branch --merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged]
            if (flag == "-r" || flag == "--remotes") && merged.starts_with("--no-merged=") =>
        {
            let rev = merged
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, false)
        }
        [flag, points_at]
            if (flag == "-a" || flag == "--all") && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, points_at, no_points_at]
            if (flag == "-a" || flag == "--all")
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::All)
        }
        [flag, no_points_at, points_at]
            if (flag == "-a" || flag == "--all")
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains.starts_with("--contains=") =>
        {
            let rev = contains
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains.starts_with("--no-contains=") =>
        {
            let rev = contains
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, merged]
            if (flag == "-a" || flag == "--all") && merged.starts_with("--merged=") =>
        {
            let rev = merged
                .strip_prefix("--merged=")
                .ok_or_else(|| GitError::Command("branch --merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, merged]
            if (flag == "-a" || flag == "--all") && merged.starts_with("--no-merged=") =>
        {
            let rev = merged
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, false)
        }
        [flag, format_flag, no_format]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && no_format == "--no-format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, format_flag, format_spec, no_format]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && no_format == "--no-format" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, no_format, format_flag]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag.starts_with("--format=") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, &[], false, format_spec)
        }
        [flag, no_format, format_flag, format_spec]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag == "--format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(git_dir, format, store, mode, &[], false, format_spec)
        }
        [flag, format_flag, no_format, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && no_format == "--no-format"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, format_flag, format_spec, no_format, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && no_format == "--no-format"
                && list == "--list" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, no_format, format_flag, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, no_format, format_flag, format_spec, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag == "--format"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, list, format_flag, no_format, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag.starts_with("--format=")
                && no_format == "--no-format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, format_flag, format_spec, no_format, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag == "--format"
                && no_format == "--no-format" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, format_flag, omit_empty]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag, omit_empty, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, list, format_flag, omit_empty, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag, format_spec, omit_empty]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag, format_spec, omit_empty, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, list, format_flag, format_spec, omit_empty, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag]
            if (flag == "-r" || flag == "--remotes") && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec]
            if (flag == "-r" || flag == "--remotes") && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && format_flag == "--format"
                && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_flag, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, format_spec, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_flag]
            if (flag == "-a" || flag == "--all") && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec]
            if (flag == "-a" || flag == "--all") && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && format_flag == "--format"
                && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_flag, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, format_spec, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, display_flag]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_list_noop_display_flag(display_flag) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, list, display_flag, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_list_noop_display_flag(display_flag) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, display_flag, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, display_flag, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectname_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objecttype_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectsize_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_upstream_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_push_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_version_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(store, mode)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_push_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_push_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_version_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_date_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                mode,
                patterns,
                false,
                (field, descending),
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_push_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                mode,
                patterns,
                false,
                (field, descending),
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, format_flag, ignore, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, list, ignore, format_flag, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && format_flag.starts_with("--format=") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, format_flag, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, format_flag, format_spec, ignore, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, list, ignore, format_flag, format_spec, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && format_flag == "--format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, format_flag, format_spec, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, list, ignore, reset, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, ignore, list, reset, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list"
                && reset == "--no-ignore-case" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, points_at, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && points_at == "--points-at" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, mode, &oid, patterns)
        }
        [flag, list, points_at, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && points_at.starts_with("--points-at=") =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, mode, &oid, patterns)
        }
        [flag, list, contains, contains_rev, no_contains, no_contains_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && contains == "--contains"
                && no_contains == "--no-contains" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, no_contains, no_contains_rev, contains, contains_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && no_contains == "--no-contains"
                && contains == "--contains" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, merged, merged_rev, no_merged, no_merged_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && merged == "--merged"
                && no_merged == "--no-merged" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, no_merged, no_merged_rev, merged, merged_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && no_merged == "--no-merged"
                && merged == "--merged" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, contains, no_contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, no_contains, contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, merged, no_merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, no_merged, merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, contains, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && contains == "--contains" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, contains, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && contains == "--no-contains" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, list, merged, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && merged == "--merged" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, merged, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && merged == "--no-merged" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, list, contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, list, merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, ignore, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Remote, patterns, true)
        }
        [flag, list, ignore, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore) =>
        {
            print_branch_list_matching(store, BranchListMode::Remote, patterns, true)
        }
        [flag, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes") && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Remote, patterns, false)
        }
        [flag, ignore, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::All, patterns, true)
        }
        [flag, list, ignore, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore) =>
        {
            print_branch_list_matching(store, BranchListMode::All, patterns, true)
        }
        [flag, list, patterns @ ..] if (flag == "-a" || flag == "--all") && list == "--list" => {
            print_branch_list_matching(store, BranchListMode::All, patterns, false)
        }
        [flag, key] if flag == "--sort" && key == "refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag] if branch_version_sort_value(flag).is_some() => {
            let descending = branch_version_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_version_sort_value(key).is_some() => {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [flag] if branch_objectname_sort_value(flag).is_some() => {
            let descending =
                branch_objectname_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_objectname_sort_value(key).is_some() => {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [flag] if branch_objecttype_sort_value(flag).is_some() => {
            let descending =
                branch_objecttype_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_objecttype_sort_value(key).is_some() => {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag] if branch_objectsize_sort_value(flag).is_some() => {
            let descending =
                branch_objectsize_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_objectsize_sort_value(key).is_some() => {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag] if branch_date_sort_value(flag).is_some() => {
            let (field, descending) =
                branch_date_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_date_sort_value(key).is_some() => {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [flag] if branch_upstream_sort_value(flag).is_some() => {
            let descending =
                branch_upstream_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag] if branch_push_sort_value(flag).is_some() => {
            let descending = branch_push_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_upstream_sort_value(key).is_some() => {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_push_sort_value(key).is_some() => {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag] if flag == "--sort=-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [flag, key] if flag == "--sort" && key == "-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, no_sort] if sort == "--sort=refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, no_sort]
            if (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, no_sort] if sort == "--sort=-refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort] if no_sort == "--no-sort" && sort == "--sort=refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_version_sort_value(sort).is_some() => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objectname_sort_value(sort).is_some() => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objecttype_sort_value(sort).is_some() => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objectsize_sort_value(sort).is_some() => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_date_sort_value(sort).is_some() => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_upstream_sort_value(sort).is_some() => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_push_sort_value(sort).is_some() => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && sort == "--sort=-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, key, no_sort] if sort == "--sort" && key == "refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, key, no_sort]
            if sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, key, no_sort] if sort == "--sort" && key == "-refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort, key] if no_sort == "--no-sort" && sort == "--sort" && key == "refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort" && sort == "--sort" && branch_version_sort_value(key).is_some() =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key] if no_sort == "--no-sort" && sort == "--sort" && key == "-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [first, second]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [first, second] if branch_column_noop_flag(first) && branch_column_noop_flag(second) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [first, second] if branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, no_format] if flag.starts_with("--format=") && no_format == "--no-format" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, format_spec, no_format] if flag == "--format" && no_format == "--no-format" => {
            let _ = format_spec;
            print_branch_list(store, BranchListMode::Local)
        }
        [no_format, flag] if no_format == "--no-format" && flag.starts_with("--format=") => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                false,
                format_spec,
            )
        }
        [no_format, flag, format_spec] if no_format == "--no-format" && flag == "--format" => {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                false,
                format_spec,
            )
        }
        [flag, omit_empty]
            if flag.starts_with("--format=")
                && (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty") =>
        {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [omit_empty, flag]
            if (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty")
                && flag.starts_with("--format=") =>
        {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [flag, format_spec, omit_empty]
            if flag == "--format"
                && (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty") =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [omit_empty, flag, format_spec]
            if (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty")
                && flag == "--format" =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [flag] if flag.starts_with("--format=") => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, BranchListMode::Local, &[], false, format_spec)
        }
        [flag, format_spec] if flag == "--format" => {
            print_branch_list_format(git_dir, format, store, BranchListMode::Local, &[], false, format_spec)
        }
        [flag, list, patterns @ ..] if flag.starts_with("--format=") && list == "--list" => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag]
            if flag == "--no-color"
                || flag == "--color=never"
                || flag == "--color=auto"
                || branch_column_noop_flag(flag)
                || flag == "--abbrev"
                || flag == "--no-abbrev"
                || flag.starts_with("--abbrev=")
                || flag == "--sort=refname"
                || flag == "--no-sort"
                || flag == "--no-delete"
                || flag == "--no-list"
                || flag == "--no-show-current"
                || flag == "--no-format"
                || flag == "--omit-empty"
                || flag == "--no-omit-empty" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag] if flag == "--show-current" => {
            if let Some(branch) = store.current_branch()? {
                println!("{branch}");
            }
            Ok(())
        }
        [show_current, no_show_current]
            if show_current == "--show-current" && no_show_current == "--no-show-current" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [delete, no_delete, branch]
            if (delete == "-d" || delete == "--delete") && no_delete == "--no-delete" =>
        {
            create_branch_from_start(git_dir, format, store, branch, None)
        }
        [delete, no_delete, branch, start]
            if (delete == "-d" || delete == "--delete") && no_delete == "--no-delete" =>
        {
            create_branch_from_start(git_dir, format, store, branch, Some(start))
        }
        [flag] if flag == "-f" || flag == "--force" => print_branch_list(store, BranchListMode::Local),
        [flag, branches @ ..] if flag == "-D" => force_delete_branches(git_dir, store, branches, false),
        [flag, force, branches @ ..]
            if (flag == "-d" || flag == "--delete") && (force == "-f" || force == "--force") =>
        {
            force_delete_branches(git_dir, store, branches, false)
        }
        [force, flag, branches @ ..]
            if (force == "-f" || force == "--force") && (flag == "-d" || flag == "--delete") =>
        {
            force_delete_branches(git_dir, store, branches, false)
        }
        [flag, branches @ ..] if flag == "-d" || flag == "--delete" => {
            delete_merged_branches(git_dir, format, store, branches, false)
        }
        [flag, branch] if flag == "-f" || flag == "--force" => {
            force_update_branch(git_dir, format, store, branch, None)
        }
        [flag, branch, start] if flag == "-f" || flag == "--force" => {
            force_update_branch(git_dir, format, store, branch, Some(start))
        }
        [branch] => {
            create_branch_from_start(git_dir, format, store, branch, None)?;
            branch_create_set_tracking(git_dir, store, branch, None, None, false)
        }
        [branch, start] => {
            create_branch_from_start(git_dir, format, store, branch, Some(start))?;
            branch_create_set_tracking(git_dir, store, branch, Some(start), None, false)
        }
        _ => Err(GitError::Command(
            "branch currently supports: branch [--list [<pattern>...]] [<name> [<start>]] or branch -d|-D <name>... or branch --force <name> [<start>]"
                .into(),
        )),
    }
}

struct BranchCreateOptions {
    force: bool,
    quiet: bool,
    track: Option<BranchTrackMode>,
    recurse_submodules: bool,
    legacy_set_upstream: bool,
    edit_description: bool,
    create_reflog: bool,
    positionals: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchTrackMode {
    Direct,
    Inherit,
    Never,
}

struct BranchVerboseListOptions {
    mode: BranchListMode,
    patterns: Vec<String>,
    ignore_case: bool,
    verbosity: usize,
    abbrev: Option<Option<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchColumnStyle {
    Column,
    Dense,
}

struct BranchGeneralListOptions {
    mode: BranchListMode,
    patterns: Vec<String>,
    ignore_case: bool,
    color: bool,
    column: Option<BranchColumnStyle>,
    sort: Option<BranchSort>,
}

#[derive(Clone, Copy)]
enum BranchSort {
    Refname(bool),
    Version(bool),
    ObjectName(bool),
    ObjectType(bool),
    ObjectSize(bool),
    Date(ForEachRefDateSortField, bool),
    Upstream(bool),
    Push(bool),
    AheadBehind(ObjectId, bool),
}

struct BranchFormatListOptions {
    mode: BranchListMode,
    patterns: Vec<String>,
    ignore_case: bool,
    color: bool,
    sort: Option<BranchSort>,
    format_spec: String,
    omit_empty: bool,
}

const BRANCH_USAGE_LINES: [&str; 8] = [
    "git branch [<options>] [-r | -a] [--merged] [--no-merged]",
    "git branch [<options>] [-f] [--recurse-submodules] <branch-name> [<start-point>]",
    "git branch [<options>] [-l] [<pattern>...]",
    "git branch [<options>] [-r] (-d | -D) <branch-name>...",
    "git branch [<options>] (-m | -M) [<old-branch>] <new-branch>",
    "git branch [<options>] (-c | -C) [<old-branch>] <new-branch>",
    "git branch [<options>] [-r | -a] [--points-at]",
    "git branch [<options>] [-r | -a] [--format]",
];

fn branch_usage_error(error: UsageError) -> GitError {
    eprint!("{}", error.render_stderr());
    GitError::Exit(error.exit_code())
}

fn branch_error_is_unknown(error: &UsageError) -> bool {
    error.message().is_some_and(|message| {
        message.starts_with("unknown option `") || message.starts_with("unknown switch `")
    })
}

fn parse_branch_options<'a>(
    args: &'a [String],
    specs: &'a [OptionSpec<'a>],
) -> Result<Option<Parsed<'a>>> {
    match parse_options(args, specs, &BRANCH_USAGE_LINES) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(error) if branch_error_is_unknown(&error) => Ok(None),
        Err(error) => Err(branch_usage_error(error)),
    }
}

fn branch_option_bool(option: &ParsedOption<'_>) -> Option<bool> {
    match option.value {
        ParsedValue::Bool(value) => Some(value),
        _ => None,
    }
}

fn branch_positionals(parsed: &Parsed<'_>) -> Vec<String> {
    parsed
        .positionals
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

fn parse_branch_track_value(
    value: CallbackValue<'_>,
) -> std::result::Result<Option<String>, String> {
    if value.unset {
        return Ok(Some("never".into()));
    }
    match value.value.unwrap_or("direct") {
        "direct" => Ok(Some("direct".into())),
        "inherit" => Ok(Some("inherit".into())),
        _ => {
            let option = match value.option {
                OptionName::Short(short) => format!("-{short}"),
                OptionName::Long(long) | OptionName::NegatedLong(long) => format!("--{long}"),
            };
            Err(format!(
                "option `{option}' expects \"direct\" or \"inherit\""
            ))
        }
    }
}

fn branch_track_mode(value: &str) -> BranchTrackMode {
    match value {
        "inherit" => BranchTrackMode::Inherit,
        "never" => BranchTrackMode::Never,
        _ => BranchTrackMode::Direct,
    }
}

#[rustfmt::skip]
macro_rules! branch_bool_option {
    ($short:expr, $long:expr, $flags:expr, $help:expr) => { OptionSpec { short: $short, long: $long, value: OptValue::Bool, flags: $flags, help: $help } };
}

#[rustfmt::skip]
macro_rules! branch_str_option {
    ($short:expr, $long:expr, $metavar:expr, $flags:expr, $help:expr) => { OptionSpec { short: $short, long: $long, value: OptValue::Str($metavar), flags: $flags, help: $help } };
}

#[rustfmt::skip]
macro_rules! branch_track_option {
    () => { OptionSpec { short: Some('t'), long: Some("track"), value: OptValue::Callback { metavar: Some("(direct|inherit)"), parse: parse_branch_track_value }, flags: OptFlags::OPTARG, help: "set branch tracking configuration" } };
}

#[rustfmt::skip]
fn branch_option_specs() -> [OptionSpec<'static>; 25] {
    [
        branch_bool_option!(Some('v'), Some("verbose"), OptFlags::NONE, "show hash and subject, give twice for upstream branch"),
        branch_bool_option!(Some('q'), Some("quiet"), OptFlags::NONE, "suppress informational messages"),
        branch_track_option!(),
        branch_bool_option!(None, Some("unset-upstream"), OptFlags::NONE, "unset the upstream info"),
        branch_str_option!(None, Some("color"), "when", OptFlags::OPTARG, "use colored output"),
        branch_bool_option!(Some('r'), Some("remotes"), OptFlags::NONEG, "act on remote-tracking branches"),
        branch_str_option!(None, Some("abbrev"), "n", OptFlags::OPTARG, "use <n> digits to display object names"),
        branch_bool_option!(Some('a'), Some("all"), OptFlags::NONEG, "list both remote-tracking and local branches"),
        branch_bool_option!(Some('d'), Some("delete"), OptFlags::NONE, "delete fully merged branch"),
        branch_bool_option!(Some('D'), None, OptFlags::NONE, "delete branch (even if not merged)"),
        branch_bool_option!(Some('m'), Some("move"), OptFlags::NONE, "move/rename a branch and its reflog"),
        branch_bool_option!(Some('M'), None, OptFlags::NONE, "move/rename a branch, even if target exists"),
        branch_bool_option!(None, Some("omit-empty"), OptFlags::NONE, "do not output a newline after empty formatted refs"),
        branch_bool_option!(Some('c'), Some("copy"), OptFlags::NONE, "copy a branch and its reflog"),
        branch_bool_option!(Some('C'), None, OptFlags::NONE, "copy a branch, even if target exists"),
        branch_bool_option!(Some('l'), Some("list"), OptFlags::NONE, "list branch names"),
        branch_bool_option!(None, Some("show-current"), OptFlags::NONE, "show current branch name"),
        branch_bool_option!(None, Some("create-reflog"), OptFlags::NONE, "create the branch's reflog"),
        branch_bool_option!(None, Some("edit-description"), OptFlags::NONE, "edit the description for the branch"),
        branch_bool_option!(Some('f'), Some("force"), OptFlags::NONE, "force creation, move/rename, deletion"),
        branch_str_option!(None, Some("column"), "style", OptFlags::OPTARG, "list branches in columns"),
        branch_str_option!(None, Some("sort"), "key", OptFlags::NONE, "field name to sort on"),
        branch_bool_option!(Some('i'), Some("ignore-case"), OptFlags::NONE, "sorting and filtering are case insensitive"),
        branch_bool_option!(None, Some("recurse-submodules"), OptFlags::NONE, "recurse through submodules"),
        branch_str_option!(None, Some("format"), "format", OptFlags::NONE, "format to use for the output"),
    ]
}

fn parse_branch_show_current_options(args: &[String]) -> Result<Option<bool>> {
    let specs = branch_option_specs();
    let Some(parsed) = parse_branch_options(args, &specs)? else {
        return Ok(None);
    };
    let show_current = parsed
        .options
        .iter()
        .filter(|option| option.long == Some("show-current"))
        .filter_map(branch_option_bool)
        .next_back();
    let has_other_options = parsed
        .options
        .iter()
        .any(|option| option.long != Some("show-current"));
    match show_current {
        Some(true) => Ok(Some(true)),
        Some(false) if parsed.positionals.is_empty() && !has_other_options => Ok(Some(false)),
        _ => Ok(None),
    }
}

fn branch_has_conflicting_action_modes(args: &[String]) -> bool {
    let mut delete = false;
    let mut move_or_copy = false;
    let mut list = false;
    for arg in args {
        match arg.as_str() {
            "-d" | "-D" | "--delete" => delete = true,
            "-m" | "-M" | "--move" | "-c" | "-C" | "--copy" => move_or_copy = true,
            "-l" | "--list" => list = true,
            _ => {}
        }
    }
    (delete && move_or_copy) || (delete && list)
}

fn parse_branch_general_list_options(
    git_dir: &Path,
    args: &[String],
) -> Result<Option<BranchGeneralListOptions>> {
    let mut mode = BranchListMode::Local;
    let mut patterns = Vec::new();
    let mut ignore_case = false;
    let mut color = false;
    let mut column = None;
    let mut sort = None;
    let mut explicit_no_sort = false;
    let mut explicit_list = false;
    let mut saw_list_control = args.is_empty();
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-l" | "--list" => {
                explicit_list = true;
                saw_list_control = true;
            }
            "-r" | "--remotes" => {
                mode = BranchListMode::Remote;
                saw_list_control = true;
            }
            "-a" | "--all" => {
                mode = BranchListMode::All;
                saw_list_control = true;
            }
            "-i" | "--ignore-case" => {
                ignore_case = true;
                saw_list_control = true;
            }
            "--no-ignore-case" => {
                ignore_case = false;
                saw_list_control = true;
            }
            "--color" | "--color=always" => {
                color = true;
                saw_list_control = true;
            }
            "--no-color" | "--color=never" | "--color=auto" => {
                color = false;
                saw_list_control = true;
            }
            "--column" | "--column=column" => {
                column = Some(BranchColumnStyle::Column);
                saw_list_control = true;
            }
            "--column=dense" => {
                column = Some(BranchColumnStyle::Dense);
                saw_list_control = true;
            }
            "--no-column" | "--column=never" | "--column=plain" | "--column=auto" => {
                column = None;
                saw_list_control = true;
            }
            "--sort" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--sort requires a value".into()));
                };
                sort = Some(branch_sort_from_key(git_dir, repository_object_format(git_dir)?, value)?);
                explicit_no_sort = false;
                saw_list_control = true;
            }
            value if value.starts_with("--sort=") => {
                let value = value
                    .strip_prefix("--sort=")
                    .expect("prefix checked by match guard");
                sort = Some(branch_sort_from_key(git_dir, repository_object_format(git_dir)?, value)?);
                explicit_no_sort = false;
                saw_list_control = true;
            }
            "--no-sort" => {
                sort = None;
                explicit_no_sort = true;
                saw_list_control = true;
            }
            value if value.starts_with('-') => return Ok(None),
            value => patterns.push(value.to_string()),
        }
        idx += 1;
    }

    if !patterns.is_empty()
        && !explicit_list
        && !matches!(mode, BranchListMode::Remote | BranchListMode::All)
    {
        return Ok(None);
    }

    let config = read_repo_config(git_dir)?;
    if sort.is_none()
        && !explicit_no_sort
        && let Some(config_sort) = config.get("branch", None, "sort")
    {
        sort = Some(branch_sort_from_key(git_dir, repository_object_format(git_dir)?, config_sort)?);
        saw_list_control = true;
    }
    if column.is_none()
        && !args
            .iter()
            .any(|arg| arg.starts_with("--column") || arg == "--no-column")
        && branch_config_enables_columns(&config)
    {
        column = Some(branch_config_column_style(&config));
        saw_list_control = true;
    }

    Ok(saw_list_control.then_some(BranchGeneralListOptions {
        mode,
        patterns,
        ignore_case,
        color,
        column,
        sort,
    }))
}

fn parse_branch_format_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    args: &[String],
) -> Result<Option<BranchFormatListOptions>> {
    if !args
        .iter()
        .any(|arg| arg == "--format" || arg.starts_with("--format="))
    {
        return Ok(None);
    }

    let mut mode = BranchListMode::Local;
    let mut patterns = Vec::new();
    let mut ignore_case = false;
    let mut color = false;
    let mut sort = None;
    let mut format_spec = None;
    let mut omit_empty = false;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-l" | "--list" => {}
            "-r" | "--remotes" => mode = BranchListMode::Remote,
            "-a" | "--all" => mode = BranchListMode::All,
            "-i" | "--ignore-case" => ignore_case = true,
            "--no-ignore-case" => ignore_case = false,
            "--color" | "--color=always" => color = true,
            "--no-color" | "--color=never" | "--color=auto" => color = false,
            "--omit-empty" => omit_empty = true,
            "--no-omit-empty" => omit_empty = false,
            "--no-format" => format_spec = None,
            "--format" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("branch --format requires a value".into()));
                };
                format_spec = Some(value.to_string());
            }
            value if value.starts_with("--format=") => {
                format_spec = Some(
                    value
                        .strip_prefix("--format=")
                        .expect("prefix checked by match guard")
                        .to_string(),
                );
            }
            "--sort" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--sort requires a value".into()));
                };
                sort = Some(branch_sort_from_key(git_dir, format, value)?);
            }
            value if value.starts_with("--sort=") => {
                let value = value
                    .strip_prefix("--sort=")
                    .expect("prefix checked by match guard");
                sort = Some(branch_sort_from_key(git_dir, format, value)?);
            }
            "--no-sort" => sort = None,
            value if value.starts_with('-') => return Ok(None),
            value => patterns.push(value.to_string()),
        }
        idx += 1;
    }

    Ok(format_spec.map(|format_spec| BranchFormatListOptions {
        mode,
        patterns,
        ignore_case,
        color,
        sort,
        format_spec,
        omit_empty,
    }))
}

fn branch_sort_from_key(git_dir: &Path, format: ObjectFormat, key: &str) -> Result<BranchSort> {
    let key = key.strip_prefix("--sort=").unwrap_or(key);
    match key {
        "refname" => Ok(BranchSort::Refname(false)),
        "-refname" => Ok(BranchSort::Refname(true)),
        value if branch_version_sort_value(value).is_some() => Ok(BranchSort::Version(
            branch_version_sort_value(value).expect("checked branch version sort"),
        )),
        value if branch_objectname_sort_value(value).is_some() => Ok(BranchSort::ObjectName(
            branch_objectname_sort_value(value).expect("checked branch objectname sort"),
        )),
        value if branch_objecttype_sort_value(value).is_some() => Ok(BranchSort::ObjectType(
            branch_objecttype_sort_value(value).expect("checked branch objecttype sort"),
        )),
        value if branch_objectsize_sort_value(value).is_some() => Ok(BranchSort::ObjectSize(
            branch_objectsize_sort_value(value).expect("checked branch objectsize sort"),
        )),
        value if branch_date_sort_value(value).is_some() => {
            let (field, descending) =
                branch_date_sort_value(value).expect("checked branch date sort");
            Ok(BranchSort::Date(field, descending))
        }
        value if branch_upstream_sort_value(value).is_some() => Ok(BranchSort::Upstream(
            branch_upstream_sort_value(value).expect("checked branch upstream sort"),
        )),
        value if branch_push_sort_value(value).is_some() => Ok(BranchSort::Push(
            branch_push_sort_value(value).expect("checked branch push sort"),
        )),
        value if branch_ahead_behind_sort_value(value).is_some() => {
            let (rev, descending) =
                branch_ahead_behind_sort_value(value).expect("checked ahead-behind sort");
            let oid = resolve_revision(git_dir, format, rev)?;
            Ok(BranchSort::AheadBehind(oid, descending))
        }
        _ => {
            eprintln!("fatal: unknown field name: {key}");
            Err(GitError::Exit(128))
        }
    }
}

fn branch_config_enables_columns(config: &GitConfig) -> bool {
    config
        .get("column", Some("branch"), "branch")
        .or_else(|| config.get("column", None, "branch"))
        .or_else(|| config.get("column", None, "ui"))
        .is_some_and(|value| matches!(value, "column" | "dense" | "always"))
}

fn branch_config_column_style(config: &GitConfig) -> BranchColumnStyle {
    match config
        .get("column", Some("branch"), "branch")
        .or_else(|| config.get("column", None, "branch"))
    {
        Some("dense") => BranchColumnStyle::Dense,
        _ => BranchColumnStyle::Column,
    }
}

#[rustfmt::skip]
fn branch_verbose_list_option_specs() -> [OptionSpec<'static>; 11] {
    [
        branch_bool_option!(Some('v'), Some("verbose"), OptFlags::NONE, "show hash and subject, give twice for upstream branch"),
        branch_bool_option!(Some('l'), Some("list"), OptFlags::NONE, "list branch names"),
        branch_bool_option!(None, Some("no-delete"), OptFlags::NONEG, "do not delete branches"),
        branch_bool_option!(None, Some("show-current"), OptFlags::NONE, "show current branch name"),
        branch_bool_option!(Some('r'), Some("remotes"), OptFlags::NONEG, "act on remote-tracking branches"),
        branch_bool_option!(Some('a'), Some("all"), OptFlags::NONEG, "list both remote-tracking and local branches"),
        branch_bool_option!(Some('i'), Some("ignore-case"), OptFlags::NONE, "sorting and filtering are case insensitive"),
        branch_str_option!(None, Some("color"), "when", OptFlags::OPTARG, "use colored output"),
        branch_str_option!(None, Some("column"), "style", OptFlags::OPTARG, "list branches in columns"),
        branch_str_option!(None, Some("abbrev"), "n", OptFlags::OPTARG, "use <n> digits to display object names"),
        branch_str_option!(None, Some("sort"), "key", OptFlags::NONE, "field name to sort on"),
    ]
}

fn parse_branch_verbose_list_options(args: &[String]) -> Result<Option<BranchVerboseListOptions>> {
    let mut verbosity = 0usize;
    let mut explicit_list = false;
    let mut mode = BranchListMode::Local;
    let mut ignore_case = false;
    let mut abbrev = None;
    let mut saw_verbose = false;
    let mut saw_column = false;
    let specs = branch_verbose_list_option_specs();
    let Some(parsed) = parse_branch_options(args, &specs)? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match option.long {
            Some("verbose") => {
                saw_verbose = true;
                if branch_option_bool(option).unwrap_or(true) {
                    verbosity = verbosity.saturating_add(1);
                } else {
                    verbosity = 0;
                }
            }
            Some("list") => {
                if branch_option_bool(option).unwrap_or(true) {
                    explicit_list = true;
                }
            }
            Some("remotes") => mode = BranchListMode::Remote,
            Some("all") => mode = BranchListMode::All,
            Some("ignore-case") => ignore_case = branch_option_bool(option).unwrap_or(true),
            Some("column") => saw_column = true,
            _ => {}
        }
    }
    for arg in args {
        match arg.as_str() {
            "--abbrev" => abbrev = None,
            "--no-abbrev" => abbrev = Some(None),
            value if value.starts_with("--abbrev=") => {
                let value = value
                    .strip_prefix("--abbrev=")
                    .expect("prefix checked by match guard");
                let width = value
                    .parse::<usize>()
                    .map_err(|_| GitError::Command(format!("invalid abbrev length {value}")))?;
                abbrev = if width == 0 { Some(None) } else { Some(Some(width)) };
            }
            _ => {}
        }
    }
    if !saw_verbose {
        return Ok(None);
    }
    if saw_column && verbosity > 0 {
        eprintln!("fatal: options '--column' and '--verbose' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if !explicit_list
        && !matches!(mode, BranchListMode::Remote | BranchListMode::All)
        && !parsed.positionals.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(BranchVerboseListOptions {
        mode,
        patterns: branch_positionals(&parsed),
        ignore_case,
        verbosity,
        abbrev,
    }))
}

fn run_branch_verbose_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchVerboseListOptions,
) -> Result<()> {
    if options.verbosity == 0 {
        return print_branch_list_matching(
            store,
            options.mode,
            &options.patterns,
            options.ignore_case,
        );
    }
    print_branch_list_verbose(git_dir, format, store, options)
}

#[derive(Clone, Copy)]
enum BranchMoveKind {
    Rename,
    Copy,
}

struct BranchMoveOptions {
    kind: BranchMoveKind,
    force: bool,
    branches: Vec<String>,
}

#[rustfmt::skip]
fn branch_move_option_specs() -> [OptionSpec<'static>; 7] {
    [
        branch_bool_option!(Some('m'), Some("move"), OptFlags::NONE, "move/rename a branch and its reflog"),
        branch_bool_option!(Some('M'), None, OptFlags::NONE, "move/rename a branch, even if target exists"),
        branch_bool_option!(Some('c'), Some("copy"), OptFlags::NONE, "copy a branch and its reflog"),
        branch_bool_option!(Some('C'), None, OptFlags::NONE, "copy a branch, even if target exists"),
        branch_bool_option!(Some('f'), Some("force"), OptFlags::NONE, "force creation, move/rename, deletion"),
        branch_bool_option!(Some('q'), Some("quiet"), OptFlags::NONE, "suppress informational messages"),
        branch_bool_option!(Some('v'), Some("verbose"), OptFlags::NONE, "show hash and subject, give twice for upstream branch"),
    ]
}

fn parse_branch_move_options(args: &[String]) -> Result<Option<BranchMoveOptions>> {
    let mut kind = None;
    let mut force = false;
    let specs = branch_move_option_specs();
    let Some(parsed) = parse_branch_options(args, &specs)? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match (option.short, option.long) {
            (Some('m'), _) | (_, Some("move")) => {
                if branch_option_bool(option).unwrap_or(true) {
                    kind = Some(BranchMoveKind::Rename);
                } else {
                    kind = None;
                }
            }
            (Some('M'), _) => {
                kind = Some(BranchMoveKind::Rename);
                force = true;
            }
            (Some('c'), _) | (_, Some("copy")) => {
                if branch_option_bool(option).unwrap_or(true) {
                    kind = Some(BranchMoveKind::Copy);
                } else {
                    kind = None;
                }
            }
            (Some('C'), _) => {
                kind = Some(BranchMoveKind::Copy);
                force = true;
            }
            (_, Some("force")) => force = branch_option_bool(option).unwrap_or(true),
            _ => {}
        }
    }
    Ok(kind.map(|kind| BranchMoveOptions {
        kind,
        force,
        branches: branch_positionals(&parsed),
    }))
}

fn run_branch_move_options(
    git_dir: &Path,
    store: &FileRefStore,
    options: BranchMoveOptions,
) -> Result<()> {
    let (old_branch, new_branch) = branch_move_branches(store, options.kind, &options.branches)?;
    if old_branch == new_branch {
        return Ok(());
    }
    let old_ref = validate_branch_source_name(&old_branch)?;
    let new_ref = validate_branch_creation_name(&new_branch)?;
    if store.read_ref(&old_ref)?.is_none() {
        // branch.c `copy_or_rename_branch`: renaming the current *unborn*
        // branch (HEAD points at it but no commit exists) is allowed and only
        // repoints the HEAD symref; copying it (or touching any other missing
        // branch) dies.
        let old_is_head = store.current_branch_ref()?.as_deref() == Some(old_ref.as_str());
        if matches!(options.kind, BranchMoveKind::Rename) && old_is_head {
            if !options.force && store.read_ref(&new_ref)?.is_some() {
                eprintln!("fatal: a branch named '{new_branch}' already exists");
                return Err(GitError::Exit(128));
            }
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(new_ref.clone()),
                reflog: None,
            });
            tx.commit()?;
            rename_branch_config(git_dir, &old_branch, &new_branch)?;
            return Ok(());
        }
        if old_is_head {
            eprintln!("fatal: no commit on branch '{old_branch}' yet");
        } else {
            eprintln!("fatal: no branch named '{old_branch}'");
        }
        return Err(GitError::Exit(128));
    }
    // A dangling symref destination does not "exist" for the purposes of the
    // rename collision check (git's validate_branchname uses RESOLVE_REF_READING),
    // so `branch -m m broken_symref` overwrites it without --force (t3200 #16).
    if !options.force && sley_refs::resolve_ref_peeled(store, &new_ref)?.is_some() {
        eprintln!("fatal: a branch named '{new_branch}' already exists");
        return Err(GitError::Exit(128));
    }
    if options.force
        && old_ref != new_ref
        && let Some(worktree_root) = branch_checked_out_worktree_path(git_dir, store, &new_ref)?
    {
        eprintln!(
            "fatal: cannot force update the branch '{new_branch}' used by worktree at '{}'",
            worktree_root
        );
        return Err(GitError::Exit(128));
    }

    match options.kind {
        BranchMoveKind::Rename => {
            let committer = branch_reflog_committer_identity(store, &old_branch)?;
            let head_was_old = store.current_branch_ref()?.as_deref() == Some(old_ref.as_str());
            let old_oid = match store.read_ref(&old_ref)? {
                Some(RefTarget::Direct(oid)) => oid,
                _ => zero_oid(repository_object_format(git_dir)?)?,
            };
            let head_reflog = ReflogEntry {
                old_oid,
                new_oid: old_oid,
                committer: committer.clone(),
                message: format!("Branch: renamed {old_ref} to {new_ref}").into_bytes(),
            };
            store.move_branch(&old_branch, &new_branch, options.force, committer)?;
            let linked_update = update_linked_worktree_heads(git_dir, &old_ref, &new_ref);
            if head_was_old {
                store.append_reflog("HEAD", &head_reflog)?;
            }
            rename_branch_config(git_dir, &old_branch, &new_branch)?;
            linked_update?;
        }
        BranchMoveKind::Copy => {
            let committer = branch_reflog_committer_identity(store, &old_branch)?;
            store.copy_branch(&old_branch, &new_branch, options.force, committer)?;
            copy_branch_config(git_dir, &old_branch, &new_branch)?;
        }
    }
    Ok(())
}

fn update_linked_worktree_heads(git_dir: &Path, old_ref: &str, new_ref: &str) -> Result<()> {
    let worktrees_dir = common_git_dir_for_git_dir(git_dir)?.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(());
    };
    let mut failed = false;
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let head_path = admin_dir.join("HEAD");
        let Ok(head) = fs::read_to_string(&head_path) else {
            continue;
        };
        if head.trim().strip_prefix("ref: ") != Some(old_ref) {
            continue;
        }
        if admin_dir.join("HEAD.lock").exists() {
            failed = true;
            continue;
        }
        fs::write(head_path, format!("ref: {new_ref}\n"))?;
    }
    if failed {
        eprintln!("error: could not update one or more linked worktree HEADs");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn branch_reflog_committer_identity(store: &FileRefStore, branch: &str) -> Result<Vec<u8>> {
    if env::var("GIT_COMMITTER_DATE").is_ok() {
        return commit_identity_from_env("COMMITTER");
    }
    let refname = branch_ref_name(branch)?;
    let max_existing = store
        .read_reflog(&refname)?
        .iter()
        .filter_map(|entry| entry.timestamp_seconds().ok())
        .max()
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let date = format!("@{} +0000", now.max(max_existing + 1));
    let name = env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| "Git Rs".into());
    let email = env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| "sley@example.invalid".into());
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

fn branch_move_branches(
    store: &FileRefStore,
    kind: BranchMoveKind,
    branches: &[String],
) -> Result<(String, String)> {
    match branches {
        [] => {
            eprintln!("fatal: branch name required");
            Err(GitError::Exit(128))
        }
        [new_branch] => {
            let Some(old_branch) = store.current_branch()? else {
                match kind {
                    BranchMoveKind::Rename => {
                        eprintln!("fatal: cannot rename the current branch while not on any");
                    }
                    BranchMoveKind::Copy => {
                        eprintln!("fatal: cannot copy the current branch while not on any");
                    }
                }
                return Err(GitError::Exit(128));
            };
            Ok((old_branch, new_branch.to_string()))
        }
        [old_branch, new_branch] => Ok((old_branch.to_string(), new_branch.to_string())),
        _ => {
            match kind {
                BranchMoveKind::Rename => {
                    eprintln!("fatal: too many arguments for a rename operation");
                }
                BranchMoveKind::Copy => {
                    eprintln!("fatal: too many branches for a copy operation");
                }
            }
            Err(GitError::Exit(128))
        }
    }
}

fn rename_branch_config(git_dir: &Path, old_branch: &str, new_branch: &str) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let mut renamed = false;
    for section in &mut config.sections {
        if section.name == "branch" && section.subsection.as_deref() == Some(old_branch) {
            section.subsection = Some(new_branch.to_string());
            renamed = true;
        }
    }
    if renamed {
        write_repo_config(git_dir, &config)?;
    }
    Ok(())
}

fn copy_branch_config(git_dir: &Path, old_branch: &str, new_branch: &str) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let mut copied = false;
    let mut sections = Vec::with_capacity(config.sections.len());
    for section in config.sections {
        if section.name == "branch" && section.subsection.as_deref() == Some(old_branch) {
            let mut copied_section = section.clone();
            copied_section.subsection = Some(new_branch.to_string());
            sections.push(section);
            sections.push(copied_section);
            copied = true;
        } else {
            sections.push(section);
        }
    }
    if copied {
        config.sections = sections;
        write_repo_config(git_dir, &config)?;
    }
    Ok(())
}

enum BranchUpstreamAction {
    Set(String),
    Unset,
}

struct BranchUpstreamOptions {
    action: BranchUpstreamAction,
    branches: Vec<String>,
}

#[rustfmt::skip]
fn branch_upstream_option_specs() -> [OptionSpec<'static>; 3] {
    [
        branch_bool_option!(None, Some("set-upstream"), OptFlags::NONE, "set upstream for git pull/status"),
        branch_str_option!(Some('u'), Some("set-upstream-to"), "upstream", OptFlags::NONE, "change the upstream info"),
        branch_bool_option!(None, Some("unset-upstream"), OptFlags::NONE, "unset the upstream info"),
    ]
}

fn parse_branch_upstream_options(args: &[String]) -> Result<Option<BranchUpstreamOptions>> {
    let mut action = None;
    let specs = branch_upstream_option_specs();
    let Some(parsed) = parse_branch_options(args, &specs)? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match option.long {
            Some("set-upstream-to") => match option.value {
                ParsedValue::Str(_) if matches!(option.name, OptionName::NegatedLong(_)) => {
                    action = None;
                }
                ParsedValue::Str(value) => {
                    action = Some(BranchUpstreamAction::Set(value.to_string()));
                }
                _ => {}
            },
            Some("unset-upstream") => {
                if branch_option_bool(option).unwrap_or(true) {
                    action = Some(BranchUpstreamAction::Unset);
                } else {
                    action = None;
                }
            }
            _ => {}
        }
    }
    Ok(action.map(|action| BranchUpstreamOptions {
        action,
        branches: branch_positionals(&parsed),
    }))
}

fn run_branch_upstream_options(
    git_dir: &Path,
    store: &FileRefStore,
    options: BranchUpstreamOptions,
) -> Result<()> {
    match options.action {
        BranchUpstreamAction::Set(upstream) => {
            if options.branches.len() > 1 {
                eprintln!("fatal: too many arguments to set new upstream");
                return Err(GitError::Exit(128));
            }
            let upstream = branch_upstream_resolve_previous_checkout(git_dir, &upstream)?;
            let branch =
                branch_upstream_target_branch(store, options.branches.first(), true, &upstream)?;
            set_branch_upstream(git_dir, store, &branch, &upstream)
        }
        BranchUpstreamAction::Unset => {
            if options.branches.len() > 1 {
                eprintln!("fatal: too many arguments to unset upstream");
                return Err(GitError::Exit(128));
            }
            let branch = branch_upstream_target_branch(store, options.branches.first(), false, "")?;
            unset_branch_upstream(git_dir, &branch)
        }
    }
}

fn branch_upstream_resolve_previous_checkout(git_dir: &Path, upstream: &str) -> Result<String> {
    let Some(inner) = upstream
        .strip_prefix("@{-")
        .and_then(|value| value.strip_suffix('}'))
    else {
        return Ok(upstream.to_string());
    };
    let n = inner
        .parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid branch name: '{upstream}'")))?;
    let format = repository_object_format(git_dir)?;
    Ok(sley_rev::nth_prior_checkout_branch_name(git_dir, format, n)?
        .unwrap_or_else(|| upstream.to_string()))
}

fn branch_upstream_target_branch(
    store: &FileRefStore,
    explicit: Option<&String>,
    setting: bool,
    upstream: &str,
) -> Result<String> {
    if let Some(branch) = explicit {
        let refname = match branch_ref_name(branch) {
            Ok(refname) => refname,
            Err(GitError::InvalidPath(_)) => {
                branch_upstream_missing_branch(branch, setting);
                return Err(GitError::Exit(128));
            }
            Err(err) => return Err(err),
        };
        if store.read_ref(&refname)?.is_none() {
            if setting {
                eprintln!("fatal: branch '{branch}' does not exist");
            } else {
                eprintln!("fatal: branch '{branch}' has no upstream information");
            }
            return Err(GitError::Exit(128));
        }
        return Ok(branch.to_string());
    }
    let Some(branch) = store.current_branch()? else {
        if setting {
            eprintln!(
                "fatal: could not set upstream of HEAD to {upstream} when it does not point to any branch"
            );
        } else {
            eprintln!(
                "fatal: could not unset upstream of HEAD when it does not point to any branch"
            );
        }
        return Err(GitError::Exit(128));
    };
    Ok(branch)
}

fn branch_upstream_missing_branch(branch: &str, setting: bool) {
    if setting {
        eprintln!("fatal: branch '{branch}' does not exist");
    } else {
        eprintln!("fatal: branch '{branch}' has no upstream information");
    }
}

fn set_branch_upstream(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    upstream: &str,
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let Some(upstream) = resolve_branch_upstream(store, &config, upstream)? else {
        eprintln!("fatal: the requested upstream branch '{upstream}' does not exist");
        eprintln!("hint:");
        eprintln!("hint: If you are planning on basing your work on an upstream");
        eprintln!("hint: branch that already exists at the remote, you may need to");
        eprintln!("hint: run \"git fetch\" to retrieve it.");
        eprintln!("hint:");
        eprintln!("hint: If you are planning to push out a new local branch that");
        eprintln!("hint: will track its remote counterpart, you may want to use");
        eprintln!("hint: \"git push -u\" to set the upstream config as you push.");
        eprintln!(
            "hint: Disable this message with \"git config set advice.setUpstreamFailure false\""
        );
        return Err(GitError::Exit(128));
    };
    let branch_ref = branch_ref_name(branch)?;
    if upstream.remote == "." && upstream.merge == branch_ref {
        eprintln!("warning: not setting branch '{branch}' as its own upstream");
        return Ok(());
    }
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "remote",
        &upstream.remote,
    );
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "merge",
        &upstream.merge,
    );
    write_repo_config(git_dir, &config)?;
    println!("branch '{branch}' set up to track '{}'.", upstream.display);
    Ok(())
}

struct ResolvedBranchUpstream {
    remote: String,
    merge: String,
    display: String,
}

fn resolve_branch_upstream(
    store: &FileRefStore,
    config: &GitConfig,
    upstream: &str,
) -> Result<Option<ResolvedBranchUpstream>> {
    let local_branch = upstream.strip_prefix("refs/heads/").unwrap_or(upstream);
    if let Ok(local_ref) = branch_ref_name(local_branch)
        && store.read_ref(&local_ref)?.is_some()
    {
        return Ok(Some(ResolvedBranchUpstream {
            remote: ".".into(),
            merge: local_ref,
            display: local_branch.to_string(),
        }));
    }
    for remote in remote_names(config) {
        let Some((remote_ref, merge)) = branch_upstream_remote_ref(config, &remote, upstream)
        else {
            continue;
        };
        if store.read_ref(&remote_ref)?.is_some() {
            let display = remote_ref
                .strip_prefix("refs/remotes/")
                .unwrap_or(remote_ref.as_str())
                .to_string();
            return Ok(Some(ResolvedBranchUpstream {
                remote,
                merge,
                display,
            }));
        }
    }
    Ok(None)
}

fn branch_upstream_remote_ref(
    config: &GitConfig,
    remote: &str,
    upstream: &str,
) -> Option<(String, String)> {
    let remote_ref = if upstream.starts_with("refs/") {
        upstream.to_string()
    } else {
        upstream
            .strip_prefix("refs/remotes/")
            .map(str::to_string)
            .or_else(|| {
                upstream
                    .strip_prefix(&format!("{remote}/"))
                    .map(|branch| format!("{remote}/{branch}"))
            })
            .map(|name| format!("refs/remotes/{name}"))?
    };
    for fetch in config
        .get_all("remote", Some(remote), "fetch")
        .into_iter()
        .flatten()
    {
        let refspec = parse_refspec(fetch).ok()?;
        if refspec.negative {
            continue;
        }
        let dst = refspec.dst.as_deref()?;
        let src = refspec.src.as_deref()?;
        if refspec.pattern {
            let (dst_prefix, dst_suffix) = dst.split_once('*')?;
            let Some(middle) = remote_ref
                .strip_prefix(dst_prefix)
                .and_then(|value| value.strip_suffix(dst_suffix))
            else {
                continue;
            };
            let (src_prefix, src_suffix) = src.split_once('*')?;
            let merge = format!("{src_prefix}{middle}{src_suffix}");
            return Some((remote_ref, merge));
        }
        if dst == remote_ref {
            return Some((remote_ref, src.to_string()));
        }
    }
    None
}

fn unset_branch_upstream(git_dir: &Path, branch: &str) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let Some(section_idx) = config.sections.iter().rposition(|section| {
        section.name == "branch" && section.subsection.as_deref() == Some(branch)
    }) else {
        eprintln!("fatal: branch '{branch}' has no upstream information");
        return Err(GitError::Exit(128));
    };
    let had_upstream = {
        let section = &mut config.sections[section_idx];
        let before = section.entries.len();
        section
            .entries
            .retain(|entry| !matches!(entry.key.as_str(), "remote" | "merge"));
        section.entries.len() != before
    };
    if !had_upstream {
        eprintln!("fatal: branch '{branch}' has no upstream information");
        return Err(GitError::Exit(128));
    }
    config
        .sections
        .retain(|section| !(section.name == "branch" && section.entries.is_empty()));
    write_repo_config(git_dir, &config)
}

fn remove_branch_config(git_dir: &Path, branch: &str) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let before = config.sections.len();
    config.sections.retain(|section| {
        !(section.name == "branch" && section.subsection.as_deref() == Some(branch))
    });
    if config.sections.len() != before {
        write_repo_config(git_dir, &config)?;
    }
    Ok(())
}

fn parse_branch_create_options(args: &[String]) -> Result<Option<BranchCreateOptions>> {
    let mut saw_create_option = false;
    let mut force = false;
    let mut quiet = false;
    let mut track = None;
    let mut recurse_submodules = false;
    let mut legacy_set_upstream = false;
    let mut edit_description = false;
    let mut create_reflog = false;
    let saw_separator = args.iter().any(|arg| arg == "--");
    let specs = branch_create_option_specs();
    let Some(parsed) = parse_branch_options(args, &specs)? else {
        return Ok(None);
    };
    for option in &parsed.options {
        saw_create_option = true;
        match option.long {
            Some("force") => force = branch_option_bool(option).unwrap_or(true),
            Some("quiet") => quiet = branch_option_bool(option).unwrap_or(true),
            Some("track") => {
                if let ParsedValue::Callback(Some(value)) = &option.value {
                    track = Some(branch_track_mode(value));
                }
            }
            Some("recurse-submodules") => {
                recurse_submodules = branch_option_bool(option).unwrap_or(true);
            }
            Some("set-upstream") => {
                legacy_set_upstream = branch_option_bool(option).unwrap_or(true);
            }
            Some("edit-description") => {
                edit_description = branch_option_bool(option).unwrap_or(true);
            }
            Some("create-reflog") => {
                create_reflog = branch_option_bool(option).unwrap_or(true);
            }
            _ => {}
        }
    }

    Ok(
        (saw_create_option || saw_separator).then_some(BranchCreateOptions {
            force,
            quiet,
            track,
            recurse_submodules,
            legacy_set_upstream,
            edit_description,
            create_reflog,
            positionals: branch_positionals(&parsed),
        }),
    )
}

#[rustfmt::skip]
fn branch_create_option_specs() -> [OptionSpec<'static>; 8] {
    [
        branch_bool_option!(Some('f'), Some("force"), OptFlags::NONE, "force creation, move/rename, deletion"),
        branch_bool_option!(Some('q'), Some("quiet"), OptFlags::NONE, "suppress informational messages"),
        branch_track_option!(),
        branch_bool_option!(None, Some("recurse-submodules"), OptFlags::NONE, "recurse through submodules"),
        branch_bool_option!(None, Some("set-upstream"), OptFlags::NONE, "set upstream for git pull/status"),
        branch_bool_option!(None, Some("edit-description"), OptFlags::NONE, "edit the description for the branch"),
        branch_bool_option!(None, Some("create-reflog"), OptFlags::NONE, "create the branch's reflog"),
        branch_bool_option!(Some('v'), Some("verbose"), OptFlags::NONE, "show hash and subject, give twice for upstream branch"),
    ]
}

fn run_branch_create_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchCreateOptions,
) -> Result<()> {
    if options.recurse_submodules {
        eprintln!(
            "fatal: branch with --recurse-submodules can only be used if submodule.propagateBranches is enabled"
        );
        return Err(GitError::Exit(128));
    }
    if options.edit_description {
        return branch_edit_description(store, &options.positionals);
    }
    if options.legacy_set_upstream && !options.positionals.is_empty() {
        eprintln!(
            "fatal: the '--set-upstream' option is no longer supported. Please use '--track' or '--set-upstream-to' instead"
        );
        return Err(GitError::Exit(128));
    }
    match options.positionals.as_slice() {
        [] => print_branch_list(store, BranchListMode::Local),
        [branch] if options.force => {
            force_update_branch(git_dir, format, store, branch, None)?;
            branch_create_set_tracking(git_dir, store, branch, None, options.track, options.quiet)
        }
        [branch] => {
            create_branch_from_start_with_reflog(
                git_dir,
                format,
                store,
                branch,
                None,
                options.create_reflog,
            )?;
            branch_create_set_tracking_or_rollback(
                git_dir,
                store,
                branch,
                None,
                options.track,
                options.quiet,
            )
        }
        [branch, start] if options.force => {
            force_update_branch(git_dir, format, store, branch, Some(start))?;
            branch_create_set_tracking(
                git_dir,
                store,
                branch,
                Some(start),
                options.track,
                options.quiet,
            )
        }
        [branch, start] => {
            create_branch_from_start_with_reflog(
                git_dir,
                format,
                store,
                branch,
                Some(start),
                options.create_reflog,
            )?;
            branch_create_set_tracking_or_rollback(
                git_dir,
                store,
                branch,
                Some(start),
                options.track,
                options.quiet,
            )
        }
        _ => Err(GitError::Command(
            "branch currently supports: branch [--list [<pattern>...]] [<name> [<start>]] or branch -d|-D <name>... or branch --force <name> [<start>]"
                .into(),
        )),
    }
}

fn branch_create_set_tracking_or_rollback(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    track: Option<BranchTrackMode>,
    quiet: bool,
) -> Result<()> {
    match branch_create_set_tracking(git_dir, store, branch, start, track, quiet) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = store.delete_branch(branch);
            Err(err)
        }
    }
}

fn branch_edit_description(store: &FileRefStore, positionals: &[String]) -> Result<()> {
    match positionals {
        [] => {
            if store.current_branch()?.is_none() {
                eprintln!("fatal: cannot give description to detached HEAD");
                return Err(GitError::Exit(128));
            }
            Ok(())
        }
        [branch] => {
            if store.read_ref(&branch_ref_name(branch)?)?.is_none() {
                eprintln!("error: no branch named '{branch}'");
                return Err(GitError::Exit(1));
            }
            Ok(())
        }
        _ => {
            eprintln!("fatal: cannot edit description of more than one branch");
            Err(GitError::Exit(128))
        }
    }
}

/// The effective tracking mode, mirroring git's `enum branch_track`. When the
/// command line does not request a mode, `branch.autosetupmerge` (parsed in
/// [`config_default_track`]) selects the default — which is `Remote`, not
/// "off", so creating a branch from a remote-tracking start-point sets up
/// tracking automatically.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EffectiveTrack {
    Never,
    Remote,
    Always,
    Explicit,
    Inherit,
    Simple,
}

/// Resolve `branch.autosetupmerge` into the default tracking mode used when the
/// command line gives no `--track`/`--no-track`. Matches git's
/// `git_default_branch_config` (environment.c).
fn config_default_track(config: &GitConfig) -> EffectiveTrack {
    match config.get("branch", None, "autosetupmerge") {
        None => EffectiveTrack::Remote,
        Some("always") => EffectiveTrack::Always,
        Some("inherit") => EffectiveTrack::Inherit,
        Some("simple") => EffectiveTrack::Simple,
        Some(other) => {
            if config_bool_value(other) {
                EffectiveTrack::Remote
            } else {
                EffectiveTrack::Never
            }
        }
    }
}

/// git's `git_config_bool` truthiness for non-special strings.
fn config_bool_value(value: &str) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "" | "true" | "yes" | "on" => true,
        "false" | "no" | "off" | "0" => false,
        other => other.parse::<i64>().map(|n| n != 0).unwrap_or(true),
    }
}

pub(crate) fn branch_create_set_tracking(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    track: Option<BranchTrackMode>,
    quiet: bool,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let effective = match track {
        Some(BranchTrackMode::Never) => EffectiveTrack::Never,
        Some(BranchTrackMode::Direct) => EffectiveTrack::Explicit,
        Some(BranchTrackMode::Inherit) => EffectiveTrack::Inherit,
        None => config_default_track(&config),
    };
    match effective {
        EffectiveTrack::Never => Ok(()),
        EffectiveTrack::Inherit => {
            branch_create_inherit_upstream(git_dir, store, branch, start, quiet)
        }
        EffectiveTrack::Explicit | EffectiveTrack::Always => {
            // --track / autosetupmerge=always: track even a local start-point.
            let upstream = branch_create_direct_upstream(store, start)?;
            set_branch_upstream_quiet(git_dir, store, branch, &upstream, quiet)
        }
        EffectiveTrack::Remote | EffectiveTrack::Simple => {
            // Default / autosetupmerge=simple: only track when the start-point
            // is a remote-tracking branch matched by some remote's fetch
            // refspec. `simple` additionally requires the remote branch name
            // to equal the new branch name.
            let Some(start) = start else { return Ok(()) };
            let resolved = match resolve_remote_tracking_upstream(store, &config, start.as_str())? {
                Some(resolved) => resolved,
                None => return Ok(()),
            };
            if effective == EffectiveTrack::Simple {
                let tracked = resolved.merge.strip_prefix("refs/heads/");
                if tracked != Some(branch) {
                    return Ok(());
                }
            }
            install_tracking_config(git_dir, store, branch, &resolved, quiet)
        }
    }
}

/// Resolve a start-point to a remote-tracking upstream, mirroring git's
/// `setup_tracking` for `BRANCH_TRACK_REMOTE`: only matches when the
/// start-point names a remote-tracking branch covered by some remote's fetch
/// refspec. Returns `None` for local branches (which the default mode must not
/// track).
fn resolve_remote_tracking_upstream(
    store: &FileRefStore,
    config: &GitConfig,
    start: &str,
) -> Result<Option<ResolvedBranchUpstream>> {
    for remote in remote_names(config) {
        let Some((remote_ref, merge)) = branch_upstream_remote_ref(config, &remote, start) else {
            continue;
        };
        if store.read_ref(&remote_ref)?.is_some() {
            let display = remote_ref
                .strip_prefix("refs/remotes/")
                .unwrap_or(remote_ref.as_str())
                .to_string();
            return Ok(Some(ResolvedBranchUpstream {
                remote,
                merge,
                display,
            }));
        }
    }
    Ok(None)
}

/// Resolve `branch.autosetuprebase` (environment.c), returning whether the
/// newly-created branch should get `branch.<name>.rebase = true` given whether
/// its upstream is on a remote (`is_remote`). Errors on a malformed value, like
/// git's `git branch` does.
fn should_setup_rebase(config: &GitConfig, is_remote: bool) -> Result<bool> {
    match validate_autosetuprebase(config)? {
        AutoRebase::Never => Ok(false),
        AutoRebase::Local => Ok(!is_remote),
        AutoRebase::Remote => Ok(is_remote),
        AutoRebase::Always => Ok(true),
    }
}

enum AutoRebase {
    Never,
    Local,
    Remote,
    Always,
}

/// Parse and validate `branch.autosetuprebase`, mirroring git's
/// `git_default_branch_config`. A missing key defaults to `never`; a bare key
/// with no value (`config_error_nonbool`) or an unrecognised value is an error,
/// which makes plain `git branch` fail (t3200 #145/#146).
fn validate_autosetuprebase(config: &GitConfig) -> Result<AutoRebase> {
    match config.get_entry("branch", None, "autosetuprebase") {
        None => Ok(AutoRebase::Never),
        Some(None) => {
            eprintln!("error: missing value for 'branch.autosetuprebase'");
            Err(GitError::Exit(128))
        }
        Some(Some("never")) => Ok(AutoRebase::Never),
        Some(Some("local")) => Ok(AutoRebase::Local),
        Some(Some("remote")) => Ok(AutoRebase::Remote),
        Some(Some("always")) => Ok(AutoRebase::Always),
        Some(Some(other)) => {
            eprintln!("error: malformed value for 'branch.autosetuprebase': {other}");
            Err(GitError::Exit(128))
        }
    }
}

/// Install `branch.<name>.{remote,merge}` (and `.rebase` per autosetuprebase)
/// for a resolved remote-tracking upstream, printing the tracking message
/// unless quiet. Mirrors git's `install_branch_config_multiple_remotes`.
fn install_tracking_config(
    git_dir: &Path,
    _store: &FileRefStore,
    branch: &str,
    resolved: &ResolvedBranchUpstream,
    quiet: bool,
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let rebasing = should_setup_rebase(&config, resolved.remote != ".")?;
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "remote",
        &resolved.remote,
    );
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "merge",
        &resolved.merge,
    );
    if rebasing {
        set_config_value(&mut config, "branch", Some(branch), "rebase", "true");
    }
    write_repo_config(git_dir, &config)?;
    if !quiet {
        if rebasing {
            println!(
                "branch '{branch}' set up to track '{}' by rebasing.",
                resolved.display
            );
        } else {
            println!("branch '{branch}' set up to track '{}'.", resolved.display);
        }
    }
    Ok(())
}

fn branch_create_direct_upstream(store: &FileRefStore, start: Option<&String>) -> Result<String> {
    match start.map(String::as_str) {
        None | Some("HEAD") => Ok(store.current_branch()?.unwrap_or_else(|| "HEAD".into())),
        Some(start) => Ok(start.to_string()),
    }
}

fn set_branch_upstream_quiet(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    upstream: &str,
    quiet: bool,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let Some(upstream) = resolve_branch_upstream(store, &config, upstream)? else {
        eprintln!("fatal: the requested upstream branch '{upstream}' does not exist");
        return Err(GitError::Exit(128));
    };
    if upstream.remote == "." && upstream.merge == branch_ref_name(branch)? {
        eprintln!("warning: not setting branch '{branch}' as its own upstream");
        return Ok(());
    }
    install_tracking_config(git_dir, store, branch, &upstream, quiet)
}

fn branch_create_inherit_upstream(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    quiet: bool,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let source = branch_create_inherit_source(store, start)?;
    let Some(remote) = config
        .get("branch", Some(&source.name), "remote")
        .map(str::to_string)
    else {
        if !quiet {
            eprintln!(
                "warning: asked to inherit tracking from '{}', but no remote is set",
                source.display
            );
        }
        return Ok(());
    };
    let Some(merge) = config
        .get("branch", Some(&source.name), "merge")
        .map(str::to_string)
    else {
        if !quiet {
            eprintln!(
                "warning: asked to inherit tracking from '{}', but no merge configuration is set",
                source.display
            );
        }
        return Ok(());
    };
    let mut config = config;
    set_config_value(&mut config, "branch", Some(branch), "remote", &remote);
    set_config_value(&mut config, "branch", Some(branch), "merge", &merge);
    write_repo_config(git_dir, &config)?;
    if !quiet {
        let display = branch_tracking_display(&config, &remote, &merge);
        println!("branch '{branch}' set up to track '{display}'.");
    }
    Ok(())
}

struct BranchInheritSource {
    name: String,
    display: String,
}

fn branch_create_inherit_source(
    store: &FileRefStore,
    start: Option<&String>,
) -> Result<BranchInheritSource> {
    let start = start.map(String::as_str).unwrap_or("HEAD");
    if start == "HEAD"
        && let Some(branch) = store.current_branch()?
    {
        return Ok(BranchInheritSource {
            name: branch.clone(),
            display: branch,
        });
    }
    if let Some(branch) = start.strip_prefix("refs/heads/") {
        return Ok(BranchInheritSource {
            name: branch.to_string(),
            display: branch.to_string(),
        });
    }
    if start.starts_with("refs/remotes/") {
        return Ok(BranchInheritSource {
            name: start.to_string(),
            display: start.to_string(),
        });
    }
    let remote_ref = format!("refs/remotes/{start}");
    if store.read_ref(&remote_ref)?.is_some() {
        return Ok(BranchInheritSource {
            name: remote_ref.clone(),
            display: remote_ref,
        });
    }
    if store.read_ref(&branch_ref_name(start)?)?.is_some() {
        return Ok(BranchInheritSource {
            name: start.to_string(),
            display: start.to_string(),
        });
    }
    Ok(BranchInheritSource {
        name: start.to_string(),
        display: start.to_string(),
    })
}

fn branch_tracking_display(config: &GitConfig, remote: &str, merge: &str) -> String {
    if remote == "." {
        return merge
            .strip_prefix("refs/heads/")
            .unwrap_or(merge)
            .to_string();
    }
    if let Some(fetch) = config.get("remote", Some(remote), "fetch")
        && let Some(refname) = map_remote_fetch_refspec(fetch, merge)
        && let Some(short) = refname.strip_prefix("refs/remotes/")
    {
        return short.to_string();
    }
    format!(
        "{remote}/{}",
        merge.strip_prefix("refs/heads/").unwrap_or(merge)
    )
}

fn resolve_branch_start(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    start: &str,
) -> Result<ObjectId> {
    match resolve_revision(git_dir, format, start) {
        Ok(oid) => Ok(oid),
        Err(err) => {
            // A trailing range operator with an empty other side (`main..`,
            // `main...`) resolves to the named committish, exactly as git's
            // `get_oid_committish` does (t3200 #9).
            if let Some(base) = start
                .strip_suffix("...")
                .or_else(|| start.strip_suffix(".."))
                && !base.is_empty()
                && !base.contains("..")
                && let Ok(oid) = resolve_revision(git_dir, format, base)
            {
                return Ok(oid);
            }
            let remote_ref = format!("refs/remotes/{start}");
            match store.read_ref(&remote_ref)? {
                Some(RefTarget::Direct(oid)) => Ok(oid),
                _ => {
                    let remote_head = format!("{remote_ref}/HEAD");
                    if let Some(RefTarget::Symbolic(target)) = store.read_ref(&remote_head)?
                        && store.read_ref(&target)?.is_none()
                    {
                        eprintln!("fatal: dangling symref {remote_head}");
                        return Err(GitError::Exit(128));
                    }
                    Err(err)
                }
            }
        }
    }
}

pub(crate) fn create_branch_from_start(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
) -> Result<()> {
    create_branch_from_start_with_reflog(git_dir, format, store, branch, start, false)
}

fn create_branch_from_start_with_reflog(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    create_reflog: bool,
) -> Result<()> {
    let refname = validate_branch_creation_name(branch)?;
    if store.read_ref(&refname)?.is_some() {
        eprintln!("fatal: a branch named '{branch}' already exists");
        return Err(GitError::Exit(128));
    }
    let start_rev = start.map_or("HEAD", String::as_str);
    let start_oid = resolve_branch_start(git_dir, format, store, start_rev)?;
    let message = match start {
        Some(start) => format!("branch: Created from {start}").into_bytes(),
        None => format!(
            "branch: Created from {}",
            store.current_branch()?.unwrap_or_else(|| "HEAD".into())
        )
        .into_bytes(),
    };
    let reflog = if branch_should_write_reflog(git_dir, &refname, create_reflog)? {
        Some(ReflogEntry {
            old_oid: ObjectId::null(format),
            new_oid: start_oid,
            committer: commit_identity_from_env("COMMITTER")?,
            message,
        })
    } else {
        None
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: refname,
        expected: None,
        new: RefTarget::Direct(start_oid),
        reflog,
    });
    tx.commit()?;
    Ok(())
}

fn branch_should_write_reflog(git_dir: &Path, name: &str, create_reflog: bool) -> Result<bool> {
    if create_reflog || branch_reflog_path(git_dir, name)?.exists() {
        return Ok(true);
    }
    if let Some(value) = global_config_value("core.logAllRefUpdates")? {
        return Ok(branch_log_all_ref_updates_matches(name, &value));
    }
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(false);
    };
    if let Some(value) = config.get("core", None, "logAllRefUpdates") {
        return Ok(branch_log_all_ref_updates_matches(name, value));
    }
    if config.get_bool("core", None, "bare").unwrap_or(false) {
        return Ok(false);
    }
    Ok(branch_log_all_ref_updates_matches(name, "true"))
}

fn branch_reflog_path(git_dir: &Path, name: &str) -> Result<PathBuf> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    Ok(common_git_dir.join("logs").join(name))
}

fn branch_log_all_ref_updates_matches(name: &str, value: &str) -> bool {
    if value.eq_ignore_ascii_case("always") {
        return true;
    }
    if !sley_config::parse_config_bool(value).unwrap_or(false) {
        return false;
    }
    name == "HEAD"
        || name.starts_with("refs/heads/")
        || name.starts_with("refs/remotes/")
        || name.starts_with("refs/notes/")
}

fn validate_branch_creation_name(branch: &str) -> Result<String> {
    // git's strbuf_check_branch_ref rejects "HEAD" (and "@") as a branch name
    // even though refs/heads/HEAD passes check_refname_format (t3200 #10).
    if branch == "HEAD" || branch == "@" {
        eprintln!("fatal: '{branch}' is not a valid branch name");
        print_branch_ref_syntax_hint();
        return Err(GitError::Exit(128));
    }
    match branch_ref_name(branch)
        .and_then(|refname| sley_refs::check_refname_format(&refname, false).map(|()| refname))
    {
        Ok(refname) => Ok(refname),
        Err(GitError::InvalidPath(_)) => {
            eprintln!("fatal: '{branch}' is not a valid branch name");
            print_branch_ref_syntax_hint();
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn validate_branch_source_name(branch: &str) -> Result<String> {
    match branch_ref_name(branch) {
        Ok(refname) => Ok(refname),
        Err(GitError::InvalidPath(_)) => {
            eprintln!("fatal: invalid branch name: '{branch}'");
            print_branch_ref_syntax_hint();
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn print_branch_ref_syntax_hint() {
    eprintln!("hint: See 'git help check-ref-format'");
    eprintln!("hint: Disable this message with \"git config set advice.refSyntax false\"");
}

struct BranchDeleteOptions {
    force: bool,
    quiet: bool,
    mode: BranchDeleteMode,
    branches: Vec<String>,
}

#[derive(Clone, Copy)]
enum BranchDeleteMode {
    Local,
    Remote,
    All,
}

fn parse_branch_delete_options(args: &[String]) -> Result<Option<BranchDeleteOptions>> {
    let mut saw_delete_option = false;
    let mut delete = false;
    let mut force = false;
    let mut quiet = false;
    let mut mode = BranchDeleteMode::Local;
    let specs = branch_delete_option_specs();
    let Some(parsed) = parse_branch_options(args, &specs)? else {
        return Ok(None);
    };
    for option in &parsed.options {
        match (option.short, option.long) {
            (Some('d'), _) | (_, Some("delete")) => {
                saw_delete_option = true;
                delete = branch_option_bool(option).unwrap_or(true);
            }
            (Some('D'), _) => {
                saw_delete_option = true;
                delete = true;
                force = true;
            }
            (_, Some("force")) => force = branch_option_bool(option).unwrap_or(true),
            (_, Some("quiet")) => quiet = branch_option_bool(option).unwrap_or(true),
            (Some('r'), _) | (_, Some("remotes")) => mode = BranchDeleteMode::Remote,
            (Some('a'), _) | (_, Some("all")) => mode = BranchDeleteMode::All,
            _ => {}
        }
    }

    Ok(
        (saw_delete_option && delete).then_some(BranchDeleteOptions {
            force,
            quiet,
            mode,
            branches: branch_positionals(&parsed),
        }),
    )
}

#[rustfmt::skip]
fn branch_delete_option_specs() -> [OptionSpec<'static>; 7] {
    [
        branch_bool_option!(Some('d'), Some("delete"), OptFlags::NONE, "delete fully merged branch"),
        branch_bool_option!(Some('D'), None, OptFlags::NONE, "delete branch (even if not merged)"),
        branch_bool_option!(Some('f'), Some("force"), OptFlags::NONE, "force creation, move/rename, deletion"),
        branch_bool_option!(Some('q'), Some("quiet"), OptFlags::NONE, "suppress informational messages"),
        branch_bool_option!(Some('v'), Some("verbose"), OptFlags::NONE, "show hash and subject, give twice for upstream branch"),
        branch_bool_option!(Some('r'), Some("remotes"), OptFlags::NONEG, "act on remote-tracking branches"),
        branch_bool_option!(Some('a'), Some("all"), OptFlags::NONEG, "list both remote-tracking and local branches"),
    ]
}

/// If `name` is a symbolic ref, delete the symref itself (not its target),
/// printing git's `Deleted branch <branch> (was <raw-target>).` message and
/// returning `Ok(Some(()))`. Mirrors builtin/branch.c, which resolves the
/// branch with `RESOLVE_REF_NO_RECURSE`, so the merge check is bypassed and the
/// reported value is the symref's immediate target verbatim (t3200 #81-#83).
fn try_delete_symref_branch(
    store: &FileRefStore,
    name: &str,
    branch: &str,
    quiet: bool,
) -> Result<Option<()>> {
    let Some(RefTarget::Symbolic(target)) = store.read_ref(name)? else {
        return Ok(None);
    };
    store.delete_symbolic_ref(name)?;
    if !quiet {
        println!("Deleted branch {branch} (was {target}).");
    }
    Ok(Some(()))
}

fn branch_checked_out_worktree_path(
    git_dir: &Path,
    store: &FileRefStore,
    refname: &str,
) -> Result<Option<String>> {
    let head_ref = store.current_branch_ref()?;
    let paths = for_each_ref_worktree_paths(git_dir, head_ref.as_deref())?;
    Ok(paths.get(refname).cloned())
}

fn force_delete_branches(
    git_dir: &Path,
    store: &FileRefStore,
    branches: &[String],
    quiet: bool,
) -> Result<()> {
    if branches.is_empty() {
        eprintln!("fatal: branch name required");
        return Err(GitError::Exit(128));
    }
    let mut failed = false;
    for branch in branches {
        let name = format!("refs/heads/{branch}");
        if store.read_ref(&name)?.is_none() {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        }
        if let Some(worktree_root) = branch_checked_out_worktree_path(git_dir, store, &name)? {
            eprintln!(
                "error: cannot delete branch '{branch}' used by worktree at '{}'",
                worktree_root
            );
            failed = true;
            continue;
        }
        if try_delete_symref_branch(store, &name, branch, quiet)?.is_some() {
            continue;
        }
        let deleted = store.delete_branch(branch)?;
        remove_branch_config(git_dir, branch)?;
        if !quiet {
            println!(
                "Deleted branch {branch} (was {}).",
                short_oid(&deleted.oid.to_hex())
            );
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn delete_remote_tracking_branches(
    store: &FileRefStore,
    branches: &[String],
    quiet: bool,
) -> Result<()> {
    if branches.is_empty() {
        eprintln!("fatal: branch name required");
        return Err(GitError::Exit(128));
    }
    let mut failed = false;
    for branch in branches {
        let name = format!("refs/remotes/{branch}");
        let Some(RefTarget::Direct(_)) = store.read_ref(&name)? else {
            eprintln!("error: remote-tracking branch '{branch}' not found");
            failed = true;
            continue;
        };
        let deleted = store.delete_ref(&name)?;
        if !quiet {
            println!(
                "Deleted remote-tracking branch {branch} (was {}).",
                short_oid(&deleted.oid.to_hex())
            );
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn force_update_branch(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
) -> Result<()> {
    let name = validate_branch_creation_name(branch)?;
    if store.current_branch_ref()?.as_deref() == Some(name.as_str()) {
        let worktree_root = worktree_root_for_git_dir(git_dir)?;
        eprintln!(
            "fatal: cannot force update the branch '{branch}' used by worktree at '{}'",
            worktree_root.display()
        );
        return Err(GitError::Exit(128));
    }
    let start = start.map_or("HEAD", String::as_str);
    let new_oid = resolve_branch_start(git_dir, format, store, start)?;
    let old_oid = match store.read_ref(&name)? {
        Some(RefTarget::Direct(oid)) => oid,
        _ => zero_oid(format)?,
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name,
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid,
            committer: commit_identity_from_env("COMMITTER")?,
            message: format!("branch: Reset to {start}").into_bytes(),
        }),
    });
    tx.commit()
}

fn delete_merged_branches(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branches: &[String],
    quiet: bool,
) -> Result<()> {
    if branches.is_empty() {
        eprintln!("fatal: branch name required");
        return Err(GitError::Exit(128));
    }

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir)?;
    let head_reachable = resolve_revision(git_dir, format, "HEAD")
        .ok()
        .and_then(|head| sley_rev::peel_to_commit(&db, format, &head).ok())
        .map(|head| {
            sley_rev::walk_commits(&db, format, [head]).map(|records| {
                records
                    .into_iter()
                    .map(|record| record.oid)
                    .collect::<HashSet<_>>()
            })
        })
        .transpose()?;

    let mut failed = false;
    for branch in branches {
        let name = format!("refs/heads/{branch}");
        let Some(target) = store.read_ref(&name)? else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        if let Some(worktree_root) = branch_checked_out_worktree_path(git_dir, store, &name)? {
            eprintln!(
                "error: cannot delete branch '{branch}' used by worktree at '{}'",
                worktree_root
            );
            failed = true;
            continue;
        }
        // A symbolic-ref branch is deleted without a merge check (git resolves
        // it with RESOLVE_REF_NO_RECURSE); the symref itself is removed.
        if try_delete_symref_branch(store, &name, branch, quiet)?.is_some() {
            continue;
        }
        let RefTarget::Direct(oid) = target else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        let Ok(tip) = sley_rev::peel_to_commit(&db, format, &oid) else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        let reachable = branch_delete_reachable_base(
            store,
            &db,
            format,
            &config,
            &name,
            head_reachable.as_ref(),
        )?;
        if !reachable.is_some_and(|reachable| reachable.contains(&tip)) {
            eprintln!("error: the branch '{branch}' is not fully merged");
            eprintln!("hint: If you are sure you want to delete it, run 'git branch -D {branch}'");
            eprintln!(
                "hint: Disable this message with \"git config set advice.forceDeleteBranch false\""
            );
            failed = true;
            continue;
        }
        let deleted = store.delete_branch(branch)?;
        remove_branch_config(git_dir, branch)?;
        if !quiet {
            println!(
                "Deleted branch {branch} (was {}).",
                short_oid(&deleted.oid.to_hex())
            );
        }
    }

    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn branch_delete_reachable_base<'a>(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    refname: &str,
    head_reachable: Option<&'a HashSet<ObjectId>>,
) -> Result<Option<Cow<'a, HashSet<ObjectId>>>> {
    if let Some(upstream) = for_each_ref_upstream(config, refname)
        && let Some(target) = store.read_ref(&upstream.refname)?
    {
        let upstream_ref = sley_refs::Ref {
            name: upstream.refname,
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(store, &upstream_ref)?
            && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
        {
            let reachable = sley_rev::walk_commits(db, format, [commit])?
                .into_iter()
                .map(|record| record.oid)
                .collect::<HashSet<_>>();
            return Ok(Some(Cow::Owned(reachable)));
        }
    }
    Ok(head_reachable.map(Cow::Borrowed))
}

#[derive(Clone, Copy)]
enum BranchListMode {
    Local,
    Remote,
    All,
}

fn print_branch_list(store: &FileRefStore, mode: BranchListMode) -> Result<()> {
    print_branch_list_filtered(store, mode, |_, _| true)
}

fn print_branch_list_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color(store, mode, false, descending, |_, _| true)
}

fn print_branch_list_version_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_version_sorted(store, mode, &[], false, descending)
}

fn print_branch_list_objectname_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_objectname_sorted(store, mode, &[], false, descending)
}

fn print_branch_list_objecttype_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_objecttype_sorted(
        git_dir,
        format,
        store,
        mode,
        &[],
        false,
        descending,
    )
}

fn print_branch_list_objectsize_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_objectsize_sorted(
        git_dir,
        format,
        store,
        mode,
        &[],
        false,
        descending,
    )
}

fn print_branch_list_date_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    field: ForEachRefDateSortField,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_date_sorted(
        git_dir,
        format,
        store,
        mode,
        &[],
        false,
        (field, descending),
    )
}

fn print_branch_list_upstream_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_upstream_sorted(git_dir, store, mode, &[], false, descending)
}

fn print_branch_list_push_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    descending: bool,
) -> Result<()> {
    print_branch_list_matching_push_sorted(git_dir, store, mode, &[], false, descending)
}

fn run_branch_general_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchGeneralListOptions,
) -> Result<()> {
    let mut refs = branch_sorted_refs(git_dir, format, store, options.sort)?;
    if options.ignore_case {
        match options.sort.unwrap_or(BranchSort::Refname(false)) {
            BranchSort::Refname(descending) => {
                refs.sort_by(|left, right| {
                    let left_key = left.name.to_ascii_lowercase();
                    let right_key = right.name.to_ascii_lowercase();
                    left_key.cmp(&right_key).then_with(|| left.name.cmp(&right.name))
                });
                if descending {
                    refs.reverse();
                }
            }
            _ => {}
        }
    }
    if let Some(style) = options.column {
        let show_detached = options.patterns.is_empty();
        let rows = collect_branch_rows(
            refs,
            store.current_branch_ref()?.as_deref(),
            options.mode,
            false,
            show_detached,
            |_, name| branch_list_patterns_match(&options.patterns, name, options.ignore_case),
        )?;
        return print_branch_columns(&rows, style);
    }
    let show_detached = options.patterns.is_empty();
    let worktree_paths = if options.color {
        Some(for_each_ref_worktree_paths(
            git_dir,
            store.current_branch_ref()?.as_deref(),
        )?)
    } else {
        None
    };
    print_branch_refs(
        refs,
        store.current_branch_ref()?.as_deref(),
        options.mode,
        options.color,
        show_detached,
        worktree_paths.as_ref(),
        |_, name| branch_list_patterns_match(&options.patterns, name, options.ignore_case),
    )
}

fn branch_sorted_refs(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    sort: Option<BranchSort>,
) -> Result<Vec<sley_refs::Ref>> {
    let mut refs = store.list_refs()?;
    match sort.unwrap_or(BranchSort::Refname(false)) {
        BranchSort::Refname(descending) => {
            if descending {
                refs.reverse();
            }
            Ok(refs)
        }
        BranchSort::Version(descending) => {
            refs.sort_by(|left, right| version_sort_cmp(&left.name, &right.name, &[]));
            if descending {
                refs.reverse();
            }
            Ok(refs)
        }
        BranchSort::ObjectName(descending) => {
            refs.sort_by(|left, right| {
                let left_key = branch_ref_objectname_sort_key(left);
                let right_key = branch_ref_objectname_sort_key(right);
                let object_order = if descending {
                    right_key.cmp(&left_key)
                } else {
                    left_key.cmp(&right_key)
                };
                object_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(refs)
        }
        BranchSort::ObjectType(descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_objecttype_sort_key(store, &db, &reference)?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let object_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                object_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
        BranchSort::ObjectSize(descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_objectsize_sort_key(store, &db, &reference)?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let object_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                object_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
        BranchSort::Date(field, descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_date_sort_key(store, &db, format, &reference, field)?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let date_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                date_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
        BranchSort::Upstream(descending) => {
            let config = read_repo_config(git_dir)?;
            refs.sort_by(|left, right| {
                let left_key = branch_ref_upstream_sort_key(&config, left);
                let right_key = branch_ref_upstream_sort_key(&config, right);
                let upstream_order = if descending {
                    right_key.cmp(&left_key)
                } else {
                    left_key.cmp(&right_key)
                };
                upstream_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(refs)
        }
        BranchSort::Push(descending) => {
            let config = read_repo_config(git_dir)?;
            refs.sort_by(|left, right| {
                let left_key = branch_ref_push_sort_key(&config, left);
                let right_key = branch_ref_push_sort_key(&config, right);
                let push_order = if descending {
                    right_key.cmp(&left_key)
                } else {
                    left_key.cmp(&right_key)
                };
                push_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(refs)
        }
        BranchSort::AheadBehind(target, descending) => {
            let db = FileObjectDatabase::from_git_dir(git_dir, format);
            let mut keyed = refs
                .into_iter()
                .map(|reference| {
                    let key = branch_ref_ahead_behind_sort_key(store, &db, format, &reference, &target)?;
                    Ok::<_, GitError>((reference, key))
                })
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|(left, left_key), (right, right_key)| {
                let ahead_order = if descending {
                    right_key.cmp(left_key)
                } else {
                    left_key.cmp(right_key)
                };
                ahead_order.then_with(|| left.name.cmp(&right.name))
            });
            Ok(keyed.into_iter().map(|(reference, _)| reference).collect())
        }
    }
}

fn branch_ref_ahead_behind_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    reference: &sley_refs::Ref,
    target: &ObjectId,
) -> Result<(usize, usize)> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok((0, 0));
    };
    let Some(track) = for_each_ref_ahead_behind(db, format, &oid, target)? else {
        return Ok((0, 0));
    };
    Ok((track.ahead, track.behind))
}

fn print_branch_list_colored(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let worktree_paths = for_each_ref_worktree_paths(git_dir, current.as_deref())?;
    print_branch_refs(
        store.list_refs()?,
        current.as_deref(),
        mode,
        true,
        true,
        Some(&worktree_paths),
        |_, _| true,
    )
}

fn print_branch_list_points_at(
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
) -> Result<()> {
    print_branch_list_points_at_matching(store, mode, oid, &[])
}

fn print_branch_list_points_at_matching(
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
    patterns: &[String],
) -> Result<()> {
    print_branch_list_filtered_detached(store, mode, false, |reference, name| {
        matches!(&reference.target, RefTarget::Direct(target) if target == oid)
            && branch_list_patterns_match(patterns, name, false)
    })
}

fn print_branch_list_contains(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
    contains: bool,
) -> Result<()> {
    if contains {
        print_branch_list_contains_filters(
            git_dir,
            format,
            store,
            mode,
            std::slice::from_ref(oid),
            &[],
        )
    } else {
        print_branch_list_contains_filters(
            git_dir,
            format,
            store,
            mode,
            &[],
            std::slice::from_ref(oid),
        )
    }
}

fn print_branch_list_contains_filters(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    contains_oids: &[ObjectId],
    no_contains_oids: &[ObjectId],
) -> Result<()> {
    print_branch_list_contains_filters_matching(
        git_dir,
        format,
        store,
        mode,
        contains_oids,
        no_contains_oids,
        &[],
    )
}

fn print_branch_list_contains_filters_matching(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    contains_oids: &[ObjectId],
    no_contains_oids: &[ObjectId],
    patterns: &[String],
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let contains_targets = contains_oids
        .iter()
        .map(|oid| sley_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let no_contains_targets = no_contains_oids
        .iter()
        .map(|oid| sley_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let mut included = HashSet::new();
    for reference in store.list_refs()? {
        if !branch_ref_matches_mode(&reference.name, mode) {
            continue;
        }
        let RefTarget::Direct(tip) = &reference.target else {
            continue;
        };
        let Ok(tip) = sley_rev::peel_to_commit(&db, format, tip) else {
            continue;
        };
        let reachable = sley_rev::walk_commits(&db, format, [tip])?
            .into_iter()
            .map(|record| record.oid)
            .collect::<HashSet<_>>();
        let contains_match = contains_targets.is_empty()
            || contains_targets
                .iter()
                .any(|target| reachable.contains(target));
        let no_contains_match = no_contains_targets
            .iter()
            .any(|target| reachable.contains(target));
        if contains_match && !no_contains_match {
            included.insert(reference.name.clone());
        }
    }
    print_branch_list_filtered(store, mode, |reference, name| {
        included.contains(&reference.name) && branch_list_patterns_match(patterns, name, false)
    })
}

fn print_branch_list_merged(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    oid: &ObjectId,
    merged: bool,
) -> Result<()> {
    if merged {
        print_branch_list_merged_filters(
            git_dir,
            format,
            store,
            mode,
            std::slice::from_ref(oid),
            &[],
        )
    } else {
        print_branch_list_merged_filters(
            git_dir,
            format,
            store,
            mode,
            &[],
            std::slice::from_ref(oid),
        )
    }
}

fn print_branch_list_merged_filters(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    merged_oids: &[ObjectId],
    no_merged_oids: &[ObjectId],
) -> Result<()> {
    print_branch_list_merged_filters_matching(
        git_dir,
        format,
        store,
        mode,
        merged_oids,
        no_merged_oids,
        &[],
    )
}

fn print_branch_list_merged_filters_matching(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    merged_oids: &[ObjectId],
    no_merged_oids: &[ObjectId],
    patterns: &[String],
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let merged_reachable = merged_oids
        .iter()
        .map(|oid| {
            let target = sley_rev::peel_to_commit(&db, format, oid)?;
            sley_rev::walk_commits(&db, format, [target]).map(|records| {
                records
                    .into_iter()
                    .map(|record| record.oid)
                    .collect::<HashSet<_>>()
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let no_merged_reachable = no_merged_oids
        .iter()
        .map(|oid| {
            let target = sley_rev::peel_to_commit(&db, format, oid)?;
            sley_rev::walk_commits(&db, format, [target]).map(|records| {
                records
                    .into_iter()
                    .map(|record| record.oid)
                    .collect::<HashSet<_>>()
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut included = HashSet::new();
    for reference in store.list_refs()? {
        if !branch_ref_matches_mode(&reference.name, mode) {
            continue;
        }
        let RefTarget::Direct(tip) = &reference.target else {
            continue;
        };
        let Ok(tip) = sley_rev::peel_to_commit(&db, format, tip) else {
            continue;
        };
        let merged_match =
            merged_reachable.is_empty() || merged_reachable.iter().any(|set| set.contains(&tip));
        let no_merged_match = no_merged_reachable.iter().any(|set| set.contains(&tip));
        if merged_match && !no_merged_match {
            included.insert(reference.name.clone());
        }
    }
    print_branch_list_filtered(store, mode, |reference, name| {
        included.contains(&reference.name) && branch_list_patterns_match(patterns, name, false)
    })
}

fn branch_ref_matches_mode(name: &str, mode: BranchListMode) -> bool {
    match mode {
        BranchListMode::Local => name.starts_with("refs/heads/"),
        BranchListMode::Remote => name.starts_with("refs/remotes/"),
        BranchListMode::All => name.starts_with("refs/heads/") || name.starts_with("refs/remotes/"),
    }
}

fn print_branch_list_matching(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
) -> Result<()> {
    print_branch_list_filtered(store, mode, |_, name| {
        branch_list_patterns_match(patterns, name, ignore_case)
    })
}

fn print_branch_list_matching_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color(store, mode, false, descending, |_, name| {
        branch_list_patterns_match(patterns, name, ignore_case)
    })
}

fn print_branch_list_matching_version_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_version_sorted_with_color(
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

fn print_branch_list_matching_objectname_sorted(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_objectname_sorted_with_color(
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

fn print_branch_list_matching_objecttype_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_objecttype_sorted_with_color(
        git_dir,
        format,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

fn print_branch_list_matching_objectsize_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_objectsize_sorted_with_color(
        git_dir,
        format,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

fn print_branch_list_matching_date_sorted(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    sort: (ForEachRefDateSortField, bool),
) -> Result<()> {
    print_branch_list_filtered_date_sorted_with_color(
        git_dir,
        format,
        store,
        mode,
        false,
        sort,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

fn print_branch_list_matching_upstream_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_upstream_sorted_with_color(
        git_dir,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

fn print_branch_list_matching_push_sorted(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    descending: bool,
) -> Result<()> {
    print_branch_list_filtered_push_sorted_with_color(
        git_dir,
        store,
        mode,
        false,
        descending,
        |_, name| branch_list_patterns_match(patterns, name, ignore_case),
    )
}

fn branch_list_patterns_match(patterns: &[String], name: &str, ignore_case: bool) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            if ignore_case {
                refname_pattern_matches_case(pattern, name, true)
            } else {
                refname_pattern_matches(pattern, name)
            }
        })
}

fn print_branch_list_matching_colored(
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
) -> Result<()> {
    print_branch_list_filtered_with_color(store, mode, true, |_, name| {
        patterns.is_empty()
            || patterns
                .iter()
                .any(|pattern| refname_pattern_matches(pattern, name))
    })
}

fn branch_color_always_flag(value: &str) -> bool {
    value == "--color" || value == "--color=always"
}

fn branch_color_noop_flag(value: &str) -> bool {
    matches!(value, "--no-color" | "--color=auto" | "--color=never")
}

fn branch_ignore_case_flag(value: &str) -> bool {
    branch_ignore_case_enabled_flag(value) || value == "--no-ignore-case"
}

fn branch_ignore_case_enabled_flag(value: &str) -> bool {
    matches!(value, "-i" | "--ignore-case")
}

fn branch_omit_empty_value(value: &str) -> Option<bool> {
    match value {
        "--omit-empty" => Some(true),
        "--no-omit-empty" => Some(false),
        _ => None,
    }
}

fn branch_list_noop_display_flag(value: &str) -> bool {
    branch_color_noop_flag(value)
        || branch_column_noop_flag(value)
        || matches!(
            value,
            "--abbrev"
                | "--no-abbrev"
                | "--sort=refname"
                | "--no-sort"
                | "--no-delete"
                | "--no-list"
                | "--no-show-current"
                | "--no-points-at"
                | "--omit-empty"
                | "--no-omit-empty"
                | "--no-format"
        )
        || value.starts_with("--abbrev=")
}

fn branch_remote_or_all_mode(value: &str) -> Option<BranchListMode> {
    match value {
        "-r" | "--remotes" => Some(BranchListMode::Remote),
        "-a" | "--all" => Some(BranchListMode::All),
        _ => None,
    }
}

fn branch_column_noop_flag(value: &str) -> bool {
    matches!(
        value,
        "--no-column" | "--column=auto" | "--column=never" | "--column=plain"
    )
}

fn branch_abbrev_noop_flag(value: &str) -> bool {
    matches!(value, "--abbrev" | "--no-abbrev") || value.starts_with("--abbrev=")
}

fn branch_version_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=version:refname" | "--sort=v:refname" | "version:refname" | "v:refname" => {
            Some(false)
        }
        "--sort=-version:refname" | "--sort=-v:refname" | "-version:refname" | "-v:refname" => {
            Some(true)
        }
        _ => None,
    }
}

fn branch_objectname_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=objectname" | "objectname" => Some(false),
        "--sort=-objectname" | "-objectname" => Some(true),
        _ => None,
    }
}

fn branch_objecttype_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=objecttype" | "--sort=type" | "objecttype" | "type" => Some(false),
        "--sort=-objecttype" | "--sort=-type" | "-objecttype" | "-type" => Some(true),
        _ => None,
    }
}

fn branch_objectsize_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=objectsize" | "objectsize" => Some(false),
        "--sort=-objectsize" | "-objectsize" => Some(true),
        _ => None,
    }
}

fn branch_date_sort_value(value: &str) -> Option<(ForEachRefDateSortField, bool)> {
    match value {
        "--sort=authordate" | "authordate" => Some((ForEachRefDateSortField::Author, false)),
        "--sort=-authordate" | "-authordate" => Some((ForEachRefDateSortField::Author, true)),
        "--sort=committerdate" | "committerdate" => {
            Some((ForEachRefDateSortField::Committer, false))
        }
        "--sort=-committerdate" | "-committerdate" => {
            Some((ForEachRefDateSortField::Committer, true))
        }
        "--sort=creatordate" | "creatordate" => Some((ForEachRefDateSortField::Creator, false)),
        "--sort=-creatordate" | "-creatordate" => Some((ForEachRefDateSortField::Creator, true)),
        _ => None,
    }
}

fn branch_upstream_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=upstream" | "upstream" => Some(false),
        "--sort=-upstream" | "-upstream" => Some(true),
        _ => None,
    }
}

fn branch_push_sort_value(value: &str) -> Option<bool> {
    match value {
        "--sort=push" | "push" => Some(false),
        "--sort=-push" | "-push" => Some(true),
        _ => None,
    }
}

fn branch_ahead_behind_sort_value(value: &str) -> Option<(&str, bool)> {
    value
        .strip_prefix("ahead-behind:")
        .map(|rev| (rev, false))
        .or_else(|| value.strip_prefix("-ahead-behind:").map(|rev| (rev, true)))
}

fn branch_non_refname_sort_value(value: &str) -> bool {
    branch_version_sort_value(value).is_some()
        || branch_objectname_sort_value(value).is_some()
        || branch_objecttype_sort_value(value).is_some()
        || branch_objectsize_sort_value(value).is_some()
        || branch_date_sort_value(value).is_some()
        || branch_upstream_sort_value(value).is_some()
        || branch_push_sort_value(value).is_some()
}

fn branch_contains_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--contains=")
}

fn branch_no_contains_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--no-contains=")
}

fn branch_merged_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--merged=")
}

fn branch_no_merged_eq_value(value: &str) -> Option<&str> {
    value.strip_prefix("--no-merged=")
}

fn print_branch_list_format(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    patterns: &[String],
    ignore_case: bool,
    format_spec: &str,
) -> Result<()> {
    print_branch_list_format_omit_empty(
        git_dir,
        format,
        store,
        BranchFormatPrintOptions {
            mode,
            patterns,
            ignore_case,
            format_spec,
            omit_empty: false,
        },
    )
}

struct BranchFormatPrintOptions<'a> {
    mode: BranchListMode,
    patterns: &'a [String],
    ignore_case: bool,
    format_spec: &'a str,
    omit_empty: bool,
}

fn print_branch_list_format_omit_empty(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchFormatPrintOptions<'_>,
) -> Result<()> {
    print_branch_list_format_omit_empty_with_sort_color(
        git_dir,
        format,
        store,
        options,
        None,
        false,
    )
}

fn run_branch_format_list_options(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchFormatListOptions,
) -> Result<()> {
    print_branch_list_format_omit_empty_with_sort_color(
        git_dir,
        format,
        store,
        BranchFormatPrintOptions {
            mode: options.mode,
            patterns: &options.patterns,
            ignore_case: options.ignore_case,
            format_spec: &options.format_spec,
            omit_empty: options.omit_empty,
        },
        options.sort,
        options.color,
    )
}

fn print_branch_list_format_omit_empty_with_sort_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchFormatPrintOptions<'_>,
    sort: Option<BranchSort>,
    color: bool,
) -> Result<()> {
    let format_spec = ForEachRefFormat::parse(options.format_spec)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir)?;
    let head_ref = store.current_branch_ref()?;
    let objectname_abbrev = repository_abbrev(git_dir, format)?;
    let objectname_candidates = cat_file_all_object_ids(git_dir, format)?;
    let deltabase = zero_oid(format)?;
    let mailmap = commands::utility::Mailmap::load_default(git_dir, format)?;
    let all_refs = branch_sorted_refs(git_dir, format, store, sort)?;
    let ref_names: std::collections::HashSet<String> = all_refs
        .iter()
        .map(|reference| reference.name.clone())
        .collect();
    let warn_ambiguous_refs = config
        .get_bool("core", None, "warnambiguousrefs")
        .unwrap_or(true);
    let mut stdout = io::stdout().lock();
    if matches!(options.mode, BranchListMode::Local | BranchListMode::All)
        && head_ref.is_none()
        && options.patterns.is_empty()
        && let Some(refname) = detached_head_branch_line()
        && let Some((oid, _)) = resolve_for_each_ref_target(
            store,
            &sley_refs::Ref {
                name: "HEAD".into(),
                target: store.read_ref("HEAD")?.unwrap_or(RefTarget::Direct(zero_oid(format)?)),
            },
        )?
    {
        print_branch_format_reference(
            &mut stdout,
            &format_spec,
            git_dir,
            format,
            store,
            &db,
            &config,
            &refname,
            oid,
            None,
            true,
            None,
            &deltabase,
            objectname_abbrev,
            &objectname_candidates,
            &mailmap,
            &ref_names,
            warn_ambiguous_refs,
            color,
            options.omit_empty,
        )?;
    }
    for reference in all_refs.iter() {
        if !branch_ref_matches_mode(&reference.name, options.mode) {
            continue;
        }
        let Some(name) = branch_pattern_name(&reference.name, options.mode) else {
            continue;
        };
        if !options.patterns.is_empty()
            && !options.patterns.iter().any(|pattern| {
                if options.ignore_case {
                    refname_pattern_matches_case(pattern, &name, true)
                } else {
                    refname_pattern_matches(pattern, &name)
                }
            })
        {
            continue;
        }
        let Some((oid, symref)) = resolve_for_each_ref_target(store, reference)? else {
            continue;
        };
        let worktree_path =
            for_each_ref_worktree_path(git_dir, head_ref.as_deref(), &reference.name)?;
        print_branch_format_reference(
            &mut stdout,
            &format_spec,
            git_dir,
            format,
            store,
            &db,
            &config,
            &reference.name,
            oid,
            symref,
            head_ref.as_deref() == Some(reference.name.as_str()),
            worktree_path.as_deref(),
            &deltabase,
            objectname_abbrev,
            &objectname_candidates,
            &mailmap,
            &ref_names,
            warn_ambiguous_refs,
            color,
            options.omit_empty,
        )?;
    }
    stdout.flush()?;
    Ok(())
}

fn print_branch_format_reference(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    db: &FileObjectDatabase,
    config: &GitConfig,
    refname: &str,
    oid: ObjectId,
    symref: Option<String>,
    is_head: bool,
    worktree_path: Option<&str>,
    deltabase: &ObjectId,
    objectname_abbrev: Option<usize>,
    objectname_candidates: &[ObjectId],
    mailmap: &commands::utility::Mailmap,
    ref_names: &std::collections::HashSet<String>,
    warn_ambiguous_refs: bool,
    color: bool,
    omit_empty: bool,
) -> Result<()> {
    let upstream = for_each_ref_upstream(config, refname);
    let push = for_each_ref_push(config, refname);
    let upstream_track = upstream
        .as_ref()
        .map(|upstream| for_each_ref_upstream_track(store, db, format, &oid, &upstream.refname))
        .transpose()?
        .flatten();
    let push_track = push
        .as_ref()
        .and_then(|push| push.refname.as_deref())
        .map(|push_ref| for_each_ref_upstream_track(store, db, format, &oid, push_ref))
        .transpose()?
        .flatten();
    let object = db.read_object(&oid)?;
    let object_disk_size = for_each_ref_loose_object_disk_size(git_dir, &oid)?;
    let contents = for_each_ref_contents(format, &object)?;
    let context = ForEachRefFormatContext {
        git_dir,
        db,
        format,
        refname,
        oid: &oid,
        deltabase,
        object_type: object.object_type,
        object_body: &object.body,
        object_size: object.body.len(),
        object_disk_size,
        color,
        quote: ForEachRefQuoteMode::None,
        objectname_abbrev,
        objectname_candidates,
        worktree_path,
        is_head,
        symref: symref.as_deref(),
        upstream,
        push,
        upstream_track,
        push_track,
        contents,
        peeled_object: None,
        mailmap,
        ref_names,
        warn_ambiguous_refs,
    };
    let mut line = Vec::new();
    print_for_each_ref_format(&mut line, format_spec, &context)?;
    if omit_empty && line.is_empty() {
        return Ok(());
    }
    stdout.write_all(&line)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn branch_pattern_name(name: &str, mode: BranchListMode) -> Option<String> {
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/heads/")
    {
        return Some(name.to_string());
    }
    if matches!(mode, BranchListMode::Remote | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/remotes/")
    {
        return Some(name.to_string());
    }
    None
}

fn print_branch_list_filtered(
    store: &FileRefStore,
    mode: BranchListMode,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_with_color(store, mode, false, |reference, name| {
        include(reference, name)
    })
}

fn print_branch_list_filtered_detached(
    store: &FileRefStore,
    mode: BranchListMode,
    show_detached: bool,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color_detached(
        store,
        mode,
        false,
        false,
        show_detached,
        |reference, name| include(reference, name),
    )
}

fn print_branch_list_filtered_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color(store, mode, color, false, include)
}

fn print_branch_list_filtered_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color_detached(
        store,
        mode,
        color,
        descending,
        true,
        include,
    )
}

fn print_branch_list_filtered_sorted_with_color_detached(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    show_detached: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = store.list_refs()?;
    if descending {
        refs.reverse();
    }
    print_branch_refs(refs, current.as_deref(), mode, color, show_detached, None, include)
}

fn print_branch_list_filtered_version_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = store.list_refs()?;
    refs.sort_by(|left, right| version_sort_cmp(&left.name, &right.name, &[]));
    if descending {
        refs.reverse();
    }
    print_branch_refs(refs, current.as_deref(), mode, color, true, None, include)
}

fn print_branch_list_filtered_objectname_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = store.list_refs()?;
    refs.sort_by(|left, right| {
        let left_key = branch_ref_objectname_sort_key(left);
        let right_key = branch_ref_objectname_sort_key(right);
        let object_order = if descending {
            right_key.cmp(&left_key)
        } else {
            left_key.cmp(&right_key)
        };
        object_order.then_with(|| left.name.cmp(&right.name))
    });
    print_branch_refs(refs, current.as_deref(), mode, color, true, None, include)
}

fn branch_ref_objectname_sort_key(reference: &sley_refs::Ref) -> String {
    match &reference.target {
        RefTarget::Direct(oid) => oid.to_hex(),
        RefTarget::Symbolic(target) => target.clone(),
    }
}

fn print_branch_list_filtered_objecttype_sorted_with_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut keyed = Vec::new();
    for reference in store.list_refs()? {
        let key = branch_ref_objecttype_sort_key(store, &db, &reference)?;
        keyed.push((reference, key));
    }
    keyed.sort_by(|(left, left_key), (right, right_key)| {
        let object_order = if descending {
            right_key.cmp(left_key)
        } else {
            left_key.cmp(right_key)
        };
        object_order.then_with(|| left.name.cmp(&right.name))
    });
    let refs = keyed
        .into_iter()
        .map(|(reference, _)| reference)
        .collect::<Vec<_>>();
    print_branch_refs(refs, current.as_deref(), mode, color, true, None, include)
}

fn branch_ref_objecttype_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    reference: &sley_refs::Ref,
) -> Result<String> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(String::new());
    };
    Ok(db.read_object(&oid)?.object_type.as_str().to_string())
}

fn print_branch_list_filtered_objectsize_sorted_with_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut keyed = Vec::new();
    for reference in store.list_refs()? {
        let key = branch_ref_objectsize_sort_key(store, &db, &reference)?;
        keyed.push((reference, key));
    }
    keyed.sort_by(|(left, left_key), (right, right_key)| {
        let object_order = if descending {
            right_key.cmp(left_key)
        } else {
            left_key.cmp(right_key)
        };
        object_order.then_with(|| left.name.cmp(&right.name))
    });
    let refs = keyed
        .into_iter()
        .map(|(reference, _)| reference)
        .collect::<Vec<_>>();
    print_branch_refs(refs, current.as_deref(), mode, color, true, None, include)
}

fn branch_ref_objectsize_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    reference: &sley_refs::Ref,
) -> Result<usize> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(0);
    };
    Ok(db.read_object(&oid)?.body.len())
}

fn print_branch_list_filtered_date_sorted_with_color(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    sort: (ForEachRefDateSortField, bool),
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let (field, descending) = sort;
    let mut keyed = Vec::new();
    for reference in store.list_refs()? {
        let key = branch_ref_date_sort_key(store, &db, format, &reference, field)?;
        keyed.push((reference, key));
    }
    keyed.sort_by(|(left, left_key), (right, right_key)| {
        let date_order = if descending {
            right_key.cmp(left_key)
        } else {
            left_key.cmp(right_key)
        };
        date_order.then_with(|| left.name.cmp(&right.name))
    });
    let refs = keyed
        .into_iter()
        .map(|(reference, _)| reference)
        .collect::<Vec<_>>();
    print_branch_refs(refs, current.as_deref(), mode, color, true, None, include)
}

fn branch_ref_date_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    reference: &sley_refs::Ref,
    field: ForEachRefDateSortField,
) -> Result<i128> {
    let Some((oid, _)) = resolve_for_each_ref_target(store, reference)? else {
        return Ok(0);
    };
    let object = db.read_object(&oid)?;
    let contents = for_each_ref_contents(format, &object)?;
    Ok(for_each_ref_sort_date_key(contents, field))
}

fn print_branch_list_filtered_upstream_sorted_with_color(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let config = read_repo_config(git_dir)?;
    let mut refs = store.list_refs()?;
    refs.sort_by(|left, right| {
        let left_key = branch_ref_upstream_sort_key(&config, left);
        let right_key = branch_ref_upstream_sort_key(&config, right);
        let upstream_order = if descending {
            right_key.cmp(&left_key)
        } else {
            left_key.cmp(&right_key)
        };
        upstream_order.then_with(|| left.name.cmp(&right.name))
    });
    print_branch_refs(refs, current.as_deref(), mode, color, true, None, include)
}

fn branch_ref_upstream_sort_key(config: &GitConfig, reference: &sley_refs::Ref) -> String {
    for_each_ref_upstream(config, &reference.name)
        .map(|upstream| upstream.refname)
        .unwrap_or_default()
}

fn print_branch_list_filtered_push_sorted_with_color(
    git_dir: &Path,
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let config = read_repo_config(git_dir)?;
    let mut refs = store.list_refs()?;
    refs.sort_by(|left, right| {
        let left_key = branch_ref_push_sort_key(&config, left);
        let right_key = branch_ref_push_sort_key(&config, right);
        let push_order = if descending {
            right_key.cmp(&left_key)
        } else {
            left_key.cmp(&right_key)
        };
        push_order.then_with(|| left.name.cmp(&right.name))
    });
    print_branch_refs(refs, current.as_deref(), mode, color, true, None, include)
}

fn branch_ref_push_sort_key(config: &GitConfig, reference: &sley_refs::Ref) -> String {
    for_each_ref_push(config, &reference.name)
        .and_then(|push| push.refname)
        .unwrap_or_default()
}

/// The `* (no branch, ...)` / `* (HEAD detached at ...)` first line `git
/// branch` prints when HEAD is detached, with the in-progress-operation
/// variants (bisect / rebase) taking precedence -- mirroring upstream
/// `wt_status_get_state` + `get_head_description`.
fn detached_head_branch_line() -> Option<String> {
    let git_dir = discover_git_dir(env::current_dir().ok()?).ok()?;
    let format = repository_object_format(&git_dir).ok()?;
    let store = FileRefStore::new(&git_dir, format);
    let RefTarget::Direct(oid) = store.read_ref("HEAD").ok()?? else {
        return None;
    };
    if let Ok(start) = fs::read_to_string(git_dir.join("BISECT_START")) {
        let start = start.trim();
        if !start.is_empty() {
            return Some(format!("(no branch, bisect started on {start})"));
        }
    }
    for dir in ["rebase-merge", "rebase-apply"] {
        if let Ok(head_name) = fs::read_to_string(git_dir.join(dir).join("head-name")) {
            let branch = head_name
                .trim()
                .strip_prefix("refs/heads/")
                .unwrap_or(head_name.trim())
                .to_string();
            return Some(format!("(no branch, rebasing {branch})"));
        }
    }
    Some(detached_head_description(&store).unwrap_or_else(|| {
        format!("(HEAD detached at {})", format_log_abbrev_oid(&oid))
    }))
}

fn detached_head_description(store: &FileRefStore) -> Option<String> {
    let entries = store.read_reflog("HEAD").ok()?;
    let (idx, checkout) = entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, entry)| {
            let message = std::str::from_utf8(&entry.message).ok()?;
            let destination = message
                .strip_prefix("checkout: moving from ")?
                .rsplit_once(" to ")?
                .1;
            Some((idx, (entry, destination)))
        })?;
    let label = detached_checkout_label(checkout.1, &checkout.0.new_oid);
    let moved_after_checkout = entries[idx + 1..]
        .iter()
        .any(|entry| entry.old_oid != entry.new_oid);
    if moved_after_checkout {
        Some(format!("(HEAD detached from {label})"))
    } else {
        Some(format!("(HEAD detached at {label})"))
    }
}

fn detached_checkout_label(destination: &str, oid: &ObjectId) -> String {
    if destination == "HEAD"
        || destination.starts_with("HEAD^")
        || destination.starts_with("HEAD~")
        || destination == oid.to_hex()
        || oid.to_hex().starts_with(destination)
    {
        format_log_abbrev_oid(oid)
    } else {
        destination.to_string()
    }
}

fn print_branch_refs(
    refs: Vec<sley_refs::Ref>,
    current: Option<&str>,
    mode: BranchListMode,
    color: bool,
    show_detached: bool,
    worktree_paths: Option<&HashMap<String, String>>,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<()> {
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && current.is_none()
        && show_detached
        && let Some(line) = detached_head_branch_line()
    {
        if color {
            println!("* \x1b[32m{line}\x1b[m");
        } else {
            println!("* {line}");
        }
    }
    let ref_names = refs
        .iter()
        .map(|reference| reference.name.clone())
        .collect::<HashSet<_>>();
    for reference in refs {
        if matches!(mode, BranchListMode::Local | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/heads/")
        {
            if !include(&reference, name) {
                continue;
            }
            let linked_worktree = worktree_paths
                .and_then(|paths| paths.get(&reference.name))
                .is_some();
            let marker = if Some(reference.name.as_str()) == current {
                '*'
            } else if linked_worktree {
                '+'
            } else {
                ' '
            };
            let target = local_symbolic_branch_target(&reference);
            if color && marker == '*' {
                print!("{marker} \x1b[32m{name}\x1b[m");
                if let Some(target) = target {
                    print!(" -> {target}");
                }
                println!();
            } else if color && marker == '+' {
                print!("{marker} \x1b[36m{name}\x1b[m");
                if let Some(target) = target {
                    print!(" -> {target}");
                }
                println!();
            } else if color {
                print!("{marker} {name}\x1b[m");
                if let Some(target) = target {
                    print!(" -> {target}");
                }
                println!();
            } else if let Some(target) = target {
                println!("{marker} {name} -> {target}");
            } else {
                println!("{marker} {name}");
            }
            continue;
        }
        if matches!(mode, BranchListMode::Remote | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/remotes/")
        {
            if remote_symbolic_ref_is_dangling(&reference, &ref_names) {
                continue;
            }
            let display = remote_branch_display(&reference, name, mode);
            if !include(&reference, name) {
                continue;
            }
            if color {
                println!("  \x1b[31m{display}\x1b[m");
            } else {
                println!("  {display}");
            }
        }
    }
    Ok(())
}

fn collect_branch_rows(
    refs: Vec<sley_refs::Ref>,
    current: Option<&str>,
    mode: BranchListMode,
    color: bool,
    show_detached: bool,
    mut include: impl FnMut(&sley_refs::Ref, &str) -> bool,
) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && current.is_none()
        && show_detached
        && let Some(line) = detached_head_branch_line()
    {
        if color {
            rows.push(format!("* \x1b[32m{line}\x1b[m"));
        } else {
            rows.push(format!("* {line}"));
        }
    }
    let ref_names = refs
        .iter()
        .map(|reference| reference.name.clone())
        .collect::<HashSet<_>>();
    for reference in refs {
        if matches!(mode, BranchListMode::Local | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/heads/")
        {
            if !include(&reference, name) {
                continue;
            }
            let marker = if Some(reference.name.as_str()) == current {
                '*'
            } else {
                ' '
            };
            let target = local_symbolic_branch_target(&reference);
            if color && marker == '*' {
                let mut row = format!("{marker} \x1b[32m{name}\x1b[m");
                if let Some(target) = target {
                    row.push_str(&format!(" -> {target}"));
                }
                rows.push(row);
            } else if color {
                let mut row = format!("{marker} {name}\x1b[m");
                if let Some(target) = target {
                    row.push_str(&format!(" -> {target}"));
                }
                rows.push(row);
            } else if let Some(target) = target {
                rows.push(format!("{marker} {name} -> {target}"));
            } else {
                rows.push(format!("{marker} {name}"));
            }
            continue;
        }
        if matches!(mode, BranchListMode::Remote | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/remotes/")
        {
            if remote_symbolic_ref_is_dangling(&reference, &ref_names) {
                continue;
            }
            let display = remote_branch_display(&reference, name, mode);
            if !include(&reference, name) {
                continue;
            }
            if color {
                rows.push(format!("  \x1b[31m{display}\x1b[m"));
            } else {
                rows.push(format!("  {display}"));
            }
        }
    }
    Ok(rows)
}

fn local_symbolic_branch_target(reference: &sley_refs::Ref) -> Option<String> {
    let RefTarget::Symbolic(target) = &reference.target else {
        return None;
    };
    target
        .strip_prefix("refs/heads/")
        .or_else(|| target.strip_prefix("refs/remotes/"))
        .map(str::to_string)
}

fn remote_symbolic_ref_is_dangling(
    reference: &sley_refs::Ref,
    ref_names: &HashSet<String>,
) -> bool {
    match &reference.target {
        RefTarget::Symbolic(target) => !ref_names.contains(target.as_str()),
        RefTarget::Direct(_) => false,
    }
}

fn remote_branch_display(reference: &sley_refs::Ref, name: &str, mode: BranchListMode) -> String {
    let display = if matches!(mode, BranchListMode::All) {
        format!("remotes/{name}")
    } else {
        name.to_string()
    };
    let RefTarget::Symbolic(target) = &reference.target else {
        return display;
    };
    let Some(target_name) = target.strip_prefix("refs/remotes/") else {
        return display;
    };
    format!("{display} -> {target_name}")
}

fn print_branch_columns(rows: &[String], style: BranchColumnStyle) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let width = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width > 0)
        .unwrap_or(80);
    let max_len = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    if max_len.saturating_mul(2) >= width {
        for row in rows {
            println!("{row}");
        }
        return Ok(());
    }
    let cell_width = max_len + 1;
    let requested_cols = (width / cell_width).max(1).min(rows.len());
    let row_count = rows.len().div_ceil(requested_cols);
    let col_count = rows.len().div_ceil(row_count);
    let mut col_widths = vec![cell_width; col_count];
    if style == BranchColumnStyle::Dense {
        for col in 0..col_count {
            let mut col_len = 0usize;
            for row in 0..row_count {
                let idx = col * row_count + row;
                if let Some(value) = rows.get(idx) {
                    col_len = col_len.max(value.len());
                }
            }
            col_widths[col] = col_len + 1;
        }
    }
    for row in 0..row_count {
        let mut line = String::new();
        for (col, width) in col_widths.iter().enumerate() {
            let idx = col * row_count + row;
            let Some(value) = rows.get(idx) else {
                continue;
            };
            if col + 1 == col_count {
                line.push_str(value);
            } else {
                line.push_str(&format!("{value:<width$}"));
            }
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

fn print_branch_list_verbose(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchVerboseListOptions,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir)?;
    let current = store.current_branch_ref()?;
    let objectname_abbrev = options
        .abbrev
        .map(|abbrev| abbrev.map(|width| width.min(format.hex_len())))
        .unwrap_or(repository_abbrev(git_dir, format)?);
    let objectname_candidates = cat_file_all_object_ids(git_dir, format)?;
    let worktree_paths = for_each_ref_worktree_paths(git_dir, current.as_deref())?;
    let mut rows = Vec::new();
    if matches!(options.mode, BranchListMode::Local | BranchListMode::All)
        && current.is_none()
        && options.patterns.is_empty()
        && let Some(display) = detached_head_branch_line()
        && let Some((oid, _)) = resolve_for_each_ref_target(
            store,
            &sley_refs::Ref {
                name: "HEAD".into(),
                target: store.read_ref("HEAD")?.unwrap_or(RefTarget::Direct(zero_oid(format)?)),
            },
        )?
    {
        rows.push(BranchVerboseRow {
            display,
            oid: for_each_ref_abbrev_oid(&oid, objectname_abbrev, &objectname_candidates),
            subject: branch_verbose_subject(&db, format, &oid)?,
            is_head: true,
            worktree_path: None,
            upstream: None,
            upstream_track: None,
        });
    }
    for reference in store.list_refs()? {
        let Some((display, pattern_name)) =
            branch_verbose_display_name(&reference.name, options.mode)
        else {
            continue;
        };
        if !branch_list_patterns_match(&options.patterns, &pattern_name, options.ignore_case) {
            continue;
        }
        let Some((oid, _)) = resolve_for_each_ref_target(store, &reference)? else {
            continue;
        };
        let subject = branch_verbose_subject(&db, format, &oid)?;
        let upstream = for_each_ref_upstream(&config, &reference.name);
        let upstream_track = upstream
            .as_ref()
            .map(|upstream| for_each_ref_upstream_track(store, &db, format, &oid, &upstream.refname))
            .transpose()?
            .flatten();
        rows.push(BranchVerboseRow {
            display,
            oid: for_each_ref_abbrev_oid(&oid, objectname_abbrev, &objectname_candidates),
            subject,
            is_head: current.as_deref() == Some(reference.name.as_str()),
            worktree_path: worktree_paths.get(&reference.name).cloned(),
            upstream,
            upstream_track,
        });
    }
    let width = rows.iter().map(|row| row.display.len()).max().unwrap_or(0);
    for row in rows {
        let marker = if row.is_head {
            '*'
        } else if row.worktree_path.is_some() {
            '+'
        } else {
            ' '
        };
        let mut tracking =
            branch_verbose_tracking(row.upstream.as_ref(), row.upstream_track, options.verbosity);
        if options.verbosity >= 2
            && !row.is_head
            && let Some(worktree_path) = &row.worktree_path
        {
            tracking.push_str(&format!(" ({worktree_path})"));
        }
        println!(
            "{marker} {:width$} {}{} {}",
            row.display,
            row.oid,
            tracking,
            row.subject,
            width = width
        );
    }
    Ok(())
}

struct BranchVerboseRow {
    display: String,
    oid: String,
    subject: String,
    is_head: bool,
    worktree_path: Option<String>,
    upstream: Option<ForEachRefUpstream>,
    upstream_track: Option<ForEachRefTrack>,
}

fn branch_verbose_display_name(name: &str, mode: BranchListMode) -> Option<(String, String)> {
    if matches!(mode, BranchListMode::Local | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/heads/")
    {
        return Some((name.to_string(), name.to_string()));
    }
    if matches!(mode, BranchListMode::Remote | BranchListMode::All)
        && let Some(name) = name.strip_prefix("refs/remotes/")
    {
        let display = if matches!(mode, BranchListMode::All) {
            format!("remotes/{name}")
        } else {
            name.to_string()
        };
        return Some((display, name.to_string()));
    }
    None
}

fn branch_verbose_subject(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<String> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Ok(String::new());
    }
    let commit = Commit::parse(format, &object.body)?;
    Ok(commit_subject(&commit.message))
}

fn branch_verbose_tracking(
    upstream: Option<&ForEachRefUpstream>,
    track: Option<ForEachRefTrack>,
    verbosity: usize,
) -> String {
    match (verbosity, upstream, track) {
        (0, _, _) => String::new(),
        (1, _, Some(track)) if track.gone || track.ahead > 0 || track.behind > 0 => {
            let mut out = Vec::new();
            write_for_each_ref_track(&mut out, track, true).expect("write to vec");
            format!(" {}", String::from_utf8_lossy(&out))
        }
        (1, _, _) => String::new(),
        (_, Some(upstream), Some(track)) if track.gone || track.ahead > 0 || track.behind > 0 => {
            let mut out = Vec::new();
            write_for_each_ref_track(&mut out, track, false).expect("write to vec");
            format!(
                " [{}: {}]",
                for_each_ref_short_name(&upstream.refname),
                String::from_utf8_lossy(&out)
            )
        }
        (_, Some(upstream), _) => format!(" [{}]", for_each_ref_short_name(&upstream.refname)),
        (_, None, _) => String::new(),
    }
}
