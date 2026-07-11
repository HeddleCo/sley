//! `git branch` dispatcher.

use super::BranchCommandContext;
use super::branch_options::{
    setup_branch_create_options, setup_branch_delete_options, setup_branch_format_list_options,
    setup_branch_general_list_options, setup_branch_move_options,
    setup_branch_show_current_options, setup_branch_upstream_options,
    setup_branch_verbose_list_options,
};
use super::config::validate_autosetuprebase;
use super::create::run_branch_create_options;
use super::delete::{
    BranchDeleteMode, BranchDeleteOptions, delete_merged_branches, delete_remote_tracking_branches,
    force_delete_branches,
};
use super::list::{
    BranchListMode, print_branch_list, run_branch_format_list_options,
    run_branch_general_list_options, run_branch_verbose_list_options,
};
use super::move_::run_branch_move_options;
use super::positional::dispatch_branch_positional_args;
use super::upstream::run_branch_upstream_options;
use crate::*;

pub(crate) fn cmd_branch(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let context = BranchCommandContext::open(cli_session)?;
    let git_dir = context.git_dir();
    let format = context.format();
    let store = &context.refs;
    // git validates branch.autosetuprebase up front, so even a plain listing
    // fails on a malformed value (t3200 #145/#146).
    validate_autosetuprebase(&read_repo_config(git_dir)?)?;
    if let Some(option) = args
        .iter()
        .find_map(|arg| matches!(arg.as_str(), "--no-remotes" | "--no-all").then_some(arg))
    {
        eprintln!(
            "error: unknown option `{}`",
            option.trim_start_matches("--")
        );
        return Err(GitError::Exit(129));
    }
    if let Some(format_options) =
        setup_branch_format_list_options(git_dir, format, context.replace_objects, args)?
    {
        return run_branch_format_list_options(
            git_dir,
            format,
            store,
            context.replace_objects,
            format_options,
        );
    }
    if let Some(show_current) = setup_branch_show_current_options(args)? {
        if show_current {
            if let Some(branch) = store.current_branch()? {
                println!("{branch}");
            }
            return Ok(());
        }
        return print_branch_list(store, BranchListMode::Local);
    }
    if let Some(move_options) = setup_branch_move_options(args)? {
        return run_branch_move_options(git_dir, store, &context.config, move_options);
    }
    if let Some(upstream) = setup_branch_upstream_options(args)? {
        return run_branch_upstream_options(git_dir, store, context.replace_objects, upstream);
    }
    if branch_has_conflicting_action_modes(args) {
        eprintln!("fatal: options are incompatible");
        return Err(GitError::Exit(128));
    }
    if let Some(verbose) = setup_branch_verbose_list_options(args)? {
        return run_branch_verbose_list_options(
            git_dir,
            format,
            store,
            context.replace_objects,
            verbose,
        );
    }
    if let Some(delete) = setup_branch_delete_options(args)? {
        let BranchDeleteOptions {
            force,
            quiet,
            mode,
            branches,
        } = delete;
        return if matches!(mode, BranchDeleteMode::Remote) {
            delete_remote_tracking_branches(git_dir, format, store, &branches, quiet)
        } else if matches!(mode, BranchDeleteMode::All) {
            eprintln!("fatal: cannot use -a with -d");
            Err(GitError::Exit(128))
        } else if force {
            force_delete_branches(git_dir, format, store, &branches, quiet)
        } else {
            delete_merged_branches(
                git_dir,
                format,
                context.objects(),
                store,
                context.replace_objects,
                &branches,
                quiet,
            )
        };
    }
    if let Some(create) = setup_branch_create_options(args)? {
        return run_branch_create_options(
            git_dir,
            format,
            store,
            &context.config,
            context.replace_objects,
            create,
        );
    }
    if let Some(list) = setup_branch_general_list_options(git_dir, context.replace_objects, args)? {
        return run_branch_general_list_options(
            git_dir,
            format,
            store,
            context.replace_objects,
            list,
        );
    }
    dispatch_branch_positional_args(&context, args)
}
pub(super) fn branch_has_conflicting_action_modes(args: &[String]) -> bool {
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
