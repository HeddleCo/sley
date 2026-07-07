use crate::commands;
use crate::setup;
use crate::{apply_global_options, session, sley_core, GlobalConfigOverride};
use sley::{GitError, Result};
use sley_protocol::set_packet_trace_identity;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(crate) fn dispatch_with_aliases(
    args: &[String],
    global_config: &[GlobalConfigOverride],
    _alias_depth: usize,
) -> Result<()> {
    // git's `run_argv` loop: repeatedly expand the leading command name through
    // the `alias.*` namespace until it resolves to a built-in (or external)
    // command, tracking the expansion chain for loop detection.
    let mut args: Vec<String> = args.to_vec();
    let mut expanded_aliases: Vec<String> = Vec::new();
    for _ in 0..commands::alias::MAX_ALIAS_DEPTH {
        let Some(command) = args.first().cloned() else {
            return dispatch_command(&args, global_config);
        };
        // git's main-level dashed options/pseudo-commands (`--version`,
        // `--list-cmds=...`, `--exec-path`, ...) are handled by `handle_options`
        // before the `run_argv` alias loop; they are never alias or external
        // command names (alias keys cannot start with `-`). Dispatch them
        // directly so they reach `dispatch_command`'s option arms instead of
        // being misdiagnosed as an unknown external command.
        if command.starts_with('-') {
            return dispatch_command(&args, global_config);
        }
        // A name is alias-expandable when it is not a built-in, or it is a
        // *deprecated* built-in (which an alias is allowed to override).
        let try_alias = !commands::alias::is_builtin_command(&command)
            || commands::alias::is_deprecated_command(&command);
        if try_alias {
            match commands::alias::alias_lookup(&command)? {
                commands::alias::AliasLookup::None => {}
                commands::alias::AliasLookup::MissingValue(key) => {
                    eprintln!("error: missing value for '{key}'");
                    return Err(GitError::Exit(128));
                }
                commands::alias::AliasLookup::Value(alias_string) => {
                    trace2_run_dashed(&command, expanded_aliases.len());
                    // `git <alias> -h` prints what the alias resolves to.
                    if args.len() == 2 && args[1] == "-h" {
                        eprintln!("'{command}' is aliased to '{alias_string}'");
                    }
                    if let Some(shell) = alias_string.strip_prefix('!') {
                        trace2_alias(&command, &[shell.to_string()]);
                        trace2_alias_cmd_name("_run_shell_alias_", expanded_aliases.len());
                        trace2_alias_child_command(
                            "version",
                            "_run_shell_alias_",
                            expanded_aliases.len(),
                        );
                        return commands::alias::run_shell_alias(shell, &args[1..]);
                    }
                    let mut expanded = commands::alias::split_alias_value(&alias_string);
                    trace2_alias(&command, &expanded);
                    expanded.extend(args[1..].iter().cloned());
                    // An alias body may begin with global options (`-c`, `-C`,
                    // `--config-env`, ...); git re-parses those before the real
                    // subcommand. Fold any `-c`/`--config-env` and apply `-C`,
                    // then take the remaining argv (with the real command first)
                    // for recursion / loop detection.
                    let new_args = reapply_global_options(&expanded)?;
                    let Some(real_command) = new_args.first().cloned() else {
                        eprintln!("fatal: empty alias for {command}");
                        return Err(GitError::Exit(128));
                    };
                    if real_command == command {
                        eprintln!("fatal: recursive alias: {command}");
                        return Err(GitError::Exit(128));
                    }
                    if commands::alias::is_builtin_command(&real_command) {
                        trace2_alias_cmd_name("_run_git_alias_", expanded_aliases.len());
                        trace2_alias_child_command(
                            &real_command,
                            "_run_git_alias_",
                            expanded_aliases.len(),
                        );
                    }
                    expanded_aliases.push(command);
                    if let Some(seen) = expanded_aliases
                        .iter()
                        .position(|name| name == &real_command)
                    {
                        report_alias_loop(&expanded_aliases, seen);
                        return Err(GitError::Exit(128));
                    }
                    args = new_args;
                    continue;
                }
            }
        }
        if commands::alias::is_builtin_command(&command) {
            return dispatch_command(&args, global_config);
        }
        // Not a built-in and not an alias: try it as an external `git-<cmd>`,
        // falling back to git's "not a git command" diagnostic.
        return run_external_or_unknown(&command, &args);
    }
    // Backstop: exceeded the expansion-iteration limit without converging.
    eprintln!("fatal: alias loop detected");
    Err(GitError::Exit(128))
}

fn trace2_dashed_hierarchy(alias_depth: usize) -> String {
    std::iter::repeat_n("_run_dashed_", alias_depth + 1)
        .collect::<Vec<_>>()
        .join("/")
}

fn trace2_alias_hierarchy(kind: &str, alias_depth: usize) -> String {
    let mut parts = Vec::with_capacity(alias_depth + 2);
    parts.extend(std::iter::repeat_n("_run_dashed_", alias_depth + 1));
    parts.push(kind);
    parts.join("/")
}

fn trace2_run_dashed(command: &str, alias_depth: usize) {
    let hierarchy = trace2_dashed_hierarchy(alias_depth);
    sley_core::trace2::cmd_name("_run_dashed_", Some(&hierarchy));
    sley_core::trace2::child_start("dashed", &[format!("git-{command}")]);
}

fn trace2_alias(command: &str, argv: &[String]) {
    sley_core::trace2::alias(command, argv);
}

fn trace2_alias_cmd_name(kind: &str, alias_depth: usize) {
    let hierarchy = trace2_alias_hierarchy(kind, alias_depth);
    sley_core::trace2::cmd_name(kind, Some(&hierarchy));
}

fn trace2_alias_child_command(command: &str, alias_kind: &str, alias_depth: usize) {
    let hierarchy = format!(
        "{}/{}",
        trace2_alias_hierarchy(alias_kind, alias_depth),
        command
    );
    crate::trace2_emit_process_ancestry_at_depth(1, &["git"]);
    sley_core::trace2::cmd_name_at_depth(1, command, Some(&hierarchy));
    crate::trace2_emit_def_params_at_depth(1);
}

/// Re-parse leading global options on an expanded alias argv, applying them the
/// same way the top-level parser does, and return the remaining argv.
fn reapply_global_options(expanded: &[String]) -> Result<Vec<String>> {
    let nested = apply_global_options(expanded)?;
    session::merge_global_overrides(
        nested.git_dir.clone(),
        nested.work_tree.clone(),
        nested.attr_source.clone(),
        nested.bare,
        nested.lazy_fetch,
        nested.pathspec_flags,
    );
    Ok(nested.args.to_vec())
}

/// Print git's `alias loop detected` diagnostic for the accumulated expansion
/// chain, marking the already-seen entry with ` <==` and the latest with ` ==>`.
fn report_alias_loop(expanded_aliases: &[String], seen: usize) {
    let mut chain = String::new();
    let last = expanded_aliases.len().saturating_sub(1);
    for (index, name) in expanded_aliases.iter().enumerate() {
        chain.push_str("\n  ");
        chain.push_str(name);
        if index == seen {
            chain.push_str(" <==");
        } else if index == last {
            chain.push_str(" ==>");
        }
    }
    eprintln!(
        "fatal: alias loop detected: expansion of '{}' does not terminate:{}",
        expanded_aliases[0], chain
    );
}

/// Dispatch a non-built-in, non-alias command as an external `git-<cmd>`,
/// emitting git's `trace: run_command:` line and falling back to the
/// "not a git command" diagnostic when no such external exists.
fn run_external_or_unknown(command: &str, args: &[String]) -> Result<()> {
    let external = format!("git-{command}");
    let mut argv = Vec::with_capacity(args.len());
    argv.push(external.clone());
    argv.extend(args[1..].iter().cloned());
    trace2_run_dashed(command, 0);
    trace2_external_child_metadata(command);
    if setup::git_trace_enabled() {
        let mut line = String::from("trace: run_command:");
        for arg in &argv {
            line.push(' ');
            line.push_str(&setup::trace_quote_sq(arg));
        }
        setup::git_trace_line("run-command.c:672", &line);
    }
    if let Some(path) = locate_external_in_path(&external) {
        let mut process = std::process::Command::new(path);
        process.args(&args[1..]);
        process.env(
            "SLEY_TRACE2_DEPTH",
            (sley_core::trace2::depth() + 1).to_string(),
        );
        let status = process
            .status()
            .map_err(|err| GitError::Io(err.to_string()))?;
        return match status.code() {
            Some(0) => Ok(()),
            Some(code) => Err(GitError::Exit(code)),
            None => Err(GitError::Exit(1)),
        };
    }
    commands::help::unknown_command(command, 1)
}

fn trace2_external_child_metadata(command: &str) {
    let child_name = match command {
        "remote-http" | "remote-https" | "remote-ftp" | "remote-ftps" => "remote-curl",
        other => other,
    };
    sley_core::trace2::cmd_name_at_depth(1, child_name, None);
    crate::trace2_emit_def_params_at_depth(1);
}

/// Locate an executable `git-<cmd>` on `PATH` (git's `locate_in_PATH`), or
/// `None` when no such external command exists.
fn locate_external_in_path(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn dispatch_command(args: &[String], global_config: &[GlobalConfigOverride]) -> Result<()> {
    // git emits `trace: built-in: git <argv>` immediately before running a
    // builtin (git.c:502). Mirror it for the general GIT_TRACE key so harnesses
    // that read the trace (t3701 "add -p does not expand argument lists") find a
    // populated trace file and the run_command/built-in argv lines.
    if setup::git_trace_enabled() {
        let mut msg = String::from("trace: built-in: git");
        for arg in args {
            msg.push(' ');
            msg.push_str(&setup::trace_quote_sq(arg));
        }
        setup::git_trace_line("git.c:502", &msg);
    }
    let Some(command) = args.first().map(String::as_str) else {
        commands::help::print_common_help();
        return Err(GitError::Exit(1));
    };
    // GIT_TRACE_PACKET identity: git's `packet_trace_identity` is the running
    // program's basename (the subcommand here). The transports' pkt-line traces
    // are prefixed `packet: %12s` with this value (e.g. `ls-remote`, `clone`,
    // `fetch`, `upload-pack`).
    set_packet_trace_identity(command);
    let trace2_command_name = match command {
        "--exec-path" | "--html-path" | "--man-path" | "--info-path" | "--list-cmds=builtins" => {
            "_query_"
        }
        "-v" | "--version" => "version",
        _ => command,
    };
    sley_core::trace2::cmd_name(trace2_command_name, None);
    if command != "help"
        && args.len() == 2
        && args.get(1).is_some_and(|arg| arg == "-h")
        && commands::help::is_builtin_command(command)
        && !commands::help::has_command_specific_help(command)
    {
        commands::help::print_command_usage(command);
        return Err(GitError::Exit(129));
    }
    match command {
        "help" => commands::help::cmd_help(&args[1..]),
        "--exec-path" => cmd_exec_path(),
        "--html-path" | "--man-path" | "--info-path" => cmd_info_path(),
        "--list-cmds=builtins" => {
            commands::help::print_builtin_commands();
            Ok(())
        }
        "init" => commands::plumbing::cmd_init(&args[1..], global_config),
        "add" => commands::plumbing::cmd_add(&args[1..]),
        "archive" => commands::plumbing::cmd_archive(&args[1..]),
        "branch" => commands::branch::cmd_branch(&args[1..]),
        "bundle" => commands::plumbing::cmd_bundle(&args[1..]),
        "hash-object" => commands::hash_object::cmd_hash_object(&args[1..]),
        "index-pack" => commands::pack::cmd_index_pack(&args[1..]),
        "pack-objects" => commands::pack_objects::cmd_pack_objects(&args[1..]),
        "cat-file" => commands::cat_file::cmd_cat_file(&args[1..]),
        "checkout" => commands::checkout::cmd_checkout(&args[1..]),
        "check-attr" => commands::attrs::cmd_check_attr(&args[1..]),
        "check-ignore" => commands::attrs::cmd_check_ignore(&args[1..]),
        "check-mailmap" => commands::utility::cmd_check_mailmap(&args[1..]),
        "check-ref-format" => commands::utility::cmd_check_ref_format(&args[1..]),
        "clean" => commands::plumbing::cmd_clean(&args[1..]),
        "clone" => commands::remote::cmd_clone(&args[1..]),
        "config" => commands::config_cmd::cmd_config(&args[1..]),
        "count-objects" => commands::pack::cmd_count_objects(&args[1..]),
        "gc" => commands::pack::cmd_gc(&args[1..]),
        "maintenance" => commands::pack::cmd_maintenance(&args[1..]),
        "repack" => commands::pack::cmd_repack(&args[1..]),
        "pack-redundant" => commands::pack::cmd_pack_redundant(&args[1..]),
        "repo" => commands::utility::cmd_repo(&args[1..]),
        "apply" => commands::plumbing::cmd_apply(&args[1..]),
        "commit" => commands::commit::cmd_commit(&args[1..]),
        "commit-graph" => commands::plumbing::cmd_commit_graph(&args[1..]),
        "commit-tree" => commands::plumbing::cmd_commit_tree(&args[1..]),
        "diff" => commands::diff::cmd_diff(&args[1..]),
        "range-diff" => commands::range_diff::cmd_range_diff(&args[1..]),
        "difftool" => commands::difftool::cmd_difftool(&args[1..]),
        "fetch" => commands::remote::cmd_fetch(&args[1..]),
        "for-each-ref" => commands::for_each_ref::cmd_for_each_ref(&args[1..]),
        "for-each-repo" => commands::for_each_repo::cmd_for_each_repo(&args[1..]),
        "refs" => commands::refs::cmd_refs(&args[1..]),
        "fsck" => commands::plumbing::cmd_fsck(&args[1..]),
        "get-tar-commit-id" => commands::utility::cmd_get_tar_commit_id(&args[1..]),
        "ls-remote" => commands::remote::cmd_ls_remote(&args[1..]),
        "ls-files" => commands::index::cmd_ls_files(&args[1..]),
        "ls-tree" => commands::index::cmd_ls_tree(&args[1..]),
        "log" => commands::log::cmd_log(&args[1..]),
        "whatchanged" => commands::log::cmd_whatchanged(&args[1..]),
        "merge" => commands::merge_rebase::cmd_merge(&args[1..]),
        "merge-base" => commands::merge_rebase::cmd_merge_base(&args[1..]),
        "merge-recursive" => commands::merge_rebase::cmd_merge_recursive(&args[1..]),
        "fmt-merge-msg" => commands::merge_rebase::cmd_fmt_merge_msg(&args[1..]),
        "mergetool" => commands::mergetool::cmd_mergetool(&args[1..]),
        "pull" => {
            // `-s`/`--strategy` pulls take a narrow dedicated path; the general
            // pull implementation rejects the option.
            if commands::pull_strategy::pull_has_strategy_option(&args[1..]) {
                commands::pull_strategy::cmd_pull_with_strategy(&args[1..])
            } else {
                commands::merge_rebase::cmd_pull(&args[1..])
            }
        }
        "replay" => commands::replay::cmd_replay(&args[1..]),
        "rebase" => commands::rebase::cmd_rebase(&args[1..]),
        "cherry-pick" => commands::replay::cmd_cherry_pick(&args[1..]),
        "revert" => commands::replay::cmd_revert(&args[1..]),
        "mktree" => commands::index::cmd_mktree(&args[1..]),
        "multi-pack-index" => commands::pack::cmd_multi_pack_index(&args[1..]),
        "mv" => commands::plumbing::cmd_mv(&args[1..]),
        "pack-refs" => commands::pack::cmd_pack_refs(&args[1..]),
        "prune" => commands::pack::cmd_prune(&args[1..]),
        "prune-packed" => commands::plumbing::cmd_prune_packed(&args[1..]),
        "push" => commands::remote::cmd_push(&args[1..]),
        "send-pack" => commands::remote::cmd_send_pack(&args[1..]),
        "fetch-pack" => commands::fetch_pack::cmd_fetch_pack(&args[1..]),
        "filter-branch" => commands::filter_branch::cmd_filter_branch(&args[1..]),
        "unpack-objects" => commands::pack::cmd_unpack_objects(&args[1..]),
        "receive-pack" => commands::remote::cmd_receive_pack(&args[1..]),
        "upload-pack" => commands::remote::cmd_upload_pack(&args[1..]),
        "daemon" => commands::daemon::cmd_daemon(&args[1..]),
        "write-tree" => commands::trees::cmd_write_tree(&args[1..]),
        "worktree" => commands::worktree::cmd_worktree(&args[1..]),
        "update-index" => commands::index::cmd_update_index(&args[1..]),
        "update-ref" => commands::refs::cmd_update_ref(&args[1..]),
        "rev-parse" => commands::rev_parse::cmd_rev_parse(&args[1..]),
        "rev-list" => commands::rev_list::cmd_rev_list(&args[1..]),
        "reflog" => commands::refs::cmd_reflog(&args[1..]),
        "remote" => commands::remote::cmd_remote(&args[1..]),
        "replace" => commands::plumbing::cmd_replace(&args[1..]),
        "rerere" => commands::rerere::cmd_rerere(&args[1..]),
        "reset" => commands::reset::cmd_reset(&args[1..]),
        "restore" => commands::checkout::cmd_restore(&args[1..]),
        "rm" => commands::plumbing::cmd_rm(&args[1..]),
        "show-ref" => commands::refs::cmd_show_ref(&args[1..]),
        "show-index" => commands::utility::cmd_show_index(&args[1..]),
        "stripspace" => commands::utility::cmd_stripspace(&args[1..]),
        "stash" => commands::stash::cmd_stash(&args[1..]),
        "submodule" => commands::submodule::cmd_submodule(&args[1..]),
        "submodule--helper" => commands::submodule::cmd_submodule_helper(&args[1..]),
        "symbolic-ref" => commands::refs::cmd_symbolic_ref(&args[1..]),
        "status" => commands::status::cmd_status(&args[1..]),
        "switch" => commands::checkout::cmd_switch(&args[1..]),
        "tag" => commands::tag::cmd_tag(&args[1..]),
        "testkit" => commands::utility::cmd_testkit(&args[1..]),
        "unpack-file" => commands::utility::cmd_unpack_file(&args[1..]),
        "update-server-info" => commands::refs::cmd_update_server_info(&args[1..]),
        "var" => commands::utility::cmd_var(&args[1..]),
        "verify-pack" => commands::pack::cmd_verify_pack(&args[1..]),
        "version" => commands::utility::cmd_version(&args[1..]),
        "-v" | "--version" => commands::utility::cmd_version(&[]),
        "show" => commands::show::cmd_show(&args[1..]),
        "blame" => commands::blame::cmd_blame(&args[1..]),
        "annotate" => commands::blame::cmd_annotate(&args[1..]),
        "bugreport" => commands::utility::cmd_bugreport(&args[1..]),
        "describe" => commands::describe::cmd_describe(&args[1..]),
        "diagnose" => commands::diagnose::cmd_diagnose(&args[1..]),
        "shortlog" => commands::shortlog::cmd_shortlog(&args[1..]),
        "grep" => commands::grep::cmd_grep(&args[1..]),
        "last-modified" => commands::last_modified::cmd_last_modified(&args[1..]),
        "hook" => commands::hooks::cmd_hook(&args[1..]),
        "notes" => commands::notes::cmd_notes(&args[1..]),
        "bisect" => commands::bisect::cmd_bisect(&args[1..]),
        "sparse-checkout" => commands::sparse_checkout::cmd_sparse_checkout(&args[1..]),
        "format-patch" => commands::format_patch::cmd_format_patch(&args[1..]),
        "format-rev" => commands::format_rev::cmd_format_rev(&args[1..]),
        "am" => commands::am::cmd_am(&args[1..]),
        "read-tree" => commands::read_tree::cmd_read_tree(&args[1..]),
        "checkout-index" => commands::checkout_index::cmd_checkout_index(&args[1..]),
        "diff-tree" => commands::diff_tree::cmd_diff_tree(&args[1..]),
        "diff-index" => commands::diff_index::cmd_diff_index(&args[1..]),
        "diff-files" => commands::diff_files::cmd_diff_files(&args[1..]),
        "fast-export" => commands::fast_export::cmd_fast_export(&args[1..]),
        "fast-import" => commands::fast_import::cmd_fast_import(&args[1..]),
        #[cfg(feature = "git-compat-i18n")]
        "sh-i18n--envsubst" => cmd_sh_i18n_envsubst(&args[1..]),
        "merge-tree" => commands::merge_tree::cmd_merge_tree(&args[1..]),
        "merge-file" => commands::merge_file::cmd_merge_file(&args[1..]),
        "merge-index" => commands::merge_index::cmd_merge_index(&args[1..]),
        "name-rev" => commands::name_rev::cmd_name_rev(&args[1..]),
        "show-branch" => commands::show_branch::cmd_show_branch(&args[1..]),
        "verify-commit" => commands::verify_commit::cmd_verify_commit(&args[1..]),
        "verify-tag" => commands::verify_tag::cmd_verify_tag(&args[1..]),
        "mktag" => commands::mktag::cmd_mktag(&args[1..]),
        "patch-id" => commands::patch_id::cmd_patch_id(&args[1..]),
        "interpret-trailers" => commands::interpret_trailers::cmd_interpret_trailers(&args[1..]),
        _ => commands::help::unknown_command(command, 1),
    }
}

fn cmd_exec_path() -> Result<()> {
    println!("{}", git_exec_path()?.display());
    Ok(())
}

fn cmd_info_path() -> Result<()> {
    println!("{}", git_exec_path()?.display());
    Ok(())
}

fn git_exec_path() -> Result<PathBuf> {
    #[cfg(feature = "git-compat-i18n")]
    {
        // When another sley process probes us via `git --exec-path` to discover
        // a *system* git's exec dir, refuse to answer instead of materializing
        // (which itself probes PATH and would recurse without bound). Exiting
        // non-zero here makes the probing parent reject this candidate — sley is
        // not a system git worth borrowing exec helpers from.
        if env::var_os(sley_i18n::EXEC_PATH_PROBE_ENV).is_some() {
            return Err(GitError::Exit(1));
        }
        sley_i18n::materialize_git_i18n_helpers().map_err(|err| GitError::Io(err.to_string()))
    }
    #[cfg(not(feature = "git-compat-i18n"))]
    {
        if let Ok(exe) = env::current_exe()
            && let Some(parent) = exe.parent()
        {
            return Ok(parent.to_path_buf());
        }
        env::current_dir().map_err(|err| GitError::Io(err.to_string()))
    }
}

#[cfg(feature = "git-compat-i18n")]
fn cmd_sh_i18n_envsubst(args: &[String]) -> Result<()> {
    if args.len() == 2 && args[0] == "--variables" {
        for variable in sley_i18n::envsubst_variables(&args[1]) {
            println!("{variable}");
        }
        return Ok(());
    }
    if args.len() != 1 {
        eprintln!("fatal: first argument must be --variables when two are given");
        return Err(GitError::Exit(128));
    }
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let expanded = sley_i18n::envsubst(&input, &args[0], |name| env::var(name).ok());
    print!("{expanded}");
    Ok(())
}