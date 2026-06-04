//! `git branch` and all its modes
//! (list/create/delete/rename/copy/set-upstream/edit-description).

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_branch(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    if let Some(show_current) = parse_branch_show_current_options(args)? {
        if show_current {
            if let Some(branch) = store.current_branch()? {
                println!("{branch}");
            }
            return Ok(());
        }
        return print_branch_list(&store, BranchListMode::Local);
    }
    if let Some(move_options) = parse_branch_move_options(args)? {
        return run_branch_move_options(&git_dir, &store, move_options);
    }
    if let Some(upstream) = parse_branch_upstream_options(args)? {
        return run_branch_upstream_options(&git_dir, &store, upstream);
    }
    if let Some(verbose) = parse_branch_verbose_list_options(args)? {
        return run_branch_verbose_list_options(&git_dir, format, &store, verbose);
    }
    if let Some(delete) = parse_branch_delete_options(args)? {
        let BranchDeleteOptions {
            force,
            quiet,
            mode,
            branches,
        } = delete;
        return if matches!(mode, BranchDeleteMode::Remote) {
            delete_remote_tracking_branches(&store, &branches, quiet)
        } else if matches!(mode, BranchDeleteMode::All) {
            eprintln!("fatal: cannot use -a with -d");
            Err(GitError::Exit(128))
        } else if force {
            force_delete_branches(&git_dir, &store, &branches, quiet)
        } else {
            delete_merged_branches(&git_dir, format, &store, &branches, quiet)
        };
    }
    if let Some(create) = parse_branch_create_options(args)? {
        return run_branch_create_options(&git_dir, format, &store, create);
    }
    match args {
        [] => print_branch_list(&store, BranchListMode::Local),
        [flag] if flag == "--list" => print_branch_list(&store, BranchListMode::Local),
        [flag] if flag == "-r" || flag == "--remotes" => {
            print_branch_list(&store, BranchListMode::Remote)
        }
        [flag] if flag == "-a" || flag == "--all" => print_branch_list(&store, BranchListMode::All),
        [flag] if flag == "--color" || flag == "--color=always" => {
            print_branch_list_colored(&store, BranchListMode::Local)
        }
        [color, no_color] if branch_color_always_flag(color) && no_color == "--no-color" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_color, color] if no_color == "--no-color" && branch_color_always_flag(color) => {
            print_branch_list_colored(&store, BranchListMode::Local)
        }
        [flag, color]
            if (flag == "-r" || flag == "--remotes")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(&store, BranchListMode::Remote)
        }
        [color, flag]
            if (flag == "-r" || flag == "--remotes")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(&store, BranchListMode::Remote)
        }
        [flag, color]
            if (flag == "-a" || flag == "--all")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(&store, BranchListMode::All)
        }
        [color, flag]
            if (flag == "-a" || flag == "--all")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(&store, BranchListMode::All)
        }
        [flag, color, no_color]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_color_always_flag(color)
                && no_color == "--no-color" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, no_color, color]
            if branch_remote_or_all_mode(flag).is_some()
                && no_color == "--no-color"
                && branch_color_always_flag(color) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_colored(&store, mode)
        }
        [flag, display_flag]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [display_flag, flag]
            if branch_list_noop_display_flag(display_flag)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [first, second, flag]
            if branch_column_noop_flag(first)
                && branch_column_noop_flag(second)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [first, second, flag]
            if branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [first, second, flag]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, mode, descending)
        }
        [sort, flag]
            if branch_version_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, mode, descending)
        }
        [sort, flag]
            if branch_objectname_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
        }
        [sort, flag]
            if branch_objecttype_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_date_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(&git_dir, format, &store, mode, field, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some() && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(&git_dir, format, &store, mode, field, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
        }
        [sort, flag]
            if branch_objectsize_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
        }
        [sort, flag]
            if branch_date_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(&git_dir, format, &store, mode, field, descending)
        }
        [sort, flag]
            if branch_upstream_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [sort, flag]
            if branch_push_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(&git_dir, format, &store, mode, field, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some() && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [sort, flag]
            if sort == "--sort=-refname" && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [sort, key, flag]
            if sort == "--sort"
                && key == "-refname"
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_date_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(&git_dir, format, &store, mode, field, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
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
            print_branch_list_version_sorted(&store, mode, descending)
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
            print_branch_list_objectname_sorted(&store, mode, descending)
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
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
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
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
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
            print_branch_list_date_sorted(&git_dir, format, &store, mode, field, descending)
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
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [sort, key, flag]
            if sort == "--sort"
                && key == "refname"
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag] if branch_ignore_case_flag(flag) => print_branch_list(&store, BranchListMode::Local),
        [list, flag] if list == "--list" && branch_ignore_case_flag(flag) => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [flag, list] if branch_ignore_case_flag(flag) && list == "--list" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [flag, ignore]
            if branch_remote_or_all_mode(flag).is_some() && branch_ignore_case_flag(ignore) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [ignore, flag]
            if branch_ignore_case_flag(ignore) && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag] if flag == "--no-points-at" => print_branch_list(&store, BranchListMode::Local),
        [points_at, _rev, no_points_at]
            if points_at == "--points-at" && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_points_at, points_at, rev]
            if no_points_at == "--no-points-at" && points_at == "--points-at" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Local, &oid)
        }
        [points_at, no_points_at]
            if points_at.starts_with("--points-at=") && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_points_at, points_at]
            if no_points_at == "--no-points-at" && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Local, &oid)
        }
        [flag, color] if flag == "--list" && (color == "--color" || color == "--color=always") => {
            print_branch_list_colored(&store, BranchListMode::Local)
        }
        [color, flag] if flag == "--list" && (color == "--color" || color == "--color=always") => {
            print_branch_list_colored(&store, BranchListMode::Local)
        }
        [list, color, no_color]
            if list == "--list" && branch_color_always_flag(color) && no_color == "--no-color" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [color, no_color, list, patterns @ ..]
            if branch_color_always_flag(color) && no_color == "--no-color" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, no_color, color]
            if list == "--list" && no_color == "--no-color" && branch_color_always_flag(color) =>
        {
            print_branch_list_colored(&store, BranchListMode::Local)
        }
        [list, no_color, color, patterns @ ..]
            if list == "--list" && no_color == "--no-color" && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(&store, BranchListMode::Local, patterns)
        }
        [no_color, color, list, patterns @ ..]
            if no_color == "--no-color" && branch_color_always_flag(color) && list == "--list" =>
        {
            print_branch_list_matching_colored(&store, BranchListMode::Local, patterns)
        }
        [list, color, patterns @ ..] if list == "--list" && branch_color_always_flag(color) => {
            print_branch_list_matching_colored(&store, BranchListMode::Local, patterns)
        }
        [color, list, patterns @ ..] if branch_color_always_flag(color) && list == "--list" => {
            print_branch_list_matching_colored(&store, BranchListMode::Local, patterns)
        }
        [list, color] if list == "--list" && branch_color_noop_flag(color) => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [list, color, patterns @ ..] if list == "--list" && branch_color_noop_flag(color) => {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [color, list] if branch_color_noop_flag(color) && list == "--list" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [color, list, patterns @ ..] if branch_color_noop_flag(color) && list == "--list" => {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, points_at, _rev, no_points_at]
            if list == "--list" && points_at == "--points-at" && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [list, no_points_at, points_at, rev]
            if list == "--list" && no_points_at == "--no-points-at" && points_at == "--points-at" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Local, &oid)
        }
        [list, points_at, no_points_at]
            if list == "--list"
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [list, no_points_at, points_at]
            if list == "--list"
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Local, &oid)
        }
        [list, display_flag]
            if list == "--list" && branch_list_noop_display_flag(display_flag) =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [list, display_flag, patterns @ ..]
            if list == "--list" && branch_list_noop_display_flag(display_flag) =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [display_flag, list]
            if branch_list_noop_display_flag(display_flag) && list == "--list" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [display_flag, list, patterns @ ..]
            if branch_list_noop_display_flag(display_flag) && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list"
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key] if list == "--list" && sort == "--sort" && key == "refname" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [list, sort] if list == "--list" && branch_version_sort_value(sort).is_some() => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_objectname_sort_value(sort).is_some() => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_objecttype_sort_value(sort).is_some() => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_objectsize_sort_value(sort).is_some() => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_date_sort_value(sort).is_some() => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_upstream_sort_value(sort).is_some() => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_push_sort_value(sort).is_some() => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list" && sort == "--sort" && branch_push_sort_value(key).is_some() =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_objectname_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                &store,
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
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_version_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                &store,
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
                &store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort] if list == "--list" && sort == "--sort=-refname" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [list, sort, key] if list == "--list" && sort == "--sort" && key == "-refname" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort=-refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, sort, patterns @ ..] if list == "--list" && sort == "--sort=-refname" => {
            print_branch_list_matching_sorted(
                &store,
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
                &store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "refname" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [sort, key, list] if sort == "--sort" && key == "refname" && list == "--list" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [sort, list] if branch_version_sort_value(sort).is_some() && list == "--list" => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_objectname_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_objecttype_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, list] if branch_objectsize_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, list] if branch_date_sort_value(sort).is_some() && list == "--list" => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [sort, list] if branch_upstream_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_push_sort_value(sort).is_some() && list == "--list" => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort" && branch_push_sort_value(key).is_some() && list == "--list" =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [sort, list, patterns @ ..]
            if branch_objectname_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                &store,
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
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
                &store,
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
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
                &store,
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
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                &store,
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
                &git_dir,
                &store,
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
                &store,
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
                &store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list] if sort == "--sort=-refname" && list == "--list" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [sort, key, list] if sort == "--sort" && key == "-refname" && list == "--list" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [sort, list, patterns @ ..] if sort == "--sort=-refname" && list == "--list" => {
            print_branch_list_matching_sorted(
                &store,
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
                &store,
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
                &store,
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
                &store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort" && key == "refname" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if sort == "--sort=refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if sort == "--sort=-refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort=refname" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort=refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort" && key == "refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort" && key == "-refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort" && key == "refname" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [flag, list, color, no_color, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_color_always_flag(color)
                && no_color == "--no-color" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, color, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(&store, BranchListMode::Remote, patterns)
        }
        [flag, color, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            print_branch_list_matching_colored(&store, BranchListMode::Remote, patterns)
        }
        [flag, list, color, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(&store, BranchListMode::All, patterns)
        }
        [flag, color, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            print_branch_list_matching_colored(&store, BranchListMode::All, patterns)
        }
        [flag, color, no_color, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_color_always_flag(color)
                && no_color == "--no-color"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, no_color, color, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && no_color == "--no-color"
                && branch_color_always_flag(color) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_colored(&store, mode, patterns)
        }
        [flag, no_color, color, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_color == "--no-color"
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_colored(&store, mode, patterns)
        }
        [flag, rev] if flag == "--points-at" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Local, &oid)
        }
        [flag, rev] if flag == "--contains" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [contains, contains_rev, no_contains, no_contains_rev]
            if contains == "--contains" && no_contains == "--no-contains" =>
        {
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [no_contains, no_contains_rev, contains, contains_rev]
            if no_contains == "--no-contains" && contains == "--contains" =>
        {
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag] if flag == "--contains" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [flag, rev] if flag == "--no-contains" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag == "--no-contains" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag == "--merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [flag, rev] if flag == "--merged" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [merged, merged_rev, no_merged, no_merged_rev]
            if merged == "--merged" && no_merged == "--no-merged" =>
        {
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [no_merged, no_merged_rev, merged, merged_rev]
            if no_merged == "--no-merged" && merged == "--merged" =>
        {
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag] if flag == "--no-merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, false)
        }
        [flag, rev] if flag == "--no-merged" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, false)
        }
        [flag, points_at, rev, patterns @ ..] if flag == "--list" && points_at == "--points-at" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at_matching(&store, BranchListMode::Local, &oid, patterns)
        }
        [flag, contains, rev, patterns @ ..]
            if flag == "--list"
                && contains == "--contains"
                && patterns
                    .first()
                    .is_none_or(|value| *value != "--no-contains") =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [list, contains, contains_rev, no_contains, no_contains_rev, patterns @ ..]
            if list == "--list" && contains == "--contains" && no_contains == "--no-contains" =>
        {
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [list, no_contains, no_contains_rev, contains, contains_rev, patterns @ ..]
            if list == "--list" && no_contains == "--no-contains" && contains == "--contains" =>
        {
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, contains] if flag == "--list" && contains == "--contains" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [flag, contains, rev, patterns @ ..]
            if flag == "--list"
                && contains == "--no-contains"
                && patterns.first().is_none_or(|value| *value != "--contains") =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, contains] if flag == "--list" && contains == "--no-contains" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag, merged] if flag == "--list" && merged == "--merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [flag, merged, rev, patterns @ ..]
            if flag == "--list"
                && merged == "--merged"
                && patterns.first().is_none_or(|value| *value != "--no-merged") =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [list, merged, merged_rev, no_merged, no_merged_rev, patterns @ ..]
            if list == "--list" && merged == "--merged" && no_merged == "--no-merged" =>
        {
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [list, no_merged, no_merged_rev, merged, merged_rev, patterns @ ..]
            if list == "--list" && no_merged == "--no-merged" && merged == "--merged" =>
        {
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, merged] if flag == "--list" && merged == "--no-merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, false)
        }
        [flag, merged, rev, patterns @ ..]
            if flag == "--list"
                && merged == "--no-merged"
                && patterns.first().is_none_or(|value| *value != "--merged") =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, points_at, rev]
            if (flag == "-r" || flag == "--remotes") && points_at == "--points-at" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Remote, &oid)
        }
        [flag, points_at, _rev, no_points_at]
            if (flag == "-r" || flag == "--remotes")
                && points_at == "--points-at"
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::Remote)
        }
        [flag, no_points_at, points_at, rev]
            if (flag == "-r" || flag == "--remotes")
                && no_points_at == "--no-points-at"
                && points_at == "--points-at" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Remote, &oid)
        }
        [flag, contains, rev]
            if (flag == "-r" || flag == "--remotes") && contains == "--contains" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
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
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
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
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains == "--contains" =>
        {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::Remote,
                &oid,
                true,
            )
        }
        [flag, contains, rev]
            if (flag == "-r" || flag == "--remotes") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, merged] if (flag == "-r" || flag == "--remotes") && merged == "--merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged, rev]
            if (flag == "-r" || flag == "--remotes") && merged == "--merged" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged, merged_rev, no_merged, no_merged_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && merged == "--merged"
                && no_merged == "--no-merged" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
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
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, merged] if (flag == "-r" || flag == "--remotes") && merged == "--no-merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Remote, &oid, false)
        }
        [flag, merged, rev]
            if (flag == "-r" || flag == "--remotes") && merged == "--no-merged" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Remote, &oid, false)
        }
        [flag, points_at, rev]
            if (flag == "-a" || flag == "--all") && points_at == "--points-at" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::All, &oid)
        }
        [flag, points_at, _rev, no_points_at]
            if (flag == "-a" || flag == "--all")
                && points_at == "--points-at"
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::All)
        }
        [flag, no_points_at, points_at, rev]
            if (flag == "-a" || flag == "--all")
                && no_points_at == "--no-points-at"
                && points_at == "--points-at" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::All, &oid)
        }
        [flag, contains, rev] if (flag == "-a" || flag == "--all") && contains == "--contains" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(&git_dir, format, &store, BranchListMode::All, &oid, true)
        }
        [flag, contains] if (flag == "-a" || flag == "--all") && contains == "--contains" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(&git_dir, format, &store, BranchListMode::All, &oid, true)
        }
        [flag, contains, rev]
            if (flag == "-a" || flag == "--all") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, merged] if (flag == "-a" || flag == "--all") && merged == "--merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::All, &oid, true)
        }
        [flag, merged, rev] if (flag == "-a" || flag == "--all") && merged == "--merged" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::All, &oid, true)
        }
        [flag, merged] if (flag == "-a" || flag == "--all") && merged == "--no-merged" => {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::All, &oid, false)
        }
        [flag, merged, rev] if (flag == "-a" || flag == "--all") && merged == "--no-merged" => {
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::All, &oid, false)
        }
        [contains, no_contains]
            if branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag] if flag.starts_with("--points-at=") => {
            let rev = flag
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Local, &oid)
        }
        [flag] if flag.starts_with("--contains=") => {
            let rev = flag
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [flag] if flag.starts_with("--no-contains=") => {
            let rev = flag
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag.starts_with("--merged=") => {
            let rev = flag
                .strip_prefix("--merged=")
                .ok_or_else(|| GitError::Command("branch --merged requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, true)
        }
        [flag] if flag.starts_with("--no-merged=") => {
            let rev = flag
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Local, &oid, false)
        }
        [flag, points_at, patterns @ ..] if flag == "--list" && points_at.starts_with("--points-at=") => {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at_matching(&store, BranchListMode::Local, &oid, patterns)
        }
        [list, contains, no_contains, patterns @ ..]
            if list == "--list"
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, contains, patterns @ ..] if flag == "--list" && contains.starts_with("--contains=") => {
            let oid = resolve_revision(
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, contains, patterns @ ..] if flag == "--list" && contains.starts_with("--no-contains=") => {
            let oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, merged, patterns @ ..] if flag == "--list" && merged.starts_with("--merged=") => {
            let oid = resolve_revision(
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, merged, patterns @ ..] if flag == "--list" && merged.starts_with("--no-merged=") => {
            let oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [ignore, list, reset, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && list == "--list"
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [ignore, reset, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [flag, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(flag) && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, true)
        }
        [list, flag, reset, patterns @ ..]
            if list == "--list"
                && branch_ignore_case_enabled_flag(flag)
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, flag, patterns @ ..]
            if list == "--list" && branch_ignore_case_enabled_flag(flag) =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, true)
        }
        [list, column] if list == "--list" && branch_column_noop_flag(column) => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [list, column, patterns @ ..]
            if list == "--list" && branch_column_noop_flag(column) =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [column, list] if branch_column_noop_flag(column) && list == "--list" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [column, list, patterns @ ..]
            if branch_column_noop_flag(column) && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_column_noop_flag(first) && branch_column_noop_flag(second) && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list" && branch_column_noop_flag(first) && branch_column_noop_flag(second) =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list" && branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [format_flag, no_format, list, patterns @ ..]
            if format_flag.starts_with("--format=") && no_format == "--no-format" && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [format_flag, format_spec, no_format, list, patterns @ ..]
            if format_flag == "--format" && no_format == "--no-format" && list == "--list" =>
        {
            let _ = format_spec;
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [no_format, format_flag, list, patterns @ ..]
            if no_format == "--no-format" && format_flag.starts_with("--format=") && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, format_flag, no_format, patterns @ ..]
            if list == "--list" && format_flag.starts_with("--format=") && no_format == "--no-format" =>
        {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [list, format_flag, format_spec, no_format, patterns @ ..]
            if list == "--list" && format_flag == "--format" && no_format == "--no-format" =>
        {
            let _ = format_spec;
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_spec, list, patterns @ ..] if flag == "--format" && list == "--list" => {
            print_branch_list_format(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, flag, format_spec, patterns @ ..] if list == "--list" && flag == "--format" => {
            print_branch_list_format(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, patterns @ ..] if flag == "--list" => {
            print_branch_list_matching(&store, BranchListMode::Local, patterns, false)
        }
        [flag, points_at]
            if (flag == "-r" || flag == "--remotes") && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Remote, &oid)
        }
        [flag, points_at, no_points_at]
            if (flag == "-r" || flag == "--remotes")
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::Remote)
        }
        [flag, no_points_at, points_at]
            if (flag == "-r" || flag == "--remotes")
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::Remote, &oid)
        }
        [flag, contains, no_contains]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged]
            if (flag == "-r" || flag == "--remotes") && merged.starts_with("--no-merged=") =>
        {
            let rev = merged
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::Remote, &oid, false)
        }
        [flag, points_at]
            if (flag == "-a" || flag == "--all") && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::All, &oid)
        }
        [flag, points_at, no_points_at]
            if (flag == "-a" || flag == "--all")
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(&store, BranchListMode::All)
        }
        [flag, no_points_at, points_at]
            if (flag == "-a" || flag == "--all")
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at(&store, BranchListMode::All, &oid)
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains.starts_with("--contains=") =>
        {
            let rev = contains
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(&git_dir, format, &store, BranchListMode::All, &oid, true)
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains.starts_with("--no-contains=") =>
        {
            let rev = contains
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::All, &oid, true)
        }
        [flag, merged]
            if (flag == "-a" || flag == "--all") && merged.starts_with("--no-merged=") =>
        {
            let rev = merged
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged(&git_dir, format, &store, BranchListMode::All, &oid, false)
        }
        [flag, format_flag, no_format]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && no_format == "--no-format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, format_flag, format_spec, no_format]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && no_format == "--no-format" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
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
            print_branch_list_format(&git_dir, format, &store, mode, &[], false, format_spec)
        }
        [flag, no_format, format_flag, format_spec]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag == "--format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(&git_dir, format, &store, mode, &[], false, format_spec)
        }
        [flag, format_flag, no_format, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && no_format == "--no-format"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, format_flag, format_spec, no_format, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && no_format == "--no-format"
                && list == "--list" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
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
            print_branch_list_format(&git_dir, format, &store, mode, patterns, false, format_spec)
        }
        [flag, no_format, format_flag, format_spec, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag == "--format"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(&git_dir, format, &store, mode, patterns, false, format_spec)
        }
        [flag, list, format_flag, no_format, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag.starts_with("--format=")
                && no_format == "--no-format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, format_flag, format_spec, no_format, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag == "--format"
                && no_format == "--no-format" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
            print_branch_list(&store, mode)
        }
        [flag, list, display_flag, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_list_noop_display_flag(display_flag) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, display_flag, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, display_flag, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, mode, descending)
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
            print_branch_list_version_sorted(&store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, mode, descending)
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
            print_branch_list_objectname_sorted(&store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
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
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
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
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
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
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
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
            print_branch_list_matching_objectname_sorted(&store, mode, patterns, false, descending)
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
            print_branch_list_matching_objectname_sorted(&store, mode, patterns, false, descending)
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
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
            print_branch_list_matching_version_sorted(&store, mode, patterns, false, descending)
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
            print_branch_list_matching_version_sorted(&store, mode, patterns, false, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(&store, mode, patterns, false, true)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(&store, mode, patterns, false, true)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list(&store, mode)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, mode, descending)
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
            print_branch_list_version_sorted(&store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, mode, descending)
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
            print_branch_list_objectname_sorted(&store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
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
            print_branch_list_objecttype_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
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
            print_branch_list_objectsize_sorted(&git_dir, format, &store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_push_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
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
            print_branch_list_upstream_sorted(&git_dir, &store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, mode, descending)
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
            print_branch_list_matching_objectname_sorted(&store, mode, patterns, false, descending)
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
            print_branch_list_matching_objectname_sorted(&store, mode, patterns, false, descending)
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
            print_branch_list_matching_version_sorted(&store, mode, patterns, false, descending)
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
            print_branch_list_matching_objectname_sorted(&store, mode, patterns, false, descending)
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir,
                format,
                &store,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
            print_branch_list_matching_version_sorted(&store, mode, patterns, false, descending)
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
            print_branch_list_matching_objectname_sorted(&store, mode, patterns, false, descending)
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir, format, &store, mode, patterns, false, descending,
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
                &git_dir,
                format,
                &store,
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
                &git_dir, &store, mode, patterns, false, descending,
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
                &git_dir, &store, mode, patterns, false, descending,
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
            print_branch_list_matching_version_sorted(&store, mode, patterns, false, descending)
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
            print_branch_list_matching_version_sorted(&store, mode, patterns, false, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_sorted(&store, mode, true)
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(&store, mode, patterns, false, true)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(&store, mode, patterns, false, true)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(&store, mode, patterns, false, true)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching_sorted(&store, mode, patterns, false, true)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
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
            print_branch_list_format(&git_dir, format, &store, mode, patterns, true, format_spec)
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
            print_branch_list_format(&git_dir, format, &store, mode, patterns, true, format_spec)
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
            print_branch_list_format(&git_dir, format, &store, mode, patterns, false, format_spec)
        }
        [flag, format_flag, format_spec, ignore, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(&git_dir, format, &store, mode, patterns, true, format_spec)
        }
        [flag, list, ignore, format_flag, format_spec, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && format_flag == "--format" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(&git_dir, format, &store, mode, patterns, true, format_spec)
        }
        [flag, format_flag, format_spec, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_format(&git_dir, format, &store, mode, patterns, false, format_spec)
        }
        [flag, list, ignore, reset, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, ignore, list, reset, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list"
                && reset == "--no-ignore-case" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            print_branch_list_matching(&store, mode, patterns, false)
        }
        [flag, list, points_at, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && points_at == "--points-at" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at_matching(&store, mode, &oid, patterns)
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_points_at_matching(&store, mode, &oid, patterns)
        }
        [flag, list, contains, contains_rev, no_contains, no_contains_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && contains == "--contains"
                && no_contains == "--no-contains" =>
        {
            let mode = branch_remote_or_all_mode(flag).expect("guard checked branch mode");
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
            let contains_oid = resolve_revision(&git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(&git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
            let merged_oid = resolve_revision(&git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(&git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                &git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
            let oid = resolve_revision(&git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_no_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                branch_no_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                &git_dir,
                format,
                &store,
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
            print_branch_list_matching(&store, BranchListMode::Remote, patterns, true)
        }
        [flag, list, ignore, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore) =>
        {
            print_branch_list_matching(&store, BranchListMode::Remote, patterns, true)
        }
        [flag, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes") && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::Remote, patterns, false)
        }
        [flag, ignore, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            print_branch_list_matching(&store, BranchListMode::All, patterns, true)
        }
        [flag, list, ignore, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore) =>
        {
            print_branch_list_matching(&store, BranchListMode::All, patterns, true)
        }
        [flag, list, patterns @ ..] if (flag == "-a" || flag == "--all") && list == "--list" => {
            print_branch_list_matching(&store, BranchListMode::All, patterns, false)
        }
        [flag, key] if flag == "--sort" && key == "refname" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [flag] if branch_version_sort_value(flag).is_some() => {
            let descending = branch_version_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_version_sort_value(key).is_some() => {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [flag] if branch_objectname_sort_value(flag).is_some() => {
            let descending =
                branch_objectname_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_objectname_sort_value(key).is_some() => {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [flag] if branch_objecttype_sort_value(flag).is_some() => {
            let descending =
                branch_objecttype_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_objecttype_sort_value(key).is_some() => {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag] if branch_objectsize_sort_value(flag).is_some() => {
            let descending =
                branch_objectsize_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_objectsize_sort_value(key).is_some() => {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag] if branch_date_sort_value(flag).is_some() => {
            let (field, descending) =
                branch_date_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_date_sort_value(key).is_some() => {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [flag] if branch_upstream_sort_value(flag).is_some() => {
            let descending =
                branch_upstream_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [flag] if branch_push_sort_value(flag).is_some() => {
            let descending = branch_push_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_upstream_sort_value(key).is_some() => {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_push_sort_value(key).is_some() => {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [flag] if flag == "--sort=-refname" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [flag, key] if flag == "--sort" && key == "-refname" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [sort, no_sort] if sort == "--sort=refname" && no_sort == "--no-sort" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [sort, no_sort]
            if (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [sort, no_sort] if sort == "--sort=-refname" && no_sort == "--no-sort" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_sort, sort] if no_sort == "--no-sort" && sort == "--sort=refname" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_version_sort_value(sort).is_some() => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objectname_sort_value(sort).is_some() => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objecttype_sort_value(sort).is_some() => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objectsize_sort_value(sort).is_some() => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_date_sort_value(sort).is_some() => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_upstream_sort_value(sort).is_some() => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_push_sort_value(sort).is_some() => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && sort == "--sort=-refname" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [sort, key, no_sort] if sort == "--sort" && key == "refname" && no_sort == "--no-sort" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [sort, key, no_sort]
            if sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [sort, key, no_sort] if sort == "--sort" && key == "-refname" && no_sort == "--no-sort" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_sort, sort, key] if no_sort == "--no-sort" && sort == "--sort" && key == "refname" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort" && sort == "--sort" && branch_version_sort_value(key).is_some() =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(&store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(&store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
            print_branch_list_upstream_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(&git_dir, &store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key] if no_sort == "--no-sort" && sort == "--sort" && key == "-refname" => {
            print_branch_list_sorted(&store, BranchListMode::Local, true)
        }
        [first, second]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            print_branch_list(&store, BranchListMode::Local)
        }
        [first, second] if branch_column_noop_flag(first) && branch_column_noop_flag(second) => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [first, second] if branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [flag, no_format] if flag.starts_with("--format=") && no_format == "--no-format" => {
            print_branch_list(&store, BranchListMode::Local)
        }
        [flag, format_spec, no_format] if flag == "--format" && no_format == "--no-format" => {
            let _ = format_spec;
            print_branch_list(&store, BranchListMode::Local)
        }
        [no_format, flag] if no_format == "--no-format" && flag.starts_with("--format=") => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                &git_dir,
                format,
                &store,
                BranchListMode::Local,
                &[],
                false,
                format_spec,
            )
        }
        [no_format, flag, format_spec] if no_format == "--no-format" && flag == "--format" => {
            print_branch_list_format(
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
                &git_dir,
                format,
                &store,
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
            print_branch_list_format(&git_dir, format, &store, BranchListMode::Local, &[], false, format_spec)
        }
        [flag, format_spec] if flag == "--format" => {
            print_branch_list_format(&git_dir, format, &store, BranchListMode::Local, &[], false, format_spec)
        }
        [flag, list, patterns @ ..] if flag.starts_with("--format=") && list == "--list" => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                &git_dir,
                format,
                &store,
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
            print_branch_list(&store, BranchListMode::Local)
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
            print_branch_list(&store, BranchListMode::Local)
        }
        [delete, no_delete, branch]
            if (delete == "-d" || delete == "--delete") && no_delete == "--no-delete" =>
        {
            create_branch_from_start(&git_dir, format, &store, branch, None)
        }
        [delete, no_delete, branch, start]
            if (delete == "-d" || delete == "--delete") && no_delete == "--no-delete" =>
        {
            create_branch_from_start(&git_dir, format, &store, branch, Some(start))
        }
        [flag] if flag == "-f" || flag == "--force" => print_branch_list(&store, BranchListMode::Local),
        [flag, branches @ ..] if flag == "-D" => force_delete_branches(&git_dir, &store, branches, false),
        [flag, force, branches @ ..]
            if (flag == "-d" || flag == "--delete") && (force == "-f" || force == "--force") =>
        {
            force_delete_branches(&git_dir, &store, branches, false)
        }
        [force, flag, branches @ ..]
            if (force == "-f" || force == "--force") && (flag == "-d" || flag == "--delete") =>
        {
            force_delete_branches(&git_dir, &store, branches, false)
        }
        [flag, branches @ ..] if flag == "-d" || flag == "--delete" => {
            delete_merged_branches(&git_dir, format, &store, branches, false)
        }
        [flag, branch] if flag == "-f" || flag == "--force" => {
            force_update_branch(&git_dir, format, &store, branch, None)
        }
        [flag, branch, start] if flag == "-f" || flag == "--force" => {
            force_update_branch(&git_dir, format, &store, branch, Some(start))
        }
        [branch] => create_branch_from_start(&git_dir, format, &store, branch, None),
        [branch, start] => create_branch_from_start(&git_dir, format, &store, branch, Some(start)),
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
    positionals: Vec<String>,
}

#[derive(Clone, Copy)]
enum BranchTrackMode {
    Direct,
    Inherit,
}

struct BranchVerboseListOptions {
    mode: BranchListMode,
    patterns: Vec<String>,
    ignore_case: bool,
    verbosity: usize,
}

fn parse_branch_show_current_options(args: &[String]) -> Result<Option<bool>> {
    let mut show_current = None;
    let mut saw_positional = false;
    let mut end_of_options = false;
    for arg in args {
        if end_of_options {
            saw_positional = true;
            continue;
        }
        match arg.as_str() {
            "--" => end_of_options = true,
            "--show-current" => show_current = Some(true),
            "--no-show-current" => show_current = Some(false),
            value if value.starts_with("--show-current=") => {
                branch_option_takes_no_value("show-current")?;
            }
            value if value.starts_with("--no-show-current=") => {
                branch_option_takes_no_value("no-show-current")?;
            }
            value if value.starts_with('-') => return Ok(None),
            _ => saw_positional = true,
        }
    }
    match show_current {
        Some(true) => Ok(Some(true)),
        Some(false) if !saw_positional => Ok(Some(false)),
        _ => Ok(None),
    }
}

fn parse_branch_verbose_list_options(args: &[String]) -> Result<Option<BranchVerboseListOptions>> {
    let mut saw_verbose = false;
    let mut verbosity = 0usize;
    let mut explicit_list = false;
    let mut mode = BranchListMode::Local;
    let mut ignore_case = false;
    let mut patterns = Vec::new();
    let mut end_of_options = false;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if end_of_options {
            if explicit_list || matches!(mode, BranchListMode::Remote | BranchListMode::All) {
                patterns.push(arg.to_string());
                continue;
            }
            return Ok(None);
        }
        match arg.as_str() {
            "--" => end_of_options = true,
            "-v" | "--verbose" => {
                saw_verbose = true;
                verbosity = verbosity.saturating_add(1);
            }
            "-vv" => {
                saw_verbose = true;
                verbosity = verbosity.saturating_add(2);
            }
            "--no-verbose" => {
                saw_verbose = true;
                verbosity = 0;
            }
            "--list" | "-l" => explicit_list = true,
            "--no-list" | "--no-delete" | "--no-show-current" => {}
            "-r" | "--remotes" => mode = BranchListMode::Remote,
            "-a" | "--all" => mode = BranchListMode::All,
            "-i" | "--ignore-case" => ignore_case = true,
            "--no-ignore-case" => ignore_case = false,
            "--color" | "--color=always" | "--color=never" | "--color=auto" | "--no-color" => {}
            "--no-column" | "--column=auto" | "--column=never" | "--column=plain" => {}
            "--abbrev" | "--no-abbrev" => {}
            "--sort" => {
                let Some(_) = iter.next() else {
                    return Err(GitError::Command("branch --sort requires a value".into()));
                };
            }
            "--no-sort" => {}
            value if value.starts_with("--sort=") => {}
            value if value.starts_with("--abbrev=") => {}
            value if value.starts_with("--column=") => {}
            value if value.starts_with("--color=") => {}
            value if value.starts_with('-') => return Ok(None),
            value => {
                if explicit_list || matches!(mode, BranchListMode::Remote | BranchListMode::All) {
                    patterns.push(value.to_string());
                } else {
                    return Ok(None);
                }
            }
        }
    }
    if !saw_verbose {
        return Ok(None);
    }
    Ok(Some(BranchVerboseListOptions {
        mode,
        patterns,
        ignore_case,
        verbosity,
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

fn parse_branch_move_options(args: &[String]) -> Result<Option<BranchMoveOptions>> {
    let mut kind = None;
    let mut force = false;
    let mut branches = Vec::new();
    let mut end_of_options = false;
    for arg in args {
        if end_of_options {
            branches.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => end_of_options = true,
            "-m" | "--move" => kind = Some(BranchMoveKind::Rename),
            "-M" => {
                kind = Some(BranchMoveKind::Rename);
                force = true;
            }
            "-c" | "--copy" => kind = Some(BranchMoveKind::Copy),
            "-C" => {
                kind = Some(BranchMoveKind::Copy);
                force = true;
            }
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "--no-move" | "--no-copy" => kind = None,
            "-q" | "--quiet" | "--no-quiet" => {}
            value if value.starts_with("--move=") => {
                branch_option_takes_no_value("move")?;
            }
            value if value.starts_with("--no-move=") => {
                branch_option_takes_no_value("no-move")?;
            }
            value if value.starts_with("--copy=") => {
                branch_option_takes_no_value("copy")?;
            }
            value if value.starts_with("--no-copy=") => {
                branch_option_takes_no_value("no-copy")?;
            }
            value if value.starts_with("--force=") => {
                branch_option_takes_no_value("force")?;
            }
            value if value.starts_with("--no-force=") => {
                branch_option_takes_no_value("no-force")?;
            }
            value if value.starts_with("--quiet=") => {
                branch_option_takes_no_value("quiet")?;
            }
            value if value.starts_with("--no-quiet=") => {
                branch_option_takes_no_value("no-quiet")?;
            }
            value if value.starts_with('-') => return Ok(None),
            value => branches.push(value.to_string()),
        }
    }
    Ok(kind.map(|kind| BranchMoveOptions {
        kind,
        force,
        branches,
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
        eprintln!("fatal: no branch named '{old_branch}'");
        return Err(GitError::Exit(128));
    }
    if !options.force && store.read_ref(&new_ref)?.is_some() {
        eprintln!("fatal: a branch named '{new_branch}' already exists");
        return Err(GitError::Exit(128));
    }
    if options.force
        && old_ref != new_ref
        && store.current_branch_ref()?.as_deref() == Some(new_ref.as_str())
    {
        let worktree_root = worktree_root_for_git_dir(git_dir)?;
        eprintln!(
            "fatal: cannot force update the branch '{new_branch}' used by worktree at '{}'",
            worktree_root.display()
        );
        return Err(GitError::Exit(128));
    }

    match options.kind {
        BranchMoveKind::Rename => {
            let committer = branch_reflog_committer_identity(store, &old_branch)?;
            store.move_branch(&old_branch, &new_branch, options.force, committer)?;
            rename_branch_config(git_dir, &old_branch, &new_branch)?;
        }
        BranchMoveKind::Copy => {
            let committer = branch_reflog_committer_identity(store, &old_branch)?;
            store.copy_branch(&old_branch, &new_branch, options.force, committer)?;
            copy_branch_config(git_dir, &old_branch, &new_branch)?;
        }
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
    let email = env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| "git-rs@example.invalid".into());
    git_sequencer::format_commit_identity(&name, &email, &date)
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

fn parse_branch_upstream_options(args: &[String]) -> Result<Option<BranchUpstreamOptions>> {
    let mut action = None;
    let mut branches = Vec::new();
    let mut end_of_options = false;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if end_of_options {
            branches.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => end_of_options = true,
            "-u" | "--set-upstream-to" => {
                let Some(value) = iter.next() else {
                    if arg == "-u" {
                        eprintln!("error: switch `u' requires a value");
                    } else {
                        eprintln!("error: option `set-upstream-to' requires a value");
                    }
                    return Err(GitError::Exit(129));
                };
                action = Some(BranchUpstreamAction::Set(value.to_string()));
            }
            "--no-set-upstream-to" => action = None,
            "--unset-upstream" => action = Some(BranchUpstreamAction::Unset),
            "--no-unset-upstream" => action = None,
            value if value.starts_with("--set-upstream-to=") => {
                action = Some(BranchUpstreamAction::Set(
                    value["--set-upstream-to=".len()..].to_string(),
                ));
            }
            value if value.starts_with("--no-set-upstream-to=") => {
                branch_option_takes_no_value("no-set-upstream-to")?;
            }
            value if value.starts_with("--unset-upstream=") => {
                branch_option_takes_no_value("unset-upstream")?;
            }
            value if value.starts_with("--no-unset-upstream=") => {
                branch_option_takes_no_value("no-unset-upstream")?;
            }
            value if value.starts_with("-u") && value.len() > 2 => {
                action = Some(BranchUpstreamAction::Set(value[2..].to_string()));
            }
            value if value.starts_with('-') => return Ok(None),
            value => branches.push(value.to_string()),
        }
    }
    Ok(action.map(|action| BranchUpstreamOptions { action, branches }))
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
    let fetch = config.get("remote", Some(remote), "fetch")?;
    let refspec = parse_refspec(fetch).ok()?;
    if refspec.negative {
        return None;
    }
    let dst = refspec.dst.as_deref()?;
    let src = refspec.src.as_deref()?;
    let remote_ref = upstream
        .strip_prefix("refs/remotes/")
        .map(str::to_string)
        .or_else(|| {
            upstream
                .strip_prefix(&format!("{remote}/"))
                .map(|branch| format!("{remote}/{branch}"))
        })
        .map(|name| format!("refs/remotes/{name}"))?;
    if refspec.pattern {
        let (dst_prefix, dst_suffix) = dst.split_once('*')?;
        let middle = remote_ref
            .strip_prefix(dst_prefix)?
            .strip_suffix(dst_suffix)?;
        let (src_prefix, src_suffix) = src.split_once('*')?;
        let merge = format!("{src_prefix}{middle}{src_suffix}");
        return Some((remote_ref, merge));
    }
    (dst == remote_ref).then(|| (remote_ref, src.to_string()))
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

fn parse_branch_create_options(args: &[String]) -> Result<Option<BranchCreateOptions>> {
    let mut saw_create_option = false;
    let mut force = false;
    let mut quiet = false;
    let mut track = None;
    let mut recurse_submodules = false;
    let mut legacy_set_upstream = false;
    let mut edit_description = false;
    let mut positionals = Vec::new();
    let mut end_of_options = false;
    let mut saw_separator = false;

    for arg in args {
        if end_of_options {
            positionals.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => {
                saw_separator = true;
                end_of_options = true;
            }
            "-f" | "--force" => {
                saw_create_option = true;
                force = true;
            }
            "--no-force" => {
                saw_create_option = true;
                force = false;
            }
            "-q" | "--quiet" => {
                saw_create_option = true;
                quiet = true;
            }
            "--no-quiet" => {
                saw_create_option = true;
                quiet = false;
            }
            "-t" | "--track" => {
                saw_create_option = true;
                track = Some(BranchTrackMode::Direct);
            }
            "--track=direct" => {
                saw_create_option = true;
                track = Some(BranchTrackMode::Direct);
            }
            "--track=inherit" => {
                saw_create_option = true;
                track = Some(BranchTrackMode::Inherit);
            }
            "--no-track" => {
                saw_create_option = true;
                track = None;
            }
            "--recurse-submodules" => {
                saw_create_option = true;
                recurse_submodules = true;
            }
            "--no-recurse-submodules" => {
                saw_create_option = true;
                recurse_submodules = false;
            }
            "--set-upstream" => {
                saw_create_option = true;
                legacy_set_upstream = true;
            }
            "--no-set-upstream" => {
                saw_create_option = true;
                legacy_set_upstream = false;
            }
            "--edit-description" => {
                saw_create_option = true;
                edit_description = true;
            }
            "--no-edit-description" => {
                saw_create_option = true;
                edit_description = false;
            }
            "--create-reflog" | "--no-create-reflog" | "-v" | "--verbose" | "--no-verbose" => {
                saw_create_option = true;
            }
            value if value.starts_with("--track=") => {
                eprintln!("error: option `track' expects \"direct\" or \"inherit\"");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--no-track=") => {
                eprintln!("error: option `no-track' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--recurse-submodules=") => {
                eprintln!("error: option `recurse-submodules' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--no-recurse-submodules=") => {
                eprintln!("error: option `no-recurse-submodules' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--set-upstream=") => {
                eprintln!("error: option `set-upstream' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--no-set-upstream=") => {
                eprintln!("error: option `no-set-upstream' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--edit-description=") => {
                eprintln!("error: option `edit-description' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--no-edit-description=") => {
                eprintln!("error: option `no-edit-description' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with('-') => return Ok(None),
            value => positionals.push(value.to_string()),
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
            positionals,
        }),
    )
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
            create_branch_from_start(git_dir, format, store, branch, None)?;
            branch_create_set_tracking(git_dir, store, branch, None, options.track, options.quiet)
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
            create_branch_from_start(git_dir, format, store, branch, Some(start))?;
            branch_create_set_tracking(
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

fn branch_create_set_tracking(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    start: Option<&String>,
    track: Option<BranchTrackMode>,
    quiet: bool,
) -> Result<()> {
    match track {
        None => Ok(()),
        Some(BranchTrackMode::Direct) => {
            let upstream = branch_create_direct_upstream(store, start)?;
            set_branch_upstream_quiet(git_dir, store, branch, &upstream, quiet)
        }
        Some(BranchTrackMode::Inherit) => {
            branch_create_inherit_upstream(git_dir, store, branch, start, quiet)
        }
    }
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
    let mut config = read_repo_config(git_dir)?;
    let Some(upstream) = resolve_branch_upstream(store, &config, upstream)? else {
        eprintln!("fatal: the requested upstream branch '{upstream}' does not exist");
        return Err(GitError::Exit(128));
    };
    if upstream.remote == "." && upstream.merge == branch_ref_name(branch)? {
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
    if !quiet {
        println!("branch '{branch}' set up to track '{}'.", upstream.display);
    }
    Ok(())
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
            let remote_ref = format!("refs/remotes/{start}");
            match store.read_ref(&remote_ref)? {
                Some(RefTarget::Direct(oid)) => Ok(oid),
                _ => Err(err),
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
    let refname = validate_branch_creation_name(branch)?;
    if store.read_ref(&refname)?.is_some() {
        eprintln!("fatal: a branch named '{branch}' already exists");
        return Err(GitError::Exit(128));
    }
    let start_rev = start.map_or("HEAD", String::as_str);
    let start_oid = resolve_branch_start(git_dir, format, store, start_rev)?;
    let message = match start {
        Some(start) => format!("branch: Created from {start}").into_bytes(),
        None => b"branch: Created from HEAD".to_vec(),
    };
    store.create_branch(
        branch,
        start_oid,
        commit_identity_from_env("COMMITTER")?,
        message,
    )?;
    Ok(())
}

fn validate_branch_creation_name(branch: &str) -> Result<String> {
    match branch_ref_name(branch) {
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
    let mut branches = Vec::new();
    let mut end_of_options = false;

    for arg in args {
        if end_of_options {
            branches.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "--" => end_of_options = true,
            "-d" | "--delete" => {
                saw_delete_option = true;
                delete = true;
            }
            "--no-delete" => {
                saw_delete_option = true;
                delete = false;
            }
            "-D" => {
                saw_delete_option = true;
                delete = true;
                force = true;
            }
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-v" | "--verbose" | "--no-verbose" => {}
            "-r" | "--remotes" => mode = BranchDeleteMode::Remote,
            "-a" | "--all" => mode = BranchDeleteMode::All,
            value if value.starts_with("--delete=") => {
                branch_option_takes_no_value("delete")?;
            }
            value if value.starts_with("--no-delete=") => {
                branch_option_takes_no_value("no-delete")?;
            }
            value if value.starts_with("--force=") => {
                branch_option_takes_no_value("force")?;
            }
            value if value.starts_with("--no-force=") => {
                branch_option_takes_no_value("no-force")?;
            }
            value if value.starts_with("--quiet=") => {
                branch_option_takes_no_value("quiet")?;
            }
            value if value.starts_with("--no-quiet=") => {
                branch_option_takes_no_value("no-quiet")?;
            }
            value if value.starts_with("--verbose=") => {
                branch_option_takes_no_value("verbose")?;
            }
            value if value.starts_with("--no-verbose=") => {
                branch_option_takes_no_value("no-verbose")?;
            }
            value if value.starts_with("--remotes=") => {
                branch_option_takes_no_value("remotes")?;
            }
            value if value.starts_with("--all=") => {
                branch_option_takes_no_value("all")?;
            }
            value if value.starts_with('-') && !value.starts_with("--") => {
                for option in value[1..].chars() {
                    match option {
                        'd' => {
                            saw_delete_option = true;
                            delete = true;
                        }
                        'D' => {
                            saw_delete_option = true;
                            delete = true;
                            force = true;
                        }
                        'f' => force = true,
                        'q' => quiet = true,
                        'v' => {}
                        'r' => mode = BranchDeleteMode::Remote,
                        'a' => mode = BranchDeleteMode::All,
                        _ => return Ok(None),
                    }
                }
            }
            value if value.starts_with('-') => return Ok(None),
            value => branches.push(value.to_string()),
        }
    }

    Ok(
        (saw_delete_option && delete).then_some(BranchDeleteOptions {
            force,
            quiet,
            mode,
            branches,
        }),
    )
}

fn branch_option_takes_no_value<T>(option: &str) -> Result<T> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
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
    let current_branch = store.current_branch_ref()?;
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let mut failed = false;
    for branch in branches {
        let name = format!("refs/heads/{branch}");
        if store.read_ref(&name)?.is_none() {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        }
        if current_branch.as_deref() == Some(name.as_str()) {
            eprintln!(
                "error: cannot delete branch '{branch}' used by worktree at '{}'",
                worktree_root.display()
            );
            failed = true;
            continue;
        }
        let deleted = store.delete_branch(branch)?;
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
        new: RefTarget::Direct(new_oid.clone()),
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

    let current_branch = store.current_branch_ref()?;
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let head = resolve_revision(git_dir, format, "HEAD")?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let reachable =
        git_rev::walk_commits(&db, format, [git_rev::peel_to_commit(&db, format, &head)?])?
            .into_iter()
            .map(|record| record.oid)
            .collect::<HashSet<_>>();

    let mut failed = false;
    for branch in branches {
        let name = format!("refs/heads/{branch}");
        let Some(target) = store.read_ref(&name)? else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        if current_branch.as_deref() == Some(name.as_str()) {
            eprintln!(
                "error: cannot delete branch '{branch}' used by worktree at '{}'",
                worktree_root.display()
            );
            failed = true;
            continue;
        }
        let RefTarget::Direct(oid) = target else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        let Ok(tip) = git_rev::peel_to_commit(&db, format, &oid) else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        if !reachable.contains(&tip) {
            eprintln!("error: the branch '{branch}' is not fully merged");
            eprintln!("hint: If you are sure you want to delete it, run 'git branch -D {branch}'");
            eprintln!(
                "hint: Disable this message with \"git config set advice.forceDeleteBranch false\""
            );
            failed = true;
            continue;
        }
        let deleted = store.delete_branch(branch)?;
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

fn print_branch_list_colored(store: &FileRefStore, mode: BranchListMode) -> Result<()> {
    print_branch_list_filtered_with_color(store, mode, true, |_, _| true)
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
    print_branch_list_filtered(store, mode, |reference, name| {
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
        .map(|oid| git_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let no_contains_targets = no_contains_oids
        .iter()
        .map(|oid| git_rev::peel_to_commit(&db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    let mut included = HashSet::new();
    for reference in store.list_refs()? {
        if !branch_ref_matches_mode(&reference.name, mode) {
            continue;
        }
        let RefTarget::Direct(tip) = &reference.target else {
            continue;
        };
        let Ok(tip) = git_rev::peel_to_commit(&db, format, tip) else {
            continue;
        };
        let reachable = git_rev::walk_commits(&db, format, [tip])?
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
            let target = git_rev::peel_to_commit(&db, format, oid)?;
            git_rev::walk_commits(&db, format, [target]).map(|records| {
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
            let target = git_rev::peel_to_commit(&db, format, oid)?;
            git_rev::walk_commits(&db, format, [target]).map(|records| {
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
        let Ok(tip) = git_rev::peel_to_commit(&db, format, tip) else {
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
        "--sort=objecttype" | "objecttype" => Some(false),
        "--sort=-objecttype" | "-objecttype" => Some(true),
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir)?;
    let head_ref = store.current_branch_ref()?;
    let objectname_abbrev = repository_abbrev(git_dir, format)?;
    let objectname_candidates = cat_file_all_object_ids(git_dir, format)?;
    let deltabase = zero_oid(format)?;
    let mut stdout = io::stdout().lock();
    for reference in store.list_refs()? {
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
        let RefTarget::Direct(oid) = &reference.target else {
            continue;
        };
        let upstream = for_each_ref_upstream(&config, &reference.name);
        let push = for_each_ref_push(&config, &reference.name);
        let upstream_track = upstream
            .as_ref()
            .map(|upstream| for_each_ref_upstream_track(store, &db, format, oid, &upstream.refname))
            .transpose()?
            .flatten();
        let push_track = push
            .as_ref()
            .and_then(|push| push.refname.as_deref())
            .map(|push_ref| for_each_ref_upstream_track(store, &db, format, oid, push_ref))
            .transpose()?
            .flatten();
        let object = db.read_object(oid)?;
        let object_disk_size = for_each_ref_loose_object_disk_size(git_dir, oid)?;
        let worktree_path =
            for_each_ref_worktree_path(git_dir, head_ref.as_deref(), &reference.name)?;
        let contents = for_each_ref_contents(format, &object)?;
        let context = ForEachRefFormatContext {
            git_dir,
            db: &db,
            format,
            refname: &reference.name,
            oid,
            deltabase: &deltabase,
            object_type: object.object_type,
            object_body: &object.body,
            object_size: object.body.len(),
            object_disk_size,
            color: false,
            quote: ForEachRefQuoteMode::None,
            objectname_abbrev,
            objectname_candidates: &objectname_candidates,
            worktree_path: worktree_path.as_deref(),
            is_head: head_ref.as_deref() == Some(reference.name.as_str()),
            symref: None,
            upstream,
            push,
            upstream_track,
            push_track,
            contents,
            peeled_object: None,
        };
        let mut line = Vec::new();
        print_for_each_ref_format(&mut line, options.format_spec, &context)?;
        if options.omit_empty && line.is_empty() {
            continue;
        }
        stdout.write_all(&line)?;
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
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
    mut include: impl FnMut(&git_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_with_color(store, mode, false, |reference, name| {
        include(reference, name)
    })
}

fn print_branch_list_filtered_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
) -> Result<()> {
    print_branch_list_filtered_sorted_with_color(store, mode, color, false, include)
}

fn print_branch_list_filtered_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = store.list_refs()?;
    if descending {
        refs.reverse();
    }
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn print_branch_list_filtered_version_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
) -> Result<()> {
    let current = store.current_branch_ref()?;
    let mut refs = store.list_refs()?;
    refs.sort_by(|left, right| version_sort_cmp(&left.name, &right.name));
    if descending {
        refs.reverse();
    }
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn print_branch_list_filtered_objectname_sorted_with_color(
    store: &FileRefStore,
    mode: BranchListMode,
    color: bool,
    descending: bool,
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
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
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn branch_ref_objectname_sort_key(reference: &git_refs::Ref) -> String {
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
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
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
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn branch_ref_objecttype_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    reference: &git_refs::Ref,
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
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
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
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn branch_ref_objectsize_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    reference: &git_refs::Ref,
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
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
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
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn branch_ref_date_sort_key(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    reference: &git_refs::Ref,
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
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
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
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn branch_ref_upstream_sort_key(config: &GitConfig, reference: &git_refs::Ref) -> String {
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
    include: impl FnMut(&git_refs::Ref, &str) -> bool,
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
    print_branch_refs(refs, current.as_deref(), mode, color, include)
}

fn branch_ref_push_sort_key(config: &GitConfig, reference: &git_refs::Ref) -> String {
    for_each_ref_push(config, &reference.name)
        .and_then(|push| push.refname)
        .unwrap_or_default()
}

fn print_branch_refs(
    refs: Vec<git_refs::Ref>,
    current: Option<&str>,
    mode: BranchListMode,
    color: bool,
    mut include: impl FnMut(&git_refs::Ref, &str) -> bool,
) -> Result<()> {
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
            if color && marker == '*' {
                println!("{marker} \x1b[32m{name}\x1b[m");
            } else if color {
                println!("{marker} {name}\x1b[m");
            } else {
                println!("{marker} {name}");
            }
            continue;
        }
        if matches!(mode, BranchListMode::Remote | BranchListMode::All)
            && let Some(name) = reference.name.strip_prefix("refs/remotes/")
        {
            let display = if matches!(mode, BranchListMode::All) {
                format!("remotes/{name}")
            } else {
                name.to_string()
            };
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

fn print_branch_list_verbose(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    options: BranchVerboseListOptions,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let config = read_repo_config(git_dir)?;
    let current = store.current_branch_ref()?;
    let objectname_abbrev = repository_abbrev(git_dir, format)?;
    let objectname_candidates = cat_file_all_object_ids(git_dir, format)?;
    let mut rows = Vec::new();
    for reference in store.list_refs()? {
        let Some((display, pattern_name)) =
            branch_verbose_display_name(&reference.name, options.mode)
        else {
            continue;
        };
        if !branch_list_patterns_match(&options.patterns, &pattern_name, options.ignore_case) {
            continue;
        }
        let RefTarget::Direct(oid) = &reference.target else {
            continue;
        };
        let subject = branch_verbose_subject(&db, format, oid)?;
        let upstream = for_each_ref_upstream(&config, &reference.name);
        let upstream_track = upstream
            .as_ref()
            .map(|upstream| for_each_ref_upstream_track(store, &db, format, oid, &upstream.refname))
            .transpose()?
            .flatten();
        rows.push(BranchVerboseRow {
            display,
            oid: for_each_ref_abbrev_oid(oid, objectname_abbrev, &objectname_candidates),
            subject,
            is_head: current.as_deref() == Some(reference.name.as_str()),
            upstream,
            upstream_track,
        });
    }
    let width = rows.iter().map(|row| row.display.len()).max().unwrap_or(0);
    for row in rows {
        let marker = if row.is_head { '*' } else { ' ' };
        let tracking =
            branch_verbose_tracking(row.upstream.as_ref(), row.upstream_track, options.verbosity);
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
        (1, _, Some(track)) if track.ahead > 0 || track.behind > 0 => {
            let mut out = Vec::new();
            write_for_each_ref_track(&mut out, track, true).expect("write to vec");
            format!(" {}", String::from_utf8_lossy(&out))
        }
        (1, _, _) => String::new(),
        (_, Some(upstream), Some(track)) if track.ahead > 0 || track.behind > 0 => {
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

