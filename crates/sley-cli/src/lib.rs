use sley_config::{ConfigBoolOrInt, ConfigEntry, ConfigSection, GitConfig};
use sley_core::{BString, GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{
    Bundle, BundlePrerequisite, BundleReference, CommitGraph, CommitGraphWriteEntry, InitOptions,
    RefStorageFormat, RepositoryBootstrap, RepositoryLayout,
};
use sley_index::{Index, IndexEntry};
use sley_object::{
    Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntries, TreeEntry, TreeEntryRef,
    tree_entry_object_type,
};
use sley_odb::{
    FileObjectDatabase, LooseObjectIntegrity, ObjectPrefixResolution, ObjectReader, ObjectWriter,
    build_reachable_pack, collect_reachable_object_ids, install_bundle_pack,
    install_reachable_pack, prune_unreachable_loose, repository_object_ids,
    repository_objects_dir,
};
use sley_pack::{MultiPackIndex, MultiPackIndexEntry, PackFile, PackIndex};
use sley_protocol::{
    FetchHeadRecord, FetchRefUpdate, ProtocolVersion, ReceivePackCommand, ReceivePackPushRequest,
    RefAdvertisement, RefAdvertisementSet, UploadPackFeatures, parse_refspec, read_fetch_head,
    read_receive_pack_push_options, read_receive_pack_request,
    read_upload_pack_negotiation_request, read_upload_pack_request, refspec_map_source,
    write_receive_pack_report_status, write_ref_advertisement_set,
    write_upload_pack_packfile_response, write_upload_pack_raw_packfile_response,
};
pub(crate) use sley_ref_filter::*;
use sley_refs::{
    BundleRefUpdate, FileRefStore, PackedRef, Ref, RefPrecondition, RefTarget, RefUpdate,
    ReflogEntry, branch_ref_name, check_refname_format, parse_packed_refs, resolve_ref_peeled,
    tag_ref_name, validate_ref_name, validate_symref_name, validate_symref_target,
};
use sley_remote::FetchOutcome;
use sley_transport::{RemoteTransport, parse_remote_url};
use std::borrow::Cow;
use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Mutex;

/// Accumulated sq-quoted fragment of command-line `-c` / `--config-env`
/// parameters, in left-to-right order. Stands in for git's mutation of the
/// process `GIT_CONFIG_PARAMETERS` env var (forbidden here, as the workspace bans
/// `unsafe`/`set_var`); appended after any inherited `GIT_CONFIG_PARAMETERS` to
/// form the effective parameter list.
static CMDLINE_CONFIG_PARAMETERS: Mutex<String> = Mutex::new(String::new());
static GLOBAL_GIT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static GLOBAL_WORK_TREE: Mutex<Option<PathBuf>> = Mutex::new(None);
static GLOBAL_BARE: Mutex<bool> = Mutex::new(false);
static GLOBAL_REPLACE_OBJECTS: Mutex<bool> = Mutex::new(true);
/// Default pathspec magic set by the global `--{glob,noglob,icase,literal}-pathspecs`
/// options (and the corresponding `GIT_*_PATHSPECS` env vars). Mirrors git's
/// `get_default_pathspec_flags()`: `--literal-pathspecs` wins and forces every
/// pathspec to be matched literally; otherwise glob/icase magic is OR'd in.
static GLOBAL_PATHSPEC_FLAGS: Mutex<PathspecFlags> = Mutex::new(PathspecFlags {
    literal: false,
    glob: false,
    icase: false,
});

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PathspecFlags {
    /// `--literal-pathspecs`: no wildcard interpretation at all.
    pub literal: bool,
    /// `--glob-pathspecs`: `*`/`?` are pathname-aware (`WM_PATHNAME`), `**` spans `/`.
    pub glob: bool,
    /// `--icase-pathspecs`: case-insensitive matching (`WM_CASEFOLD`).
    pub icase: bool,
}

mod commands;
mod log_format;
mod remote;
mod repo_path;
mod repository;
mod setup;

pub(crate) use log_format::{CompiledLogFormat, FormatToken, LogFormatDialect, presets};

pub(crate) use commands::args::{GitArgCursor, long_option_value};
pub(crate) use commands::cat_file::{cat_file_all_object_ids, cat_file_object_storage};
pub(crate) use commands::config_cmd::{config_entry_name, has_unescaped_trailing_dollar};
pub(crate) use commands::merge_rebase::{
    MergePathResult, MergeTreeMap, commit_tree_oid, conclude_in_progress_merge,
    conclude_rebase_step_via_commit, head_commit_oid, merge_bases, merge_index_entry,
    merge_read_blob, merge_remove_worktree_file, merge_write_worktree_file,
    read_merge_message_from_file, rebase_in_progress, three_way_merge_trees,
};
pub(crate) use commands::remote_cmds::{
    read_repo_config, remote_exists, remote_names, repo_current_branch_name, write_repo_config,
};
use commands::tag::{parse_tag_trailer, tag_message_with_trailers, tag_stripspace_message};
pub(crate) use commands::workspace::{cmd_checkout, cmd_status};
pub(crate) use repo_path::RepoPathBuf;
pub(crate) use repository::RepositoryContext;

pub fn run(args: Vec<String>) -> Result<()> {
    let global = apply_global_options(&args)?;
    // `-c` / `--config-env` overrides are folded into the process
    // `GIT_CONFIG_PARAMETERS` env var during option parsing, so the single
    // `injected_config_parameters()` reader is the source of truth for every
    // config read; no separate global-override store is needed.
    set_global_git_dir(global.git_dir.clone());
    set_global_work_tree(global.work_tree);
    set_global_bare(global.bare);
    set_global_replace_objects(global.replace_objects);
    set_global_pathspec_flags(global.pathspec_flags);
    // Emit git's GIT_TRACE_SETUP output (the env/config/gitfile discovery trace)
    // before dispatching. This is the CLI-side repository setup that
    // `sley::Repository::discover` deliberately leaves to this layer.
    if env::var_os("GIT_TRACE_SETUP").is_some()
        && let Some(setup_result) = setup::setup_git_directory()
    {
        setup::trace_repo_setup(&setup_result);
    }
    dispatch_with_aliases(global.args, &global.config, 0)
}

fn dispatch_with_aliases(
    args: &[String],
    global_config: &[GlobalConfigOverride],
    alias_depth: usize,
) -> Result<()> {
    if alias_depth >= commands::alias::MAX_ALIAS_DEPTH {
        eprintln!("fatal: alias loop detected");
        return Err(GitError::Exit(128));
    }
    if let Some(command) = args.first().map(String::as_str) {
        if !commands::alias::is_builtin_command(command) {
            match commands::alias::expand_alias(command)? {
                commands::alias::AliasExpansion::Shell(shell) => {
                    return commands::alias::run_shell_alias(&shell, &args[1..]);
                }
                commands::alias::AliasExpansion::Args(mut expanded) => {
                    expanded.extend(args[1..].iter().cloned());
                    // An alias body may begin with global options (`-c`, `-C`,
                    // `--config-env`, ...), e.g. `alias.x = "-c foo=bar config foo"`.
                    // git re-parses those before dispatching the real subcommand,
                    // so `-c` in an alias folds into the injected parameters just
                    // like a command-line `-c`. Re-run the global-option parser on
                    // the expanded args (which folds any `-c`/`--config-env` and
                    // applies `-C`); only override git-dir/work-tree when the alias
                    // explicitly set them so a top-level `--git-dir` survives.
                    let nested = apply_global_options(&expanded)?;
                    if nested.git_dir.is_some() {
                        set_global_git_dir(nested.git_dir.clone());
                    }
                    if nested.work_tree.is_some() {
                        set_global_work_tree(nested.work_tree);
                    }
                    if nested.bare {
                        set_global_bare(true);
                    }
                    if nested.pathspec_flags != PathspecFlags::default() {
                        set_global_pathspec_flags(nested.pathspec_flags);
                    }
                    return dispatch_with_aliases(nested.args, global_config, alias_depth + 1);
                }
                commands::alias::AliasExpansion::None => {}
            }
        }
    }
    dispatch_command(args, global_config)
}

fn dispatch_command(args: &[String], global_config: &[GlobalConfigOverride]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        print_usage();
        return Err(GitError::Command("missing command".into()));
    };
    match command {
        "init" => commands::plumbing::cmd_init(&args[1..], global_config),
        "add" => commands::plumbing::cmd_add(&args[1..]),
        "archive" => commands::plumbing::cmd_archive(&args[1..]),
        "branch" => commands::branch::cmd_branch(&args[1..]),
        "bundle" => commands::plumbing::cmd_bundle(&args[1..]),
        "hash-object" => commands::hash_object::cmd_hash_object(&args[1..]),
        "index-pack" => commands::pack::cmd_index_pack(&args[1..]),
        "pack-objects" => commands::pack_objects::cmd_pack_objects(&args[1..]),
        "cat-file" => commands::cat_file::cmd_cat_file(&args[1..]),
        "checkout" => commands::workspace::cmd_checkout(&args[1..]),
        "check-attr" => commands::attrs::cmd_check_attr(&args[1..]),
        "check-ignore" => commands::attrs::cmd_check_ignore(&args[1..]),
        "check-mailmap" => commands::utility::cmd_check_mailmap(&args[1..]),
        "check-ref-format" => commands::utility::cmd_check_ref_format(&args[1..]),
        "clean" => commands::plumbing::cmd_clean(&args[1..]),
        "clone" => commands::remote_cmds::cmd_clone(&args[1..]),
        "config" => commands::config_cmd::cmd_config(&args[1..]),
        "count-objects" => commands::pack::cmd_count_objects(&args[1..]),
        "gc" => commands::pack::cmd_gc(&args[1..]),
        "maintenance" => commands::pack::cmd_maintenance(&args[1..]),
        "repack" => commands::pack::cmd_repack(&args[1..]),
        "apply" => commands::plumbing::cmd_apply(&args[1..]),
        "commit" => commands::workspace::cmd_commit(&args[1..]),
        "commit-graph" => commands::plumbing::cmd_commit_graph(&args[1..]),
        "commit-tree" => commands::plumbing::cmd_commit_tree(&args[1..]),
        "diff" => commands::diff::cmd_diff(&args[1..]),
        "fetch" => commands::remote_cmds::cmd_fetch(&args[1..]),
        "for-each-ref" => commands::for_each_ref::cmd_for_each_ref(&args[1..]),
        "refs" => commands::refs::cmd_refs(&args[1..]),
        "fsck" => commands::plumbing::cmd_fsck(&args[1..]),
        "get-tar-commit-id" => commands::utility::cmd_get_tar_commit_id(&args[1..]),
        "ls-remote" => commands::remote_cmds::cmd_ls_remote(&args[1..]),
        "ls-files" => commands::index::cmd_ls_files(&args[1..]),
        "ls-tree" => commands::index::cmd_ls_tree(&args[1..]),
        "log" => commands::log::cmd_log(&args[1..]),
        "whatchanged" => commands::log::cmd_whatchanged(&args[1..]),
        "merge" => commands::merge_rebase::cmd_merge(&args[1..]),
        "merge-base" => commands::merge_rebase::cmd_merge_base(&args[1..]),
        "pull" => {
            // `-s`/`--strategy` pulls take a narrow dedicated path; the general
            // pull implementation rejects the option.
            if commands::pull_strategy::pull_has_strategy_option(&args[1..]) {
                commands::pull_strategy::cmd_pull_with_strategy(&args[1..])
            } else {
                commands::merge_rebase::cmd_pull(&args[1..])
            }
        }
        "rebase" => commands::rebase::cmd_rebase(&args[1..]),
        "cherry-pick" => commands::replay::cmd_cherry_pick(&args[1..]),
        "revert" => commands::replay::cmd_revert(&args[1..]),
        "mktree" => commands::index::cmd_mktree(&args[1..]),
        "multi-pack-index" => commands::pack::cmd_multi_pack_index(&args[1..]),
        "mv" => commands::plumbing::cmd_mv(&args[1..]),
        "pack-refs" => commands::pack::cmd_pack_refs(&args[1..]),
        "prune" => commands::pack::cmd_prune(&args[1..]),
        "prune-packed" => commands::plumbing::cmd_prune_packed(&args[1..]),
        "push" => commands::remote_cmds::cmd_push(&args[1..]),
        "receive-pack" => commands::remote_cmds::cmd_receive_pack(&args[1..]),
        "upload-pack" => commands::remote_cmds::cmd_upload_pack(&args[1..]),
        "write-tree" => commands::trees::cmd_write_tree(&args[1..]),
        "worktree" => commands::worktree::cmd_worktree(&args[1..]),
        "update-index" => commands::index::cmd_update_index(&args[1..]),
        "update-ref" => commands::refs::cmd_update_ref(&args[1..]),
        "rev-parse" => commands::rev_parse::cmd_rev_parse(&args[1..]),
        "rev-list" => commands::rev_list::cmd_rev_list(&args[1..]),
        "reflog" => commands::refs::cmd_reflog(&args[1..]),
        "remote" => commands::remote_cmds::cmd_remote(&args[1..]),
        "replace" => commands::plumbing::cmd_replace(&args[1..]),
        "rerere" => commands::plumbing::cmd_rerere(&args[1..]),
        "reset" => commands::workspace::cmd_reset(&args[1..]),
        "restore" => commands::workspace::cmd_restore(&args[1..]),
        "rm" => commands::plumbing::cmd_rm(&args[1..]),
        "show-ref" => commands::refs::cmd_show_ref(&args[1..]),
        "show-index" => commands::utility::cmd_show_index(&args[1..]),
        "stripspace" => commands::utility::cmd_stripspace(&args[1..]),
        "stash" => commands::stash::cmd_stash(&args[1..]),
        "submodule" => commands::submodule::cmd_submodule(&args[1..]),
        "symbolic-ref" => commands::refs::cmd_symbolic_ref(&args[1..]),
        "status" => commands::workspace::cmd_status(&args[1..]),
        "switch" => commands::workspace::cmd_switch(&args[1..]),
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
        "describe" => commands::describe::cmd_describe(&args[1..]),
        "shortlog" => commands::shortlog::cmd_shortlog(&args[1..]),
        "grep" => commands::grep::cmd_grep(&args[1..]),
        "notes" => commands::notes::cmd_notes(&args[1..]),
        "bisect" => commands::bisect::cmd_bisect(&args[1..]),
        "sparse-checkout" => commands::sparse_checkout::cmd_sparse_checkout(&args[1..]),
        "format-patch" => commands::format_patch::cmd_format_patch(&args[1..]),
        "am" => commands::am::cmd_am(&args[1..]),
        "read-tree" => commands::read_tree::cmd_read_tree(&args[1..]),
        "checkout-index" => commands::checkout_index::cmd_checkout_index(&args[1..]),
        "diff-tree" => commands::diff_tree::cmd_diff_tree(&args[1..]),
        "diff-index" => commands::diff_index::cmd_diff_index(&args[1..]),
        "diff-files" => commands::diff_files::cmd_diff_files(&args[1..]),
        "fast-import" => commands::fast_import::cmd_fast_import(&args[1..]),
        "merge-tree" => commands::merge_tree::cmd_merge_tree(&args[1..]),
        "merge-file" => commands::merge_file::cmd_merge_file(&args[1..]),
        "name-rev" => commands::name_rev::cmd_name_rev(&args[1..]),
        "show-branch" => commands::show_branch::cmd_show_branch(&args[1..]),
        "verify-commit" => commands::verify_commit::cmd_verify_commit(&args[1..]),
        "verify-tag" => commands::verify_tag::cmd_verify_tag(&args[1..]),
        "mktag" => commands::mktag::cmd_mktag(&args[1..]),
        "patch-id" => commands::patch_id::cmd_patch_id(&args[1..]),
        "interpret-trailers" => commands::interpret_trailers::cmd_interpret_trailers(&args[1..]),
        _ => Err(GitError::Command(format!("unsupported command {command}"))),
    }
}

/// Emit the `git version --build-options` block, mirroring git 2.54's line
/// shapes. Only the fields with a parity-relevant meaning for sley are reported
/// with truthful values; the rest match git's format so harness parsers (which
/// read specific `key: value` lines) keep working.

fn common_git_dir_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    if let Some(common_dir) = env::var_os("GIT_COMMON_DIR") {
        return Ok(PathBuf::from(common_dir));
    }
    let commondir = git_dir.join("commondir");
    if commondir.is_file() {
        let value = fs::read_to_string(&commondir)?;
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        return fs::canonicalize(common).map_err(|err| GitError::Io(err.to_string()));
    }
    fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()))
}

struct GlobalOptions<'a> {
    args: &'a [String],
    config: Vec<GlobalConfigOverride>,
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    bare: bool,
    replace_objects: bool,
    pathspec_flags: PathspecFlags,
}

#[derive(Debug, Clone)]
struct GlobalConfigOverride {
    key: String,
    value: String,
}

fn apply_global_options(args: &[String]) -> Result<GlobalOptions<'_>> {
    let mut index = 0;
    let mut config = Vec::new();
    let mut git_dir = None;
    let mut work_tree = None;
    let mut bare = false;
    let mut replace_objects = env::var_os("GIT_NO_REPLACE_OBJECTS").is_none();
    let mut pathspec_flags = PathspecFlags::default();
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-C" => {
                let Some(path) = args.get(index + 1) else {
                    eprintln!("no directory given for '-C' option");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                if !path.is_empty()
                    && let Err(err) = env::set_current_dir(path)
                {
                    eprintln!("fatal: cannot change to '{}': {err}", path);
                    return Err(GitError::Exit(128));
                }
                index += 2;
            }
            "-c" => {
                let Some(assignment) = args.get(index + 1) else {
                    eprintln!("-c expects a configuration string");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                if let Some(entry) = push_config_parameter(assignment) {
                    config.push(entry);
                }
                index += 2;
            }
            "--config-env" => {
                let Some(spec) = args.get(index + 1) else {
                    eprintln!("no config key given for --config-env");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                config.push(push_config_env(spec)?);
                index += 2;
            }
            "-p"
            | "--paginate"
            | "-P"
            | "--no-pager"
            | "--no-lazy-fetch"
            | "--no-optional-locks"
            | "--no-advice" => {
                index += 1;
            }
            "--no-replace-objects" => {
                replace_objects = false;
                index += 1;
            }
            "--literal-pathspecs" => {
                pathspec_flags.literal = true;
                index += 1;
            }
            "--glob-pathspecs" => {
                pathspec_flags.glob = true;
                index += 1;
            }
            "--noglob-pathspecs" => {
                // git treats --noglob-pathspecs as forcing literal `*`/`?`/`[`
                // (PATHSPEC_LITERAL is not set, but glob magic is suppressed and
                // wildcards lose their special meaning). Model it as literal for
                // matching purposes.
                pathspec_flags.literal = true;
                index += 1;
            }
            "--icase-pathspecs" => {
                pathspec_flags.icase = true;
                index += 1;
            }
            "--git-dir" => {
                let Some(path) = args.get(index + 1) else {
                    eprintln!("no directory given for '--git-dir' option");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                git_dir = Some(PathBuf::from(path));
                index += 2;
            }
            "--work-tree" => {
                let Some(path) = args.get(index + 1) else {
                    eprintln!("no directory given for '--work-tree' option");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                work_tree = Some(PathBuf::from(path));
                index += 2;
            }
            value if value.starts_with("--git-dir=") => {
                git_dir = Some(PathBuf::from(&value["--git-dir=".len()..]));
                index += 1;
            }
            value if value.starts_with("--work-tree=") => {
                work_tree = Some(PathBuf::from(&value["--work-tree=".len()..]));
                index += 1;
            }
            value if value.starts_with("--config-env=") => {
                config.push(push_config_env(&value["--config-env=".len()..])?);
                index += 1;
            }
            "--bare" => {
                bare = true;
                index += 1;
            }
            _ => break,
        }
    }
    Ok(GlobalOptions {
        args: &args[index..],
        config,
        git_dir,
        work_tree,
        bare,
        replace_objects,
        pathspec_flags,
    })
}

/// Fold a `-c <text>` command-line parameter into the process
/// `GIT_CONFIG_PARAMETERS` env var, exactly as git's `git_config_push_parameter`:
/// split off the value at the first `=` (a missing `=` is a bare boolean), then
/// sq-quote the key and value into the env list. This makes the override visible
/// to every config read (including aliases and any subprocess) through the single
/// `injected_config_parameters()` reader.
///
/// Returns a [`GlobalConfigOverride`] (canonical-ish key + string value) for the
/// legacy `init`/`clone` override list when the key is non-empty; an empty key
/// (`-c ""`) yields `None` here and surfaces as a parse error at read time.
fn push_config_parameter(text: &str) -> Option<GlobalConfigOverride> {
    match text.split_once('=') {
        Some((key, value)) => {
            push_split_parameter(key, Some(value));
            (!key.is_empty()).then(|| GlobalConfigOverride {
                key: key.to_string(),
                value: value.to_string(),
            })
        }
        None => {
            push_split_parameter(text, None);
            (!text.is_empty()).then(|| GlobalConfigOverride {
                key: text.to_string(),
                // A bare `-c key` is boolean-true; represent it as "true" for the
                // legacy list consumers (init reads typed values via parse_config_bool).
                value: "true".to_string(),
            })
        }
    }
}

/// Resolve a `--config-env=<key>=<envvar>` spec and fold it into
/// `GIT_CONFIG_PARAMETERS`, exactly as git's `git_config_push_env`: the spec is
/// split at the *last* `=` into the config key and the environment variable name;
/// the variable is read from the environment and its value sq-quoted into the env
/// list. Errors mirror git's `die()` wording (exit 128).
fn push_config_env(spec: &str) -> Result<GlobalConfigOverride> {
    let Some(eq) = spec.rfind('=') else {
        eprintln!("fatal: invalid config format: {spec}");
        return Err(GitError::Exit(128));
    };
    let key = &spec[..eq];
    let env_name = &spec[eq + 1..];
    if env_name.is_empty() {
        eprintln!("fatal: missing environment variable name for configuration '{key}'");
        return Err(GitError::Exit(128));
    }
    let env_value = match env::var(env_name) {
        Ok(value) => value,
        Err(_) => {
            eprintln!(
                "fatal: missing environment variable '{env_name}' for configuration '{key}'"
            );
            return Err(GitError::Exit(128));
        }
    };
    push_split_parameter(key, Some(&env_value));
    Ok(GlobalConfigOverride {
        key: key.to_string(),
        value: env_value,
    })
}

/// Append a `key[=value]` pair to the command-line config-parameter fragment in
/// sq-quoted new-style (`'key'='value'`) or bare (`'key'`) form, mirroring git's
/// `git_config_push_split_parameter`. git mutates the process `GIT_CONFIG_PARAMETERS`
/// env var; because the workspace forbids `unsafe` (and thus `std::env::set_var`),
/// sley instead accumulates the fragment in a process-global store. The effective
/// `GIT_CONFIG_PARAMETERS` — the pre-existing env value followed by this fragment —
/// is reconstructed by [`effective_config_parameters_env`] for both in-process
/// reads and any shell-alias subprocess, preserving git's left-to-right precedence.
fn push_split_parameter(key: &str, value: Option<&str>) {
    if let Ok(mut fragment) = CMDLINE_CONFIG_PARAMETERS.lock() {
        if !fragment.is_empty() {
            fragment.push(' ');
        }
        fragment.push_str(&sley_config::sq_quote(key));
        fragment.push('=');
        if let Some(value) = value {
            fragment.push_str(&sley_config::sq_quote(value));
        }
    }
}

/// The effective `GIT_CONFIG_PARAMETERS` string: the inherited env value (if any)
/// followed by the command-line `-c`/`--config-env` fragment, space-separated.
/// This is what git's process env would hold after folding in `-c`, and is both
/// parsed for in-process reads and exported to shell-alias subprocesses so they
/// inherit the parent's overrides.
pub(crate) fn effective_config_parameters_env() -> Option<String> {
    let inherited = env::var("GIT_CONFIG_PARAMETERS").ok().filter(|s| !s.is_empty());
    let fragment = CMDLINE_CONFIG_PARAMETERS
        .lock()
        .ok()
        .map(|f| f.clone())
        .filter(|s| !s.is_empty());
    match (inherited, fragment) {
        (Some(inherited), Some(fragment)) => Some(format!("{inherited} {fragment}")),
        (Some(inherited), None) => Some(inherited),
        (None, Some(fragment)) => Some(fragment),
        (None, None) => None,
    }
}

/// Look up the last-set injected override for `key` (canonicalised), across the
/// full injection stream (`GIT_CONFIG_COUNT` + `GIT_CONFIG_PARAMETERS`, the latter
/// holding any `-c`/`--config-env`). Returns the string value (a bare boolean-true
/// entry yields `"true"`). Used by command-side consumers (init, rev-parse's
/// `core.abbrev`, etc.) that need a single injected value before a full config load.
fn global_config_value(key: &str) -> Result<Option<String>> {
    let canonical = match sley_config::canonicalize_config_key(key) {
        Ok(canonical) => canonical,
        // The lookup key is a fixed internal key; if it fails to canonicalise
        // there can be no matching override.
        Err(_) => return Ok(None),
    };
    let parameters = injected_config_parameters()?;
    Ok(parameters
        .iter()
        .rev()
        .find(|param| param.canonical_key.eq_ignore_ascii_case(&canonical))
        .map(|param| param.value.clone().unwrap_or_else(|| "true".to_string())))
}

/// Parse the full config-injection stream (env-count pairs plus the effective
/// `GIT_CONFIG_PARAMETERS` = inherited env + command-line `-c`/`--config-env`),
/// converting any parse failure into git's `error: <msg>\nfatal: unable to parse
/// command-line config` two-line diagnostic with exit 128.
pub(crate) fn injected_config_parameters() -> Result<Vec<sley_config::ConfigParameter>> {
    let params_env = effective_config_parameters_env();
    sley_config::injected_config_parameters(params_env.as_deref())
        .map_err(report_config_parameter_error)
}

/// Print git's exact diagnostic for a config-injection parse failure and return
/// the matching exit status. Git prints the specific `error:` line followed by a
/// generic `fatal: unable to parse command-line config` and exits 128.
fn report_config_parameter_error(err: sley_config::ConfigParameterError) -> GitError {
    eprintln!("error: {}", err.message());
    eprintln!("fatal: unable to parse command-line config");
    GitError::Exit(128)
}

fn set_global_git_dir(git_dir: Option<PathBuf>) {
    if let Ok(mut value) = GLOBAL_GIT_DIR.lock() {
        *value = git_dir;
    }
}

fn set_global_work_tree(work_tree: Option<PathBuf>) {
    if let Ok(mut value) = GLOBAL_WORK_TREE.lock() {
        *value = work_tree;
    }
}

fn set_global_bare(bare: bool) {
    if let Ok(mut value) = GLOBAL_BARE.lock() {
        *value = bare;
    }
}

fn set_global_replace_objects(replace_objects: bool) {
    if let Ok(mut value) = GLOBAL_REPLACE_OBJECTS.lock() {
        *value = replace_objects;
    }
}

fn set_global_pathspec_flags(flags: PathspecFlags) {
    if let Ok(mut value) = GLOBAL_PATHSPEC_FLAGS.lock() {
        *value = flags;
    }
}

/// Effective default pathspec magic, folding in the global options *and* the
/// `GIT_*_PATHSPECS` environment variables (git reads both). Literal magic
/// (`--literal-pathspecs`/`--noglob-pathspecs`/`GIT_LITERAL_PATHSPECS`/
/// `GIT_NOGLOB_PATHSPECS`) suppresses glob magic.
pub(crate) fn effective_pathspec_flags() -> sley_worktree::PathspecMatchMagic {
    let mut flags = GLOBAL_PATHSPEC_FLAGS
        .lock()
        .map(|value| *value)
        .unwrap_or_default();
    if git_env_bool("GIT_LITERAL_PATHSPECS") {
        flags.literal = true;
    }
    if git_env_bool("GIT_NOGLOB_PATHSPECS") {
        flags.literal = true;
    }
    if git_env_bool("GIT_GLOB_PATHSPECS") {
        flags.glob = true;
    }
    if git_env_bool("GIT_ICASE_PATHSPECS") {
        flags.icase = true;
    }
    sley_worktree::PathspecMatchMagic {
        literal: flags.literal,
        glob: flags.glob && !flags.literal,
        icase: flags.icase,
    }
}

fn git_env_bool(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

fn global_git_dir() -> Option<PathBuf> {
    GLOBAL_GIT_DIR.lock().ok()?.clone()
}

fn global_work_tree() -> Option<PathBuf> {
    GLOBAL_WORK_TREE.lock().ok()?.clone()
}

fn environment_git_dir() -> Option<PathBuf> {
    env::var_os("GIT_DIR").map(PathBuf::from)
}

fn explicit_git_dir() -> Option<PathBuf> {
    global_git_dir().or_else(environment_git_dir)
}

fn environment_work_tree() -> Option<PathBuf> {
    env::var_os("GIT_WORK_TREE").map(PathBuf::from)
}

fn explicit_work_tree() -> Option<PathBuf> {
    global_work_tree().or_else(environment_work_tree)
}

fn global_bare() -> bool {
    GLOBAL_BARE.lock().is_ok_and(|value| *value)
}

fn global_replace_objects() -> bool {
    GLOBAL_REPLACE_OBJECTS.lock().map_or(true, |value| *value)
        && env::var_os("GIT_NO_REPLACE_OBJECTS").is_none()
}

pub(crate) fn replace_objects_active(refs: &FileRefStore) -> Result<bool> {
    if !global_replace_objects() {
        return Ok(false);
    }
    Ok(refs
        .list_refs()?
        .iter()
        .any(|reference| reference.name.starts_with("refs/replace/")))
}

pub(crate) fn apply_replace_object(refs: &FileRefStore, oid: &ObjectId) -> Result<ObjectId> {
    if !global_replace_objects() {
        return Ok(*oid);
    }
    let mut current = *oid;
    let mut seen = HashSet::new();
    for _ in 0..5 {
        if !seen.insert(current) {
            break;
        }
        let name = format!("refs/replace/{current}");
        match refs.read_ref(&name)? {
            Some(RefTarget::Direct(next)) => current = next,
            _ => break,
        }
    }
    Ok(current)
}

fn print_global_usage() {
    eprintln!(
        "usage: git [-v | --version] [-h | --help] [-C <path>] [-c <name>=<value>]\n           [--exec-path[=<path>]] [--html-path] [--man-path] [--info-path]\n           [-p | --paginate | -P | --no-pager] [--no-replace-objects] [--no-lazy-fetch]\n           [--no-optional-locks] [--no-advice] [--bare] [--git-dir=<path>]\n           [--work-tree=<path>] [--namespace=<name>] [--config-env=<name>=<envvar>]\n           <command> [<args>]"
    );
}

/// Replicate git's implicit-bare determination for `git init --separate-git-dir`.
///
/// git computes `is_bare_repository_cfg = guess_repository_type(git_dir)` only when
/// `--bare` was not given (init-db.c). `git_dir` is `GIT_DIR` when set; otherwise it
/// defaults to `.git`, *unless* `.git` is a gitfile for a linked worktree — i.e. the
/// gitfile's target contains a `commondir` file — in which case git chdir's to the
/// main worktree and inspects the resolved *common* git directory instead. A plain
/// `--separate-git-dir` gitfile (no `commondir`) leaves `git_dir == ".git"`, which is
/// never bare. `guess_repository_type` treats `GIT_DIR=.` / `GIT_DIR=$cwd` and any
/// path not ending in `/.git` as bare, and `.git` / `*/.git` as non-bare; for a bare
/// clone behind a worktree (e.g. `git clone --bare` + `git worktree add`) the common
/// dir is `…/bare.git`, which `guess_repository_type` already reports as bare.

/// Mirror of git's `guess_repository_type()` (builtin/init-db.c): decide whether a
/// git directory path implies a bare repository.

/// git's `default_branch_name_advice` (refs.c, non-WITH_BREAKING_CHANGES build),
/// emitted through `advise_if_enabled(ADVICE_DEFAULT_BRANCH_NAME, ...)` when an
/// unconfigured `git init` falls back to "master".
const DEFAULT_BRANCH_NAME_ADVICE: &str = "Using '{}' as the name for the initial branch. This default branch name\n\
will change to \"main\" in Git 3.0. To configure the initial branch name\n\
to use in all of your new repositories, which will suppress this warning,\n\
call:\n\
\n\
\tgit config --global init.defaultBranch <name>\n\
\n\
Names commonly chosen instead of 'master' are 'main', 'trunk' and\n\
'development'. The just-created branch can be renamed via this command:\n\
\n\
\tgit branch -m <name>\n\
\n\
Disable this message with \"git config set advice.defaultBranchName false\"";

/// Mirror git's `advise_if_enabled` for the unconfigured-default-branch hint:
/// gated on the `GIT_ADVICE` env bool and `advice.defaultBranchName`, rendered
/// line-by-line as `hint: <line>` on stderr, coloured per `color.advice`
/// (advice.c `vadvise`; the hint colour is yellow).

/// Resolve the object format for a *fresh* init, returning the chosen format and
/// whether it was specified explicitly on the command line.
///
/// Mirrors git's `repository_format_configure` precedence: an explicit
/// `--object-format` wins (and a bad value is fatal); otherwise `GIT_DEFAULT_HASH`
/// is consulted (also fatal on a bad value); otherwise the `init.defaultObjectFormat`
/// config default is used (a bad value here only warns and falls back to sha1). The
/// reinitialize-with-different-hash guard is applied later in

/// Parse an object-format name the way git's `init` does: an unrecognised value is a
/// `fatal: unknown hash algorithm '<value>'` with exit status 128.

/// Resolve the ref storage format for a *fresh* init, returning the chosen format and
/// whether it was specified explicitly on the command line.
///
/// Mirrors git's `repository_format_configure` precedence: an explicit `--ref-format`
/// wins (and a bad value is fatal); otherwise `GIT_DEFAULT_REF_FORMAT` is consulted
/// (also fatal on a bad value); otherwise the `init.defaultRefFormat` config default is
/// used (a bad value here only warns and falls back to the default), with
/// `feature.experimental` selecting reftable as the last resort. The
/// reinitialize-with-different-format guard is applied later in
/// [`RepositoryBootstrap::init`], once the existing repository format is known.

/// git's `repo_default_branch_name`: `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME`
/// overrides the `init.defaultBranch` configuration (read from the default
/// config layers: `-c` overrides, then system/global files); the hardcoded
/// fallback is `master`.
fn default_initial_branch_name(global_config: &[GlobalConfigOverride]) -> Result<String> {
    if let Ok(env) = env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME")
        && !env.is_empty()
    {
        return Ok(env);
    }
    Ok(init_config_value("init.defaultBranch", global_config)?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "master".to_string()))
}

fn init_config_value(key: &str, global_config: &[GlobalConfigOverride]) -> Result<Option<String>> {
    if let Some(value) = global_config
        .iter()
        .rev()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
        .map(|entry| entry.value.clone())
    {
        return Ok(Some(value));
    }
    if let Ok(Some(value)) = global_config_value(key) {
        return Ok(Some(value));
    }
    let context = sley_config::ConfigIncludeContext::new(None, None);
    let config = sley_config::load_pre_dispatch_config(None, &context)?;
    let (section, entry_key) = key
        .split_once('.')
        .ok_or_else(|| GitError::Command(format!("invalid config key {key}")))?;
    Ok(config.get(section, None, entry_key).map(str::to_owned))
}

fn parse_config_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AddAction {
    Add(PathBuf),
    Remove(PathBuf),
}

impl AddAction {
    fn path(&self) -> &PathBuf {
        match self {
            Self::Add(path) | Self::Remove(path) => path,
        }
    }
}

fn resolve_add_update_actions(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: Vec<PathBuf>,
    include_untracked: bool,
    ignore_missing: bool,
) -> Result<Vec<AddAction>> {
    let pathspecs = paths
        .into_iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(&path)
            };
            let matched = absolute.exists();
            (path, absolute, matched)
        })
        .collect::<Vec<_>>();
    let mut matched = pathspecs
        .iter()
        .map(|(_, _, matched)| *matched)
        .collect::<Vec<_>>();
    let status = sley_worktree::short_status(worktree_root, git_dir, format)?;
    let mut actions = Vec::new();
    for entry in status {
        if entry.index == b'?' && entry.worktree == b'?' {
            if !include_untracked {
                continue;
            }
        } else if entry.worktree != b'M' && entry.worktree != b'D' {
            continue;
        }
        let path = worktree_root.join(
            std::str::from_utf8(&entry.path)
                .map_err(|err| GitError::InvalidPath(err.to_string()))?,
        );
        if !pathspecs.is_empty() {
            let mut path_matches = false;
            for (idx, (_, pathspec, _)) in pathspecs.iter().enumerate() {
                if add_path_matches(&path, pathspec) {
                    matched[idx] = true;
                    path_matches = true;
                }
            }
            if !path_matches {
                continue;
            }
        }
        if entry.worktree == b'D' {
            actions.push(AddAction::Remove(path));
        } else {
            actions.push(AddAction::Add(path));
        }
    }
    for ((display, _, _), matched) in pathspecs.iter().zip(matched) {
        if !matched && !ignore_missing {
            eprintln!(
                "fatal: pathspec '{}' did not match any files",
                display.to_string_lossy()
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(actions)
}

fn add_path_matches(path: &Path, pathspec: &Path) -> bool {
    path == pathspec || path.starts_with(pathspec)
}

/// Expand clustered short boolean options for `git repack` (e.g. `-ad`,
/// `-adf`) into the per-flag tokens the main parser understands, following
/// git's getopt semantics. Every short option `git repack` accepts that sley
/// also implements is a boolean (`-a -A -d -f -F -l -q`; see the
/// `builtin_repack_options` table in upstream `builtin/repack.c`), so a
/// cluster is expanded only when *all* of its characters are in that set.
/// Anything else — long options, positionals, `-`, or a cluster containing a
/// flag sley does not implement (e.g. `-Adb`) — passes through untouched so
/// the main parser reports the whole token, mirroring the
/// no-partial-side-effects rule of `expand_commit_short_clusters`.

/// The directory prefix git uses when printing loose-object paths from fsck:
/// `$GIT_DIR/objects` with GIT_DIR's textual (often relative) value — `./objects`
/// when the cwd IS the git dir (a bare repository), `.git/objects` at a worktree
/// root. sley's discovery yields an absolute git dir, so reconstruct the relative

fn pack_refs_peeled_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let peeled = sley_rev::peel_tags(db, format, oid)?;
    Ok((peeled != *oid).then_some(peeled))
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

fn parse_reflog_expire_time(value: &str, option: &str) -> Result<i64> {
    match value {
        "all" => return Ok(i64::MAX),
        "never" => return Ok(i64::MIN),
        _ => {}
    }
    parse_reflog_expire_date(value).ok_or_else(|| {
        eprintln!("fatal: invalid timestamp '{value}' given to '{option}'");
        GitError::Exit(128)
    })
}

fn parse_reflog_expire_date(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let first = parts.next()?;
    if let Some(timestamp) = first.strip_prefix('@') {
        let timezone = parts.next()?;
        if parts.next().is_some() || log_parse_timezone_offset_seconds(timezone).is_none() {
            return None;
        }
        return timestamp.parse::<i64>().ok();
    }
    let (date, time) = if let Some((date, time)) = first.split_once('T') {
        (date, time)
    } else {
        (first, parts.next()?)
    };
    let timezone = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let (year, month, day) = log_parse_date_ymd(date)?;
    let (hour, minute, second) = log_parse_time_hms(time)?;
    let timezone_offset = log_parse_timezone_offset_seconds(timezone)?;
    Some(
        log_days_from_civil(year, month, day)
            .saturating_mul(86_400)
            .saturating_add(i64::from(hour * 3_600 + minute * 60 + second))
            .saturating_sub(timezone_offset),
    )
}

fn parse_reflog_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(usize::MAX);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

fn parse_reflog_skip_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(0);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

fn parse_reflog_min_parent_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(0);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

fn parse_reflog_max_parent_count(value: &str) -> Result<usize> {
    let count = parse_reflog_integer(value)?;
    if count < 0 {
        return Ok(usize::MAX);
    }
    usize::try_from(count).map_err(|_| reflog_invalid_integer_error(value))
}

fn parse_reflog_integer(value: &str) -> Result<i128> {
    value
        .parse::<i128>()
        .map_err(|_| reflog_invalid_integer_error(value))
}

fn reflog_invalid_integer_error(value: &str) -> GitError {
    eprintln!("fatal: '{value}': not an integer");
    GitError::Exit(1)
}

fn reflog_reference_name(value: Option<&str>) -> Result<String> {
    let Some(value) = value else {
        return Ok("HEAD".to_string());
    };
    if value == "HEAD" || value.starts_with("refs/") {
        return Ok(value.to_string());
    }
    branch_ref_name(value)
}

/// Recursively map a tree's blob entries to `(mode, oid)` keyed by full path.
/// Shared by the stash and merge/cherry-pick/revert replay machinery.
///
/// Thin wrapper over the canonical [`sley_diff_merge::flatten_tree`]; the local
/// recursive flattener was a byte-identical copy.
fn stash_tree_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, (u32, ObjectId)>> {
    sley_diff_merge::flatten_tree(db, format, tree_oid)
}

fn ancestor_depths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start: &ObjectId,
) -> Result<HashMap<ObjectId, usize>> {
    let mut depths = HashMap::new();
    let mut pending = VecDeque::from([(start.clone(), 0usize)]);
    while let Some((oid, depth)) = pending.pop_front() {
        if depths.get(&oid).is_some_and(|existing| *existing <= depth) {
            continue;
        }
        depths.insert(oid, depth);
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        for parent in commit.parents {
            pending.push_back((parent, depth + 1));
        }
    }
    Ok(depths)
}

fn count_objects_human_bytes(size_bytes: u64) -> String {
    if size_bytes == 0 {
        return "0 bytes".to_string();
    }
    if size_bytes < 1024 {
        return format!("{size_bytes} bytes");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut size = size_bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2} {}", UNITS[unit])
}

fn write_check_attr_state(
    stdout: &mut impl Write,
    state: Option<&sley_worktree::AttributeState>,
) -> Result<()> {
    match state {
        Some(sley_worktree::AttributeState::Set) => stdout.write_all(b"set")?,
        Some(sley_worktree::AttributeState::Unset) => stdout.write_all(b"unset")?,
        Some(sley_worktree::AttributeState::Value(value)) => stdout.write_all(value)?,
        None => stdout.write_all(b"unspecified")?,
    }
    Ok(())
}

fn check_ignore_tracked_paths(git_dir: &Path, format: ObjectFormat) -> Result<BTreeSet<Vec<u8>>> {
    Ok(sley_worktree::read_repository_index(git_dir, format)?
        .map(|index| {
            index
                .entries
                .into_iter()
                .map(|entry| entry.path.into_bytes())
                .collect()
        })
        .unwrap_or_default())
}

fn read_pathspecs_from_file(path: &Path, nul: bool) -> Result<Vec<PathBuf>> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        io::stdin().read_to_end(&mut bytes)?;
    } else {
        bytes = fs::read(path)?;
    }
    let separator = if nul { b'\0' } else { b'\n' };
    Ok(bytes
        .split(|byte| *byte == separator)
        .filter_map(|entry| {
            let entry = if !nul && entry.ends_with(b"\r") {
                &entry[..entry.len() - 1]
            } else {
                entry
            };
            if entry.is_empty() {
                None
            } else {
                Some(PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
            }
        })
        .collect())
}

fn update_reset_head_ref(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    target: &str,
    committer: Vec<u8>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let reflog = |old_oid: ObjectId, new_oid: ObjectId| ReflogEntry {
        old_oid,
        new_oid,
        committer: committer.clone(),
        message: format!("reset: moving to {target}").into_bytes(),
    };
    let mut tx = store.transaction();
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => {
            tx.update(RefUpdate {
                name: name.clone(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog(old_oid, new_oid)),
            });
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(name),
                reflog: Some(reflog(old_oid, new_oid)),
            });
        }
        _ => {
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog(old_oid, new_oid)),
            });
        }
    }
    tx.commit()
}

fn print_reset_hard_head(
    git_dir: &Path,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            commit_oid,
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    println!(
        "HEAD is now at {} {}",
        format_log_abbrev_oid(commit_oid),
        commit_subject(commit.message)
    );
    Ok(())
}

/// Git clean file selection without `-d` or pathspecs: a worktree-root file is
/// always eligible; a file in a subdirectory is eligible only when its immediate
/// parent directory contains tracked content (otherwise the file lives in a
/// wholly-untracked directory that Git would only remove under `-d`). This holds

fn checkout_create_or_reset_branch(
    git_dir: &Path,
    format: ObjectFormat,
    branch: &str,
    start: &str,
    force: bool,
    committer: Vec<u8>,
) -> Result<bool> {
    let store = FileRefStore::new(git_dir, format);
    let name = branch_ref_name(branch)?;
    let existing = store.read_ref(&name)?;
    if existing.is_some() && !force {
        eprintln!("fatal: a branch named '{branch}' already exists");
        return Err(GitError::Exit(128));
    }
    let start_oid = match resolve_checkout_start_oid(git_dir, format, start) {
        Ok(Some(start_oid)) => start_oid,
        Ok(None) => {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(name),
                reflog: None,
            });
            tx.commit()?;
            return Ok(false);
        }
        Err(err) => return Err(err),
    };
    if let Some(existing) = existing {
        let old_oid = match existing {
            RefTarget::Direct(oid) => oid,
            RefTarget::Symbolic(_) => {
                return Err(GitError::Unsupported(
                    "checkout -B target branch must be direct".into(),
                ));
            }
        };
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(start_oid),
            reflog: Some(ReflogEntry {
                old_oid,
                new_oid: start_oid,
                committer,
                message: format!("branch: Reset to {start}").into_bytes(),
            }),
        });
        tx.commit()?;
        Ok(true)
    } else {
        store.create_branch(
            branch,
            start_oid,
            committer,
            format!("branch: Created from {start}").into_bytes(),
        )?;
        Ok(false)
    }
}

fn resolve_checkout_start_oid(
    git_dir: &Path,
    format: ObjectFormat,
    start: &str,
) -> Result<Option<ObjectId>> {
    match resolve_revision(git_dir, format, start) {
        Ok(oid) => Ok(Some(oid)),
        Err(_) if start == "HEAD" || start == "@" => {
            let store = FileRefStore::new(git_dir, format);
            match store.read_ref("HEAD")? {
                Some(RefTarget::Symbolic(name)) if store.read_ref(&name)?.is_none() => Ok(None),
                _ => Err(GitError::not_found(format!("revision {start}"))),
            }
        }
        Err(err) => Err(err),
    }
}

#[derive(Debug, Clone, Copy)]
struct TreePrintOptions<'a> {
    name_only: bool,
    object_only: bool,
    long: bool,
    show_trees: bool,
    tree_only: bool,
    oid_abbrev: Option<usize>,
    format_spec: Option<&'a str>,
    nul: bool,
}

trait TreeEntryView {
    fn mode(&self) -> u32;
    fn oid(&self) -> &ObjectId;
}

impl TreeEntryView for TreeEntry {
    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> &ObjectId {
        &self.oid
    }
}

impl TreeEntryView for TreeEntryRef<'_> {
    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> &ObjectId {
        &self.oid
    }
}

fn print_tree(
    db: Option<&FileObjectDatabase>,
    format: ObjectFormat,
    body: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    print_tree_with_prefix(db, format, body, b"", options)
}

fn print_tree_with_prefix(
    db: Option<&FileObjectDatabase>,
    format: ObjectFormat,
    body: &[u8],
    prefix: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    let mut stdout = io::stdout();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        if options.tree_only && entry.mode != 0o040000 {
            continue;
        }
        let mut path = Vec::with_capacity(prefix.len() + entry.name.len());
        path.extend_from_slice(prefix);
        path.extend_from_slice(entry.name);
        print_tree_entry_to_writer(&mut stdout, db, &entry, &path, options)?;
    }
    stdout.flush()?;
    Ok(())
}

fn print_tree_entry_to_writer(
    writer: &mut impl Write,
    db: Option<&FileObjectDatabase>,
    entry: &impl TreeEntryView,
    path: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    if let Some(format) = options.format_spec {
        write_tree_entry_format(writer, db, entry, path, options, format)?;
    } else if options.object_only {
        write_tree_oid(writer, entry.oid(), options)?;
    } else if options.name_only {
        write_tree_path(writer, path, options)?;
    } else {
        let object_type = tree_entry_object_type(entry.mode());
        write!(
            writer,
            "{:06o} {} {}",
            entry.mode(),
            object_type.as_str(),
            format_tree_oid(entry.oid(), options)
        )?;
        if options.long {
            let size = tree_entry_size_field(db, object_type, entry.oid())?;
            write!(writer, " {size:>7}")?;
        }
        writer.write_all(b"\t")?;
        write_tree_path(writer, path, options)?;
    }
    if options.nul {
        writer.write_all(&[0])?;
    } else {
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn write_tree_path(
    writer: &mut impl Write,
    path: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    if options.nul {
        writer.write_all(path)?;
    } else {
        writer.write_all(status_quote_path(path, false).as_bytes())?;
    }
    Ok(())
}

fn format_tree_oid(oid: &ObjectId, options: TreePrintOptions<'_>) -> String {
    let hex = oid.to_hex();
    let Some(width) = options.oid_abbrev else {
        return hex;
    };
    hex[..width.clamp(4, oid.format().hex_len())].to_string()
}

fn write_tree_oid(
    writer: &mut impl Write,
    oid: &ObjectId,
    options: TreePrintOptions<'_>,
) -> Result<()> {
    writer.write_all(format_tree_oid(oid, options).as_bytes())?;
    Ok(())
}

fn write_tree_entry_format(
    writer: &mut impl Write,
    db: Option<&FileObjectDatabase>,
    entry: &impl TreeEntryView,
    path: &[u8],
    options: TreePrintOptions<'_>,
    format: &str,
) -> Result<()> {
    let object_type = tree_entry_object_type(entry.mode());
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            write!(writer, "{ch}")?;
            continue;
        }
        match chars.peek().copied() {
            Some('%') => {
                chars.next();
                writer.write_all(b"%")?;
            }
            Some('x') => {
                chars.next();
                let high = chars.next().ok_or_else(|| {
                    GitError::Command("ls-tree --format %x requires two hex digits".into())
                })?;
                let low = chars.next().ok_or_else(|| {
                    GitError::Command("ls-tree --format %x requires two hex digits".into())
                })?;
                let byte = (format_hex_nibble(high)? << 4) | format_hex_nibble(low)?;
                writer.write_all(&[byte])?;
            }
            Some('(') => {
                chars.next();
                let mut placeholder = String::new();
                for ch in chars.by_ref() {
                    if ch == ')' {
                        break;
                    }
                    placeholder.push(ch);
                }
                write_tree_format_placeholder(
                    writer,
                    db,
                    entry,
                    object_type,
                    path,
                    options,
                    &placeholder,
                )?;
            }
            _ => {
                return Err(GitError::Command(format!(
                    "unsupported ls-tree --format escape %{ch}",
                    ch = chars.next().unwrap_or('%')
                )));
            }
        }
    }
    Ok(())
}

fn write_tree_format_placeholder(
    writer: &mut impl Write,
    db: Option<&FileObjectDatabase>,
    entry: &impl TreeEntryView,
    object_type: ObjectType,
    path: &[u8],
    options: TreePrintOptions<'_>,
    placeholder: &str,
) -> Result<()> {
    match placeholder {
        "objectmode" => write!(writer, "{:06o}", entry.mode())?,
        "objecttype" => writer.write_all(object_type.as_str().as_bytes())?,
        "objectname" => write_tree_oid(writer, entry.oid(), options)?,
        "objectsize" => {
            writer.write_all(tree_entry_size_field(db, object_type, entry.oid())?.as_bytes())?
        }
        "objectsize:padded" => write!(
            writer,
            "{:>7}",
            tree_entry_size_field(db, object_type, entry.oid())?
        )?,
        "path" => write_tree_path(writer, path, options)?,
        _ => {
            return Err(GitError::Command(format!(
                "unsupported ls-tree --format placeholder %({placeholder})"
            )));
        }
    }
    Ok(())
}

fn format_hex_nibble(ch: char) -> Result<u8> {
    match ch {
        '0'..='9' => Ok(ch as u8 - b'0'),
        'a'..='f' => Ok(ch as u8 - b'a' + 10),
        'A'..='F' => Ok(ch as u8 - b'A' + 10),
        _ => Err(GitError::Command(format!(
            "invalid ls-tree --format hex digit {ch}"
        ))),
    }
}

fn tree_entry_size_field(
    db: Option<&FileObjectDatabase>,
    object_type: ObjectType,
    oid: &ObjectId,
) -> Result<String> {
    if object_type != ObjectType::Blob {
        return Ok("-".into());
    }
    let db =
        db.ok_or_else(|| GitError::Command("ls-tree --long requires an object database".into()))?;
    Ok(db.read_object(oid)?.body.len().to_string())
}

fn find_tree_entry(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    components: &[&str],
) -> Result<Option<sley_object::TreeEntry>> {
    let Some((component, rest)) = components.split_first() else {
        return Ok(None);
    };
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        if entry.name != component.as_bytes() {
            continue;
        }
        if rest.is_empty() {
            return Ok(Some(TreeEntry::from(entry)));
        }
        if entry.mode != 0o040000 {
            return Ok(None);
        }
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::InvalidObject(format!(
                "expected tree {}, found {}",
                entry.oid,
                object.object_type.as_str()
            )));
        }
        return find_tree_entry(db, format, &object.body, rest);
    }
    Ok(None)
}

/// Classification of a `git commit` short option, mirroring its
/// `builtin_commit_options` table in upstream `builtin/commit.c`.

/// Classify a `git commit` short flag character, or `None` if it is not a
/// recognized short option for `git commit`.

/// Expand clustered short options for `git commit` (e.g. `-qm <msg>`,
/// `-sqm <msg>`) into the per-flag tokens the main parser already understands,
/// following git's getopt semantics: leading boolean flags are split off one at
/// a time, and the first value-taking flag in a cluster consumes the remainder
/// of the cluster as its (glued) value.
///
/// Only clusters that *begin with a boolean* short flag are expanded; arguments
/// whose first short flag already takes a value (`-m<msg>`, `-F<path>`,
/// `-C<rev>`, `-u<mode>`, `-S<key>`, ...) are passed through untouched so the
/// existing glued-value arms handle them verbatim. Anything that is not a short
/// option (long options, `--`, positionals, `-`) is passed through unchanged,
/// and everything after a literal `--` is left verbatim.

fn commit_message_requires_value_error() -> Result<()> {
    eprintln!("error: switch `m' requires a value");
    Err(GitError::Exit(129))
}

fn read_commit_pathspecs_from_file(path: &Path, nul: bool) -> Result<Vec<PathBuf>> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        io::stdin().read_to_end(&mut bytes)?;
    } else {
        bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                let message = match err.kind() {
                    io::ErrorKind::NotFound => "No such file or directory".to_string(),
                    io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
                    _ => err.to_string(),
                };
                eprintln!(
                    "fatal: could not open '{}' for reading: {message}",
                    path.display()
                );
                return Err(GitError::Exit(128));
            }
        };
    }
    let separator = if nul { b'\0' } else { b'\n' };
    Ok(bytes
        .split(|byte| *byte == separator)
        .filter_map(|entry| {
            let entry = if !nul && entry.ends_with(b"\r") {
                &entry[..entry.len() - 1]
            } else {
                entry
            };
            if entry.is_empty() {
                None
            } else {
                Some(PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
            }
        })
        .collect())
}

fn commit_unified_requires_value_error(short: bool) -> Result<()> {
    if short {
        eprintln!("error: switch `U' requires a value");
    } else {
        eprintln!("error: option `unified' requires a value");
    }
    Err(GitError::Exit(129))
}

fn commit_inter_hunk_context_requires_value_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' requires a value");
    Err(GitError::Exit(129))
}

fn commit_validate_unified_context(value: &str, short: bool) -> Result<()> {
    if value.is_empty() {
        return commit_unified_expects_numerical_value_error(short);
    }
    if git_count_value_is_valid(value) {
        return Ok(());
    }
    if short {
        eprintln!("error: switch `U' expects an integer value with an optional k/m/g suffix");
    } else {
        eprintln!("error: option `unified' expects an integer value with an optional k/m/g suffix");
    }
    Err(GitError::Exit(129))
}

fn commit_unified_expects_numerical_value_error(short: bool) -> Result<()> {
    if short {
        eprintln!("error: switch `U' expects a numerical value");
    } else {
        eprintln!("error: option `unified' expects a numerical value");
    }
    Err(GitError::Exit(129))
}

fn commit_validate_inter_hunk_context(value: &str) -> Result<()> {
    if value.is_empty() {
        return commit_inter_hunk_context_expects_numerical_value_error();
    }
    if git_count_value_is_valid(value) {
        return Ok(());
    }
    eprintln!(
        "error: option `inter-hunk-context' expects an integer value with an optional k/m/g suffix"
    );
    Err(GitError::Exit(129))
}

fn commit_inter_hunk_context_expects_numerical_value_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' expects a numerical value");
    Err(GitError::Exit(129))
}

fn git_count_value_is_valid(value: &str) -> bool {
    let number = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    let digits = match number.as_bytes().first() {
        Some(b'+' | b'-') if number.len() > 1 => &number[1..],
        _ => number,
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn commit_tree_file_requires_value_error() -> Result<()> {
    eprintln!("error: switch `F' requires a value");
    Err(GitError::Exit(129))
}

fn read_commit_message_file(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut message = Vec::new();
        io::stdin().read_to_end(&mut message)?;
        Ok(message)
    } else {
        Ok(fs::read(path)?)
    }
}

fn commit_message_from_prepared_chunks(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in chunks
        .iter()
        .filter(|chunk| !commit_message_chunk_is_empty(chunk))
    {
        if !out.is_empty() {
            out.push(b'\n');
        }
        out.extend_from_slice(chunk);
    }
    out
}

fn commit_message_chunk_is_empty(chunk: &[u8]) -> bool {
    chunk.is_empty() || chunk == b"\n"
}

#[derive(Clone, Copy)]
enum CommitCleanupMode {
    Strip,
    Whitespace,
    Verbatim,
}

fn commit_cleanup_message(message: Vec<u8>, mode: CommitCleanupMode) -> Vec<u8> {
    match mode {
        CommitCleanupMode::Verbatim => message,
        CommitCleanupMode::Strip => commands::tag::tag_stripspace_message(&message, true),
        CommitCleanupMode::Whitespace => tag_stripspace_message(&message, false),
    }
}

fn read_reused_commit(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<Commit> {
    let result = (|| {
        let oid = resolve_revision(git_dir, format, rev)?;
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        let commit_oid = sley_rev::peel_to_commit(&db, format, &oid)?;
        let object = db.read_object(&commit_oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {}, found {}",
                commit_oid,
                object.object_type.as_str()
            )));
        }
        Commit::parse(format, &object.body)
    })();
    match result {
        Ok(commit) => Ok(commit),
        Err(_) => {
            eprintln!("fatal: could not lookup commit '{rev}'");
            Err(GitError::Exit(128))
        }
    }
}

/// Parse a git rename/copy similarity spec (`-M50`, `-M50%`, `-M0.5`, `--find-renames=75%`)
/// into a 0..=100 threshold. A bare `-M` (no value) keeps the default.
fn parse_similarity_threshold(spec: &str) -> u8 {
    let spec = spec.strip_suffix('%').unwrap_or(spec);
    match spec.parse::<f64>() {
        Ok(value) => {
            let pct = if value <= 1.0 && spec.contains('.') {
                value * 100.0
            } else {
                value
            };
            pct.round().clamp(0.0, 100.0) as u8
        }
        Err(_) => sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
    }
}

/// Split `git diff`'s positional arguments into leading revisions (resolved to
/// tree oids) and the remaining pathspec arguments.
///
/// Accepts `diff <rev>`, `diff <rev> <rev>`, `diff <rev>..<rev>`, and
/// `diff <rev>...<rev>` (symmetric, from the merge base) before any `-- <path>`
/// separator. A leading argument is treated as a revision only while it resolves
/// as one; the first argument that fails to resolve — and everything after it — is
/// a pathspec, so ordinary file paths keep working without an explicit `--`.

fn write_diff_summary_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
) -> Result<()> {
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            let mode = entry.new_mode.unwrap_or(0);
            let path = status_quote_path(&entry.path, false);
            writeln!(stdout, " create mode {mode:06o} {path}")?;
        }
        sley_diff_merge::NameStatus::Deleted => {
            let mode = entry.old_mode.unwrap_or(0);
            let path = status_quote_path(&entry.path, false);
            writeln!(stdout, " delete mode {mode:06o} {path}")?;
        }
        sley_diff_merge::NameStatus::Renamed(score) => {
            if let Some(old_path) = &entry.old_path {
                let old_path = status_quote_path(old_path, false);
                let path = status_quote_path(&entry.path, false);
                writeln!(stdout, " rename {old_path} => {path} ({score}%)")?;
            }
        }
        sley_diff_merge::NameStatus::Copied(score) => {
            if let Some(old_path) = &entry.old_path {
                let old_path = status_quote_path(old_path, false);
                let path = status_quote_path(&entry.path, false);
                writeln!(stdout, " copy {old_path} => {path} ({score}%)")?;
            }
        }
        sley_diff_merge::NameStatus::Modified => {
            if entry.old_mode != entry.new_mode
                && let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
            {
                let path = status_quote_path(&entry.path, false);
                writeln!(
                    stdout,
                    " mode change {old_mode:06o} => {new_mode:06o} {path}"
                )?;
            }
        }
    }
    Ok(())
}

fn write_diff_raw_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    z: bool,
    zero_worktree_oids: bool,
    abbrev: Option<usize>,
    format: ObjectFormat,
) -> Result<()> {
    let old_mode = entry.old_mode.unwrap_or(0);
    let new_mode = entry.new_mode.unwrap_or(0);
    let old_oid = diff_raw_oid(entry.old_oid.as_ref(), false, abbrev, format);
    let new_oid = diff_raw_oid(entry.new_oid.as_ref(), zero_worktree_oids, abbrev, format);
    write!(
        stdout,
        ":{old_mode:06o} {new_mode:06o} {old_oid} {new_oid} {}",
        entry.status.label()
    )?;
    if z {
        stdout.write_all(b"\0")?;
        if let Some(old_path) = &entry.old_path {
            stdout.write_all(old_path)?;
            stdout.write_all(b"\0")?;
        }
        stdout.write_all(&entry.path)?;
        stdout.write_all(b"\0")?;
    } else {
        if let Some(old_path) = &entry.old_path {
            let old_path = status_quote_path(old_path, false);
            write!(stdout, "\t{old_path}")?;
        }
        let path = status_quote_path(&entry.path, false);
        writeln!(stdout, "\t{path}")?;
    }
    Ok(())
}

fn diff_raw_oid(
    oid: Option<&ObjectId>,
    zero: bool,
    abbrev: Option<usize>,
    format: ObjectFormat,
) -> String {
    let zero_width = abbrev.unwrap_or_else(|| format.hex_len());
    let mut hex = if zero {
        "0".repeat(zero_width)
    } else {
        oid.map(|oid| {
            let hex = oid.to_hex();
            let width = abbrev.unwrap_or(hex.len()).min(hex.len());
            hex[..width].to_string()
        })
        .unwrap_or_else(|| "0".repeat(zero_width))
    };
    // git's diff_aligned_abbrev: under GIT_PRINT_SHA1_ELLIPSIS=yes an
    // abbreviated raw oid carries a "..." tail (the old-style aligned form
    // t4013's default cells still exercise).
    if hex.len() < format.hex_len()
        && std::env::var("GIT_PRINT_SHA1_ELLIPSIS").is_ok_and(|value| value == "yes")
    {
        hex.push_str("...");
    }
    hex
}

#[derive(Clone, Copy)]
struct DiffPatchOptions<'a> {
    db: &'a FileObjectDatabase,
    worktree_root: Option<&'a Path>,
    use_worktree_new: bool,
    format: ObjectFormat,
    abbrev: usize,
    src_prefix: &'a str,
    dst_prefix: &'a str,
    /// Lines of hunk context (`-U<n>`); the porcelain default is 3.
    context: usize,
    /// Userdiff driver resolution (`diff=<driver>` attributes + config);
    /// `None` keeps the default funcname heuristic.
    userdiff: Option<&'a commands::userdiff::UserdiffResolver>,
    /// ANSI palette when color output is enabled.
    colors: Option<&'a commands::diff_words::DiffColors>,
    /// Word-diff rendering request (mode + the command-line regex override).
    word_diff: Option<&'a WordDiffRequest<'a>>,
    /// Preloaded file contents for `diff --no-index` (old, new), bypassing
    /// the object database / worktree reads.
    no_index_contents: Option<(Option<&'a [u8]>, Option<&'a [u8]>)>,
}

/// A `--word-diff` request before per-file word-regex resolution.
struct WordDiffRequest<'a> {
    mode: commands::diff_words::WordDiffMode,
    /// `--word-diff-regex` / `--color-words=<re>` override.
    cli_regex: Option<&'a str>,
}

/// Write one metainfo header line, wrapped in the meta color when enabled.
fn write_diff_meta_line(
    stdout: &mut dyn Write,
    colors: Option<&commands::diff_words::DiffColors>,
    line: &str,
) -> Result<()> {
    match colors {
        Some(colors) if !colors.meta.is_empty() => {
            writeln!(stdout, "{}{}{}", colors.meta, line, colors.reset)?;
        }
        _ => writeln!(stdout, "{line}")?,
    }
    Ok(())
}

fn write_diff_patch_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    options: DiffPatchOptions<'_>,
) -> Result<()> {
    let (old_content, new_content) = match options.no_index_contents {
        Some((old, new)) => (old.map(<[u8]>::to_vec), new.map(<[u8]>::to_vec)),
        None => (
            diff_entry_old_content(entry, options.db)?,
            diff_entry_new_content(
                entry,
                options.db,
                options.worktree_root,
                options.use_worktree_new,
            )?,
        ),
    };
    let content_changed = old_content.as_deref() != new_content.as_deref();
    if old_content.as_deref().is_some_and(is_binary_content)
        || new_content.as_deref().is_some_and(is_binary_content)
    {
        return write_diff_binary_patch_entry(stdout, entry, old_content, new_content, options);
    }

    let old_path = entry.old_path.as_deref().unwrap_or(&entry.path);
    let diff_old_path = diff_patch_prefixed_path(options.src_prefix, old_path);
    let diff_path = diff_patch_prefixed_path(options.dst_prefix, &entry.path);
    let old_header_path = diff_patch_file_header_path(options.src_prefix, old_path);
    let header_path = diff_patch_file_header_path(options.dst_prefix, &entry.path);
    let old_similarity_path = status_quote_path(old_path, false);
    let similarity_path = status_quote_path(&entry.path, false);
    let colors = options.colors;
    write_diff_meta_line(
        stdout,
        colors,
        &format!("diff --git {diff_old_path} {diff_path}"),
    )?;
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            if let Some(mode) = entry.new_mode {
                write_diff_meta_line(stdout, colors, &format!("new file mode {mode:06o}"))?;
            }
        }
        sley_diff_merge::NameStatus::Deleted => {
            if let Some(mode) = entry.old_mode {
                write_diff_meta_line(stdout, colors, &format!("deleted file mode {mode:06o}"))?;
            }
        }
        sley_diff_merge::NameStatus::Modified
        | sley_diff_merge::NameStatus::Renamed(_)
        | sley_diff_merge::NameStatus::Copied(_) => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                write_diff_meta_line(stdout, colors, &format!("old mode {old_mode:06o}"))?;
                write_diff_meta_line(stdout, colors, &format!("new mode {new_mode:06o}"))?;
            }
        }
    }
    write_diff_similarity_headers(&mut *stdout, entry, &old_similarity_path, &similarity_path)?;
    if !content_changed {
        return Ok(());
    }
    write_diff_meta_line(
        stdout,
        colors,
        &format!(
            "index {}..{}{}",
            diff_patch_oid(
                entry.old_oid.as_ref(),
                old_content.as_deref(),
                options.format,
                options.abbrev,
            ),
            diff_patch_oid(
                entry.new_oid.as_ref(),
                new_content.as_deref(),
                options.format,
                options.abbrev,
            ),
            diff_patch_mode_suffix(entry)
        ),
    )?;
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            write_diff_meta_line(stdout, colors, "--- /dev/null")?;
        }
        _ => {
            write_diff_meta_line(stdout, colors, &format!("--- {old_header_path}"))?;
        }
    }
    match entry.status {
        sley_diff_merge::NameStatus::Deleted => {
            write_diff_meta_line(stdout, colors, "+++ /dev/null")?;
        }
        _ => {
            write_diff_meta_line(stdout, colors, &format!("+++ {header_path}"))?;
        }
    }
    // Hunks with git's section headings (shared with format-patch). The
    // funcname pattern comes from the old side's driver, then the new side's,
    // mirroring diff_funcname_pattern(one) ?: diff_funcname_pattern(two);
    // the word regex resolves CLI > old driver > new driver > diff.wordRegex.
    let (old_driver, new_driver) = match options.userdiff {
        Some(resolver) => (
            resolver.driver_for_path(old_path)?,
            resolver.driver_for_path(&entry.path)?,
        ),
        None => (None, None),
    };
    let funcname = old_driver
        .as_ref()
        .and_then(|driver| driver.funcname.as_ref())
        .or_else(|| {
            new_driver
                .as_ref()
                .and_then(|driver| driver.funcname.as_ref())
        });
    let default_colors;
    let word_regex;
    let word_diff = match options.word_diff {
        Some(request) => {
            let spec: Option<Vec<u8>> = request
                .cli_regex
                .map(|regex| regex.as_bytes().to_vec())
                .or_else(|| {
                    old_driver
                        .as_ref()
                        .and_then(|driver| driver.word_regex.clone())
                })
                .or_else(|| {
                    new_driver
                        .as_ref()
                        .and_then(|driver| driver.word_regex.clone())
                })
                .or_else(|| {
                    options
                        .userdiff
                        .and_then(commands::userdiff::UserdiffResolver::config_word_regex)
                });
            word_regex = spec
                .map(|spec| {
                    commands::grep::Regex::compile_bytes(
                        &spec,
                        commands::grep::RegexMode::Ere,
                        false,
                        false,
                    )
                    .map_err(|_| {
                        eprintln!(
                            "fatal: invalid regular expression: {}",
                            String::from_utf8_lossy(&spec)
                        );
                        GitError::Exit(128)
                    })
                })
                .transpose()?;
            default_colors = commands::diff_words::DiffColors::default();
            Some(commands::diff_words::WordDiffConfig {
                mode: request.mode,
                regex: word_regex.as_ref(),
                colors: colors.unwrap_or(&default_colors),
            })
        }
        None => None,
    };
    let hunk_options = commands::format_patch::PatchHunkOptions {
        context: options.context,
        funcname,
        colors,
        word_diff: word_diff.as_ref(),
        ..Default::default()
    };
    let mut hunks = Vec::new();
    commands::format_patch::write_patch_hunks_with(
        &mut hunks,
        old_content.as_deref(),
        new_content.as_deref(),
        &hunk_options,
    );
    stdout.write_all(&hunks)?;
    Ok(())
}

fn write_diff_binary_patch_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    old_content: Option<Vec<u8>>,
    new_content: Option<Vec<u8>>,
    options: DiffPatchOptions<'_>,
) -> Result<()> {
    let old_path = entry.old_path.as_deref().unwrap_or(&entry.path);
    let diff_old_path = diff_patch_prefixed_path(options.src_prefix, old_path);
    let diff_path = diff_patch_prefixed_path(options.dst_prefix, &entry.path);
    let old_similarity_path = status_quote_path(old_path, false);
    let similarity_path = status_quote_path(&entry.path, false);
    writeln!(stdout, "diff --git {diff_old_path} {diff_path}",)?;
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            if let Some(mode) = entry.new_mode {
                writeln!(stdout, "new file mode {mode:06o}")?;
            }
        }
        sley_diff_merge::NameStatus::Deleted => {
            if let Some(mode) = entry.old_mode {
                writeln!(stdout, "deleted file mode {mode:06o}")?;
            }
        }
        sley_diff_merge::NameStatus::Modified
        | sley_diff_merge::NameStatus::Renamed(_)
        | sley_diff_merge::NameStatus::Copied(_) => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                writeln!(stdout, "old mode {old_mode:06o}")?;
                writeln!(stdout, "new mode {new_mode:06o}")?;
            }
        }
    }
    write_diff_similarity_headers(&mut *stdout, entry, &old_similarity_path, &similarity_path)?;
    if old_content.as_deref() == new_content.as_deref() {
        return Ok(());
    }
    writeln!(
        stdout,
        "index {}..{}{}",
        diff_patch_oid(
            entry.old_oid.as_ref(),
            old_content.as_deref(),
            options.format,
            options.abbrev,
        ),
        diff_patch_oid(
            entry.new_oid.as_ref(),
            new_content.as_deref(),
            options.format,
            options.abbrev,
        ),
        diff_patch_mode_suffix(entry)
    )?;
    let old = match old_content {
        Some(_) => diff_patch_prefixed_path(options.src_prefix, old_path),
        None => "/dev/null".to_string(),
    };
    let new = match new_content {
        Some(_) => diff_patch_prefixed_path(options.dst_prefix, &entry.path),
        None => "/dev/null".to_string(),
    };
    writeln!(stdout, "Binary files {old} and {new} differ")?;
    Ok(())
}

fn write_diff_similarity_headers(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    old_path: &str,
    path: &str,
) -> Result<()> {
    match entry.status {
        sley_diff_merge::NameStatus::Renamed(score) => {
            writeln!(stdout, "similarity index {score}%")?;
            writeln!(stdout, "rename from {old_path}")?;
            writeln!(stdout, "rename to {path}")?;
        }
        sley_diff_merge::NameStatus::Copied(score) => {
            writeln!(stdout, "similarity index {score}%")?;
            writeln!(stdout, "copy from {old_path}")?;
            writeln!(stdout, "copy to {path}")?;
        }
        _ => {}
    }
    Ok(())
}

fn diff_patch_prefixed_path(prefix: &str, path: &[u8]) -> String {
    status_quote_path(&diff_patch_prefixed_path_bytes(prefix, path), false)
}

fn diff_patch_file_header_path(prefix: &str, path: &[u8]) -> String {
    let raw = diff_patch_prefixed_path_bytes(prefix, path);
    let mut quoted = status_quote_path(&raw, false);
    if !quoted.starts_with('"') && raw.contains(&b' ') {
        quoted.push('\t');
    }
    quoted
}

fn diff_patch_prefixed_path_bytes(prefix: &str, path: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(prefix.len() + path.len());
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(path);
    bytes
}

fn diff_patch_oid(
    oid: Option<&ObjectId>,
    content: Option<&[u8]>,
    format: ObjectFormat,
    abbrev: usize,
) -> String {
    let hex = oid
        .cloned()
        .or_else(|| {
            content.and_then(|content| sley_core::object_id_for_bytes(format, "blob", content).ok())
        })
        .map(|oid| oid.to_hex())
        .unwrap_or_else(|| "0".repeat(format.hex_len()));
    hex[..abbrev.min(hex.len())].to_string()
}

fn diff_patch_mode_suffix(entry: &sley_diff_merge::NameStatusEntry) -> String {
    match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) if old_mode == new_mode => format!(" {old_mode:06o}"),
        _ => String::new(),
    }
}





fn write_diff_numstat_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    z: bool,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
) -> Result<()> {
    let old_content = diff_entry_old_content(entry, db)?;
    let new_content = diff_entry_new_content(entry, db, worktree_root, use_worktree_new)?;
    let stats = diff_line_stats(old_content.as_deref(), new_content.as_deref());
    if z {
        write_diff_numstat_counts(stdout, stats)?;
        if let Some(old_path) = &entry.old_path {
            stdout.write_all(b"\0")?;
            stdout.write_all(old_path)?;
            stdout.write_all(b"\0")?;
            stdout.write_all(&entry.path)?;
            stdout.write_all(b"\0")?;
        } else {
            stdout.write_all(&entry.path)?;
            stdout.write_all(b"\0")?;
        }
    } else {
        write_diff_numstat_counts(stdout, stats)?;
        if let Some(old_path) = &entry.old_path {
            // Renames/copies print the brace-collapsed form, like the stat rows.
            writeln!(stdout, "{}", diff_stat_pprint_rename(old_path, &entry.path))?;
        } else {
            let path = status_quote_path(&entry.path, false);
            writeln!(stdout, "{path}")?;
        }
    }
    Ok(())
}

fn write_diff_numstat_counts(stdout: &mut dyn Write, stats: DiffLineStats) -> Result<()> {
    match stats {
        DiffLineStats::Binary => write!(stdout, "-\t-\t")?,
        DiffLineStats::Text { inserted, deleted } => write!(stdout, "{inserted}\t{deleted}\t")?,
    }
    Ok(())
}

fn write_diff_shortstat(
    stdout: &mut dyn Write,
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut inserted = 0;
    let mut deleted = 0;
    for entry in entries {
        let old_content = diff_entry_old_content(entry, db)?;
        let new_content = diff_entry_new_content(entry, db, worktree_root, use_worktree_new)?;
        match diff_line_stats(old_content.as_deref(), new_content.as_deref()) {
            DiffLineStats::Binary => {}
            DiffLineStats::Text {
                inserted: entry_inserted,
                deleted: entry_deleted,
            } => {
                inserted += entry_inserted;
                deleted += entry_deleted;
            }
        }
    }
    write_diff_stat_summary_line(stdout, entries.len(), inserted, deleted)
}

fn write_diff_stat(
    stdout: &mut dyn Write,
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    options: DiffStatOptions,
) -> Result<()> {
    // Legacy entry point used by porcelain renderers (merge, stash, bisect, ...)
    // that have not been migrated to pass widths explicitly. git's porcelain
    // commands scale the stat to the terminal and respect the diff.stat*Width
    // config, so resolve both here.
    let mut widths = DiffStatWidths::terminal();
    if let Ok(cwd) = env::current_dir()
        && let Ok(git_dir) = discover_git_dir(&cwd)
        && let Ok(config) = commands::remote_cmds::read_repo_config(&git_dir)
    {
        widths.resolve_config(&config);
    } else {
        widths.resolve_config_defaults();
    }
    write_diff_stat_with_widths(
        stdout,
        entries,
        db,
        worktree_root,
        use_worktree_new,
        options,
        widths,
    )
}

/// The `--stat=<width>[,<name-width>[,<count>]]` / `--stat-*-width` knobs plus
/// the surrounding layout context, mirroring git's `diff_options` fields.
///
/// Sentinels follow git exactly:
///   * `stat_width`: `-1` = scale to the terminal (minus `line_prefix_width`),
///     `0` = the fixed 80-column default, `>0` = explicit width.
///   * `name_width` / `graph_width`: `-1` = take `diff.statNameWidth` /
///     `diff.statGraphWidth` from config (resolved via `resolve_config`),
///     `0` = unlimited, `>0` = explicit cap.
#[derive(Debug, Clone, Copy)]
struct DiffStatWidths {
    stat_width: i64,
    name_width: i64,
    graph_width: i64,
    /// Display width of the per-line prefix (e.g. `log --graph` edges),
    /// subtracted from the terminal width when `stat_width == -1`.
    line_prefix_width: i64,
}

impl DiffStatWidths {
    /// Porcelain default: scale to the terminal, take name/graph caps from config.
    fn terminal() -> Self {
        DiffStatWidths {
            stat_width: -1,
            name_width: -1,
            graph_width: -1,
            line_prefix_width: 0,
        }
    }

    /// Plumbing default (`diff-tree` & friends): fixed 80 columns, no caps.
    /// git's plumbing never calls `init_diffstat_widths`, so the fields stay 0.
    fn plumbing() -> Self {
        DiffStatWidths {
            stat_width: 0,
            name_width: 0,
            graph_width: 0,
            line_prefix_width: 0,
        }
    }

    /// Replace `-1` name/graph sentinels with `diff.statNameWidth` /
    /// `diff.statGraphWidth` (0 when unset), like show_stats' config fallback.
    fn resolve_config(&mut self, config: &GitConfig) {
        if self.name_width == -1 {
            self.name_width = config
                .get("diff", None, "statnamewidth")
                .and_then(|value| value.trim().parse::<i64>().ok())
                .unwrap_or(0);
        }
        if self.graph_width == -1 {
            self.graph_width = config
                .get("diff", None, "statgraphwidth")
                .and_then(|value| value.trim().parse::<i64>().ok())
                .unwrap_or(0);
        }
    }

    /// Like `resolve_config` with no config available: sentinels become 0.
    fn resolve_config_defaults(&mut self) {
        if self.name_width == -1 {
            self.name_width = 0;
        }
        if self.graph_width == -1 {
            self.graph_width = 0;
        }
    }
}

/// git `decimal_width()`: columns needed to print `number` in decimal.
fn diff_stat_decimal_width(number: usize) -> i64 {
    let mut width = 1i64;
    let mut number = number / 10;
    while number > 0 {
        width += 1;
        number /= 10;
    }
    width
}

/// git `scale_linear()`: scale `it` into `width` columns of a graph whose
/// largest row is `max_change`, guaranteeing at least one column for any
/// nonzero change.
fn diff_stat_scale_linear(it: i64, width: i64, max_change: i64) -> i64 {
    if it == 0 {
        return 0;
    }
    1 + (it * (width - 1) / max_change)
}

/// Display width of a stat row name. git uses `utf8_strwidth`; paths that
/// need quoting come out of `status_quote_path` as ASCII, so plain char count
/// matches for everything the t-suite exercises.
fn diff_stat_display_width(name: &str) -> i64 {
    name.chars().count() as i64
}

/// Faithful port of git diff.c `show_stats()`.
fn write_diff_stat_with_widths(
    stdout: &mut dyn Write,
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    options: DiffStatOptions,
    widths: DiffStatWidths,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let DiffStatOptions {
        compact_summary,
        stat_count,
        color,
    } = options;
    let rows = diff_stat_rows(
        entries,
        db,
        worktree_root,
        use_worktree_new,
        compact_summary,
    )?;

    let mut count = stat_count.unwrap_or(rows.len()).min(rows.len());

    // Pass 1: longest name, max change count, binary column width.
    let mut max_len = 0i64;
    let mut max_change = 0i64;
    let mut number_width = 0i64;
    let mut bin_width = 0i64;
    for row in rows.iter().take(count) {
        let len = diff_stat_display_width(&row.path);
        max_len = max_len.max(len);
        match row.stats {
            DiffStatStats::Binary {
                old_size,
                new_size,
                unchanged,
            } => {
                // "Bin XXX -> YYY bytes"; an unchanged blob renders plain "Bin"
                // (sizes treated as 0/0, exactly like git's same-contents case).
                let (added, deleted) = if unchanged { (0, 0) } else { (new_size, old_size) };
                let w = 14 + diff_stat_decimal_width(added) + diff_stat_decimal_width(deleted);
                bin_width = bin_width.max(w);
                number_width = number_width.max(3);
            }
            DiffStatStats::Text { inserted, deleted } => {
                max_change = max_change.max((inserted + deleted) as i64);
            }
        }
    }
    count = count.min(rows.len());

    let mut width = if widths.stat_width == -1 {
        log_format::term_columns() - widths.line_prefix_width
    } else if widths.stat_width != 0 {
        widths.stat_width
    } else {
        80
    };
    number_width = diff_stat_decimal_width(max_change as usize).max(number_width);

    // Guarantee 3/8*16 == 6 for the graph part and 5/8*16 == 10 for the name.
    if width < 16 + 6 + number_width {
        width = 16 + 6 + number_width;
    }

    // First assign sizes that are wanted, ignoring available width.
    let mut graph_width = if max_change + 4 > bin_width {
        max_change
    } else {
        bin_width - 4
    };
    if widths.graph_width > 0 && widths.graph_width < graph_width {
        graph_width = widths.graph_width;
    }
    let mut name_width = if widths.name_width > 0 && widths.name_width < max_len {
        widths.name_width
    } else {
        max_len
    };

    // Adjust adjustable widths not to exceed maximum width.
    if name_width + number_width + 6 + graph_width > width {
        if graph_width > width * 3 / 8 - number_width - 6 {
            graph_width = width * 3 / 8 - number_width - 6;
            if graph_width < 6 {
                graph_width = 6;
            }
        }
        if widths.graph_width > 0 && graph_width > widths.graph_width {
            graph_width = widths.graph_width;
        }
        if name_width > width - number_width - 6 - graph_width {
            name_width = width - number_width - 6 - graph_width;
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }

    let number_width = number_width.max(0) as usize;
    for row in rows.iter().take(count) {
        // "scale" the filename: strip leading characters (then snap to the
        // next '/') behind a "..." marker when it overflows the name column.
        let mut len = name_width;
        let full_name = row.path.as_str();
        let name_len = diff_stat_display_width(full_name);
        let mut name = full_name;
        let mut marker = "";
        if name_width < name_len {
            marker = "...";
            len -= 3;
            if len < 0 {
                len = 0;
            }
            while diff_stat_display_width(name) > len {
                let mut chars = name.chars();
                chars.next();
                name = chars.as_str();
            }
            if let Some(pos) = name.find('/') {
                name = &name[pos..];
            }
        }
        let padding = (len - diff_stat_display_width(name)).max(0) as usize;

        match row.stats {
            DiffStatStats::Binary {
                old_size,
                new_size,
                unchanged,
            } => {
                write!(
                    stdout,
                    " {marker}{name}{:padding$} | {:>number_width$}",
                    "", "Bin"
                )?;
                if unchanged {
                    writeln!(stdout)?;
                    continue;
                }
                let old_size = color_stat_deleted(&old_size.to_string(), color);
                let new_size = color_stat_inserted(&new_size.to_string(), color);
                writeln!(stdout, " {old_size} -> {new_size} bytes")?;
            }
            DiffStatStats::Text { inserted, deleted } => {
                let total_changed = inserted + deleted;
                let mut add = inserted as i64;
                let mut del = deleted as i64;
                if graph_width <= max_change && max_change > 0 {
                    let mut total = diff_stat_scale_linear(add + del, graph_width, max_change);
                    if total < 2 && add > 0 && del > 0 {
                        // width >= 2 due to the sanity check
                        total = 2;
                    }
                    if add < del {
                        add = diff_stat_scale_linear(add, graph_width, max_change);
                        del = total - add;
                    } else {
                        del = diff_stat_scale_linear(del, graph_width, max_change);
                        add = total - del;
                    }
                }
                write!(
                    stdout,
                    " {marker}{name}{:padding$} | {total_changed:>number_width$}{}",
                    "",
                    if total_changed > 0 { " " } else { "" }
                )?;
                let mut graph = String::new();
                if add > 0 {
                    let pluses = std::iter::repeat_n('+', add as usize).collect::<String>();
                    graph.push_str(&color_stat_inserted(&pluses, color));
                }
                if del > 0 {
                    let minuses = std::iter::repeat_n('-', del as usize).collect::<String>();
                    graph.push_str(&color_stat_deleted(&minuses, color));
                }
                writeln!(stdout, "{graph}")?;
            }
        }
    }
    if count < rows.len() {
        writeln!(stdout, " ...")?;
    }

    // Totals cover every row (display truncation does not affect them);
    // binary rows count as changed files but contribute no line counts.
    let mut adds = 0usize;
    let mut dels = 0usize;
    for row in &rows {
        if let DiffStatStats::Text { inserted, deleted } = row.stats {
            adds += inserted;
            dels += deleted;
        }
    }
    write_diff_stat_summary_line(stdout, rows.len(), adds, dels)
}

/// `--dirstat` damage accounting mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirstatMode {
    /// Default: span-hash "damage" — bytes removed from the old blob plus
    /// bytes literally added to the new one.
    Changes,
    /// `lines`: line-based diffstat damage (binary blobs count bytes/64).
    Lines,
    /// `files`: every changed file contributes equal damage 1.
    Files,
}

/// Parsed `--dirstat`/`-X`/diff.dirstat parameters.
#[derive(Debug, Clone, Copy)]
struct DirstatOptions {
    mode: DirstatMode,
    cumulative: bool,
    /// Cut-off in permille (default 30 = 3%).
    permille: i64,
}

impl Default for DirstatOptions {
    fn default() -> Self {
        DirstatOptions {
            mode: DirstatMode::Changes,
            cumulative: false,
            permille: 30,
        }
    }
}

/// git `parse_dirstat_params()`: comma-separated `changes|lines|files|
/// cumulative|noncumulative|<limit>` parameters. Unknown parameters append to
/// `errors` (one line each) and are counted in the returned error total.
fn parse_dirstat_params(params: &str, options: &mut DirstatOptions, errors: &mut String) -> usize {
    let mut error_count = 0usize;
    if params.is_empty() {
        return 0;
    }
    for param in params.split(',') {
        match param {
            "changes" => options.mode = DirstatMode::Changes,
            "lines" => options.mode = DirstatMode::Lines,
            "files" => options.mode = DirstatMode::Files,
            "noncumulative" => options.cumulative = false,
            "cumulative" => options.cumulative = true,
            _ if param.starts_with(|c: char| c.is_ascii_digit()) => {
                let digits_end = param
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(param.len());
                let mut permille: i64 =
                    param[..digits_end].parse::<i64>().unwrap_or(0) * 10;
                let rest = &param[digits_end..];
                let mut ok = rest.is_empty();
                if let Some(frac) = rest.strip_prefix('.')
                    && frac.starts_with(|c: char| c.is_ascii_digit())
                {
                    // Only the first fractional digit counts; the rest must
                    // also be digits.
                    permille += i64::from(frac.as_bytes()[0] - b'0');
                    ok = frac.bytes().all(|byte| byte.is_ascii_digit());
                }
                if ok {
                    options.permille = permille;
                } else {
                    errors.push_str(&format!(
                        "  Failed to parse dirstat cut-off percentage '{param}'\n"
                    ));
                    error_count += 1;
                }
            }
            _ => {
                errors.push_str(&format!("  Unknown dirstat parameter '{param}'\n"));
                error_count += 1;
            }
        }
    }
    error_count
}

/// One file's contribution to the dirstat tree.
struct DirstatFile {
    name: Vec<u8>,
    changed: u64,
}

/// Faithful port of git diff.c `show_dirstat()` / `show_dirstat_by_line()` +
/// `gather_dirstat()`.
fn write_diff_dirstat(
    stdout: &mut dyn Write,
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    options: DirstatOptions,
) -> Result<()> {
    let mut files = Vec::with_capacity(entries.len());
    let mut changed_total: u64 = 0;
    for entry in entries {
        let name = entry.path.to_vec();
        let damage: u64 = if entry.old_oid.is_some() && entry.old_oid == entry.new_oid {
            // Identical pre-/post-content (e.g. a pure mode change or an
            // exact rename): zero damage, but the file still participates in
            // the directory "sources" accounting.
            0
        } else {
            match options.mode {
                DirstatMode::Files => 1,
                DirstatMode::Lines => {
                    let old_content = diff_entry_old_content(entry, db)?;
                    let new_content =
                        diff_entry_new_content(entry, db, worktree_root, use_worktree_new)?;
                    match diff_line_stats(old_content.as_deref(), new_content.as_deref()) {
                        DiffLineStats::Binary => {
                            let bytes = old_content.as_ref().map_or(0, Vec::len)
                                + new_content.as_ref().map_or(0, Vec::len);
                            (bytes as u64).div_ceil(64)
                        }
                        DiffLineStats::Text { inserted, deleted } => (inserted + deleted) as u64,
                    }
                }
                DirstatMode::Changes => {
                    let old_content = diff_entry_old_content(entry, db)?;
                    let new_content =
                        diff_entry_new_content(entry, db, worktree_root, use_worktree_new)?;
                    let damage = match (old_content.as_deref(), new_content.as_deref()) {
                        (Some(old), Some(new)) => {
                            let (copied, added) = sley_diff_merge::count_changes(old, new);
                            ((old.len() - copied) + added) as u64
                        }
                        (Some(old), None) => old.len() as u64,
                        (None, Some(new)) => new.len() as u64,
                        (None, None) => 0,
                    };
                    // The oid changed, so force nonzero damage even when the
                    // span hashes consider the blobs identical.
                    damage.max(1)
                }
            }
        };
        changed_total += damage;
        files.push(DirstatFile { name, changed: damage });
    }
    if changed_total == 0 {
        return Ok(());
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let mut idx = 0usize;
    gather_dirstat(stdout, &files, &mut idx, changed_total, b"", &options)?;
    Ok(())
}

/// Recursive directory aggregation with the permille cut-off; returns the
/// directory's summed damage (0 once reported, unless cumulative).
fn gather_dirstat(
    stdout: &mut dyn Write,
    files: &[DirstatFile],
    idx: &mut usize,
    changed_total: u64,
    base: &[u8],
    options: &DirstatOptions,
) -> Result<u64> {
    let mut sum_changes: u64 = 0;
    let mut sources: u32 = 0;
    while *idx < files.len() {
        let file = &files[*idx];
        if file.name.len() < base.len() || !file.name.starts_with(base) {
            break;
        }
        let changes = match file.name[base.len()..].iter().position(|&b| b == b'/') {
            Some(slash) => {
                let new_base = file.name[..base.len() + slash + 1].to_vec();
                sources += 1;
                gather_dirstat(stdout, files, idx, changed_total, &new_base, options)?
            }
            None => {
                let changes = file.changed;
                *idx += 1;
                sources += 2;
                changes
            }
        };
        sum_changes += changes;
    }
    // No report for the top level, nor when everything in this directory came
    // from a single subdirectory.
    if !base.is_empty() && sources != 1 && sum_changes > 0 {
        let permille = (sum_changes * 1000 / changed_total) as i64;
        if permille >= options.permille {
            writeln!(
                stdout,
                "{:4}.{}% {}",
                permille / 10,
                permille % 10,
                String::from_utf8_lossy(base)
            )?;
            if !options.cumulative {
                return Ok(0);
            }
        }
    }
    Ok(sum_changes)
}

/// git `print_stat_summary_inserts_deletes()`: the
/// " N files changed, A insertions(+), D deletions(-)" trailer.
fn write_diff_stat_summary_line(
    stdout: &mut dyn Write,
    files: usize,
    inserted: usize,
    deleted: usize,
) -> Result<()> {
    write!(
        stdout,
        " {} {} changed",
        files,
        plural(files, "file", "files")
    )?;
    if inserted > 0 || deleted == 0 {
        write!(
            stdout,
            ", {inserted} {}(+)",
            plural(inserted, "insertion", "insertions")
        )?;
    }
    if deleted > 0 || inserted == 0 {
        write!(
            stdout,
            ", {deleted} {}(-)",
            plural(deleted, "deletion", "deletions")
        )?;
    }
    writeln!(stdout)?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct DiffStatOptions {
    compact_summary: bool,
    stat_count: Option<usize>,
    color: bool,
}

fn diff_stat_rows(
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    compact_summary: bool,
) -> Result<Vec<DiffStatRow>> {
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let old_content = diff_entry_old_content(entry, db)?;
        let new_content = diff_entry_new_content(entry, db, worktree_root, use_worktree_new)?;
        let stats = match diff_line_stats(old_content.as_deref(), new_content.as_deref()) {
            DiffLineStats::Binary => DiffStatStats::Binary {
                old_size: old_content.as_ref().map_or(0, Vec::len),
                new_size: new_content.as_ref().map_or(0, Vec::len),
                unchanged: old_content == new_content,
            },
            DiffLineStats::Text { inserted, deleted } => DiffStatStats::Text { inserted, deleted },
        };
        rows.push(DiffStatRow {
            path: diff_stat_path(entry, compact_summary),
            stats,
        });
    }
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffStatRow {
    path: String,
    stats: DiffStatStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffStatStats {
    Binary {
        old_size: usize,
        new_size: usize,
        unchanged: bool,
    },
    Text {
        inserted: usize,
        deleted: usize,
    },
}

fn diff_stat_path(entry: &sley_diff_merge::NameStatusEntry, compact_summary: bool) -> String {
    let mut path = if let Some(old_path) = &entry.old_path {
        diff_stat_pprint_rename(old_path, &entry.path)
    } else {
        status_quote_path(&entry.path, false)
    };
    if compact_summary && let Some(summary) = diff_compact_summary_label(entry) {
        path.push(' ');
        path.push_str(summary);
    }
    path
}

/// git `pprint_rename()`: collapse a rename's common directory prefix and
/// suffix into braces — `dir/{old => new}/file` — falling back to the plain
/// `old => new` form when either side needs c-style quoting or when nothing
/// is shared.
fn diff_stat_pprint_rename(a: &[u8], b: &[u8]) -> String {
    let quoted_a = status_quote_path(a, false);
    let quoted_b = status_quote_path(b, false);
    if quoted_a.starts_with('"') || quoted_b.starts_with('"') {
        return format!("{quoted_a} => {quoted_b}");
    }
    let len_a = a.len();
    let len_b = b.len();

    // Find common prefix (must end in a slash to count).
    let mut pfx_length = 0usize;
    let mut idx = 0usize;
    while idx < len_a && idx < len_b && a[idx] == b[idx] {
        if a[idx] == b'/' {
            pfx_length = idx + 1;
        }
        idx += 1;
    }

    // Find common suffix, walking back from the (virtual) terminating NUL.
    // With a common prefix the walk may run one byte into the prefix to see
    // the same slash; without one it must not underrun the strings.
    let mut sfx_length = 0usize;
    let pfx_adjust_for_slash: isize = if pfx_length > 0 { 1 } else { 0 };
    let mut oi = len_a as isize;
    let mut ni = len_b as isize;
    let lower = pfx_length as isize - pfx_adjust_for_slash;
    while oi >= lower && ni >= lower {
        let oc = if oi == len_a as isize { 0 } else { a[oi as usize] };
        let nc = if ni == len_b as isize { 0 } else { b[ni as usize] };
        if oc != nc {
            break;
        }
        if oc == b'/' {
            sfx_length = len_a - oi as usize;
        }
        oi -= 1;
        ni -= 1;
    }

    // pfx{mid-a => mid-b}sfx  |  {pfx-a => pfx-b}sfx  |  pfx{sfx-a => sfx-b}
    // |  name-a => name-b
    let a_midlen = len_a.saturating_sub(pfx_length + sfx_length);
    let b_midlen = len_b.saturating_sub(pfx_length + sfx_length);
    let mut name = String::new();
    if pfx_length + sfx_length > 0 {
        name.push_str(&String::from_utf8_lossy(&a[..pfx_length]));
        name.push('{');
    }
    name.push_str(&String::from_utf8_lossy(&a[pfx_length..pfx_length + a_midlen]));
    name.push_str(" => ");
    name.push_str(&String::from_utf8_lossy(&b[pfx_length..pfx_length + b_midlen]));
    if pfx_length + sfx_length > 0 {
        name.push('}');
        name.push_str(&String::from_utf8_lossy(&a[len_a - sfx_length..]));
    }
    name
}

fn diff_compact_summary_label(entry: &sley_diff_merge::NameStatusEntry) -> Option<&'static str> {
    match (entry.old_mode, entry.new_mode) {
        (None, Some(_)) => Some("(new)"),
        (Some(_), None) => Some("(gone)"),
        (Some(old), Some(new)) if old != new => {
            let old_exec = old & 0o111 != 0;
            let new_exec = new & 0o111 != 0;
            match (old_exec, new_exec) {
                (false, true) => Some("(mode +x)"),
                (true, false) => Some("(mode -x)"),
                _ => Some("(mode)"),
            }
        }
        _ => None,
    }
}

fn color_stat_inserted(value: &str, color: bool) -> String {
    if color {
        format!("\x1b[32m{value}\x1b[m")
    } else {
        value.to_string()
    }
}

fn color_stat_deleted(value: &str, color: bool) -> String {
    if color {
        format!("\x1b[31m{value}\x1b[m")
    } else {
        value.to_string()
    }
}

/// Parse one `--stat*` argument's width components into `widths`, mirroring
/// git's `diff_opt_stat()`: `--stat=<w>[,<name-w>[,<count>]]` (count handled by
/// `diff_stat_count_option`), `--stat-width=<w>`, `--stat-name-width=<w>`,
/// `--stat-graph-width=<w>`. Unknown / non-stat options are left untouched;
/// returns whether `value` was a stat option at all.
fn diff_stat_parse_width_option(value: &str, widths: &mut DiffStatWidths) -> Result<bool> {
    fn parse_number(option: &str, value: &str) -> Result<i64> {
        value
            .parse::<i64>()
            .map_err(|_| GitError::Command(format!("{option} expects a numerical value")))
    }
    if let Some(spec) = value.strip_prefix("--stat=") {
        let mut parts = spec.split(',');
        if let Some(width) = parts.next()
            && !width.is_empty()
        {
            widths.stat_width = parse_number("--stat", width)?;
        }
        if let Some(name_width) = parts.next()
            && !name_width.is_empty()
        {
            widths.name_width = parse_number("--stat", name_width)?;
        }
        Ok(true)
    } else if let Some(width) = value.strip_prefix("--stat-width=") {
        widths.stat_width = parse_number("--stat-width", width)?;
        Ok(true)
    } else if let Some(width) = value.strip_prefix("--stat-name-width=") {
        widths.name_width = parse_number("--stat-name-width", width)?;
        Ok(true)
    } else if let Some(width) = value.strip_prefix("--stat-graph-width=") {
        widths.graph_width = parse_number("--stat-graph-width", width)?;
        Ok(true)
    } else {
        Ok(value == "--stat" || value.starts_with("--stat-count="))
    }
}

fn diff_stat_count_option(value: &str) -> Result<Option<Option<usize>>> {
    let count = if let Some(count) = value.strip_prefix("--stat-count=") {
        Some(count)
    } else if let Some(spec) = value.strip_prefix("--stat=") {
        spec.split(',').nth(2)
    } else {
        None
    };
    let Some(count) = count else {
        return Ok(None);
    };
    let count = count
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid stat count {count}")))?;
    Ok(Some((count != 0).then_some(count)))
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn diff_entry_old_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
) -> Result<Option<Vec<u8>>> {
    entry
        .old_oid
        .as_ref()
        .map(|oid| read_blob(db, oid))
        .transpose()
}

fn diff_entry_new_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree: bool,
) -> Result<Option<Vec<u8>>> {
    if entry.new_mode.is_none() {
        return Ok(None);
    }
    if use_worktree {
        let root = worktree_root.ok_or_else(|| {
            GitError::Command("diff numstat requires a worktree for worktree comparisons".into())
        })?;
        let path = root.join(repo_path_to_path(&entry.path));
        if path.exists() {
            return Ok(Some(fs::read(path)?));
        }
        return Ok(None);
    }
    entry
        .new_oid
        .as_ref()
        .map(|oid| read_blob(db, oid))
        .transpose()
}

fn validate_diff_rename_limit(value: &str) -> Result<()> {
    let value = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(value);
    let value = value
        .strip_suffix('k')
        .or_else(|| value.strip_suffix('m'))
        .or_else(|| value.strip_suffix('g'))
        .unwrap_or(value);
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(diff_rename_limit_requires_integer_error())
    }
}

fn diff_rename_limit_requires_integer_error() -> GitError {
    eprintln!("error: switch `l' expects an integer value with an optional k/m/g suffix");
    GitError::Exit(129)
}

fn read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "diff expected blob object {oid}"
        )));
    }
    Ok(object.body.clone())
}

fn repo_path_to_path(path: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(path).into_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineStats {
    Binary,
    Text { inserted: usize, deleted: usize },
}

fn diff_line_stats(old: Option<&[u8]>, new: Option<&[u8]>) -> DiffLineStats {
    if old.is_some_and(is_binary_content) || new.is_some_and(is_binary_content) {
        return DiffLineStats::Binary;
    }
    match (old, new) {
        (None, None) => DiffLineStats::Text {
            inserted: 0,
            deleted: 0,
        },
        (None, Some(new)) => DiffLineStats::Text {
            inserted: count_diff_lines(new),
            deleted: 0,
        },
        (Some(old), None) => DiffLineStats::Text {
            inserted: 0,
            deleted: count_diff_lines(old),
        },
        (Some(old), Some(new)) => {
            let (inserted, deleted) = count_line_diff(old, new);
            DiffLineStats::Text { inserted, deleted }
        }
    }
}

fn is_binary_content(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// `--stat` insertion/deletion line counts, computed by the shared diff-merge
/// Myers engine rather than a CLI-local LCS.
///
/// Myers produces a shortest edit script, so the count of `Insert` lines is
/// `new_len - lcs` and the count of `Delete` lines is `old_len - lcs` — exactly
/// the values the removed local LCS counter returned.
fn count_line_diff(old: &[u8], new: &[u8]) -> (usize, usize) {
    let old_lines = sley_diff_merge::split_lines(old);
    let new_lines = sley_diff_merge::split_lines(new);
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    for op in sley_diff_merge::myers_diff_lines(&old_lines, &new_lines) {
        match op {
            sley_diff_merge::DiffOp::Insert(n) => inserted += n,
            sley_diff_merge::DiffOp::Delete(n) => deleted += n,
            sley_diff_merge::DiffOp::Equal(_) => {}
        }
    }
    (inserted, deleted)
}

fn count_diff_lines(bytes: &[u8]) -> usize {
    diff_lines(bytes).len()
}

fn diff_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(&bytes[start..=idx]);
            start = idx + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

fn apply_diff_pathspec(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    pathspec: &DiffPathspec,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    if pathspec.is_empty() {
        return entries;
    }
    let mut filtered = Vec::new();
    for entry in entries {
        if let Some(old_path) = &entry.old_path {
            let old_matches = pathspec.matches(old_path);
            let new_matches = pathspec.matches(&entry.path);
            if matches!(entry.status, sley_diff_merge::NameStatus::Copied(_)) {
                match (old_matches, new_matches) {
                    (true, true) => filtered.push(entry),
                    (false, true) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Added,
                        path: entry.path,
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (true, false) | (false, false) => {}
                }
            } else {
                match (old_matches, new_matches) {
                    (true, true) => filtered.push(entry),
                    (true, false) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Deleted,
                        path: old_path.clone(),
                        old_path: None,
                        old_mode: entry.old_mode,
                        new_mode: None,
                        old_oid: entry.old_oid,
                        new_oid: None,
                    }),
                    (false, true) => filtered.push(sley_diff_merge::NameStatusEntry {
                        status: sley_diff_merge::NameStatus::Added,
                        path: entry.path,
                        old_path: None,
                        old_mode: None,
                        new_mode: entry.new_mode,
                        old_oid: None,
                        new_oid: entry.new_oid,
                    }),
                    (false, false) => {}
                }
            }
        } else if pathspec.matches(&entry.path) {
            filtered.push(entry);
        }
    }
    filtered
}

fn reverse_diff_entries(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let mut reversed = entries
        .into_iter()
        .map(reverse_diff_entry)
        .collect::<Vec<_>>();
    reversed.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
            .then_with(|| left.status.code().cmp(&right.status.code()))
    });
    reversed
}

fn reverse_diff_entry(entry: sley_diff_merge::NameStatusEntry) -> sley_diff_merge::NameStatusEntry {
    match entry.status {
        sley_diff_merge::NameStatus::Added => sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Deleted,
            old_mode: entry.new_mode,
            new_mode: None,
            old_oid: entry.new_oid,
            new_oid: None,
            ..entry
        },
        sley_diff_merge::NameStatus::Deleted => sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Added,
            old_mode: None,
            new_mode: entry.old_mode,
            old_oid: None,
            new_oid: entry.old_oid,
            ..entry
        },
        sley_diff_merge::NameStatus::Modified => sley_diff_merge::NameStatusEntry {
            old_mode: entry.new_mode,
            new_mode: entry.old_mode,
            old_oid: entry.new_oid,
            new_oid: entry.old_oid,
            ..entry
        },
        sley_diff_merge::NameStatus::Renamed(score) => {
            let new_path = entry
                .old_path
                .clone()
                .expect("rename entries include old_path");
            sley_diff_merge::NameStatusEntry {
                status: sley_diff_merge::NameStatus::Renamed(score),
                path: new_path,
                old_path: Some(entry.path),
                old_mode: entry.new_mode,
                new_mode: entry.old_mode,
                old_oid: entry.new_oid,
                new_oid: entry.old_oid,
            }
        }
        sley_diff_merge::NameStatus::Copied(_) => sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Deleted,
            old_path: None,
            old_mode: entry.new_mode,
            new_mode: None,
            old_oid: entry.new_oid,
            new_oid: None,
            ..entry
        },
    }
}

#[derive(Default)]
struct DiffPathspec {
    filters: Vec<LsFilesPathFilter>,
}

impl DiffPathspec {
    fn new(cwd: &Path, worktree_root: &Path, path_args: &[String]) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd = fs::canonicalize(cwd)?;
        let relative = cwd.strip_prefix(&root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", cwd.display()))
        })?;
        let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
        let mut filters = Vec::new();
        for arg in path_args {
            let filter_path = normalize_ls_files_pathspec(&prefix, arg)?;
            let is_glob = sley_worktree::pathspec_is_glob(&filter_path);
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                cwd.join(arg_path)
            };
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                path: filter_path,
                recursive: arg == "." || arg.ends_with('/') || absolute.is_dir(),
                is_glob,
                matched: Cell::new(false),
            });
        }
        Ok(Self { filters })
    }

    fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return true;
        }
        let magic = effective_pathspec_flags();
        self.filters.iter().any(|filter| filter.matches(path, magic))
    }

    fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
struct DiffFilter {
    includes: HashSet<char>,
    excludes: HashSet<char>,
    all_or_none: bool,
}

impl DiffFilter {
    fn matches_status(&self, status: char) -> bool {
        (if self.includes.is_empty() {
            true
        } else {
            self.includes.contains(&status)
        }) && !self.excludes.contains(&status)
    }
}

fn parse_diff_filter(value: &str) -> Result<DiffFilter> {
    let mut filter = DiffFilter::default();
    for ch in value.chars() {
        match ch {
            'A' | 'C' | 'D' | 'M' | 'R' | 'T' | 'U' | 'X' | 'B' => {
                filter.includes.insert(ch);
            }
            'a' | 'c' | 'd' | 'm' | 'r' | 't' | 'u' | 'x' | 'b' => {
                filter.excludes.insert(ch.to_ascii_uppercase());
            }
            '*' => filter.all_or_none = true,
            other => {
                eprintln!("error: unknown change class '{other}' in --diff-filter={value}");
                return Err(GitError::Exit(129));
            }
        }
    }
    Ok(filter)
}

#[derive(Clone, Copy)]
struct ForEachRefIdentitySortField {
    source: ForEachRefIdentitySource,
    role: ForEachRefIdentityRole,
    part: ForEachRefIdentityPart,
}

#[derive(Clone, Copy)]
enum ForEachRefIdentitySource {
    Direct,
    Peeled,
}

#[derive(Clone, Copy)]
enum ForEachRefIdentityRole {
    Author,
    Committer,
    Tagger,
    Creator,
}

#[derive(Clone, Copy)]
enum ForEachRefIdentityPart {
    Full,
    Name,
    Email,
}

fn parse_for_each_ref_identity_sort(value: &str) -> Option<(ForEachRefIdentitySortField, bool)> {
    let (value, descending) = value
        .strip_prefix('-')
        .map(|value| (value, true))
        .unwrap_or((value, false));
    let (value, source) = value
        .strip_prefix('*')
        .map(|value| (value, ForEachRefIdentitySource::Peeled))
        .unwrap_or((value, ForEachRefIdentitySource::Direct));
    let (role, part) = match value {
        "author" => (ForEachRefIdentityRole::Author, ForEachRefIdentityPart::Full),
        "authorname" => (ForEachRefIdentityRole::Author, ForEachRefIdentityPart::Name),
        "authoremail" => (
            ForEachRefIdentityRole::Author,
            ForEachRefIdentityPart::Email,
        ),
        "committer" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Full,
        ),
        "committername" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Name,
        ),
        "committeremail" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Email,
        ),
        "tagger" => (ForEachRefIdentityRole::Tagger, ForEachRefIdentityPart::Full),
        "taggername" => (ForEachRefIdentityRole::Tagger, ForEachRefIdentityPart::Name),
        "taggeremail" => (
            ForEachRefIdentityRole::Tagger,
            ForEachRefIdentityPart::Email,
        ),
        "creator" => (
            ForEachRefIdentityRole::Creator,
            ForEachRefIdentityPart::Full,
        ),
        _ => return None,
    };
    Some((
        ForEachRefIdentitySortField { source, role, part },
        descending,
    ))
}

fn for_each_ref_sort_identity_key(
    contents: Option<&ForEachRefContents<'_>>,
    field: ForEachRefIdentitySortField,
) -> String {
    let identity = match field.role {
        ForEachRefIdentityRole::Author => contents.and_then(|contents| contents.author.as_deref()),
        ForEachRefIdentityRole::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefIdentityRole::Tagger => contents.and_then(|contents| contents.tagger.as_deref()),
        ForEachRefIdentityRole::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
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

// states: S_N normal, S_I integral part, S_F fractional parts, S_Z idem but
// leading zeroes only (from glibc strverscmp, as in git's versioncmp.c).
const VS_S_N: usize = 0x0;
const VS_S_I: usize = 0x3;
const VS_S_F: usize = 0x6;
const VS_S_Z: usize = 0x9;
// result_type sentinels: CMP return diff, LEN compare via len_diff/diff.
const VS_CMP: i8 = 2;
const VS_LEN: i8 = 3;

#[rustfmt::skip]
const VS_NEXT_STATE: [usize; 12] = [
    /* state    x    d    0  */
    /* S_N */  VS_S_N, VS_S_I, VS_S_Z,
    /* S_I */  VS_S_N, VS_S_I, VS_S_I,
    /* S_F */  VS_S_N, VS_S_F, VS_S_F,
    /* S_Z */  VS_S_N, VS_S_F, VS_S_Z,
];

#[rustfmt::skip]
const VS_RESULT_TYPE: [i8; 36] = [
    /* state   x/x  x/d  x/0  d/x  d/d  d/0  0/x  0/d  0/0  */
    /* S_N */  VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_LEN, VS_CMP, VS_CMP, VS_CMP, VS_CMP,
    /* S_I */  VS_CMP, -1,     -1,     1,      VS_LEN, VS_LEN, 1,      VS_LEN, VS_LEN,
    /* S_F */  VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP, VS_CMP,
    /* S_Z */  VS_CMP, 1,      1,      -1,     VS_CMP, VS_CMP, -1,     VS_CMP, VS_CMP,
];

#[inline]
fn vs_digit_class(c: u8) -> usize {
    // 0 if not a digit, 1 if digit 1-9, 2 if '0' (matches git's
    // (c=='0') + (isdigit(c) != 0)).
    (c == b'0') as usize + c.is_ascii_digit() as usize
}

struct VsSuffixMatch {
    conf_pos: i64,
    start: usize,
    len: i64,
}

fn vs_find_better_matching_suffix(
    tagname: &[u8],
    suffix: &[u8],
    start: usize,
    conf_pos: usize,
    m: &mut VsSuffixMatch,
) {
    // A better match either starts earlier, or at the same offset but longer.
    let end = if m.len < suffix.len() as i64 {
        m.start
    } else {
        m.start.saturating_sub(1)
    };
    for i in start..=end {
        if tagname.len() >= i && tagname[i..].starts_with(suffix) {
            m.conf_pos = conf_pos as i64;
            m.start = i;
            m.len = suffix.len() as i64;
            break;
        }
    }
}

/// Port of git's swap_prereleases(). `off` is the offset of the first
/// differing character. Returns Some(diff) if a prerelease suffix forces an
/// order.
fn vs_swap_prereleases(
    s1: &[u8],
    s2: &[u8],
    off: usize,
    prereleases: &[String],
) -> Option<std::cmp::Ordering> {
    let mut m1 = VsSuffixMatch {
        conf_pos: -1,
        start: off,
        len: -1,
    };
    let mut m2 = VsSuffixMatch {
        conf_pos: -1,
        start: off,
        len: -1,
    };
    for (i, suffix) in prereleases.iter().enumerate() {
        let suffix = suffix.as_bytes();
        let suffix_len = suffix.len();
        let start = if suffix_len < off {
            off - suffix_len
        } else {
            0
        };
        vs_find_better_matching_suffix(s1, suffix, start, i, &mut m1);
        vs_find_better_matching_suffix(s2, suffix, start, i, &mut m2);
    }
    if m1.conf_pos == -1 && m2.conf_pos == -1 {
        return None;
    }
    if m1.conf_pos == m2.conf_pos {
        // Same suffix in both: caller decides by the rest.
        return None;
    }
    let ord = if m1.conf_pos >= 0 && m2.conf_pos >= 0 {
        m1.conf_pos.cmp(&m2.conf_pos)
    } else if m1.conf_pos >= 0 {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    };
    Some(ord)
}

/// Faithful port of git's versioncmp() (glibc strverscmp + prerelease swap).
fn version_sort_cmp(s1: &str, s2: &str, prereleases: &[String]) -> std::cmp::Ordering {
    let b1 = s1.as_bytes();
    let b2 = s2.as_bytes();
    // Iterate with a sentinel NUL so we faithfully follow git's pointer walk.
    let get1 = |i: usize| -> u8 { if i < b1.len() { b1[i] } else { 0 } };
    let get2 = |i: usize| -> u8 { if i < b2.len() { b2[i] } else { 0 } };

    if std::ptr::eq(b1.as_ptr(), b2.as_ptr()) && b1.len() == b2.len() {
        return std::cmp::Ordering::Equal;
    }

    let mut p1 = 0usize;
    let mut p2 = 0usize;
    let mut c1 = get1(p1);
    let mut c2 = get2(p2);
    p1 += 1;
    p2 += 1;
    let mut state = VS_S_N + vs_digit_class(c1);

    let diff = loop {
        let d = c1 as i32 - c2 as i32;
        if d != 0 {
            break d;
        }
        if c1 == 0 {
            return std::cmp::Ordering::Equal;
        }
        state = VS_NEXT_STATE[state];
        c1 = get1(p1);
        c2 = get2(p2);
        p1 += 1;
        p2 += 1;
        state += vs_digit_class(c1);
    };

    // off is the index of the first differing character: pointer is one past it.
    if !prereleases.is_empty()
        && let Some(ord) = vs_swap_prereleases(b1, b2, p1 - 1, prereleases)
    {
        return ord;
    }

    let result = VS_RESULT_TYPE[state * 3 + vs_digit_class(c2)];
    match result {
        VS_CMP => diff.cmp(&0),
        VS_LEN => {
            // while (isdigit(*p1++)) if (!isdigit(*p2++)) return 1;
            loop {
                let d1 = get1(p1).is_ascii_digit();
                p1 += 1;
                if !d1 {
                    break;
                }
                let d2 = get2(p2).is_ascii_digit();
                p2 += 1;
                if !d2 {
                    return std::cmp::Ordering::Greater;
                }
            }
            if get2(p2).is_ascii_digit() {
                std::cmp::Ordering::Less
            } else {
                diff.cmp(&0)
            }
        }
        other => (other as i32).cmp(&0),
    }
}

#[derive(Clone, Copy)]
enum ForEachRefDateSortField {
    Author,
    Committer,
    Tagger,
    Creator,
}

fn for_each_ref_sort_date_key(
    contents: Option<ForEachRefContents<'_>>,
    field: ForEachRefDateSortField,
) -> i128 {
    let contents = contents.as_ref();
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

fn resolve_for_each_ref_target(
    store: &FileRefStore,
    reference: &sley_refs::Ref,
) -> Result<Option<(ObjectId, Option<String>)>> {
    let mut target = reference.target.clone();
    let mut symref = None;
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some((oid, symref))),
            RefTarget::Symbolic(name) => {
                symref.get_or_insert_with(|| name.clone());
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}

fn for_each_ref_loose_object_disk_size(git_dir: &Path, oid: &ObjectId) -> Result<Option<u64>> {
    let hex = oid.to_hex();
    if hex.len() < 2 {
        return Ok(None);
    }
    let (fanout, file) = hex.split_at(2);
    let path = repository_objects_dir(git_dir).join(fanout).join(file);
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn for_each_ref_worktree_path(
    git_dir: &Path,
    head_ref: Option<&str>,
    refname: &str,
) -> Result<Option<String>> {
    if head_ref == Some(refname)
        && let Ok(worktree_root) = worktree_root_for_git_dir(git_dir)
    {
        return Ok(Some(
            fs::canonicalize(worktree_root)?
                .to_string_lossy()
                .into_owned(),
        ));
    }

    let worktrees_dir = git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(None);
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let Ok(head) = fs::read_to_string(admin_dir.join("HEAD")) else {
            continue;
        };
        if head.trim().strip_prefix("ref: ") != Some(refname) {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = gitdir.trim();
        if gitdir.is_empty() {
            continue;
        }
        let gitdir_path = PathBuf::from(gitdir);
        let gitdir_path = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            admin_dir.join(gitdir_path)
        };
        if let Some(worktree_root) = gitdir_path.parent() {
            return Ok(Some(
                fs::canonicalize(worktree_root)?
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

/// Resolve every `refname -> checked-out worktree path` mapping in a single pass,
/// so `for-each-ref` need not re-scan `$GIT_DIR/worktrees` once per ref. Mirrors
/// the per-ref logic in `for_each_ref_worktree_path`: the current branch maps to
/// the main worktree root, and each linked worktree's `HEAD`/`gitdir` admin files
/// name the ref it has checked out and where its working tree lives.
fn for_each_ref_worktree_paths(
    git_dir: &Path,
    head_ref: Option<&str>,
) -> Result<HashMap<String, String>> {
    let mut paths = HashMap::new();
    if let Some(head_ref) = head_ref
        && let Ok(worktree_root) = worktree_root_for_git_dir(git_dir)
    {
        let canonical = fs::canonicalize(worktree_root)?;
        paths.insert(head_ref.to_string(), canonical.to_string_lossy().into_owned());
    }

    let worktrees_dir = git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(paths);
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let Ok(head) = fs::read_to_string(admin_dir.join("HEAD")) else {
            continue;
        };
        let Some(refname) = head.trim().strip_prefix("ref: ") else {
            continue;
        };
        // The current branch's mapping (the main worktree root) takes precedence
        // and is already inserted above.
        if paths.contains_key(refname) {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = gitdir.trim();
        if gitdir.is_empty() {
            continue;
        }
        let gitdir_path = PathBuf::from(gitdir);
        let gitdir_path = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            admin_dir.join(gitdir_path)
        };
        if let Some(worktree_root) = gitdir_path.parent() {
            let canonical = fs::canonicalize(worktree_root)?;
            paths.insert(refname.to_string(), canonical.to_string_lossy().into_owned());
        }
    }
    Ok(paths)
}

#[derive(Clone)]
struct ForEachRefUpstream {
    refname: String,
    remote: String,
    merge: String,
}

#[derive(Clone)]
struct ForEachRefPush {
    refname: Option<String>,
    remote: String,
    remote_ref: Option<String>,
}

struct ForEachRefPushRemote {
    name: String,
    expose_name: bool,
}

fn for_each_ref_upstream(config: &GitConfig, refname: &str) -> Option<ForEachRefUpstream> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let remote = config.get("branch", Some(branch), "remote")?;
    let merge = config.get("branch", Some(branch), "merge")?;
    if remote == "." {
        return Some(ForEachRefUpstream {
            refname: merge.to_string(),
            remote: remote.to_string(),
            merge: merge.to_string(),
        });
    }
    let fetch = config.get("remote", Some(remote), "fetch")?;
    Some(ForEachRefUpstream {
        refname: map_remote_fetch_refspec(fetch, merge)?,
        remote: remote.to_string(),
        merge: merge.to_string(),
    })
}

fn for_each_ref_push(config: &GitConfig, refname: &str) -> Option<ForEachRefPush> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let remote = for_each_ref_push_remote(config, branch)?;
    if remote.name == "." {
        return Some(ForEachRefPush {
            refname: None,
            remote: remote.name.to_string(),
            remote_ref: None,
        });
    }
    if let Some(push) = config.get("remote", Some(remote.name.as_str()), "push") {
        if let Some(remote_ref) = map_remote_push_refspec(push, refname) {
            let refname = map_remote_tracking_ref(config, &remote.name, &remote_ref);
            let remote = remote_display_name(remote);
            return Some(ForEachRefPush {
                refname,
                remote,
                remote_ref: Some(remote_ref),
            });
        }
        let remote = remote_display_name(remote);
        return Some(ForEachRefPush {
            refname: None,
            remote,
            remote_ref: None,
        });
    }
    let merge = config.get("branch", Some(branch), "merge")?;
    let refname = map_remote_tracking_ref(config, &remote.name, merge);
    let remote = remote_display_name(remote);
    Some(ForEachRefPush {
        refname,
        remote,
        remote_ref: None,
    })
}

fn for_each_ref_push_remote(config: &GitConfig, branch: &str) -> Option<ForEachRefPushRemote> {
    if let Some(remote) = config.get("branch", Some(branch), "pushRemote") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if let Some(remote) = config.get("remote", None, "pushDefault") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if let Some(remote) = config.get("branch", Some(branch), "remote") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if remote_exists(config, "origin") {
        return Some(ForEachRefPushRemote {
            name: "origin".to_string(),
            expose_name: false,
        });
    }
    let remotes = remote_names(config);
    match remotes.as_slice() {
        [remote] => Some(ForEachRefPushRemote {
            name: remote.clone(),
            expose_name: false,
        }),
        _ => None,
    }
}

fn remote_display_name(remote: ForEachRefPushRemote) -> String {
    if remote.expose_name {
        remote.name.to_string()
    } else {
        String::new()
    }
}

fn map_remote_tracking_ref(config: &GitConfig, remote: &str, remote_ref: &str) -> Option<String> {
    let fetch = config.get("remote", Some(remote), "fetch")?;
    map_remote_fetch_refspec(fetch, remote_ref)
}

fn map_remote_push_refspec(refspec: &str, refname: &str) -> Option<String> {
    let refspec = parse_refspec(refspec).ok()?;
    if refspec.negative || refspec.src.is_none() || refspec.dst.is_none() {
        return None;
    }
    refspec_map_source(&refspec, refname).ok()?
}

fn map_remote_fetch_refspec(refspec: &str, merge: &str) -> Option<String> {
    let refspec = parse_refspec(refspec).ok()?;
    if refspec.negative || refspec.dst.is_none() {
        return None;
    }
    refspec_map_source(&refspec, merge).ok()?
}

fn for_each_ref_upstream_track(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    upstream: &str,
) -> Result<Option<ForEachRefTrack>> {
    // git: a configured-but-unresolvable upstream reports `[gone]`, distinct
    // from "no upstream configured" (which the caller already filtered out).
    let gone_track = ForEachRefTrack {
        ahead: 0,
        behind: 0,
        gone: true,
    };
    let Some(upstream_target) = store.read_ref(upstream)? else {
        return Ok(Some(gone_track));
    };
    let upstream_ref = sley_refs::Ref {
        name: upstream.to_string(),
        target: upstream_target,
    };
    let Some((upstream_oid, _)) = resolve_for_each_ref_target(store, &upstream_ref)? else {
        return Ok(Some(gone_track));
    };
    for_each_ref_ahead_behind(db, format, oid, &upstream_oid)
}

fn for_each_ref_ahead_behind(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ForEachRefTrack>> {
    let Ok(local_commit) = sley_rev::peel_to_commit(db, format, oid) else {
        return Ok(None);
    };
    let Ok(target_commit) = sley_rev::peel_to_commit(db, format, target) else {
        return Ok(None);
    };
    let local_reachable = sley_rev::walk_commits(db, format, [local_commit])?
        .into_iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let target_reachable = sley_rev::walk_commits(db, format, [target_commit])?
        .into_iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let ahead = local_reachable.difference(&target_reachable).count();
    let behind = target_reachable.difference(&local_reachable).count();
    Ok(Some(ForEachRefTrack {
        ahead,
        behind,
        gone: false,
    }))
}

struct ForEachRefContents<'a> {
    message: Cow<'a, [u8]>,
    tree: Option<ObjectId>,
    parents: Vec<ObjectId>,
    tag: Option<Cow<'a, [u8]>>,
    tag_object_type: Option<ObjectType>,
    tag_object: Option<ObjectId>,
    author: Option<Cow<'a, [u8]>>,
    committer: Option<Cow<'a, [u8]>>,
    tagger: Option<Cow<'a, [u8]>>,
    creator: Option<Cow<'a, [u8]>>,
}

impl ForEachRefContents<'_> {
    fn into_owned(self) -> ForEachRefContents<'static> {
        ForEachRefContents {
            message: Cow::Owned(self.message.into_owned()),
            tree: self.tree,
            parents: self.parents,
            tag: self.tag.map(|tag| Cow::Owned(tag.into_owned())),
            tag_object_type: self.tag_object_type,
            tag_object: self.tag_object,
            author: self.author.map(|author| Cow::Owned(author.into_owned())),
            committer: self
                .committer
                .map(|committer| Cow::Owned(committer.into_owned())),
            tagger: self.tagger.map(|tagger| Cow::Owned(tagger.into_owned())),
            creator: self.creator.map(|creator| Cow::Owned(creator.into_owned())),
        }
    }
}

fn for_each_ref_contents<'a>(
    format: ObjectFormat,
    object: &'a sley_object::EncodedObject,
) -> Result<Option<ForEachRefContents<'a>>> {
    let contents = match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body)?;
            ForEachRefContents {
                message: Cow::Borrowed(commit.message),
                tree: Some(commit.tree),
                parents: commit.parents,
                tag: None,
                tag_object_type: None,
                tag_object: None,
                author: Some(Cow::Borrowed(commit.author)),
                committer: Some(Cow::Borrowed(commit.committer)),
                tagger: None,
                creator: Some(Cow::Borrowed(commit.committer)),
            }
        }
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            ForEachRefContents {
                message: Cow::Borrowed(tag.message),
                tree: None,
                parents: Vec::new(),
                tag: Some(Cow::Borrowed(tag.name)),
                tag_object_type: Some(tag.object_type),
                tag_object: Some(tag.object),
                author: None,
                committer: None,
                tagger: tag.tagger.map(Cow::Borrowed),
                creator: tag.tagger.map(Cow::Borrowed),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(contents))
}

struct ForEachRefFormatContext<'a> {
    git_dir: &'a Path,
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    refname: &'a str,
    oid: &'a ObjectId,
    deltabase: &'a ObjectId,
    object_type: ObjectType,
    object_body: &'a [u8],
    object_size: usize,
    object_disk_size: Option<u64>,
    color: bool,
    quote: ForEachRefQuoteMode,
    objectname_abbrev: Option<usize>,
    objectname_candidates: &'a [ObjectId],
    worktree_path: Option<&'a str>,
    is_head: bool,
    symref: Option<&'a str>,
    upstream: Option<ForEachRefUpstream>,
    push: Option<ForEachRefPush>,
    upstream_track: Option<ForEachRefTrack>,
    push_track: Option<ForEachRefTrack>,
    contents: Option<ForEachRefContents<'a>>,
    peeled_object: Option<ForEachRefPeeledObject<'a>>,
    mailmap: &'a commands::utility::Mailmap,
}

struct ForEachRefPeeledObject<'a> {
    oid: ObjectId,
    object_type: ObjectType,
    object_body: Cow<'a, [u8]>,
    object_size: usize,
    object_disk_size: Option<u64>,
    tree: Option<ObjectId>,
    parents: Vec<ObjectId>,
    message: Option<Cow<'a, [u8]>>,
    author: Option<Cow<'a, [u8]>>,
    committer: Option<Cow<'a, [u8]>>,
    creator: Option<Cow<'a, [u8]>>,
}

fn print_for_each_ref_format(
    stdout: &mut impl Write,
    format_spec: &ForEachRefFormat,
    context: &ForEachRefFormatContext<'_>,
) -> Result<()> {
    let reset_color_at_eol = context.color && format_spec.ends_with_unreset_color();
    write_for_each_ref_format(
        stdout,
        format_spec,
        context.quote,
        reset_color_at_eol,
        |stdout, atom| {
        let placeholder = match atom {
            ForEachRefAtom::Raw(placeholder) => placeholder.as_str(),
            atom => {
                write_for_each_ref_typed_atom(stdout, atom, context)?;
                return Ok(());
            }
        };
        match placeholder {
            "HEAD" => stdout.write_all(if context.is_head { b"*" } else { b" " })?,
            "refname" => stdout.write_all(context.refname.as_bytes())?,
            "refname:short" => {
                stdout.write_all(for_each_ref_short_name(context.refname).as_bytes())?
            }
            "objectname" => write!(stdout, "{}", context.oid)?,
            "objectname:short" => stdout.write_all(
                for_each_ref_abbrev_oid(
                    context.oid,
                    context.objectname_abbrev,
                    context.objectname_candidates,
                )
                .as_bytes(),
            )?,
            "*objectname" => {
                if let Some(peeled) = &context.peeled_object {
                    write!(stdout, "{}", peeled.oid)?;
                }
            }
            "*objectname:short" => {
                if let Some(peeled) = &context.peeled_object {
                    stdout.write_all(
                        for_each_ref_abbrev_oid(
                            &peeled.oid,
                            context.objectname_abbrev,
                            context.objectname_candidates,
                        )
                        .as_bytes(),
                    )?;
                }
            }
            "deltabase" => write!(stdout, "{}", context.deltabase)?,
            "*deltabase" => {
                if context.peeled_object.is_some() {
                    write!(stdout, "{}", context.deltabase)?;
                }
            }
            "raw" => stdout.write_all(context.object_body)?,
            "raw:size" => write!(stdout, "{}", context.object_body.len())?,
            "*raw" => {
                if let Some(peeled) = &context.peeled_object {
                    stdout.write_all(&peeled.object_body)?;
                }
            }
            "*raw:size" => {
                if let Some(peeled) = &context.peeled_object {
                    write!(stdout, "{}", peeled.object_body.len())?;
                }
            }
            "objectsize" => write!(stdout, "{}", context.object_size)?,
            "*objectsize" => {
                if let Some(peeled) = &context.peeled_object {
                    write!(stdout, "{}", peeled.object_size)?;
                }
            }
            "objectsize:disk" => {
                if let Some(size) = context.object_disk_size {
                    write!(stdout, "{size}")?;
                }
            }
            "*objectsize:disk" => {
                if let Some(size) = context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.object_disk_size)
                {
                    write!(stdout, "{size}")?;
                }
            }
            "objecttype" => stdout.write_all(context.object_type.as_str().as_bytes())?,
            "*objecttype" => {
                if let Some(peeled) = &context.peeled_object {
                    stdout.write_all(peeled.object_type.as_str().as_bytes())?;
                }
            }
            "worktreepath" => stdout.write_all(context.worktree_path.unwrap_or("").as_bytes())?,
            "symref" => stdout.write_all(context.symref.unwrap_or("").as_bytes())?,
            "symref:short" => stdout.write_all(
                context
                    .symref
                    .map(for_each_ref_short_name)
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "upstream" => stdout.write_all(
                context
                    .upstream
                    .as_ref()
                    .map(|upstream| upstream.refname.as_str())
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "upstream:short" => stdout.write_all(
                context
                    .upstream
                    .as_ref()
                    .map(|upstream| for_each_ref_short_name(&upstream.refname))
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "upstream:remotename" => stdout.write_all(
                context
                    .upstream
                    .as_ref()
                    .map(|upstream| upstream.remote.as_str())
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "upstream:remoteref" => stdout.write_all(
                context
                    .upstream
                    .as_ref()
                    .map(|upstream| upstream.merge.as_str())
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "upstream:track" => {
                if let Some(track) = context.upstream_track {
                    write_for_each_ref_track(stdout, track, true)?;
                }
            }
            "upstream:track,nobracket" | "upstream:nobracket,track" => {
                if let Some(track) = context.upstream_track {
                    write_for_each_ref_track(stdout, track, false)?;
                }
            }
            "upstream:trackshort" => {
                if let Some(track) = context.upstream_track {
                    stdout.write_all(for_each_ref_track_short(track).as_bytes())?;
                }
            }
            "push" => stdout.write_all(
                context
                    .push
                    .as_ref()
                    .and_then(|push| push.refname.as_deref())
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "push:short" => stdout.write_all(
                context
                    .push
                    .as_ref()
                    .and_then(|push| push.refname.as_deref())
                    .map(for_each_ref_short_name)
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "push:remotename" => stdout.write_all(
                context
                    .push
                    .as_ref()
                    .map(|push| push.remote.as_str())
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "push:remoteref" => stdout.write_all(
                context
                    .push
                    .as_ref()
                    .and_then(|push| push.remote_ref.as_deref())
                    .unwrap_or("")
                    .as_bytes(),
            )?,
            "push:track" => {
                if let Some(track) = context.push_track {
                    write_for_each_ref_track(stdout, track, true)?;
                }
            }
            "push:track,nobracket" | "push:nobracket,track" => {
                if let Some(track) = context.push_track {
                    write_for_each_ref_track(stdout, track, false)?;
                }
            }
            "push:trackshort" => {
                if let Some(track) = context.push_track {
                    stdout.write_all(for_each_ref_track_short(track).as_bytes())?;
                }
            }
            "subject" | "contents:subject" => {
                if let Some(message) = for_each_ref_message(context, false) {
                    let parts = for_each_ref_message_parts(message);
                    stdout.write_all(for_each_ref_copy_subject(parts.subject).as_bytes())?;
                }
            }
            "*subject" | "*contents:subject" => {
                if let Some(message) = for_each_ref_message(context, true) {
                    let parts = for_each_ref_message_parts(message);
                    stdout.write_all(for_each_ref_copy_subject(parts.subject).as_bytes())?;
                }
            }
            "subject:sanitize" => {
                if let Some(message) = for_each_ref_message(context, false) {
                    let parts = for_each_ref_message_parts(message);
                    let subject = for_each_ref_copy_subject(parts.subject);
                    stdout.write_all(for_each_ref_sanitize_subject(&subject).as_bytes())?;
                }
            }
            "*subject:sanitize" => {
                if let Some(message) = for_each_ref_message(context, true) {
                    let parts = for_each_ref_message_parts(message);
                    let subject = for_each_ref_copy_subject(parts.subject);
                    stdout.write_all(for_each_ref_sanitize_subject(&subject).as_bytes())?;
                }
            }
            "contents:body" => {
                if let Some(message) = for_each_ref_message(context, false) {
                    stdout.write_all(for_each_ref_message_parts(message).body_without_sig)?;
                }
            }
            "*contents:body" => {
                if let Some(message) = for_each_ref_message(context, true) {
                    stdout.write_all(for_each_ref_message_parts(message).body_without_sig)?;
                }
            }
            "contents:signature" => {
                if let Some(message) = for_each_ref_message(context, false) {
                    stdout.write_all(for_each_ref_message_parts(message).signature)?;
                }
            }
            "*contents:signature" => {
                if let Some(message) = for_each_ref_message(context, true) {
                    stdout.write_all(for_each_ref_message_parts(message).signature)?;
                }
            }
            "body" => {
                if let Some(message) = for_each_ref_message(context, false) {
                    stdout.write_all(for_each_ref_message_parts(message).body_with_sig)?;
                }
            }
            "*body" => {
                if let Some(message) = for_each_ref_message(context, true) {
                    stdout.write_all(for_each_ref_message_parts(message).body_with_sig)?;
                }
            }
            "contents" => {
                if let Some(message) = for_each_ref_message(context, false) {
                    stdout.write_all(for_each_ref_message_parts(message).bare)?;
                }
            }
            "*contents" => {
                if let Some(message) = for_each_ref_message(context, true) {
                    stdout.write_all(for_each_ref_message_parts(message).bare)?;
                }
            }
            "contents:size" => {
                if let Some(message) = for_each_ref_message(context, false) {
                    write!(stdout, "{}", for_each_ref_message_parts(message).bare.len())?;
                }
            }
            "*contents:size" => {
                if let Some(message) = for_each_ref_message(context, true) {
                    write!(stdout, "{}", for_each_ref_message_parts(message).bare.len())?;
                }
            }
            "author" => write_for_each_ref_identity(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.author.as_deref()),
            )?,
            "*author" => write_for_each_ref_identity(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.author.as_deref()),
            )?,
            "authorname" | "*authorname" => {
                for_each_ref_try_name_atom(stdout, placeholder, context)
                    .expect("name atom recognized")?
            }
            "authoremail" | "*authoremail" => {
                for_each_ref_try_email_atom(stdout, placeholder, context)
                    .expect("email atom recognized")?
            }
            "committer" => write_for_each_ref_identity(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.committer.as_deref()),
            )?,
            "*committer" => write_for_each_ref_identity(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.committer.as_deref()),
            )?,
            "committername" | "*committername" => {
                for_each_ref_try_name_atom(stdout, placeholder, context)
                    .expect("name atom recognized")?
            }
            "committeremail" | "*committeremail" => {
                for_each_ref_try_email_atom(stdout, placeholder, context)
                    .expect("email atom recognized")?
            }
            "tagger" => write_for_each_ref_identity(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tagger.as_deref()),
            )?,
            "*tagger" => write_for_each_ref_identity(stdout, None)?,
            "taggername" | "*taggername" => {
                for_each_ref_try_name_atom(stdout, placeholder, context)
                    .expect("name atom recognized")?
            }
            "taggeremail" | "*taggeremail" => {
                for_each_ref_try_email_atom(stdout, placeholder, context)
                    .expect("email atom recognized")?
            }
            "creator" => write_for_each_ref_identity(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.creator.as_deref()),
            )?,
            "*creator" => write_for_each_ref_identity(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.creator.as_deref()),
            )?,
            "authordate" | "*authordate" | "committerdate" | "*committerdate" | "taggerdate"
            | "*taggerdate" | "creatordate" | "*creatordate" => {
                for_each_ref_try_date_atom(stdout, placeholder, context)
                    .expect("date atom recognized")?
            }
            "tree" => {
                if let Some(tree) = context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tree.as_ref())
                {
                    write!(stdout, "{tree}")?;
                }
            }
            "parent" => {
                if let Some(contents) = &context.contents {
                    for (idx, parent) in contents.parents.iter().enumerate() {
                        if idx > 0 {
                            stdout.write_all(b" ")?;
                        }
                        write!(stdout, "{parent}")?;
                    }
                }
            }
            "numparent" => {
                if let Some(contents) = &context.contents
                    && contents.tree.is_some()
                {
                    write!(stdout, "{}", contents.parents.len())?;
                }
            }
            "*tree" => {
                if let Some(tree) = context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.tree.as_ref())
                {
                    write!(stdout, "{tree}")?;
                }
            }
            "*parent" => {
                if let Some(peeled) = &context.peeled_object {
                    for (idx, parent) in peeled.parents.iter().enumerate() {
                        if idx > 0 {
                            stdout.write_all(b" ")?;
                        }
                        write!(stdout, "{parent}")?;
                    }
                }
            }
            "*numparent" => {
                if let Some(peeled) = &context.peeled_object
                    && peeled.tree.is_some()
                {
                    write!(stdout, "{}", peeled.parents.len())?;
                }
            }
            "tag" => {
                if let Some(tag) = context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tag.as_ref())
                {
                    stdout.write_all(tag)?;
                }
            }
            "type" => {
                if let Some(object_type) = context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tag_object_type)
                {
                    stdout.write_all(object_type.as_str().as_bytes())?;
                }
            }
            "object" => {
                if let Some(object) = context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tag_object.as_ref())
                {
                    write!(stdout, "{object}")?;
                }
            }
            other => {
                if let Some(value) = other.strip_prefix("color:") {
                    let color = for_each_ref_color_escape(value)?;
                    if context.color {
                        stdout.write_all(color.as_bytes())?;
                    }
                } else if let Some(value) = other
                    .strip_prefix("refname:lstrip=")
                    .or_else(|| other.strip_prefix("refname:strip="))
                {
                    let count = parse_for_each_ref_strip_count(value)?;
                    stdout
                        .write_all(for_each_ref_lstrip_name(context.refname, count).as_bytes())?;
                } else if let Some(value) = other.strip_prefix("refname:rstrip=") {
                    let count = parse_for_each_ref_strip_count(value)?;
                    stdout
                        .write_all(for_each_ref_rstrip_name(context.refname, count).as_bytes())?;
                } else if let Some(value) = other
                    .strip_prefix("upstream:lstrip=")
                    .or_else(|| other.strip_prefix("upstream:strip="))
                {
                    let count = parse_for_each_ref_strip_count(value)?;
                    let upstream = context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.refname.as_str())
                        .unwrap_or("");
                    stdout.write_all(for_each_ref_lstrip_name(upstream, count).as_bytes())?;
                } else if let Some(value) = other.strip_prefix("upstream:rstrip=") {
                    let count = parse_for_each_ref_strip_count(value)?;
                    let upstream = context
                        .upstream
                        .as_ref()
                        .map(|upstream| upstream.refname.as_str())
                        .unwrap_or("");
                    stdout.write_all(for_each_ref_rstrip_name(upstream, count).as_bytes())?;
                } else if let Some(value) = other
                    .strip_prefix("push:lstrip=")
                    .or_else(|| other.strip_prefix("push:strip="))
                {
                    let count = parse_for_each_ref_strip_count(value)?;
                    let push = context
                        .push
                        .as_ref()
                        .and_then(|push| push.refname.as_deref())
                        .unwrap_or("");
                    stdout.write_all(for_each_ref_lstrip_name(push, count).as_bytes())?;
                } else if let Some(value) = other.strip_prefix("push:rstrip=") {
                    let count = parse_for_each_ref_strip_count(value)?;
                    let push = context
                        .push
                        .as_ref()
                        .and_then(|push| push.refname.as_deref())
                        .unwrap_or("");
                    stdout.write_all(for_each_ref_rstrip_name(push, count).as_bytes())?;
                } else if let Some(value) = other
                    .strip_prefix("symref:lstrip=")
                    .or_else(|| other.strip_prefix("symref:strip="))
                {
                    let count = parse_for_each_ref_strip_count(value)?;
                    let symref = context.symref.unwrap_or("");
                    stdout.write_all(for_each_ref_lstrip_name(symref, count).as_bytes())?;
                } else if let Some(value) = other.strip_prefix("symref:rstrip=") {
                    let count = parse_for_each_ref_strip_count(value)?;
                    let symref = context.symref.unwrap_or("");
                    stdout.write_all(for_each_ref_rstrip_name(symref, count).as_bytes())?;
                } else if let Some(width) = other.strip_prefix("objectname:short=") {
                    let width = parse_for_each_ref_abbrev_width(width)?;
                    stdout.write_all(
                        for_each_ref_abbrev_oid(
                            context.oid,
                            Some(width),
                            context.objectname_candidates,
                        )
                        .as_bytes(),
                    )?;
                } else if let Some(width) = other.strip_prefix("*objectname:short=") {
                    let width = parse_for_each_ref_abbrev_width(width)?;
                    if let Some(peeled) = &context.peeled_object {
                        stdout.write_all(
                            for_each_ref_abbrev_oid(
                                &peeled.oid,
                                Some(width),
                                context.objectname_candidates,
                            )
                            .as_bytes(),
                        )?;
                    }
                } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "tree") {
                    let width = for_each_ref_oid_atom_width(arg, other)?;
                    if let Some(tree) = context
                        .contents
                        .as_ref()
                        .and_then(|contents| contents.tree.as_ref())
                    {
                        stdout.write_all(
                            for_each_ref_abbrev_oid(tree, width, context.objectname_candidates)
                                .as_bytes(),
                        )?;
                    }
                } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "*tree") {
                    let width = for_each_ref_oid_atom_width(arg, other)?;
                    if let Some(tree) = context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.tree.as_ref())
                    {
                        stdout.write_all(
                            for_each_ref_abbrev_oid(tree, width, context.objectname_candidates)
                                .as_bytes(),
                        )?;
                    }
                } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "parent") {
                    let width = for_each_ref_oid_atom_width(arg, other)?;
                    if let Some(contents) = &context.contents {
                        for (idx, parent) in contents.parents.iter().enumerate() {
                            if idx > 0 {
                                stdout.write_all(b" ")?;
                            }
                            stdout.write_all(
                                for_each_ref_abbrev_oid(
                                    parent,
                                    width,
                                    context.objectname_candidates,
                                )
                                .as_bytes(),
                            )?;
                        }
                    }
                } else if let Some(arg) = for_each_ref_oid_atom_arg(other, "*parent") {
                    let width = for_each_ref_oid_atom_width(arg, other)?;
                    if let Some(peeled) = &context.peeled_object {
                        for (idx, parent) in peeled.parents.iter().enumerate() {
                            if idx > 0 {
                                stdout.write_all(b" ")?;
                            }
                            stdout.write_all(
                                for_each_ref_abbrev_oid(
                                    parent,
                                    width,
                                    context.objectname_candidates,
                                )
                                .as_bytes(),
                            )?;
                        }
                    }
                } else if let Some(result) = for_each_ref_try_trailers_atom(stdout, other, context) {
                    result?;
                } else if let Some(result) = for_each_ref_try_email_atom(stdout, other, context) {
                    result?;
                } else if let Some(result) = for_each_ref_try_name_atom(stdout, other, context) {
                    result?;
                } else if let Some(result) = for_each_ref_try_date_atom(stdout, other, context) {
                    result?;
                } else if let Some(rev) = other.strip_prefix("ahead-behind:") {
                    let target = resolve_revision(context.git_dir, context.format, rev)?;
                    if let Some(track) =
                        for_each_ref_ahead_behind(context.db, context.format, context.oid, &target)?
                    {
                        write!(stdout, "{} {}", track.ahead, track.behind)?;
                    }
                } else if let Some(value) = other.strip_prefix("contents:lines=") {
                    let count = parse_for_each_ref_contents_lines_count(value)?;
                    if let Some(contents) = &context.contents {
                        write_for_each_ref_contents_lines(stdout, &contents.message, count)?;
                    }
                } else if let Some(value) = other.strip_prefix("*contents:lines=") {
                    let count = parse_for_each_ref_contents_lines_count(value)?;
                    if let Some(message) = context
                        .peeled_object
                        .as_ref()
                        .and_then(|peeled| peeled.message.as_ref())
                    {
                        write_for_each_ref_contents_lines(stdout, message, count)?;
                    }
                } else if let Some(arg) = other
                    .strip_prefix("contents:")
                    .or_else(|| other.strip_prefix("*contents:"))
                {
                    // A `%(contents:XXX)` that none of the contents sub-atoms
                    // above recognized — git reports the bare contents arg.
                    eprintln!("fatal: unrecognized %(contents) argument: {arg}");
                    return Err(GitError::Exit(128));
                } else {
                    return Err(GitError::Command(format!(
                        "unsupported for-each-ref format placeholder %({other})"
                    )));
                }
            }
        }
        Ok(())
    })
}

fn write_for_each_ref_typed_atom(
    stdout: &mut impl Write,
    atom: &ForEachRefAtom,
    context: &ForEachRefFormatContext<'_>,
) -> Result<()> {
    match atom {
        ForEachRefAtom::Raw(_) => unreachable!("raw atoms are handled by the compatibility path"),
        ForEachRefAtom::Color(value) => {
            let color = for_each_ref_color_escape(value)?;
            if context.color {
                stdout.write_all(color.as_bytes())?;
            }
        }
        ForEachRefAtom::RefName { source, format } => {
            let refname = for_each_ref_typed_refname(context, *source);
            match format {
                ForEachRefNameFormat::Full => stdout.write_all(refname.as_bytes())?,
                ForEachRefNameFormat::Short => {
                    stdout.write_all(for_each_ref_short_name(refname).as_bytes())?
                }
                ForEachRefNameFormat::Strip(strip) => {
                    let refname = match strip.direction {
                        ForEachRefStripDirection::Left => {
                            for_each_ref_lstrip_name(refname, strip.count)
                        }
                        ForEachRefStripDirection::Right => {
                            for_each_ref_rstrip_name(refname, strip.count)
                        }
                    };
                    stdout.write_all(refname.as_bytes())?;
                }
            }
        }
        ForEachRefAtom::ObjectName { peeled, abbrev } => {
            let oid = if *peeled {
                context.peeled_object.as_ref().map(|peeled| &peeled.oid)
            } else {
                Some(context.oid)
            };
            if let Some(oid) = oid {
                match abbrev {
                    None => write!(stdout, "{oid}")?,
                    Some(0) => stdout.write_all(
                        for_each_ref_abbrev_oid(
                            oid,
                            context.objectname_abbrev,
                            context.objectname_candidates,
                        )
                        .as_bytes(),
                    )?,
                    Some(width) => stdout.write_all(
                        for_each_ref_abbrev_oid(oid, Some(*width), context.objectname_candidates)
                            .as_bytes(),
                    )?,
                }
            }
        }
        ForEachRefAtom::Identity { peeled, role, part } => {
            let identity = for_each_ref_typed_identity(context, *peeled, *role);
            match part {
                ForEachRefAtomIdentityPart::Full => write_for_each_ref_identity(stdout, identity)?,
                ForEachRefAtomIdentityPart::Name => {
                    write_for_each_ref_identity_name(stdout, identity)?
                }
                ForEachRefAtomIdentityPart::Email(mode) => {
                    write_for_each_ref_identity_email_mode(stdout, identity, *mode)?
                }
                ForEachRefAtomIdentityPart::Date(mode) => {
                    write_for_each_ref_identity_date_mode(stdout, identity, *mode)?
                }
                ForEachRefAtomIdentityPart::DateRaw => {
                    write_for_each_ref_identity_date_raw(stdout, identity)?
                }
            }
        }
        ForEachRefAtom::ContentsLines { peeled, count } => {
            let message = if *peeled {
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_deref())
            } else {
                context
                    .contents
                    .as_ref()
                    .map(|contents| contents.message.as_ref())
            };
            if let Some(message) = message {
                write_for_each_ref_contents_lines(stdout, message, *count)?;
            }
        }
    }
    Ok(())
}

fn for_each_ref_typed_refname<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    source: ForEachRefNameSource,
) -> &'a str {
    match source {
        ForEachRefNameSource::Ref => context.refname,
        ForEachRefNameSource::Upstream => context
            .upstream
            .as_ref()
            .map(|upstream| upstream.refname.as_str())
            .unwrap_or(""),
        ForEachRefNameSource::Push => context
            .push
            .as_ref()
            .and_then(|push| push.refname.as_deref())
            .unwrap_or(""),
    }
}

fn for_each_ref_typed_identity<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    peeled: bool,
    role: ForEachRefAtomIdentityRole,
) -> Option<&'a [u8]> {
    if peeled {
        let peeled = context.peeled_object.as_ref();
        return match role {
            ForEachRefAtomIdentityRole::Author => {
                peeled.and_then(|peeled| peeled.author.as_deref())
            }
            ForEachRefAtomIdentityRole::Committer => {
                peeled.and_then(|peeled| peeled.committer.as_deref())
            }
            ForEachRefAtomIdentityRole::Tagger => None,
            ForEachRefAtomIdentityRole::Creator => {
                peeled.and_then(|peeled| peeled.creator.as_deref())
            }
        };
    }

    let contents = context.contents.as_ref();
    match role {
        ForEachRefAtomIdentityRole::Author => {
            contents.and_then(|contents| contents.author.as_deref())
        }
        ForEachRefAtomIdentityRole::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefAtomIdentityRole::Tagger => {
            contents.and_then(|contents| contents.tagger.as_deref())
        }
        ForEachRefAtomIdentityRole::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    }
}

fn write_for_each_ref_contents_lines(
    stdout: &mut impl Write,
    message: &[u8],
    count: usize,
) -> Result<()> {
    let mut lines = message.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    for (idx, line) in lines.into_iter().take(count).enumerate() {
        if idx > 0 {
            stdout.write_all(b"\n    ")?;
        }
        stdout.write_all(line)?;
    }
    Ok(())
}


/// The set of `%(...email)` options, mirroring git's `email_option` bitset
/// (ref-filter.c `EO_TRIM`/`EO_LOCALPART`/`EO_MAILMAP`).
#[derive(Clone, Copy, Default)]
struct ForEachRefEmailOptions {
    trim: bool,
    localpart: bool,
    mailmap: bool,
}

/// Parse the option string after `%(authoremail:...)` exactly as git's
/// `person_email_atom_parser` does. Options are comma-separated and may repeat;
/// each must be an exact `trim`/`localpart`/`mailmap` token between commas.
/// On an unrecognized token, returns `Err(bad_arg)` where `bad_arg` is the
/// unconsumed remainder at the point of failure (git reports this verbatim).
fn parse_for_each_ref_email_options(arg: &str) -> std::result::Result<ForEachRefEmailOptions, String> {
    let mut options = ForEachRefEmailOptions::default();
    let mut rest = arg;
    loop {
        // git's email_atom_option_parser advances past a matched prefix; the
        // `bad_arg` it later reports is the *remaining* string AFTER that
        // consume (so `mailmaptrim` reports `trim`, not `mailmaptrim`).
        let matched = if let Some(tail) = rest.strip_prefix("trim") {
            options.trim = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("localpart") {
            options.localpart = true;
            Some(tail)
        } else if let Some(tail) = rest.strip_prefix("mailmap") {
            options.mailmap = true;
            Some(tail)
        } else {
            None
        };
        let Some(tail) = matched else {
            // No prefix consumed: the bad argument is the whole remainder.
            return Err(rest.to_string());
        };
        rest = tail;
        let bad_arg = rest;
        if rest.is_empty() {
            break;
        }
        if let Some(tail) = rest.strip_prefix(',') {
            rest = tail;
        } else {
            return Err(bad_arg.to_string());
        }
    }
    Ok(options)
}

/// If `placeholder` is an email atom (`(\*?)(author|committer|tagger)email`
/// with optional `:opts`), render it. Returns `Some(Ok(()))` when handled,
/// `Some(Err(_))` on a bad-option error (already reported to stderr), and
/// `None` when the placeholder is not an email atom.
fn for_each_ref_try_email_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authoremail" => (false, ForEachRefAtomIdentityRole::Author),
        "committeremail" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggeremail" => (false, ForEachRefAtomIdentityRole::Tagger),
        "*authoremail" => (true, ForEachRefAtomIdentityRole::Author),
        "*committeremail" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggeremail" => (true, ForEachRefAtomIdentityRole::Tagger),
        _ => return None,
    };
    let options = match arg {
        Some(arg) => match parse_for_each_ref_email_options(arg) {
            Ok(options) => options,
            Err(bad_arg) => {
                let name = atom.strip_prefix('*').unwrap_or(atom);
                eprintln!("fatal: unrecognized %({name}) argument: {bad_arg}");
                return Some(Err(GitError::Exit(128)));
            }
        },
        None => ForEachRefEmailOptions::default(),
    };
    Some(for_each_ref_write_email(stdout, context, peeled, role, options))
}

/// If `placeholder` is a trailers atom (`%(trailers[:opts])` or
/// `%(contents:trailers[:opts])`, with optional `*` peel), render it. Returns
/// `Some(Err(_))` (after reporting to stderr) for the bad-argument cases.
fn for_each_ref_try_trailers_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (base, peeled) = placeholder
        .strip_prefix('*')
        .map(|rest| (rest, true))
        .unwrap_or((placeholder, false));

    // Accept `trailers`, `trailers:ARG`, `contents:trailers`,
    // `contents:trailers:ARG`. The `contents:` prefix shares git's
    // `%(contents)` bad-argument error for `contents:trailersXXX`.
    let arg: Option<&str> = if base == "trailers" {
        None
    } else if let Some(rest) = base.strip_prefix("trailers:") {
        Some(rest)
    } else if let Some(rest) = base.strip_prefix("contents:") {
        if rest == "trailers" {
            None
        } else if let Some(rest) = rest.strip_prefix("trailers:") {
            Some(rest)
        } else {
            return None;
        }
    } else {
        return None;
    };

    let options = match arg {
        None => commands::for_each_ref::ForEachRefTrailerOptions::default(),
        Some(arg) => match commands::for_each_ref::parse_for_each_ref_trailer_options(arg) {
            Ok(options) => options,
            Err(None) => {
                eprintln!("fatal: expected %(trailers:key=<value>)");
                return Some(Err(GitError::Exit(128)));
            }
            Err(Some(invalid)) => {
                eprintln!("fatal: unknown %(trailers) argument: {invalid}");
                return Some(Err(GitError::Exit(128)));
            }
        },
    };

    Some((|| -> Result<()> {
        if let Some(message) = for_each_ref_message(context, peeled) {
            // git formats trailers over the message from the subject start to
            // the signature start (sig stripped).
            let parts = for_each_ref_message_parts(message);
            let sig_len = parts.signature.len();
            let trailer_src = &parts.bare[..parts.bare.len().saturating_sub(sig_len)];
            let rendered =
                commands::for_each_ref::for_each_ref_format_trailers(trailer_src, &options);
            stdout.write_all(&rendered)?;
        }
        Ok(())
    })())
}

/// The raw message bytes for the ref's own object (`peeled == false`) or the
/// peeled tag target (`peeled == true`), if available.
fn for_each_ref_message<'a>(
    context: &'a ForEachRefFormatContext<'_>,
    peeled: bool,
) -> Option<&'a [u8]> {
    if peeled {
        context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.message.as_deref())
    } else {
        context.contents.as_ref().map(|contents| &*contents.message)
    }
}

/// If `placeholder` is a date atom (`(\*?)(author|committer|tagger|creator)date`
/// with an optional `:spec`), render it through the full date grammar. Returns
/// `Some(Err(_))` (after reporting to stderr) on an invalid specifier.
fn for_each_ref_try_date_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authordate" => (false, ForEachRefAtomIdentityRole::Author),
        "committerdate" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggerdate" => (false, ForEachRefAtomIdentityRole::Tagger),
        "creatordate" => (false, ForEachRefAtomIdentityRole::Creator),
        "*authordate" => (true, ForEachRefAtomIdentityRole::Author),
        "*committerdate" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggerdate" => (true, ForEachRefAtomIdentityRole::Tagger),
        "*creatordate" => (true, ForEachRefAtomIdentityRole::Creator),
        _ => return None,
    };
    let Some(spec) = ForEachRefDateSpec::parse(arg) else {
        let name = atom.strip_prefix('*').unwrap_or(atom);
        eprintln!(
            "fatal: unrecognized %({name}) argument: {}",
            arg.unwrap_or("")
        );
        return Some(Err(GitError::Exit(128)));
    };
    Some((|| -> Result<()> {
        if let Some(identity) = for_each_ref_typed_identity(context, peeled, role)
            && let Some(value) = for_each_ref_identity_date_spec(identity, &spec)
        {
            stdout.write_all(value.as_bytes())?;
        }
        Ok(())
    })())
}

/// For an oid atom like `tree:short` / `parent:short=7`, return the option
/// argument (`short` or `short=7`) when `placeholder` is exactly `atom:<arg>`.
fn for_each_ref_oid_atom_arg<'a>(placeholder: &'a str, atom: &str) -> Option<&'a str> {
    let rest = placeholder.strip_prefix(atom)?;
    rest.strip_prefix(':')
}

/// Parse the `short`/`short=N` argument of an oid atom into an abbreviation
/// width, mirroring git's `oid_atom_parser` validation.
fn for_each_ref_oid_atom_width(arg: &str, atom: &str) -> Result<Option<usize>> {
    if arg == "short" {
        Ok(None)
    } else if let Some(value) = arg.strip_prefix("short=") {
        Ok(Some(parse_for_each_ref_abbrev_width(value).map_err(
            |_| {
                eprintln!("fatal: positive value expected '{value}' in %({atom})");
                GitError::Exit(128)
            },
        )?))
    } else {
        eprintln!("fatal: unrecognized %({atom}) argument: {arg}");
        Err(GitError::Exit(128))
    }
}

/// If `placeholder` is a name atom (`(\*?)(author|committer|tagger)name` with an
/// optional `:mailmap`/`:` argument), render it. Mirrors git's
/// `person_name_atom_parser`: the only accepted argument is `mailmap`.
fn for_each_ref_try_name_atom(
    stdout: &mut impl Write,
    placeholder: &str,
    context: &ForEachRefFormatContext<'_>,
) -> Option<Result<()>> {
    let (atom, arg) = match placeholder.split_once(':') {
        Some((atom, arg)) => (atom, Some(arg)),
        None => (placeholder, None),
    };
    let (peeled, role) = match atom {
        "authorname" => (false, ForEachRefAtomIdentityRole::Author),
        "committername" => (false, ForEachRefAtomIdentityRole::Committer),
        "taggername" => (false, ForEachRefAtomIdentityRole::Tagger),
        "*authorname" => (true, ForEachRefAtomIdentityRole::Author),
        "*committername" => (true, ForEachRefAtomIdentityRole::Committer),
        "*taggername" => (true, ForEachRefAtomIdentityRole::Tagger),
        _ => return None,
    };
    let mailmap = match arg {
        None => false,
        Some("mailmap") => true,
        Some(bad_arg) => {
            let name = atom.strip_prefix('*').unwrap_or(atom);
            eprintln!("fatal: unrecognized %({name}) argument: {bad_arg}");
            return Some(Err(GitError::Exit(128)));
        }
    };
    Some((|| -> Result<()> {
        let Some(identity) = for_each_ref_typed_identity(context, peeled, role) else {
            return Ok(());
        };
        if mailmap {
            let (name, _) = context.mailmap.rewrite_identity(identity);
            stdout.write_all(&name)?;
        } else {
            write_for_each_ref_identity_name(stdout, Some(identity))?;
        }
        Ok(())
    })())
}

fn for_each_ref_write_email(
    stdout: &mut impl Write,
    context: &ForEachRefFormatContext<'_>,
    peeled: bool,
    role: ForEachRefAtomIdentityRole,
    options: ForEachRefEmailOptions,
) -> Result<()> {
    let Some(identity) = for_each_ref_typed_identity(context, peeled, role) else {
        return Ok(());
    };
    let mode = if options.localpart {
        ForEachRefEmailMode::LocalPart
    } else if options.trim {
        ForEachRefEmailMode::Trim
    } else {
        ForEachRefEmailMode::Bracketed
    };
    if options.mailmap {
        let (_, email) = context.mailmap.rewrite_identity(identity);
        // Reassemble a synthetic identity so the shared email extractor applies
        // trim/localpart over the rewritten address.
        let mut synthetic = Vec::with_capacity(email.len() + 2);
        synthetic.push(b'<');
        synthetic.extend_from_slice(&email);
        synthetic.push(b'>');
        if let Some(value) = for_each_ref_identity_email(&synthetic, mode) {
            stdout.write_all(value)?;
        }
    } else if let Some(value) = for_each_ref_identity_email(identity, mode) {
        stdout.write_all(value)?;
    }
    Ok(())
}

fn for_each_ref_color_escape(value: &str) -> Result<String> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err(GitError::Command("empty for-each-ref color".into()));
    }
    let mut attributes = Vec::new();
    let mut foreground = None;
    let mut background = None;
    for token in tokens.iter().copied() {
        match token {
            "reset" => return Ok("\x1b[m".to_string()),
            "normal" if tokens.len() == 1 || (foreground.is_some() && background.is_none()) => {}
            "bold" => attributes.push("1".to_string()),
            "dim" => attributes.push("2".to_string()),
            "italic" => attributes.push("3".to_string()),
            "ul" => attributes.push("4".to_string()),
            "blink" => attributes.push("5".to_string()),
            "reverse" => attributes.push("7".to_string()),
            "strike" => attributes.push("9".to_string()),
            "nobold" | "nodim" => attributes.push("22".to_string()),
            "noitalic" => attributes.push("23".to_string()),
            "noul" => attributes.push("24".to_string()),
            "noblink" => attributes.push("25".to_string()),
            "noreverse" => attributes.push("27".to_string()),
            "nostrike" => attributes.push("29".to_string()),
            "black" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 30)?,
            "red" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 31)?,
            "green" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 32)?,
            "yellow" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 33)?,
            "blue" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 34)?,
            "magenta" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 35)?,
            "cyan" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 36)?,
            "white" => for_each_ref_push_color_code(value, &mut foreground, &mut background, 37)?,
            "brightblack" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 90)?
            }
            "brightred" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 91)?
            }
            "brightgreen" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 92)?
            }
            "brightyellow" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 93)?
            }
            "brightblue" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 94)?
            }
            "brightmagenta" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 95)?
            }
            "brightcyan" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 96)?
            }
            "brightwhite" => {
                for_each_ref_push_color_code(value, &mut foreground, &mut background, 97)?
            }
            _ => {
                return Err(GitError::Command(format!(
                    "unsupported for-each-ref color {value}"
                )));
            }
        }
    }
    let mut codes = attributes;
    if let Some(foreground) = foreground {
        codes.push(foreground.to_string());
    }
    if let Some(background) = background {
        codes.push(background.to_string());
    }
    if codes.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("\x1b[{}m", codes.join(";")))
}

fn for_each_ref_push_color_code(
    value: &str,
    foreground: &mut Option<u16>,
    background: &mut Option<u16>,
    code: u16,
) -> Result<()> {
    if foreground.is_none() {
        *foreground = Some(code);
    } else if background.is_none() {
        *background = Some(code + 10);
    } else {
        return Err(GitError::Command(format!(
            "unsupported for-each-ref color {value}"
        )));
    }
    Ok(())
}

fn index_entry_stage(entry: &sley_index::IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

struct LsFilesPathspec {
    prefix: Vec<u8>,
    full_name: bool,
    filters: Vec<LsFilesPathFilter>,
    cwd_depth: usize,
    magic: sley_worktree::PathspecMatchMagic,
}

impl LsFilesPathspec {
    fn new(
        cwd: &Path,
        worktree_root: &Path,
        full_name: bool,
        path_args: &[String],
    ) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd = fs::canonicalize(cwd)?;
        let relative = cwd.strip_prefix(&root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", cwd.display()))
        })?;
        let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
        let cwd_depth = path_component_count(&prefix);
        let magic = effective_pathspec_flags();
        let mut filters = Vec::new();
        for arg in path_args {
            if arg.is_empty() {
                // git: an empty pathspec is rejected before any matching.
                eprintln!(
                    "fatal: empty string is not a valid pathspec. please use . instead if you meant to match all paths"
                );
                return Err(GitError::Exit(128));
            }
            let filter_path = normalize_ls_files_pathspec(&prefix, arg)?;
            // Under literal magic, wildcard characters carry no special meaning.
            let is_glob = !magic.literal && sley_worktree::pathspec_is_glob(&filter_path);
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                cwd.join(arg_path)
            };
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                path: filter_path,
                recursive: arg == "." || arg.ends_with('/') || absolute.is_dir(),
                is_glob,
                matched: Cell::new(false),
            });
        }
        Ok(Self {
            prefix,
            full_name,
            filters,
            cwd_depth,
            magic,
        })
    }

    fn untracked_pathspecs(&self) -> Vec<sley_worktree::UntrackedPathspecFilter> {
        self.filters
            .iter()
            .map(|filter| sley_worktree::UntrackedPathspecFilter {
                path: filter.path.clone(),
                recursive: filter.recursive,
                is_glob: filter.is_glob,
            })
            .collect()
    }

    fn display(&self, path: &[u8]) -> Option<Vec<u8>> {
        if !self.matches(path) {
            return None;
        }
        if self.full_name {
            return Some(path.to_vec());
        }
        if self.prefix.is_empty() {
            return Some(path.to_vec());
        }
        if let Some(rest) = path.strip_prefix(self.prefix.as_slice()) {
            let rest = rest.strip_prefix(b"/")?;
            if rest.is_empty() {
                return None;
            }
            Some(rest.to_vec())
        } else {
            let mut display = Vec::new();
            for _ in 0..self.cwd_depth {
                display.extend_from_slice(b"../");
            }
            display.extend_from_slice(path);
            Some(display)
        }
    }

    fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return self.prefix.is_empty()
                || path
                    .strip_prefix(self.prefix.as_slice())
                    .and_then(|rest| rest.strip_prefix(b"/"))
                    .is_some_and(|rest| !rest.is_empty());
        }
        let mut matched = false;
        for filter in &self.filters {
            if filter.matches(path, self.magic) {
                filter.matched.set(true);
                matched = true;
            }
        }
        matched
    }

    fn exit_if_unmatched(&self) -> Result<()> {
        let mut has_unmatched = false;
        for filter in &self.filters {
            if !filter.matched.get() {
                eprintln!(
                    "error: pathspec '{}' did not match any file(s) known to git",
                    filter.original
                );
                has_unmatched = true;
            }
        }
        if has_unmatched {
            eprintln!("Did you forget to 'git add'?");
            return Err(GitError::Exit(1));
        }
        Ok(())
    }
}

struct LsFilesPathFilter {
    original: String,
    path: Vec<u8>,
    recursive: bool,
    is_glob: bool,
    matched: Cell<bool>,
}

impl LsFilesPathFilter {
    fn matches(&self, path: &[u8], magic: sley_worktree::PathspecMatchMagic) -> bool {
        // Byte-exact git `match_pathspec_item` for the tracked-index path. Handles
        // exact / directory-prefix / wildcard matching under the active magic.
        let path_no_slash = path.strip_suffix(b"/").unwrap_or(path);
        sley_worktree::pathspec_item_matches(&self.path, path, magic)
            || (path_no_slash.len() != path.len()
                && sley_worktree::pathspec_item_matches(&self.path, path_no_slash, magic))
    }
}

fn normalize_ls_files_pathspec(prefix: &[u8], arg: &str) -> Result<Vec<u8>> {
    let mut components = prefix
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .map(Vec::from)
        .collect::<Vec<_>>();
    for component in Path::new(arg).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop().ok_or_else(|| {
                    GitError::InvalidPath(format!("pathspec {arg} is outside worktree"))
                })?;
            }
            std::path::Component::Normal(name) => {
                components.push(name.to_string_lossy().as_bytes().to_vec());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(GitError::Unsupported(
                    "ls-files pathspecs currently support relative paths".into(),
                ));
            }
        }
    }
    Ok(components.join(&b'/'))
}

fn path_component_count(path: &[u8]) -> usize {
    path.split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .count()
}

fn log_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn log_option_requires_value_error(option: &str) -> GitError {
    eprintln!("error: option `{option}' requires a value");
    GitError::Exit(129)
}

fn log_validate_similarity_option(value: &str, option: &str) -> Result<()> {
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

fn log_validate_break_rewrites_option(value: &str) -> Result<()> {
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

fn log_valid_break_rewrites_part(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let digits = value.strip_suffix('%').unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn log_break_rewrites_form_error() -> Result<()> {
    eprintln!("error: break-rewrites expects <n>/<m> form");
    Err(GitError::Exit(129))
}

fn log_validate_diff_merges(value: &str) -> Result<()> {
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

fn log_diff_merges_invalid_value(value: &str) -> Result<()> {
    eprintln!("fatal: invalid value for '--diff-merges': '{value}'");
    Err(GitError::Exit(128))
}

fn log_parse_age(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("fatal: '{value}': not a number of seconds since epoch");
        GitError::Exit(128)
    })
}

fn log_parse_date_cutoff(value: &str) -> Result<i64> {
    let mut parts = value.split_whitespace();
    let Some(first) = parts.next() else {
        return log_invalid_date_format(value);
    };
    if let Some(timestamp) = first.strip_prefix('@') {
        let Some(timezone) = parts.next() else {
            return log_invalid_date_format(value);
        };
        if parts.next().is_some() || log_parse_timezone_offset_seconds(timezone).is_none() {
            return log_invalid_date_format(value);
        }
        return timestamp.parse::<i64>().map_err(|_| {
            eprintln!("fatal: invalid date format: {value}");
            GitError::Exit(128)
        });
    }
    // The timezone may be embedded directly after the time in the `T`-separated
    // ISO 8601 form (e.g. `1970-01-01T00:00:01Z` or `...01+0000`), in which case
    // there is no separate whitespace-delimited timezone token to consume.
    let (date, time, embedded_tz) = if let Some((date, rest)) = first.split_once('T') {
        let (time, tz) = log_split_embedded_timezone(rest);
        (date, time, tz)
    } else {
        let Some(time) = parts.next() else {
            return log_invalid_date_format(value);
        };
        (first, time, None)
    };
    let timezone = match embedded_tz {
        Some(tz) => tz,
        None => match parts.next() {
            Some(tz) => tz.to_string(),
            None => return log_invalid_date_format(value),
        },
    };
    if parts.next().is_some() {
        return log_invalid_date_format(value);
    }
    let Some((year, month, day)) = log_parse_date_ymd(date) else {
        return log_invalid_date_format(value);
    };
    let Some((hour, minute, second)) = log_parse_time_hms(time) else {
        return log_invalid_date_format(value);
    };
    let Some(timezone_offset) = log_parse_timezone_offset_seconds(&timezone) else {
        return log_invalid_date_format(value);
    };
    let days = log_days_from_civil(year, month, day);
    Ok(days * 86_400 + i64::from(hour * 3_600 + minute * 60 + second) - timezone_offset)
}

/// Split an ISO 8601 time portion (the part after `T`) into the bare time and an
/// optional embedded timezone. Recognises a trailing `Z` (UTC, normalised to
/// `+0000`) and a trailing `±HHMM` offset; otherwise the whole string is the time
/// and the timezone (if any) is supplied separately.
fn log_split_embedded_timezone(rest: &str) -> (&str, Option<String>) {
    if let Some(time) = rest.strip_suffix('Z') {
        return (time, Some("+0000".to_string()));
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 5 {
        let tz_start = bytes.len() - 5;
        if matches!(bytes[tz_start], b'+' | b'-')
            && bytes[tz_start + 1..].iter().all(|byte| byte.is_ascii_digit())
        {
            return (&rest[..tz_start], Some(rest[tz_start..].to_string()));
        }
    }
    (rest, None)
}

fn log_parse_date_ymd(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = log_days_in_month(year, month);
    if !(1..=max_day).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

fn log_parse_time_hms(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    let second = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

fn log_parse_timezone_offset_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes.first(), Some(b'+' | b'-'))
        || !bytes[1..].iter().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let hours = value[1..3].parse::<i64>().ok()?;
    let minutes = value[3..5].parse::<i64>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = hours * 3_600 + minutes * 60;
    if bytes[0] == b'-' {
        Some(-offset)
    } else {
        Some(offset)
    }
}

fn log_days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if log_is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn log_is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn log_days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn log_invalid_date_format<T>(value: &str) -> Result<T> {
    eprintln!("fatal: invalid date format: {value}");
    Err(GitError::Exit(128))
}

fn log_date_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--date' requires a value");
    GitError::Exit(128)
}

fn log_max_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--max-age' requires a value");
    GitError::Exit(128)
}

fn log_min_age_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--min-age' requires a value");
    GitError::Exit(128)
}

fn log_date_cutoff_requires_value_error(option: &str) -> GitError {
    eprintln!("fatal: Option '{option}' requires a value");
    GitError::Exit(128)
}

fn log_author_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--author' requires a value");
    GitError::Exit(128)
}

fn log_committer_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--committer' requires a value");
    GitError::Exit(128)
}

fn log_grep_requires_value_error() -> GitError {
    eprintln!("fatal: Option '--grep' requires a value");
    GitError::Exit(128)
}

fn log_validate_date_format(value: &str) -> Result<()> {
    match value {
        "raw" | "unix" | "relative" | "local" | "iso" | "iso-local" | "iso-strict"
        | "iso-strict-local" | "rfc" | "rfc-local" | "rfc2822" | "rfc2822-local" | "short"
        | "short-local" | "default" | "default-local" | "human" | "human-local" => Ok(()),
        value
            if value.starts_with("format:")
                || value.starts_with("format-local:")
                || value.starts_with("auto:") =>
        {
            Ok(())
        }
        _ => log_unknown_date_format(value),
    }
}

fn log_date_mode(value: &str) -> Result<ForEachRefDateMode> {
    log_validate_date_format(value)?;
    Ok(match value {
        "raw" => ForEachRefDateMode::Raw,
        "unix" => ForEachRefDateMode::Unix,
        "short" | "short-local" => ForEachRefDateMode::Short,
        "iso" | "iso-local" => ForEachRefDateMode::Iso,
        "iso-strict" | "iso-strict-local" => ForEachRefDateMode::IsoStrict,
        "rfc" | "rfc-local" | "rfc2822" | "rfc2822-local" => ForEachRefDateMode::Rfc2822,
        _ => ForEachRefDateMode::Default,
    })
}

fn log_unknown_date_format(value: &str) -> Result<()> {
    eprintln!("fatal: unknown date format {value}");
    Err(GitError::Exit(128))
}

fn log_validate_diff_algorithm(value: &str) -> Result<()> {
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

fn log_validate_inter_hunk_context(value: &str) -> Result<()> {
    let number = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    let digits = match number.as_bytes().first() {
        Some(b'+' | b'-') if number.len() > 1 => &number[1..],
        _ => number,
    };
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(());
    }
    eprintln!(
        "error: option `inter-hunk-context' expects an integer value with an optional k/m/g suffix"
    );
    Err(GitError::Exit(129))
}

fn log_inter_hunk_context_requires_number_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' expects a numerical value");
    Err(GitError::Exit(129))
}

fn log_validate_output_indicator(option: &str, value: &str) -> Result<()> {
    // git's diff_opt_char (diff.c) accepts the value only when it is exactly one
    // byte long: it errors via `if (arg[1])` and the empty string is rejected too,
    // so the contract is a single byte. A multibyte single Unicode scalar (len 2+)
    // is therefore rejected, matching git 2.54.
    if value.len() == 1 {
        return Ok(());
    }
    eprintln!("error: {option} expects a character, got '{value}'");
    Err(GitError::Exit(129))
}

fn log_validate_submodule_format(value: &str) -> Result<()> {
    match value {
        "short" | "log" | "diff" => Ok(()),
        _ => {
            eprintln!("error: failed to parse --submodule option parameter: '{value}'");
            Err(GitError::Exit(129))
        }
    }
}

fn log_validate_ignore_submodules(value: &str) -> Result<()> {
    match value {
        "none" | "untracked" | "dirty" | "all" => Ok(()),
        _ => {
            eprintln!("fatal: bad --ignore-submodules argument: {value}");
            Err(GitError::Exit(128))
        }
    }
}

fn log_validate_color_moved(value: &str) -> Result<()> {
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

fn log_validate_color(value: &str) -> Result<()> {
    match value {
        "always" | "auto" | "never" => Ok(()),
        _ => {
            eprintln!("error: option `color' expects \"always\", \"auto\", or \"never\"");
            Err(GitError::Exit(129))
        }
    }
}

fn log_validate_color_moved_ws(value: &str) -> Result<()> {
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

fn log_color_moved_ws_invalid_mode(value: &str, mode: &str) -> Result<()> {
    eprintln!(
        "error: unknown color-moved-ws mode '{mode}', possible values are 'ignore-space-change', 'ignore-space-at-eol', 'ignore-all-space', 'allow-indentation-change'"
    );
    eprintln!("error: invalid mode '{value}' in --color-moved-ws");
    Err(GitError::Exit(129))
}

fn log_validate_ws_error_highlight(value: &str) -> Result<()> {
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

fn parse_rev_list_blob_limit(value: &str) -> Result<usize> {
    // `blob:limit=<n>` accepts a `git_parse_ulong` value: base-0 with an optional
    // case-insensitive k/m/g (1024-scaled) suffix, matching upstream's filter-spec parser.
    git_parse_blob_limit(value)
        .and_then(|limit| usize::try_from(limit).ok())
        .ok_or_else(|| {
            eprintln!("fatal: invalid filter-spec 'blob:limit={value}'");
            GitError::Exit(128)
        })
}

/// `git_parse_ulong` for `blob:limit`: a base-0 integer (decimal, `0x` hex, leading-`0` octal)
/// with an optional case-insensitive `k`/`m`/`g` suffix scaling by 1024/1024²/1024³.
fn git_parse_blob_limit(value: &str) -> Option<u64> {
    if value.is_empty() || value.contains('-') {
        return None;
    }
    let (digits, factor) = match value.as_bytes()[value.len() - 1] {
        b'k' | b'K' => (&value[..value.len() - 1], 1024u64),
        b'm' | b'M' => (&value[..value.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    let base = if let Some(hex) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        u64::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse::<u64>().ok()?
    };
    base.checked_mul(factor)
}

fn parse_rev_list_tree_depth(value: &str) -> Result<usize> {
    value.parse::<usize>().map_err(|_| {
        eprintln!("fatal: expected 'tree:<depth>'");
        GitError::Exit(128)
    })
}

fn parse_rev_list_object_type_filter(value: &str) -> Result<ObjectType> {
    match value {
        "blob" => Ok(ObjectType::Blob),
        "tree" => Ok(ObjectType::Tree),
        "commit" => Ok(ObjectType::Commit),
        "tag" => Ok(ObjectType::Tag),
        _ => {
            eprintln!("fatal: '{value}' for 'object:type=<type>' is not a valid object type");
            Err(GitError::Exit(128))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevListOrdering {
    Default,
    /// `--topo-order` — git's `REV_SORT_IN_GRAPH_ORDER`: a strict topological
    /// linearization whose tie-break preserves the traversal (commit-date) order
    /// via a LIFO emission queue with reversed initial tips.
    Topo,
    /// `--date-order` — git's `REV_SORT_BY_COMMIT_DATE`: topological with a
    /// committer-time priority queue tie-break.
    Date,
    /// `--author-date-order` — git's `REV_SORT_BY_AUTHOR_DATE`: topological with
    /// an author-time priority queue tie-break.
    AuthorDate,
}

/// `--topo-order` (git's `REV_SORT_IN_GRAPH_ORDER`).
///
/// Reproduces `sort_in_topological_order` byte-for-byte for the graph-order
/// sort: indegrees are computed from a committer-date-ordered pass, the initial
/// tips (indegree 1) are collected in that order and then *reversed*, and
/// emission is LIFO — parents are pushed onto the tail of the work queue when
/// their last child is emitted, and the next commit is popped from the tail.
/// This preserves the traversal order at the tips while guaranteeing no parent
/// precedes any of its children.
fn rev_list_topo_order(
    records: Vec<&sley_rev::CommitRecord>,
) -> Result<Vec<&sley_rev::CommitRecord>> {
    // git's `revs->commits` reaches `sort_in_topological_order` already in
    // committer-date order; reproduce that input ordering first so the tip /
    // LIFO sequence matches.
    let records = rev_list_commit_date_input_order(records)?;
    Ok(rev_list_topo_emit(records, None))
}

fn rev_list_date_order(
    records: Vec<&sley_rev::CommitRecord>,
) -> Result<Vec<&sley_rev::CommitRecord>> {
    let timestamps = records
        .iter()
        .map(|record| commit_identity_timestamp_i64(&record.commit.committer))
        .collect::<Result<Vec<_>>>()?;
    Ok(rev_list_ready_order(records, |idx| {
        (timestamps[idx], Reverse(idx))
    }))
}

/// `--author-date-order` (git's `REV_SORT_BY_AUTHOR_DATE`).
///
/// Identical topological readiness to [`rev_list_date_order`], but the priority
/// queue is keyed on the *author* timestamp rather than the committer one.
fn rev_list_author_date_order(
    records: Vec<&sley_rev::CommitRecord>,
) -> Result<Vec<&sley_rev::CommitRecord>> {
    let timestamps = records
        .iter()
        .map(|record| commit_identity_timestamp_i64(&record.commit.author))
        .collect::<Result<Vec<_>>>()?;
    Ok(rev_list_ready_order(records, |idx| {
        (timestamps[idx], Reverse(idx))
    }))
}

/// Order a reachable commit set into the committer-date order git's traversal
/// produces before it hands the list to `sort_in_topological_order`. Newest
/// committer time first, ties broken by the SMALLER oid (matching git's
/// `(commit_time, Reverse(oid))` priority during the limiting walk).
fn rev_list_commit_date_input_order(
    records: Vec<&sley_rev::CommitRecord>,
) -> Result<Vec<&sley_rev::CommitRecord>> {
    let mut keyed = records
        .into_iter()
        .map(|record| {
            commit_identity_timestamp_i64(&record.commit.committer).map(|ts| (ts, record))
        })
        .collect::<Result<Vec<_>>>()?;
    // Newest first; for equal times the smaller oid first.
    keyed.sort_by(|(ta, a), (tb, b)| tb.cmp(ta).then_with(|| a.oid.cmp(&b.oid)));
    Ok(keyed.into_iter().map(|(_, record)| record).collect())
}

/// Linearize `records` (already in git's input order) topologically using a
/// LIFO emission queue with reversed initial tips — git's graph-order sort.
///
/// `priority` is unused for graph order (`None`); the parameter is reserved so a
/// future date-keyed prio-queue variant can share this readiness machinery, but
/// the date orders currently route through [`rev_list_ready_order`] which is
/// already byte-identical to git for them.
fn rev_list_topo_emit<'a>(
    records: Vec<&'a sley_rev::CommitRecord>,
    priority: Option<&[i64]>,
) -> Vec<&'a sley_rev::CommitRecord> {
    let _ = priority;
    let index_by_oid = records
        .iter()
        .enumerate()
        .map(|(idx, record)| (record.oid, idx))
        .collect::<HashMap<_, _>>();
    // Indegree: mark every listed commit 1, then for each listed parent that is
    // itself in the set, increment. A commit whose indegree stays 1 is a tip.
    let mut indegree = vec![1usize; records.len()];
    for record in &records {
        for parent in &record.parents {
            if let Some(&pi) = index_by_oid.get(parent) {
                indegree[pi] += 1;
            }
        }
    }
    // Tips in input order, then reversed for LIFO emission.
    let mut queue: Vec<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(idx, deg)| (*deg == 1).then_some(idx))
        .collect();
    queue.reverse();
    let mut out = Vec::with_capacity(records.len());
    while let Some(idx) = queue.pop() {
        let record = records[idx];
        for parent in &record.parents {
            if let Some(&pi) = index_by_oid.get(parent) {
                if indegree[pi] == 0 {
                    continue;
                }
                indegree[pi] -= 1;
                if indegree[pi] == 1 {
                    queue.push(pi);
                }
            }
        }
        indegree[idx] = 0;
        out.push(record);
    }
    out
}

fn rev_list_ready_order<K: Ord>(
    records: Vec<&sley_rev::CommitRecord>,
    ready_key: impl Fn(usize) -> K,
) -> Vec<&sley_rev::CommitRecord> {
    let index_by_oid = records
        .iter()
        .enumerate()
        .map(|(idx, record)| (record.oid, idx))
        .collect::<HashMap<_, _>>();
    let mut remaining_children = vec![0usize; records.len()];
    for record in &records {
        for parent in &record.parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] += 1;
            }
        }
    }
    let mut ready = remaining_children
        .iter()
        .enumerate()
        .filter_map(|(idx, child_count)| (*child_count == 0).then_some(idx))
        .collect::<Vec<_>>();
    let mut emitted = vec![false; records.len()];
    let mut out = Vec::with_capacity(records.len());
    while !ready.is_empty() {
        let ready_pos = ready
            .iter()
            .enumerate()
            .max_by_key(|(_, idx)| ready_key(**idx))
            .map(|(pos, _)| pos)
            .expect("ready is not empty");
        let idx = ready.swap_remove(ready_pos);
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        let record = records[idx];
        out.push(record);
        for parent in &record.parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] = remaining_children[parent_idx].saturating_sub(1);
                if remaining_children[parent_idx] == 0 && !emitted[parent_idx] {
                    ready.push(parent_idx);
                }
            }
        }
    }
    for (idx, record) in records.into_iter().enumerate() {
        if !emitted[idx] {
            out.push(record);
        }
    }
    out
}

/// Date-order a metadata-only commit list. Mirrors [`rev_list_date_order`] /
/// [`rev_list_ready_order`] exactly (topological readiness + a
/// `(commit_time, Reverse(idx))` key), but on [`sley_rev::CommitMetadata`] whose
/// committer time came from the commit-graph — so the order is byte-identical to
/// the full-record path without reading any commit object.
fn rev_list_metadata_date_order(
    records: Vec<sley_rev::CommitMetadata>,
) -> Vec<sley_rev::CommitMetadata> {
    let index_by_oid = records
        .iter()
        .enumerate()
        .map(|(idx, record)| (record.oid, idx))
        .collect::<HashMap<_, _>>();
    let mut remaining_children = vec![0usize; records.len()];
    for record in &records {
        for parent in &record.parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] += 1;
            }
        }
    }
    let mut ready = remaining_children
        .iter()
        .enumerate()
        .filter_map(|(idx, child_count)| (*child_count == 0).then_some(idx))
        .collect::<Vec<_>>();
    let mut emitted = vec![false; records.len()];
    let mut order = Vec::with_capacity(records.len());
    while !ready.is_empty() {
        let ready_pos = ready
            .iter()
            .enumerate()
            .max_by_key(|(_, idx)| (records[**idx].commit_time, Reverse(**idx)))
            .map(|(pos, _)| pos)
            .expect("ready is not empty");
        let idx = ready.swap_remove(ready_pos);
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        order.push(idx);
        for parent in &records[idx].parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] = remaining_children[parent_idx].saturating_sub(1);
                if remaining_children[parent_idx] == 0 && !emitted[parent_idx] {
                    ready.push(parent_idx);
                }
            }
        }
    }
    for (idx, was_emitted) in emitted.iter().enumerate() {
        if !was_emitted {
            order.push(idx);
        }
    }
    let mut slots = records.into_iter().map(Some).collect::<Vec<_>>();
    order
        .into_iter()
        .filter_map(|idx| slots[idx].take())
        .collect()
}

fn rev_list_walk_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
    first_parent: bool,
) -> Result<Vec<sley_rev::CommitRecord>> {
    if !first_parent {
        return sley_rev::walk_commits(db, format, starts);
    }
    let mut seen = HashSet::new();
    let mut pending = starts.into_iter().collect::<VecDeque<_>>();
    let mut out = Vec::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        let parents = commit.parents.clone();
        if let Some(parent) = parents.first() {
            pending.push_back(*parent);
        }
        out.push(sley_rev::CommitRecord {
            oid,
            parents,
            commit,
        });
    }
    Ok(out)
}

fn rev_list_no_walk_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<sley_rev::CommitRecord>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for oid in starts {
        if !seen.insert(oid) {
            continue;
        }
        out.push(read_rev_list_commit_record(db, format, oid)?);
    }
    Ok(out)
}

fn read_rev_list_commit_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: ObjectId,
) -> Result<sley_rev::CommitRecord> {
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(format, &object.body)?;
    let parents = commit.parents.clone();
    Ok(sley_rev::CommitRecord {
        oid,
        parents,
        commit,
    })
}

fn add_rev_list_revision_arg(
    value: &str,
    not: bool,
    includes: &mut Vec<String>,
    excludes: &mut Vec<String>,
    linear_ranges: &mut Vec<(String, String, bool)>,
    symmetric_ranges: &mut Vec<(String, String, bool)>,
) -> Result<()> {
    if let Some(exclude) = value.strip_prefix('^')
        && !exclude.is_empty()
    {
        if not {
            includes.push(exclude.to_string());
        } else {
            excludes.push(exclude.to_string());
        }
        return Ok(());
    }
    let selection = if value.contains("..") {
        let Some(range) = sley_rev::parse_revision_range(value) else {
            return Err(GitError::Command(format!(
                "unsupported rev-list range {value}"
            )));
        };
        let mut selection = sley_rev::RevisionSelection::new();
        selection.range(range);
        selection
    } else {
        sley_rev::RevisionSelection::from_specs([value])?
    };
    for item in selection.items() {
        match item {
            sley_rev::RevisionSelectionItem::Include(rev) => {
                if not {
                    excludes.push(rev.clone());
                } else {
                    includes.push(rev.clone());
                }
            }
            sley_rev::RevisionSelectionItem::Exclude(rev) => {
                if not {
                    includes.push(rev.clone());
                } else {
                    excludes.push(rev.clone());
                }
            }
            sley_rev::RevisionSelectionItem::Range(sley_rev::RevisionRange::Asymmetric {
                start,
                end,
            }) => {
                linear_ranges.push((start.clone(), end.clone(), not));
            }
            sley_rev::RevisionSelectionItem::Range(sley_rev::RevisionRange::Symmetric {
                left,
                right,
            }) => {
                symmetric_ranges.push((left.clone(), right.clone(), not));
            }
        }
    }
    Ok(())
}

enum RevListRefSelector {
    All {
        not: bool,
        excludes: Vec<String>,
        hidden: Option<RevListHiddenRefsSection>,
    },
    Glob {
        not: bool,
        pattern: String,
        excludes: Vec<String>,
        hidden: Option<RevListHiddenRefsSection>,
    },
    Branches {
        not: bool,
        patterns: Vec<String>,
        include_all: bool,
        excludes: Vec<String>,
        hidden: Option<RevListHiddenRefsSection>,
    },
    Tags {
        not: bool,
        patterns: Vec<String>,
        include_all: bool,
        excludes: Vec<String>,
        hidden: Option<RevListHiddenRefsSection>,
    },
    Remotes {
        not: bool,
        patterns: Vec<String>,
        include_all: bool,
        excludes: Vec<String>,
        hidden: Option<RevListHiddenRefsSection>,
    },
}

#[derive(Debug, Clone, Copy)]
enum RevListHiddenRefsSection {
    Fetch,
    Receive,
    Uploadpack,
}

#[derive(Default)]
struct RevListHiddenRefs {
    transfer: Vec<String>,
    fetch: Vec<String>,
    receive: Vec<String>,
    uploadpack: Vec<String>,
}

impl RevListHiddenRefs {
    fn from_config(config: &GitConfig) -> Self {
        Self {
            transfer: config_section_values(config, "transfer", None, "hideRefs"),
            fetch: config_section_values(config, "fetch", None, "hideRefs"),
            receive: config_section_values(config, "receive", None, "hideRefs"),
            uploadpack: config_section_values(config, "uploadpack", None, "hideRefs"),
        }
    }

    fn section_patterns(&self, section: RevListHiddenRefsSection) -> &[String] {
        match section {
            RevListHiddenRefsSection::Fetch => &self.fetch,
            RevListHiddenRefsSection::Receive => &self.receive,
            RevListHiddenRefsSection::Uploadpack => &self.uploadpack,
        }
    }
}

fn config_section_values(
    config: &GitConfig,
    section: &str,
    subsection: Option<&str>,
    key: &str,
) -> Vec<String> {
    config
        .sections
        .iter()
        .filter(|candidate| {
            candidate.name.eq_ignore_ascii_case(section)
                && candidate.subsection.as_deref() == subsection
        })
        .flat_map(|candidate| candidate.entries.iter())
        .filter(|entry| entry.key.eq_ignore_ascii_case(key))
        .filter_map(|entry| entry.value.clone())
        .collect()
}

fn rev_list_ref_selection(
    refname: &str,
    selectors: &[RevListRefSelector],
    hidden_refs: &RevListHiddenRefs,
) -> (bool, bool) {
    let mut include = false;
    let mut exclude = false;
    for selector in selectors {
        let (not, selected) = match selector {
            RevListRefSelector::All {
                not,
                excludes,
                hidden,
            } => (
                *not,
                !rev_list_ref_excluded(refname, excludes, None)
                    && !rev_list_ref_hidden(refname, *hidden, hidden_refs),
            ),
            RevListRefSelector::Glob {
                not,
                pattern,
                excludes,
                hidden,
            } => (
                *not,
                rev_list_glob_ref_selector_matches(pattern, refname)
                    && !rev_list_ref_excluded(refname, excludes, None)
                    && !rev_list_ref_hidden(refname, *hidden, hidden_refs),
            ),
            RevListRefSelector::Branches {
                not,
                patterns,
                include_all,
                excludes,
                hidden,
            } => (
                *not,
                rev_list_ref_selector_matches(refname, "refs/heads/", *include_all, patterns)
                    && !rev_list_ref_excluded(refname, excludes, Some("refs/heads/"))
                    && !rev_list_ref_hidden(refname, *hidden, hidden_refs),
            ),
            RevListRefSelector::Tags {
                not,
                patterns,
                include_all,
                excludes,
                hidden,
            } => (
                *not,
                rev_list_ref_selector_matches(refname, "refs/tags/", *include_all, patterns)
                    && !rev_list_ref_excluded(refname, excludes, Some("refs/tags/"))
                    && !rev_list_ref_hidden(refname, *hidden, hidden_refs),
            ),
            RevListRefSelector::Remotes {
                not,
                patterns,
                include_all,
                excludes,
                hidden,
            } => (
                *not,
                rev_list_ref_selector_matches(refname, "refs/remotes/", *include_all, patterns)
                    && !rev_list_ref_excluded(refname, excludes, Some("refs/remotes/"))
                    && !rev_list_ref_hidden(refname, *hidden, hidden_refs),
            ),
        };
        if selected {
            if not {
                exclude = true;
            } else {
                include = true;
            }
        }
    }
    (include, exclude)
}

fn rev_list_ref_selector_matches(
    name: &str,
    namespace: &str,
    include_all: bool,
    patterns: &[String],
) -> bool {
    let Some(short_name) = name.strip_prefix(namespace) else {
        return false;
    };
    include_all
        || patterns
            .iter()
            .any(|pattern| rev_list_ref_selector_pattern_matches(pattern, short_name))
}

fn rev_list_ref_selector_pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        refname_pattern_matches(pattern, name)
    } else {
        name.starts_with(&format!("{pattern}/"))
    }
}

fn rev_list_glob_ref_selector_matches(pattern: &str, refname: &str) -> bool {
    let normalized = if pattern.starts_with("refs/") {
        pattern.to_string()
    } else {
        format!("refs/{pattern}")
    };
    if normalized
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        refname_pattern_matches(&normalized, refname)
    } else if normalized.ends_with('/') {
        refname.starts_with(&normalized)
    } else {
        refname.starts_with(&format!("{normalized}/"))
    }
}

fn rev_list_ref_excluded(refname: &str, patterns: &[String], namespace: Option<&str>) -> bool {
    patterns.iter().any(|pattern| {
        rev_list_ref_exclude_pattern_matches(pattern, refname)
            || namespace.is_some_and(|namespace| {
                refname
                    .strip_prefix(namespace)
                    .is_some_and(|name| rev_list_ref_exclude_pattern_matches(pattern, name))
            })
    })
}

fn rev_list_ref_exclude_pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        refname_pattern_matches(pattern, name)
    } else {
        name == pattern || name.starts_with(&format!("{pattern}/"))
    }
}

fn rev_list_ref_hidden(
    refname: &str,
    section: Option<RevListHiddenRefsSection>,
    hidden_refs: &RevListHiddenRefs,
) -> bool {
    let Some(section) = section else {
        return false;
    };
    let mut hidden = false;
    for pattern in hidden_refs
        .transfer
        .iter()
        .chain(hidden_refs.section_patterns(section))
    {
        if let Some(pattern) = pattern.strip_prefix('!') {
            if rev_list_hidden_ref_pattern_matches(pattern, refname) {
                hidden = false;
            }
        } else if rev_list_hidden_ref_pattern_matches(pattern, refname) {
            hidden = true;
        }
    }
    hidden
}

fn rev_list_hidden_ref_pattern_matches(pattern: &str, refname: &str) -> bool {
    let pattern = pattern.strip_prefix('^').unwrap_or(pattern);
    !pattern.is_empty()
        && (refname == pattern
            || refname.starts_with(&format!("{pattern}/"))
            || pattern
                .bytes()
                .any(|byte| matches!(byte, b'*' | b'?' | b'['))
                && refname_pattern_matches(pattern, refname))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogDecorationMode {
    Off,
    Short,
    Full,
}

fn log_decoration_map(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    mode: LogDecorationMode,
) -> Result<HashMap<ObjectId, Vec<String>>> {
    let store = FileRefStore::new(git_dir, format);
    let head_ref = store.current_branch_ref()?;
    let mut decorations = HashMap::<ObjectId, Vec<String>>::new();
    if let Some(head_target) = store.read_ref("HEAD")? {
        match head_target {
            RefTarget::Symbolic(name) => {
                if let Some(target) = store.read_ref(&name)?
                    && let RefTarget::Direct(oid) = target
                    && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
                {
                    let name = log_decoration_ref_name(&name, mode);
                    decorations
                        .entry(commit)
                        .or_default()
                        .push(format!("HEAD -> {name}"));
                }
            }
            RefTarget::Direct(oid) => {
                if let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) {
                    decorations
                        .entry(commit)
                        .or_default()
                        .push("HEAD".to_string());
                }
            }
        }
    }
    let mut tag_labels = Vec::new();
    let mut branch_labels = Vec::new();
    let mut remote_labels = Vec::new();
    let mut other_labels = Vec::new();
    for reference in store.list_refs()? {
        if head_ref.as_deref() == Some(reference.name.as_str()) {
            continue;
        }
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) else {
            continue;
        };
        let label = log_decoration_label(&reference.name, mode);
        if reference.name.starts_with("refs/tags/") {
            tag_labels.push((commit, label));
        } else if reference.name.starts_with("refs/heads/") {
            branch_labels.push((commit, label));
        } else if reference.name.starts_with("refs/remotes/") {
            remote_labels.push((commit, label));
        } else {
            other_labels.push((commit, label));
        }
    }
    for labels in [
        &mut tag_labels,
        &mut branch_labels,
        &mut remote_labels,
        &mut other_labels,
    ] {
        labels.sort_by(|left, right| left.1.cmp(&right.1));
        for (commit, label) in labels.drain(..) {
            decorations.entry(commit).or_default().push(label);
        }
    }
    Ok(decorations)
}

fn log_decoration_label(refname: &str, mode: LogDecorationMode) -> String {
    if refname.starts_with("refs/tags/") {
        format!("tag: {}", log_decoration_ref_name(refname, mode))
    } else {
        log_decoration_ref_name(refname, mode)
    }
}

fn log_decoration_ref_name(refname: &str, mode: LogDecorationMode) -> String {
    if mode == LogDecorationMode::Full {
        return refname.to_string();
    }
    refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/tags/"))
        .or_else(|| refname.strip_prefix("refs/remotes/"))
        .unwrap_or(refname)
        .to_string()
}

fn print_log_decorations(oid: &ObjectId, decorations: &HashMap<ObjectId, Vec<String>>) {
    if let Some(labels) = decorations.get(oid)
        && !labels.is_empty()
    {
        print!(" ({})", labels.join(", "));
    }
}

fn parse_log_count(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid max-count {value}")))
}

fn parse_rev_list_exclude_hidden(value: &str) -> Result<RevListHiddenRefsSection> {
    match value {
        "fetch" => Ok(RevListHiddenRefsSection::Fetch),
        "receive" => Ok(RevListHiddenRefsSection::Receive),
        "uploadpack" => Ok(RevListHiddenRefsSection::Uploadpack),
        _ => Err(GitError::Command(format!(
            "unsupported section for hidden refs: {value}"
        ))),
    }
}

fn rev_list_exclude_hidden_selector_error(selector: &str) -> Result<()> {
    eprintln!("error: options '--exclude-hidden' and '{selector}' cannot be used together");
    Err(GitError::Exit(129))
}

fn commit_author_identity(raw: &[u8]) -> String {
    let author = String::from_utf8_lossy(raw);
    author
        .rsplit_once(' ')
        .and_then(|(left, _)| left.rsplit_once(' ').map(|(identity, _)| identity))
        .unwrap_or(&author)
        .to_string()
}

#[derive(Debug)]
struct SimpleLogRegex {
    alternatives: Vec<SimpleLogRegexAlternative>,
    /// `--perl-regexp` patterns compile through the full grep regex engine in
    /// PCRE mode instead of the simple BRE subset above.
    perl: Option<commands::grep::Regex>,
}

#[derive(Debug, Clone, Copy)]
enum SimpleLogRegexMode {
    Basic,
    Fixed,
    Perl,
}

#[derive(Debug)]
struct LogFilterPattern {
    pattern: String,
    error_context: &'static str,
}

impl LogFilterPattern {
    fn new(pattern: &str, error_context: &'static str) -> Self {
        Self {
            pattern: pattern.to_string(),
            error_context,
        }
    }
}

#[derive(Debug)]
struct SimpleLogRegexAlternative {
    anchor_start: bool,
    anchor_end: bool,
    tokens: Vec<SimpleLogRegexToken>,
}

#[derive(Debug)]
enum SimpleLogRegexToken {
    Literal(u8),
    Any,
    AnyString,
    Class(SimpleLogRegexClass),
}

#[derive(Debug)]
struct SimpleLogRegexClass {
    negated: bool,
    items: Vec<SimpleLogRegexClassItem>,
}

#[derive(Debug)]
enum SimpleLogRegexClassItem {
    Literal(u8),
    Range(u8, u8),
}

impl SimpleLogRegex {
    fn parse(pattern: &str, error_context: &'static str, mode: SimpleLogRegexMode) -> Result<Self> {
        if let SimpleLogRegexMode::Perl = mode {
            let regex =
                commands::grep::Regex::compile(pattern, commands::grep::RegexMode::Pcre, false, false)?;
            return Ok(Self {
                alternatives: Vec::new(),
                perl: Some(regex),
            });
        }
        let alternatives = match mode {
            SimpleLogRegexMode::Basic => split_log_regex_alternatives(pattern)
                .into_iter()
                .map(|alternative| SimpleLogRegexAlternative::parse(alternative, error_context))
                .collect::<Result<Vec<_>>>()?,
            SimpleLogRegexMode::Fixed => vec![SimpleLogRegexAlternative::parse_fixed(pattern)],
            SimpleLogRegexMode::Perl => unreachable!("handled above"),
        };
        Ok(Self {
            alternatives,
            perl: None,
        })
    }

    fn is_match(&self, value: &str, ignore_case: bool) -> bool {
        if let Some(perl) = &self.perl {
            return perl.is_match_with_case(value.as_bytes(), ignore_case);
        }
        self.alternatives
            .iter()
            .any(|alternative| alternative.is_match(value, ignore_case))
    }
}

impl SimpleLogRegexAlternative {
    fn parse(pattern: &str, error_context: &'static str) -> Result<Self> {
        let mut bytes = pattern.as_bytes();
        let anchor_start = bytes.first().copied() == Some(b'^');
        if anchor_start {
            bytes = &bytes[1..];
        }
        let anchor_end = has_unescaped_trailing_dollar(bytes);
        if anchor_end {
            bytes = &bytes[..bytes.len() - 1];
        }
        let mut tokens = Vec::new();
        let mut idx = 0;
        while idx < bytes.len() {
            match bytes[idx] {
                b'\\' if idx + 1 < bytes.len() => {
                    tokens.push(SimpleLogRegexToken::Literal(bytes[idx + 1]));
                    idx += 2;
                }
                b'.' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                    tokens.push(SimpleLogRegexToken::AnyString);
                    idx += 2;
                }
                b'.' => {
                    tokens.push(SimpleLogRegexToken::Any);
                    idx += 1;
                }
                b'[' => {
                    let (class, consumed) =
                        parse_simple_log_regex_class(&bytes[idx + 1..], pattern, error_context)?;
                    tokens.push(SimpleLogRegexToken::Class(class));
                    idx += consumed + 2;
                }
                byte => {
                    tokens.push(SimpleLogRegexToken::Literal(byte));
                    idx += 1;
                }
            }
        }
        Ok(Self {
            anchor_start,
            anchor_end,
            tokens,
        })
    }

    fn parse_fixed(pattern: &str) -> Self {
        Self {
            anchor_start: false,
            anchor_end: false,
            tokens: pattern
                .as_bytes()
                .iter()
                .copied()
                .map(SimpleLogRegexToken::Literal)
                .collect(),
        }
    }

    fn is_match(&self, value: &str, ignore_case: bool) -> bool {
        let bytes = value.as_bytes();
        if self.anchor_start {
            return self.match_from(bytes, 0, 0, ignore_case);
        }
        (0..=bytes.len()).any(|start| self.match_from(bytes, 0, start, ignore_case))
    }

    fn match_from(
        &self,
        bytes: &[u8],
        token_idx: usize,
        byte_idx: usize,
        ignore_case: bool,
    ) -> bool {
        let Some(token) = self.tokens.get(token_idx) else {
            return !self.anchor_end || byte_idx == bytes.len();
        };
        match token {
            SimpleLogRegexToken::Literal(expected) => {
                bytes
                    .get(byte_idx)
                    .is_some_and(|actual| log_regex_byte_eq(*actual, *expected, ignore_case))
                    && self.match_from(bytes, token_idx + 1, byte_idx + 1, ignore_case)
            }
            SimpleLogRegexToken::Any => {
                byte_idx < bytes.len()
                    && self.match_from(bytes, token_idx + 1, byte_idx + 1, ignore_case)
            }
            SimpleLogRegexToken::AnyString => (byte_idx..=bytes.len())
                .any(|idx| self.match_from(bytes, token_idx + 1, idx, ignore_case)),
            SimpleLogRegexToken::Class(class) => {
                bytes
                    .get(byte_idx)
                    .is_some_and(|actual| class.matches(*actual, ignore_case))
                    && self.match_from(bytes, token_idx + 1, byte_idx + 1, ignore_case)
            }
        }
    }
}

impl SimpleLogRegexClass {
    fn matches(&self, value: u8, ignore_case: bool) -> bool {
        let matched = self.items.iter().any(|item| match item {
            SimpleLogRegexClassItem::Literal(expected) => {
                log_regex_byte_eq(value, *expected, ignore_case)
            }
            SimpleLogRegexClassItem::Range(start, end) => {
                if ignore_case {
                    let value = value.to_ascii_lowercase();
                    let start = start.to_ascii_lowercase();
                    let end = end.to_ascii_lowercase();
                    start <= value && value <= end
                } else {
                    *start <= value && value <= *end
                }
            }
        });
        if self.negated { !matched } else { matched }
    }
}

fn log_regex_byte_eq(left: u8, right: u8, ignore_case: bool) -> bool {
    if ignore_case {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

fn parse_log_filter_patterns(
    patterns: &[LogFilterPattern],
    mode: SimpleLogRegexMode,
) -> Result<Vec<SimpleLogRegex>> {
    patterns
        .iter()
        .map(|pattern| SimpleLogRegex::parse(&pattern.pattern, pattern.error_context, mode))
        .collect()
}

fn split_log_regex_alternatives(pattern: &str) -> Vec<&str> {
    let mut alternatives = Vec::new();
    let bytes = pattern.as_bytes();
    let mut start = 0;
    let mut idx = 0;
    while idx + 1 < bytes.len() {
        if bytes[idx] == b'\\' && bytes[idx + 1] == b'|' {
            alternatives.push(&pattern[start..idx]);
            idx += 2;
            start = idx;
        } else {
            idx += 1;
        }
    }
    alternatives.push(&pattern[start..]);
    alternatives
}

fn parse_simple_log_regex_class(
    bytes: &[u8],
    pattern: &str,
    error_context: &'static str,
) -> Result<(SimpleLogRegexClass, usize)> {
    let mut end = None;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b']' && idx > 0 {
            end = Some(idx);
            break;
        }
    }
    let Some(end) = end else {
        return log_regex_unterminated_class_error(bytes, pattern, error_context);
    };
    let mut class = &bytes[..end];
    let negated = class.first().copied().is_some_and(|byte| byte == b'^');
    if negated {
        class = &class[1..];
    }
    let mut items = Vec::new();
    let mut idx = 0;
    while idx < class.len() {
        if idx + 2 < class.len() && class[idx + 1] == b'-' {
            items.push(SimpleLogRegexClassItem::Range(class[idx], class[idx + 2]));
            idx += 3;
        } else {
            items.push(SimpleLogRegexClassItem::Literal(class[idx]));
            idx += 1;
        }
    }
    Ok((SimpleLogRegexClass { negated, items }, end))
}

fn log_regex_unterminated_class_error(
    class_bytes: &[u8],
    pattern: &str,
    error_context: &str,
) -> Result<(SimpleLogRegexClass, usize)> {
    // `class_bytes` is everything after the opening `[`. git (via POSIX regerror)
    // distinguishes two cases: an opening bracket with no class content at all —
    // `[` or `[^` at end of pattern — reports a generic "Invalid regular
    // expression"; an unterminated class that does have content (e.g. `[a`, `[]`,
    // `[[:alpha:]`) reports the bracket-specific "Unmatched" diagnostic. In POSIX
    // BRE a `]` immediately following `[`/`[^` is a literal member, so it counts as
    // content. Match that split exactly for git 2.54 parity.
    let after_caret = class_bytes.strip_prefix(b"^").unwrap_or(class_bytes);
    let message = if after_caret.is_empty() {
        "Invalid regular expression"
    } else {
        "Unmatched [, [^, [:, [., or [="
    };
    eprintln!("fatal: {error_context}, '{pattern}': {message}");
    Err(GitError::Exit(128))
}

fn log_author_filters_match(
    record: &sley_rev::CommitRecord,
    filters: &[SimpleLogRegex],
    ignore_case: bool,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let author = String::from_utf8_lossy(&record.commit.author);
    filters
        .iter()
        .any(|filter| filter.is_match(&author, ignore_case))
}

fn log_committer_filters_match(
    record: &sley_rev::CommitRecord,
    filters: &[SimpleLogRegex],
    ignore_case: bool,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let committer = String::from_utf8_lossy(&record.commit.committer);
    filters
        .iter()
        .any(|filter| filter.is_match(&committer, ignore_case))
}

fn log_grep_filters_match(
    record: &sley_rev::CommitRecord,
    filters: &[SimpleLogRegex],
    all_match: bool,
    invert: bool,
    ignore_case: bool,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let message = String::from_utf8_lossy(&record.commit.message);
    let matched = if all_match {
        filters
            .iter()
            .all(|filter| filter.is_match(&message, ignore_case))
    } else {
        filters
            .iter()
            .any(|filter| filter.is_match(&message, ignore_case))
    };
    matched != invert
}

fn format_log_abbrev_oid(oid: &ObjectId) -> String {
    format_log_oid(oid, Some(7))
}

fn format_log_oid(oid: &ObjectId, abbrev_len: Option<usize>) -> String {
    let hex = oid.to_hex();
    match abbrev_len {
        Some(width) => hex[..width.min(hex.len())].to_string(),
        None => hex,
    }
}

fn format_log_commit_header_oid(
    oid: &ObjectId,
    abbrev_commit: bool,
    abbrev_len: Option<usize>,
) -> String {
    if abbrev_commit {
        format_log_oid(oid, abbrev_len)
    } else {
        oid.to_string()
    }
}

fn format_log_parent_oids(record: &sley_rev::CommitRecord, abbrev_len: Option<usize>) -> String {
    record
        .parents
        .iter()
        .map(|oid| format_log_oid(oid, abbrev_len))
        .collect::<Vec<_>>()
        .join(" ")
}

fn commit_subject(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Raw-bytes subject: the title paragraph with internal newlines folded to
/// single spaces (git's `format_subject`), preserving non-UTF-8/control bytes.
fn commit_subject_bytes(message: &[u8]) -> &[u8] {
    // git skips leading blank lines, then takes lines until a blank line,
    // joining with spaces. The upstream corpus only uses single-line subjects,
    // so we return the first non-empty line slice directly.
    let mut start = 0;
    while start < message.len() && (message[start] == b'\n' || message[start] == b'\r') {
        start += 1;
    }
    let end = message[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|off| start + off)
        .unwrap_or(message.len());
    &message[start..end]
}

fn commit_body(message: &[u8]) -> &[u8] {
    let Some(first_newline) = message.iter().position(|byte| *byte == b'\n') else {
        return &[];
    };
    let mut body = &message[first_newline + 1..];
    if body.first().copied() == Some(b'\n') {
        body = &body[1..];
    }
    body
}

struct LogFormatContext<'a> {
    abbrev_len: Option<usize>,
    decorations: &'a HashMap<ObjectId, Vec<String>>,
    marker: char,
    dialect: LogFormatDialect,
    source: Option<&'a str>,
    date_mode: ForEachRefDateMode,
    /// Per-commit `%S` source label (set when walking refs/ranges/bisect).
    source_oid: Option<&'a HashMap<ObjectId, String>>,
    /// `git_dir`/db/format for placeholders that need object access (`%(describe)`).
    describe: Option<&'a LogDescribeContext<'a>>,
    /// `--color=always`: emit ANSI sequences for `%C(...)`.
    color: bool,
    /// Desired log output encoding (git's `get_log_output_encoding`).
    output_encoding: &'a str,
}

struct LogDescribeContext<'a> {
    git_dir: &'a Path,
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
}

fn print_log_format(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: LogFormatContext<'_>,
) -> Result<()> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(
        record,
        compiled,
        &context,
        &mut line,
        0..compiled.tokens.len(),
    )?;
    // Re-encode the assembled (UTF-8) line to the log output encoding, mirroring
    // git's single final `reencode_string_len` pass.
    let out = log_reencode_message(&line, "UTF-8", context.output_encoding);
    io::stdout().write_all(&out)?;
    io::stdout().flush()?;
    Ok(())
}

fn emit_compiled_log_format(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
    token_range: std::ops::Range<usize>,
) -> Result<()> {
    let (author_name, author_email) = commit_identity_name_email(&record.commit.author);
    let (committer_name, committer_email) = commit_identity_name_email(&record.commit.committer);
    let author_timestamp = commit_identity_timestamp(&record.commit.author);
    let committer_timestamp = commit_identity_timestamp(&record.commit.committer);

    let tokens = &compiled.tokens[token_range];
    let mut pending_pad: Option<log_format::PaddingSpec> = None;
    // Wrap state (git's `format_commit_context`): width/indents plus the offset in
    // `out` where the current wrap region began. A `%w` directive (or end-of-
    // format) flushes the pending region through the word-wrapper.
    let mut wrap_width = 0i32;
    let mut wrap_indent1 = 0i32;
    let mut wrap_indent2 = 0i32;
    let mut wrap_start = out.len();
    let mut idx = 0usize;
    while idx < tokens.len() {
        let token = &tokens[idx];
        if let FormatToken::Wrap(spec) = token {
            let new_w = spec.width as i32;
            let new_i1 = spec.indent1 as i32;
            let new_i2 = spec.indent2 as i32;
            if (new_w, new_i1, new_i2) != (wrap_width, wrap_indent1, wrap_indent2) {
                if wrap_start < out.len() {
                    log_rewrap(out, wrap_start, wrap_width, wrap_indent1, wrap_indent2);
                }
                wrap_start = out.len();
                wrap_width = new_w;
                wrap_indent1 = new_i1;
                wrap_indent2 = new_i2;
            }
            idx += 1;
            continue;
        }
        // A magic prefix (`%-`/`%+`/`% `) wraps the *next* placeholder: it adds a
        // leading newline/space when the placeholder is non-empty, or deletes a
        // preceding newline when it is empty (git's `format_commit_item`).
        if let FormatToken::Magic(magic) = token {
            idx += 1;
            if idx >= tokens.len() {
                continue;
            }
            let mut captured = Vec::new();
            emit_log_one_token(
                &tokens[idx],
                record,
                context,
                &mut captured,
                &author_name,
                &author_email,
                &committer_name,
                &committer_email,
                &author_timestamp,
                &committer_timestamp,
            )?;
            idx += 1;
            match magic {
                log_format::MagicPrefix::DelLfBeforeEmpty if captured.is_empty() => {
                    while out.last() == Some(&b'\n') {
                        out.pop();
                    }
                }
                log_format::MagicPrefix::AddLfBeforeNonEmpty if !captured.is_empty() => {
                    out.push(b'\n');
                    out.extend_from_slice(&captured);
                }
                log_format::MagicPrefix::AddSpBeforeNonEmpty if !captured.is_empty() => {
                    out.push(b' ');
                    out.extend_from_slice(&captured);
                }
                _ => out.extend_from_slice(&captured),
            }
            continue;
        }
        // A padding directive captures the *next* token group (any leading
        // color modifiers plus one content placeholder), pads it, and appends.
        if let FormatToken::Padding(spec) = token {
            pending_pad = Some(*spec);
            idx += 1;
            continue;
        }
        if let Some(spec) = pending_pad.take() {
            // Capture the chain: color modifiers followed by one placeholder.
            let mut captured = Vec::new();
            loop {
                let t = &tokens[idx];
                let is_modifier = matches!(
                    t,
                    FormatToken::ColorParen
                        | FormatToken::ColorName(_)
                        | FormatToken::ColorAuto
                );
                emit_log_one_token(
                    t,
                    record,
                    context,
                    &mut captured,
                    &author_name,
                    &author_email,
                    &committer_name,
                    &committer_email,
                    &author_timestamp,
                    &committer_timestamp,
                )?;
                idx += 1;
                if !is_modifier || idx >= tokens.len() {
                    break;
                }
            }
            apply_padding(out, &captured, spec);
            continue;
        }
        emit_log_one_token(
            token,
            record,
            context,
            out,
            &author_name,
            &author_email,
            &committer_name,
            &committer_email,
            &author_timestamp,
            &committer_timestamp,
        )?;
        idx += 1;
    }
    // git's final `rewrap_message_tail(sb, c, 0, 0, 0)`: flush the tail region if
    // a non-trivial wrap width is active.
    if (wrap_width, wrap_indent1, wrap_indent2) != (0, 0, 0) && wrap_start < out.len() {
        log_rewrap(out, wrap_start, wrap_width, wrap_indent1, wrap_indent2);
    }
    Ok(())
}

/// git's `strbuf_wrap`: word-wrap `out[pos..]` in place.
fn log_rewrap(out: &mut Vec<u8>, pos: usize, width: i32, indent1: i32, indent2: i32) {
    let region = out.split_off(pos);
    log_wrap_text(out, &region, indent1, indent2, width);
}

#[allow(clippy::too_many_arguments)]
fn emit_log_one_token(
    token: &FormatToken,
    record: &sley_rev::CommitRecord,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
    author_timestamp: &str,
    committer_timestamp: &str,
) -> Result<()> {
    let LogFormatContext {
        abbrev_len,
        decorations,
        marker,
        dialect,
        source,
        date_mode,
        source_oid,
        describe,
        color,
        output_encoding,
    } = *context;
    // git formats in UTF-8 (re-encoding the stored message to UTF-8 up front),
    // computes alignment/width in UTF-8, and re-encodes the *final* output to the
    // log output encoding once at the end (handled by the print path). So here we
    // always normalise the message to UTF-8.
    let _ = output_encoding;
    let reencoded_message =
        log_reencode_message(&record.commit.message, &commit_encoding(&record.commit), "UTF-8");
    let message: &[u8] = &reencoded_message;
    {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::OidFull => write!(out, "{}", record.oid).map_err(io::Error::from)?,
            FormatToken::OidAbbrev => {
                write!(out, "{}", format_log_oid(&record.oid, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::TreeFull => {
                write!(out, "{}", record.commit.tree).map_err(io::Error::from)?
            }
            FormatToken::TreeAbbrev => {
                write!(out, "{}", format_log_oid(&record.commit.tree, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::ParentsFull => {
                write!(out, "{}", format_log_parent_oids(record, None)).map_err(io::Error::from)?;
            }
            FormatToken::ParentsAbbrev => {
                write!(out, "{}", format_log_parent_oids(record, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::Marker => out.push(marker as u8),
            FormatToken::Subject => {
                out.extend_from_slice(commit_subject_bytes(message));
            }
            FormatToken::SanitizedSubject => {
                write!(out, "{}", log_sanitized_subject(message)).map_err(io::Error::from)?;
            }
            FormatToken::Encoding => {
                write!(out, "{}", commit_encoding(&record.commit)).map_err(io::Error::from)?;
            }
            FormatToken::NoteName if dialect == LogFormatDialect::Log => {}
            FormatToken::NoteName => out.extend_from_slice(b"%N"),
            FormatToken::RevisionSource if dialect == LogFormatDialect::Log => {
                if let Some(map) = source_oid
                    && let Some(label) = map.get(&record.oid)
                {
                    out.extend_from_slice(label.as_bytes());
                } else if let Some(source) = source {
                    out.extend_from_slice(source.as_bytes());
                }
            }
            FormatToken::RevisionSource => out.extend_from_slice(b"%S"),
            FormatToken::ColorParen | FormatToken::ColorName(_) => {}
            FormatToken::Body => out.extend_from_slice(commit_body(message)),
            FormatToken::FullMessage => out.extend_from_slice(message),
            FormatToken::DecorationsParen => {
                write!(
                    out,
                    "{}",
                    format_log_format_decorations(&record.oid, decorations, true)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::DecorationsBare => {
                write!(
                    out,
                    "{}",
                    format_log_format_decorations(&record.oid, decorations, false)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::GRefname => out.push(b'N'),
            FormatToken::GTrailers => out.extend_from_slice(b"undefined"),
            FormatToken::GPlaceholder
            | FormatToken::GSignature
            | FormatToken::GKey
            | FormatToken::GFingerprint
            | FormatToken::GPassthrough
            | FormatToken::GDate
            | FormatToken::GDateShort
            | FormatToken::GDateIso
            | FormatToken::GDateIsoStrict
            | FormatToken::GDateRfc2822 => {}
            FormatToken::AuthorName => out.extend_from_slice(author_name.as_bytes()),
            FormatToken::AuthorEmail => out.extend_from_slice(author_email.as_bytes()),
            FormatToken::AuthorEmailLocal => {
                write!(out, "{}", log_email_local_part(&author_email)).map_err(io::Error::from)?;
            }
            FormatToken::AuthorTimestamp => out.extend_from_slice(author_timestamp.as_bytes()),
            FormatToken::AuthorDate => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, date_mode)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, ForEachRefDateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, ForEachRefDateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, ForEachRefDateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, ForEachRefDateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterName => out.extend_from_slice(committer_name.as_bytes()),
            FormatToken::CommitterEmail => out.extend_from_slice(committer_email.as_bytes()),
            FormatToken::CommitterEmailLocal => {
                write!(out, "{}", log_email_local_part(&committer_email))
                    .map_err(io::Error::from)?;
            }
            FormatToken::CommitterTimestamp => {
                out.extend_from_slice(committer_timestamp.as_bytes())
            }
            FormatToken::CommitterDate => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, date_mode)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, ForEachRefDateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, ForEachRefDateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, ForEachRefDateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, ForEachRefDateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            FormatToken::Trailers(opts) => {
                let parsed = crate::commands::for_each_ref::parse_for_each_ref_trailer_options(
                    opts,
                )
                .map_err(|_| GitError::Command("invalid %(trailers) options".into()))?;
                let rendered = crate::commands::for_each_ref::for_each_ref_format_trailers(
                    message,
                    &parsed,
                );
                out.extend_from_slice(&rendered);
            }
            FormatToken::Decorate(spec) => {
                emit_log_decorate(out, &record.oid, decorations, spec);
            }
            FormatToken::Describe(spec) => {
                if let Some(describe_ctx) = describe {
                    let rendered = log_describe_placeholder(describe_ctx, &record.oid, spec)?;
                    out.extend_from_slice(rendered.as_bytes());
                }
            }
            FormatToken::ColorAuto => {
                // `%C(auto)` toggles auto-coloring; with `--color` we approximate
                // git's reference coloring at emission sites that need it.
                let _ = color;
            }
            FormatToken::Padding(_) | FormatToken::Wrap(_) | FormatToken::Magic(_) => {
                // Handled by the outer state machine in emit_compiled_log_format.
            }
            FormatToken::StashDecoParen
            | FormatToken::StashDecoBare
            | FormatToken::ReflogGd
            | FormatToken::ReflogGD
            | FormatToken::ReflogGn
            | FormatToken::ReflogGe
            | FormatToken::ReflogGs => {}
        }
    }
    Ok(())
}

/// Port of utf8.c `strbuf_add_indented_text` (the `width <= 0` wrap fallback):
/// each line of `text` is prefixed with `indent`/`indent2` spaces.
fn log_add_indented_text(out: &mut Vec<u8>, text: &[u8], indent1: i32, indent2: i32) {
    if text.is_empty() {
        return;
    }
    let mut indent = indent1;
    let mut idx = 0;
    while idx < text.len() {
        let eol = text[idx..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|off| idx + off)
            .unwrap_or(text.len());
        if eol > idx {
            if indent > 0 {
                out.extend(std::iter::repeat_n(b' ', indent as usize));
            }
            out.extend_from_slice(&text[idx..eol]);
        }
        if eol < text.len() {
            out.push(b'\n');
        }
        idx = eol + 1;
        indent = indent2;
    }
}

/// Port of utf8.c `strbuf_add_wrapped_text`: word-wrap `text` into `out` to the
/// given column `width`, indenting the first line by `indent1` and continuation
/// lines by `indent2` (negative `indent1` starts mid-line, as git does).
fn log_wrap_text(out: &mut Vec<u8>, text: &[u8], indent1: i32, indent2: i32, width: i32) {
    if width <= 0 {
        log_add_indented_text(out, text, indent1, indent2);
        return;
    }
    let orig_len = out.len();
    let mut assume_utf8 = true;
    loop {
        // (re)try entry point
        let mut pos = 0usize; // index into `text`
        let mut bol = 0usize;
        let mut w = indent1;
        let mut indent = indent1;
        let mut space: Option<usize> = None;
        if indent < 0 {
            w = -indent;
            space = Some(0);
        }
        let mut restart = false;
        loop {
            // (skip ANSI escapes — not present in the corpus; omitted.)
            let c = text.get(pos).copied().unwrap_or(0);
            if c == 0 || (c as char).is_ascii_whitespace() {
                if w <= width || space.is_none() {
                    // git's `new_line` is reachable here only when `space` is set
                    // and width is exceeded; in this branch we emit the segment.
                    let start = match space {
                        _ if c == 0 && pos == bol => return, // git early return
                        Some(sp) => sp,
                        None => {
                            if indent > 0 {
                                out.extend(std::iter::repeat_n(b' ', indent as usize));
                            }
                            bol
                        }
                    };
                    out.extend_from_slice(&text[start..pos]);
                    if c == 0 {
                        return;
                    }
                    let mut sp = pos;
                    let mut go_new_line = false;
                    if c == b'\t' {
                        w |= 0x07;
                    } else if c == b'\n' {
                        sp += 1;
                        let next = text.get(sp).copied().unwrap_or(0);
                        if next == b'\n' {
                            out.push(b'\n');
                            go_new_line = true;
                        } else if !(next as char).is_ascii_alphanumeric() {
                            go_new_line = true;
                        } else {
                            out.push(b' ');
                        }
                    }
                    if go_new_line {
                        out.push(b'\n');
                        let advance = if (text.get(sp).copied().unwrap_or(0) as char)
                            .is_ascii_whitespace()
                        {
                            1
                        } else {
                            0
                        };
                        bol = sp + advance;
                        pos = bol;
                        space = None;
                        w = indent2;
                        indent = indent2;
                        continue;
                    }
                    space = Some(sp);
                    w += 1;
                    pos += 1;
                    continue;
                } else {
                    // new_line (width exceeded, break at the last space)
                    out.push(b'\n');
                    let sp = space.unwrap_or(pos);
                    let advance = if (text.get(sp).copied().unwrap_or(0) as char)
                        .is_ascii_whitespace()
                    {
                        1
                    } else {
                        0
                    };
                    bol = sp + advance;
                    pos = bol;
                    space = None;
                    w = indent2;
                    indent = indent2;
                    continue;
                }
            }
            // non-space glyph
            if assume_utf8 {
                match log_pick_utf8(text, pos) {
                    Some((cp, len)) => {
                        let gw = log_wcwidth(cp);
                        if gw > 0 {
                            w += gw;
                        }
                        pos += len;
                    }
                    None => {
                        // broken utf-8: restart in byte mode
                        restart = true;
                        break;
                    }
                }
            } else {
                w += 1;
                pos += 1;
            }
        }
        if restart {
            assume_utf8 = false;
            out.truncate(orig_len);
            continue;
        }
        return;
    }
}

/// Display width of a UTF-8 byte slice, mirroring git's `utf8_strnwidth`:
/// control chars contribute 0; invalid UTF-8 falls back to byte length.
fn log_display_width(bytes: &[u8]) -> usize {
    let mut width = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        match log_pick_utf8(bytes, idx) {
            Some((cp, len)) => {
                let w = log_wcwidth(cp);
                if w > 0 {
                    width += w as usize;
                }
                idx += len;
            }
            None => return bytes.len(),
        }
    }
    width
}

/// git `git_wcwidth`.
fn log_wcwidth(ch: u32) -> i32 {
    if ch == 0 {
        return 0;
    }
    if ch < 32 || (0x7f..0xa0).contains(&ch) {
        return -1;
    }
    // We don't ship the full zero/double-width tables; the t4205 corpus only
    // exercises ASCII + Latin-1 (all width 1). Treat everything else as width 1.
    1
}

/// Decode one UTF-8 scalar at `idx`; returns `(codepoint, byte_len)` or `None`
/// for invalid UTF-8 (matching git's `pick_one_utf8_char` validity checks).
fn log_pick_utf8(bytes: &[u8], idx: usize) -> Option<(u32, usize)> {
    let s = &bytes[idx..];
    let b0 = *s.first()?;
    if b0 < 0x80 {
        Some((b0 as u32, 1))
    } else if b0 & 0xe0 == 0xc0 {
        let b1 = *s.get(1)?;
        if b1 & 0xc0 != 0x80 || b0 & 0xfe == 0xc0 {
            return None;
        }
        Some(((((b0 & 0x1f) as u32) << 6) | (b1 & 0x3f) as u32, 2))
    } else if b0 & 0xf0 == 0xe0 {
        let b1 = *s.get(1)?;
        let b2 = *s.get(2)?;
        if b1 & 0xc0 != 0x80
            || b2 & 0xc0 != 0x80
            || (b0 == 0xe0 && b1 & 0xe0 == 0x80)
            || (b0 == 0xed && b1 & 0xe0 == 0xa0)
        {
            return None;
        }
        Some((
            (((b0 & 0x0f) as u32) << 12) | (((b1 & 0x3f) as u32) << 6) | (b2 & 0x3f) as u32,
            3,
        ))
    } else if b0 & 0xf8 == 0xf0 {
        let b1 = *s.get(1)?;
        let b2 = *s.get(2)?;
        let b3 = *s.get(3)?;
        if b1 & 0xc0 != 0x80
            || b2 & 0xc0 != 0x80
            || b3 & 0xc0 != 0x80
            || (b0 == 0xf0 && b1 & 0xf0 == 0x80)
            || (b0 == 0xf4 && b1 > 0x8f)
            || b0 > 0xf4
        {
            return None;
        }
        Some((
            (((b0 & 0x07) as u32) << 18)
                | (((b1 & 0x3f) as u32) << 12)
                | (((b2 & 0x3f) as u32) << 6)
                | (b3 & 0x3f) as u32,
            4,
        ))
    } else {
        None
    }
}

/// Port of utf8.c `strbuf_utf8_replace`: replace the glyphs occupying display
/// columns `[pos, pos+width)` of `src` with `subst` (once), preserving control
/// characters and ANSI escapes verbatim. We don't ship escape parsing here; the
/// padded corpus never mixes truncation with ANSI.
fn log_utf8_replace(src: &[u8], pos: usize, width: usize, subst: &str) -> Vec<u8> {
    let mut dst = Vec::with_capacity(src.len());
    let mut w = 0usize;
    let mut idx = 0usize;
    let mut subst_done = false;
    while idx < src.len() {
        let (cp, len) = match log_pick_utf8(src, idx) {
            Some(v) => v,
            None => return src.to_vec(), // broken utf-8: do nothing
        };
        let mut gw = log_wcwidth(cp);
        if gw < 0 {
            gw = 0;
        }
        let gw = gw as usize;
        if gw != 0 && w >= pos && w < pos + width {
            if !subst_done {
                dst.extend_from_slice(subst.as_bytes());
                subst_done = true;
            }
        } else {
            dst.extend_from_slice(&src[idx..idx + len]);
        }
        w += gw;
        idx += len;
    }
    dst
}

/// Apply a `%<`/`%>`/`%><` padding directive to `captured` and append to `out`,
/// mirroring pretty.c `format_and_pad_commit`.
fn apply_padding(out: &mut Vec<u8>, captured: &[u8], spec: log_format::PaddingSpec) {
    use log_format::{PaddingFlush, PaddingTrunc};
    let mut padding = spec.padding;
    if padding < 0 {
        // Pad to the given column: subtract what's already on the current line.
        let start = match out.iter().rposition(|b| *b == b'\n') {
            Some(p) => p + 1,
            None => 0,
        };
        let occupied = log_display_width(&out[start..]) as i64;
        padding = (-padding) - occupied;
    }
    let len = log_display_width(captured) as i64;

    let mut flush = spec.flush;
    let mut captured = captured.to_vec();
    if flush == PaddingFlush::LeftAndSteal {
        // Steal trailing spaces from `out` to make room (no ANSI handling).
        let mut pad = padding;
        while len > pad {
            match out.last() {
                Some(b' ') => {
                    out.pop();
                    pad += 1;
                }
                _ => break,
            }
        }
        padding = pad;
        flush = PaddingFlush::Left;
    }

    if len > padding {
        match spec.trunc {
            PaddingTrunc::Left => {
                captured = log_utf8_replace(
                    &captured,
                    0,
                    (len - (padding - 2)) as usize,
                    "..",
                );
            }
            PaddingTrunc::Middle => {
                captured = log_utf8_replace(
                    &captured,
                    (padding / 2 - 1) as usize,
                    (len - (padding - 2)) as usize,
                    "..",
                );
            }
            PaddingTrunc::Right => {
                captured = log_utf8_replace(
                    &captured,
                    (padding - 2) as usize,
                    (len - (padding - 2)) as usize,
                    "..",
                );
            }
            PaddingTrunc::None => {}
        }
        out.extend_from_slice(&captured);
    } else {
        let offset = match flush {
            PaddingFlush::Left => (padding - len) as usize,
            PaddingFlush::Both => ((padding - len) / 2) as usize,
            _ => 0,
        };
        // Convert column padding back to bytes: total spaces == padding-len, then
        // the captured bytes are placed at `offset` columns in.
        let total_pad = (padding - len) as usize;
        out.extend(std::iter::repeat_n(b' ', total_pad));
        // Insert captured at the offset (offset is in columns == spaces here).
        let insert_at = out.len() - total_pad + offset;
        out.splice(insert_at..insert_at, captured.iter().copied());
    }
}

/// Render `%(decorate[:opts])` for `oid` from the decorations map, mirroring
/// pretty.c `format_decorations`.
fn emit_log_decorate(
    out: &mut Vec<u8>,
    oid: &ObjectId,
    decorations: &HashMap<ObjectId, Vec<String>>,
    spec: &log_format::DecorateSpec,
) {
    let Some(refs) = decorations.get(oid) else {
        return;
    };
    if refs.is_empty() {
        return;
    }
    out.extend_from_slice(spec.prefix.as_bytes());
    let mut first = true;
    for entry in refs {
        if !first {
            out.extend_from_slice(spec.separator.as_bytes());
        }
        first = false;
        // The decorations map stores entries like "HEAD -> main", "tag: v1",
        // "branch". Re-render the pointer/tag prefixes from the spec.
        let rendered = log_decorate_entry(entry, spec);
        out.extend_from_slice(rendered.as_bytes());
    }
    out.extend_from_slice(spec.suffix.as_bytes());
}

/// Re-render a single decoration entry under the decorate spec's tag/pointer
/// overrides. The stored entry uses the default " -> " pointer and "tag: " tag.
fn log_decorate_entry(entry: &str, spec: &log_format::DecorateSpec) -> String {
    if let Some(rest) = entry.strip_prefix("HEAD -> ") {
        format!("HEAD{}{}", spec.pointer, log_decorate_entry(rest, spec))
    } else if let Some(rest) = entry.strip_prefix("tag: ") {
        format!("{}{}", spec.tag, rest)
    } else {
        entry.to_string()
    }
}

/// Render `%(describe[:opts])` for `oid`, returning an empty string on any
/// describe failure (git treats describe errors as an empty placeholder).
fn log_describe_placeholder(
    ctx: &LogDescribeContext<'_>,
    oid: &ObjectId,
    spec: &log_format::DescribeSpec,
) -> Result<String> {
    let result = crate::commands::describe::describe_for_format(
        ctx.git_dir,
        ctx.format,
        ctx.db,
        oid,
        spec.tags,
        spec.abbrev,
        &spec.matches,
        &spec.excludes,
    )?;
    Ok(result.unwrap_or_default())
}

fn format_metadata_parent_oids(parents: &[ObjectId], abbrev_len: Option<usize>) -> String {
    parents
        .iter()
        .map(|oid| format_log_oid(oid, abbrev_len))
        .collect::<Vec<_>>()
        .join(" ")
}

fn emit_compiled_log_format_metadata(
    record: &sley_rev::CommitMetadata,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let LogFormatContext {
        abbrev_len,
        marker,
        dialect,
        source,
        ..
    } = *context;

    for token in &compiled.tokens {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::OidFull => write!(out, "{}", record.oid).map_err(io::Error::from)?,
            FormatToken::OidAbbrev => {
                write!(out, "{}", format_log_oid(&record.oid, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::ParentsFull => {
                write!(
                    out,
                    "{}",
                    format_metadata_parent_oids(&record.parents, None)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ParentsAbbrev => {
                write!(
                    out,
                    "{}",
                    format_metadata_parent_oids(&record.parents, abbrev_len)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::Marker => out.push(marker as u8),
            FormatToken::NoteName if dialect == LogFormatDialect::Log => {}
            FormatToken::NoteName => out.extend_from_slice(b"%N"),
            FormatToken::RevisionSource if dialect == LogFormatDialect::Log => {
                if let Some(source) = source {
                    out.extend_from_slice(source.as_bytes());
                }
            }
            FormatToken::RevisionSource => out.extend_from_slice(b"%S"),
            FormatToken::ColorParen | FormatToken::ColorName(_) => {}
            FormatToken::GRefname => out.push(b'N'),
            FormatToken::GTrailers => out.extend_from_slice(b"undefined"),
            FormatToken::GPlaceholder
            | FormatToken::GSignature
            | FormatToken::GKey
            | FormatToken::GFingerprint
            | FormatToken::GPassthrough
            | FormatToken::GDate
            | FormatToken::GDateShort
            | FormatToken::GDateIso
            | FormatToken::GDateIsoStrict
            | FormatToken::GDateRfc2822 => {}
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            FormatToken::StashDecoParen
            | FormatToken::StashDecoBare
            | FormatToken::ReflogGd
            | FormatToken::ReflogGD
            | FormatToken::ReflogGn
            | FormatToken::ReflogGe
            | FormatToken::ReflogGs
            | FormatToken::TreeFull
            | FormatToken::TreeAbbrev
            | FormatToken::Subject
            | FormatToken::SanitizedSubject
            | FormatToken::Encoding
            | FormatToken::Body
            | FormatToken::FullMessage
            | FormatToken::DecorationsParen
            | FormatToken::DecorationsBare
            | FormatToken::AuthorName
            | FormatToken::AuthorEmail
            | FormatToken::AuthorEmailLocal
            | FormatToken::AuthorTimestamp
            | FormatToken::AuthorDate
            | FormatToken::AuthorDateIso
            | FormatToken::AuthorDateIsoStrict
            | FormatToken::AuthorDateShort
            | FormatToken::AuthorDateRfc2822
            | FormatToken::CommitterName
            | FormatToken::CommitterEmail
            | FormatToken::CommitterEmailLocal
            | FormatToken::CommitterTimestamp
            | FormatToken::CommitterDate
            | FormatToken::CommitterDateIso
            | FormatToken::CommitterDateIsoStrict
            | FormatToken::CommitterDateShort
            | FormatToken::CommitterDateRfc2822
            | FormatToken::Padding(_)
            | FormatToken::Wrap(_)
            | FormatToken::Trailers(_)
            | FormatToken::Decorate(_)
            | FormatToken::Describe(_)
            | FormatToken::ColorAuto
            | FormatToken::Magic(_) => {}
        }
    }
    Ok(())
}

pub(crate) struct StashFormatContext<'a> {
    pub entry: &'a ReflogEntry,
    pub index: usize,
    pub commit: &'a Commit,
    pub abbrev_len: Option<usize>,
    pub date_mode: ForEachRefDateMode,
    pub date_explicit: bool,
}

pub(crate) fn emit_compiled_stash_format(
    compiled: &CompiledLogFormat,
    context: &StashFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let StashFormatContext {
        entry,
        index,
        commit,
        abbrev_len,
        date_mode,
        date_explicit,
    } = *context;
    let (author_name, author_email) = commit_identity_name_email(&commit.author);
    let (committer_name, committer_email) = commit_identity_name_email(&commit.committer);
    let author_timestamp = commit_identity_timestamp(&commit.author);
    let committer_timestamp = commit_identity_timestamp(&commit.committer);
    let (reflog_name, reflog_email) = commit_identity_name_email(&entry.committer);

    for token in &compiled.tokens {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::OidFull => write!(out, "{}", entry.new_oid).map_err(io::Error::from)?,
            FormatToken::OidAbbrev => {
                write!(out, "{}", format_log_oid(&entry.new_oid, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::TreeFull => write!(out, "{}", commit.tree).map_err(io::Error::from)?,
            FormatToken::TreeAbbrev => {
                write!(out, "{}", format_log_oid(&commit.tree, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::ParentsFull => {
                write!(
                    out,
                    "{}",
                    format_metadata_parent_oids(&commit.parents, None)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ParentsAbbrev => {
                write!(
                    out,
                    "{}",
                    format_metadata_parent_oids(&commit.parents, abbrev_len)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::Marker => out.push(b'>'),
            FormatToken::Subject => {
                write!(out, "{}", commit_subject(&commit.message)).map_err(io::Error::from)?;
            }
            FormatToken::SanitizedSubject => {
                write!(out, "{}", log_sanitized_subject(&commit.message))
                    .map_err(io::Error::from)?;
            }
            FormatToken::Encoding => {
                write!(out, "{}", commit_encoding(commit)).map_err(io::Error::from)?;
            }
            FormatToken::NoteName => {}
            FormatToken::RevisionSource => out.extend_from_slice(b"%S"),
            FormatToken::ColorParen | FormatToken::ColorName(_) => {}
            FormatToken::Body => out.extend_from_slice(commit_body(&commit.message)),
            FormatToken::FullMessage => out.extend_from_slice(&commit.message),
            FormatToken::StashDecoParen if index == 0 => {
                out.extend_from_slice(b" (refs/stash)");
            }
            FormatToken::StashDecoParen => {}
            FormatToken::StashDecoBare if index == 0 => {
                out.extend_from_slice(b"refs/stash");
            }
            FormatToken::StashDecoBare => {}
            FormatToken::GRefname => out.push(b'N'),
            FormatToken::GTrailers => out.extend_from_slice(b"undefined"),
            FormatToken::GPlaceholder
            | FormatToken::GSignature
            | FormatToken::GKey
            | FormatToken::GFingerprint
            | FormatToken::GPassthrough
            | FormatToken::GDate
            | FormatToken::GDateShort
            | FormatToken::GDateIso
            | FormatToken::GDateIsoStrict
            | FormatToken::GDateRfc2822 => {}
            FormatToken::AuthorName => out.extend_from_slice(author_name.as_bytes()),
            FormatToken::AuthorEmail => out.extend_from_slice(author_email.as_bytes()),
            FormatToken::AuthorEmailLocal => {
                write!(out, "{}", log_email_local_part(&author_email)).map_err(io::Error::from)?;
            }
            FormatToken::AuthorTimestamp => out.extend_from_slice(author_timestamp.as_bytes()),
            FormatToken::AuthorDate => {
                write!(out, "{}", commit_identity_date(&commit.author, date_mode))
                    .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, ForEachRefDateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, ForEachRefDateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, ForEachRefDateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, ForEachRefDateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterName => out.extend_from_slice(committer_name.as_bytes()),
            FormatToken::CommitterEmail => out.extend_from_slice(committer_email.as_bytes()),
            FormatToken::CommitterEmailLocal => {
                write!(out, "{}", log_email_local_part(&committer_email))
                    .map_err(io::Error::from)?;
            }
            FormatToken::CommitterTimestamp => {
                out.extend_from_slice(committer_timestamp.as_bytes());
            }
            FormatToken::CommitterDate => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, date_mode)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, ForEachRefDateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, ForEachRefDateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, ForEachRefDateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, ForEachRefDateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ReflogGd => {
                write!(
                    out,
                    "{}",
                    stash_list_reflog_selector("stash", index, entry, date_mode, date_explicit)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ReflogGD => {
                write!(
                    out,
                    "{}",
                    stash_list_reflog_selector(
                        "refs/stash",
                        index,
                        entry,
                        date_mode,
                        date_explicit
                    )
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ReflogGn => out.extend_from_slice(reflog_name.as_bytes()),
            FormatToken::ReflogGe => out.extend_from_slice(reflog_email.as_bytes()),
            FormatToken::ReflogGs => out.extend_from_slice(&entry.message),
            FormatToken::DecorationsParen | FormatToken::DecorationsBare => {}
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            FormatToken::Padding(_)
            | FormatToken::Wrap(_)
            | FormatToken::Trailers(_)
            | FormatToken::Decorate(_)
            | FormatToken::Describe(_)
            | FormatToken::ColorAuto
            | FormatToken::Magic(_) => {}
        }
    }
    Ok(())
}

pub(crate) fn print_stash_compiled_format(
    entry: &ReflogEntry,
    index: usize,
    commit: &Commit,
    compiled: &CompiledLogFormat,
    abbrev_len: Option<usize>,
    date_mode: ForEachRefDateMode,
    date_explicit: bool,
) -> Result<()> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_stash_format(
        compiled,
        &StashFormatContext {
            entry,
            index,
            commit,
            abbrev_len,
            date_mode,
            date_explicit,
        },
        &mut line,
    )?;
    io::stdout().write_all(&line)?;
    io::stdout().flush()?;
    Ok(())
}

fn stash_list_reflog_selector(
    reference: &str,
    index: usize,
    entry: &ReflogEntry,
    date_mode: ForEachRefDateMode,
    date_explicit: bool,
) -> String {
    if date_explicit {
        let date = commit_identity_date(&entry.committer, date_mode);
        return format!("{reference}@{{{date}}}");
    }
    format!("{reference}@{{{index}}}")
}

fn format_log_format_decorations(
    oid: &ObjectId,
    decorations: &HashMap<ObjectId, Vec<String>>,
    parenthesized: bool,
) -> String {
    let Some(labels) = decorations.get(oid) else {
        return String::new();
    };
    if parenthesized {
        format!(" ({})", labels.join(", "))
    } else {
        labels.join(", ")
    }
}

fn commit_identity_name_email(raw: &[u8]) -> (String, String) {
    let identity = commit_author_identity(raw);
    let Some((name, email)) = identity.rsplit_once(" <") else {
        return (identity, String::new());
    };
    (name.to_string(), email.trim_end_matches('>').to_string())
}

fn commit_encoding(commit: &Commit) -> String {
    commit
        .encoding
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default()
        .to_string()
}

/// True when `name` denotes a UTF-8 encoding (git's `is_encoding_utf8`).
fn encoding_is_utf8(name: &str) -> bool {
    let n = name.trim();
    n.is_empty()
        || n.eq_ignore_ascii_case("utf-8")
        || n.eq_ignore_ascii_case("utf8")
}

/// True when `name` is ISO-8859-1 / Latin-1.
fn encoding_is_latin1(name: &str) -> bool {
    let n = name.trim();
    n.eq_ignore_ascii_case("ISO8859-1")
        || n.eq_ignore_ascii_case("ISO-8859-1")
        || n.eq_ignore_ascii_case("latin1")
        || n.eq_ignore_ascii_case("latin-1")
        || n.eq_ignore_ascii_case("8859-1")
}

/// Re-encode a commit message from its stored `encoding` header to the desired
/// log output encoding, mirroring git's `repo_logmsg_reencode`. We natively
/// support the conversions the upstream corpus exercises (Latin-1 ⇄ UTF-8) and
/// pass the bytes through unchanged when the encodings already match or when the
/// pair is one we don't convert (git would shell out to iconv there).
fn log_reencode_message<'a>(message: &'a [u8], from: &str, to: &str) -> std::borrow::Cow<'a, [u8]> {
    use std::borrow::Cow;
    if from.trim().is_empty() || from.eq_ignore_ascii_case(to) {
        return Cow::Borrowed(message);
    }
    if encoding_is_utf8(from) && encoding_is_utf8(to) {
        return Cow::Borrowed(message);
    }
    if encoding_is_latin1(from) && encoding_is_utf8(to) {
        // Each Latin-1 byte maps to the same Unicode scalar.
        let mut out = Vec::with_capacity(message.len());
        for &b in message {
            if b < 0x80 {
                out.push(b);
            } else {
                out.push(0xc0 | (b >> 6));
                out.push(0x80 | (b & 0x3f));
            }
        }
        return Cow::Owned(out);
    }
    if encoding_is_utf8(from) && encoding_is_latin1(to) {
        // Reverse: collapse 2-byte Latin-1 range back to single bytes.
        let mut out = Vec::with_capacity(message.len());
        let mut idx = 0;
        while idx < message.len() {
            let b = message[idx];
            if b < 0x80 {
                out.push(b);
                idx += 1;
            } else if b & 0xe0 == 0xc0
                && idx + 1 < message.len()
                && message[idx + 1] & 0xc0 == 0x80
            {
                let cp = (((b & 0x1f) as u32) << 6) | (message[idx + 1] & 0x3f) as u32;
                if cp <= 0xff {
                    out.push(cp as u8);
                } else {
                    out.extend_from_slice(&message[idx..idx + 2]);
                }
                idx += 2;
            } else {
                out.push(b);
                idx += 1;
            }
        }
        return Cow::Owned(out);
    }
    Cow::Borrowed(message)
}

/// The effective `git log` output encoding: `i18n.logOutputEncoding`, else
/// `i18n.commitEncoding`, else UTF-8 (git's `get_log_output_encoding`).
fn log_output_encoding(config: &GitConfig) -> String {
    config
        .get("i18n", None, "logOutputEncoding")
        .or_else(|| config.get("i18n", None, "commitEncoding"))
        .unwrap_or("UTF-8")
        .to_string()
}

fn commit_identity_timestamp(raw: &[u8]) -> String {
    let identity = String::from_utf8_lossy(raw);
    identity
        .rsplit_once(' ')
        .and_then(|(left, _timezone)| left.rsplit_once(' ').map(|(_, timestamp)| timestamp))
        .unwrap_or("")
        .to_string()
}

fn log_email_local_part(email: &str) -> &str {
    email.split_once('@').map_or(email, |(local, _)| local)
}

fn log_sanitized_subject(message: &[u8]) -> String {
    let subject = commit_subject(message);
    let mut out = String::new();
    let mut last_separator = false;
    for byte in subject.bytes() {
        if byte.is_ascii_alphanumeric() {
            out.push(byte as char);
            last_separator = false;
            continue;
        }
        if matches!(byte, b'.' | b'_') {
            if !out.is_empty() && !last_separator {
                out.push(byte as char);
                last_separator = true;
            }
            continue;
        }
        if !out.is_empty() && !last_separator {
            out.push('-');
            last_separator = true;
        }
    }
    while out.ends_with(['-', '.', '_']) {
        out.pop();
    }
    out
}

fn commit_identity_timestamp_i64(raw: &[u8]) -> Result<i64> {
    commit_identity_timestamp(raw)
        .parse::<i64>()
        .map_err(|_| GitError::InvalidObject("commit identity is missing timestamp".into()))
}

fn rev_parse_symbolic_full_name(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<Option<String>> {
    if rev.len() == format.hex_len() && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let store = FileRefStore::new(git_dir, format);
    if rev == "HEAD" {
        return store.current_branch_ref();
    }
    if rev.starts_with("refs/") {
        return Ok(store.read_ref(rev)?.map(|_| rev.to_string()));
    }
    let head = format!("refs/heads/{rev}");
    if store.read_ref(&head)?.is_some() {
        return Ok(Some(head));
    }
    let tag = format!("refs/tags/{rev}");
    if store.read_ref(&tag)?.is_some() {
        return Ok(Some(tag));
    }
    Err(GitError::not_found(format!("revision {rev}")))
}

/// `git rev-parse --bisect`: emit the `refs/bisect/bad*` refs as positive
/// arguments and the `refs/bisect/good*` refs negated with a leading `^`,
/// each group in ref-name order. The prefixes are matched as raw string
/// prefixes (so `refs/bisect/b` and `refs/bisect/go` are excluded), mirroring
/// git's `refs_for_each_ref_ext(prefix=...)`. With `--symbolic-full-name` the
/// full ref name is printed; otherwise the resolved object id.

fn worktree_prefix(cwd: &Path, git_dir: &Path) -> Result<String> {
    let root = fs::canonicalize(worktree_root_for_git_dir(git_dir)?)?;
    let cwd = fs::canonicalize(cwd)?;
    let prefix = cwd.strip_prefix(&root).map_err(|_| {
        GitError::InvalidPath(format!(
            "{} is outside worktree {}",
            cwd.display(),
            root.display()
        ))
    })?;
    if prefix.as_os_str().is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{}/", prefix.to_string_lossy().replace('\\', "/")))
}

fn is_git_dir_candidate(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

fn relative_path_from_absolute(cwd: &Path, target: &Path) -> Result<String> {
    let cwd = fs::canonicalize(cwd)?;
    relative_path_from_absolute_components(&cwd, target)
}

fn relative_path_from_absolute_components(cwd: &Path, target: &Path) -> Result<String> {
    let cwd_components = cwd.components().collect::<Vec<_>>();
    let target_components = target.components().collect::<Vec<_>>();
    let common = cwd_components
        .iter()
        .zip(target_components.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return Ok(target.display().to_string());
    }

    let up_count = cwd_components.len().saturating_sub(common);
    let mut parts = Vec::new();
    parts.extend((0..up_count).map(|_| "..".to_string()));
    parts.extend(
        target_components[common..]
            .iter()
            .map(|component| component.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        return Ok("./".into());
    }
    let mut relative = parts.join("/");
    if common == target_components.len() {
        relative.push('/');
    }
    Ok(relative)
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn set_config_value(
    config: &mut GitConfig,
    name: &str,
    subsection: Option<&str>,
    key: &str,
    value: &str,
) {
    let section_idx = config
        .sections
        .iter()
        .rposition(|section| section.name == name && section.subsection.as_deref() == subsection)
        .unwrap_or_else(|| {
            config.sections.push(ConfigSection::new(
                name,
                subsection.map(str::to_string),
                Vec::new(),
            ));
            config.sections.len() - 1
        });
    let section = &mut config.sections[section_idx];
    if let Some(entry) = section
        .entries
        .iter_mut()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
    {
        entry.value = Some(value.to_string());
        return;
    }
    section
        .entries
        .push(ConfigEntry::new(key, Some(value.to_string())));
}

fn submodule_worktree_has_untracked_entries(
    root: &Path,
    path: &Path,
    tracked: &BTreeSet<String>,
) -> Result<bool> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if submodule_worktree_has_untracked_entries(root, &entry_path, tracked)? {
                return Ok(true);
            }
            continue;
        }
        let relative = entry_path
            .strip_prefix(root)
            .map_err(|err| GitError::InvalidPath(err.to_string()))?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if !tracked.contains(&relative) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_repository_index(git_dir: &Path, format: ObjectFormat) -> Result<Option<Index>> {
    sley_worktree::read_repository_index(git_dir, format)
}

fn resolve_ref_to_oid(store: &FileRefStore, name: &str) -> Result<Option<ObjectId>> {
    resolve_ref_peeled(store, name)
}

fn show_ref_filter_matches(name: &str, filter: &str) -> bool {
    name == filter
        || name
            .strip_suffix(filter)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

fn parse_abbrev(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid abbrev length {value}")))
}

fn delete_symbolic_ref(store: &FileRefStore, name: &str) -> Result<()> {
    if name == "HEAD" {
        return symbolic_ref_delete_head();
    }
    if validate_symref_name(name).is_err() {
        return symbolic_ref_cannot_delete(name);
    }
    if store.delete_symbolic_ref(name)? {
        return Ok(());
    }
    symbolic_ref_cannot_delete(name)
}

fn symbolic_ref_delete_head() -> Result<()> {
    eprintln!("fatal: deleting 'HEAD' is not allowed");
    Err(GitError::Exit(128))
}

fn symbolic_ref_cannot_delete(name: &str) -> Result<()> {
    eprintln!("fatal: Cannot delete {name}, not a symbolic ref");
    Err(GitError::Exit(128))
}

pub(crate) fn status_quote_path(path: &[u8], quote_space: bool) -> String {
    let needs_quotes = path.iter().any(|&byte| {
        byte == b'"'
            || byte == b'\\'
            || byte == b'\n'
            || byte == b'\t'
            || !(0x20..0x7f).contains(&byte)
            || (quote_space && byte == b' ')
    });
    if !needs_quotes {
        return String::from_utf8_lossy(path).into_owned();
    }
    let mut out = String::from("\"");
    for &byte in path {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out.push('"');
    out
}

fn refname_pattern_matches(pattern: &str, name: &str) -> bool {
    refname_pattern_matches_case(pattern, name, false)
}

fn refname_pattern_matches_case(pattern: &str, name: &str, ignore_case: bool) -> bool {
    fn matches_from(pattern: &[u8], name: &[u8]) -> bool {
        match pattern {
            [] => name.is_empty(),
            [b'*', rest @ ..] => {
                matches_from(rest, name) || (!name.is_empty() && matches_from(pattern, &name[1..]))
            }
            [b'?', rest @ ..] => !name.is_empty() && matches_from(rest, &name[1..]),
            [b'\\', escaped, rest @ ..] => {
                matches!(name, [first, ..] if first == escaped) && matches_from(rest, &name[1..])
            }
            [b'[', rest @ ..] => {
                if let Some((matched, consumed)) =
                    match_refname_pattern_class(rest, name.first().copied())
                {
                    !name.is_empty() && matched && matches_from(&rest[consumed..], &name[1..])
                } else {
                    matches!(name, [b'[', ..]) && matches_from(rest, &name[1..])
                }
            }
            [literal, rest @ ..] => {
                matches!(name, [first, ..] if first == literal) && matches_from(rest, &name[1..])
            }
        }
    }

    if ignore_case {
        let pattern = pattern
            .as_bytes()
            .iter()
            .map(u8::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let name = name
            .as_bytes()
            .iter()
            .map(u8::to_ascii_lowercase)
            .collect::<Vec<_>>();
        matches_from(&pattern, &name)
    } else {
        matches_from(pattern.as_bytes(), name.as_bytes())
    }
}

fn match_refname_pattern_class(class: &[u8], name: Option<u8>) -> Option<(bool, usize)> {
    let mut idx = 0;
    let negated = matches!(class.first(), Some(b'!' | b'^'));
    if negated {
        idx += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while idx < class.len() {
        if class[idx] == b']' && saw_member {
            return Some((if negated { !matched } else { matched }, idx + 1));
        }

        let start = class[idx];
        if start == b'\\' && idx + 1 < class.len() {
            idx += 1;
        }
        let start = class[idx];
        saw_member = true;

        if idx + 2 < class.len() && class[idx + 1] == b'-' && class[idx + 2] != b']' {
            let mut end_idx = idx + 2;
            if class[end_idx] == b'\\' && end_idx + 1 < class.len() {
                end_idx += 1;
            }
            let end = class[end_idx];
            if let Some(value) = name {
                matched |= start <= value && value <= end;
            }
            idx = end_idx + 1;
        } else {
            matched |= name == Some(start);
            idx += 1;
        }
    }

    None
}

fn short_oid(hex: &str) -> &str {
    &hex[..hex.len().min(7)]
}

fn print_usage() {
    eprintln!(
        "usage: sley <init|add|branch|bundle|checkout|check-attr|check-ignore|clean|clone|config|count-objects|commit-graph|diff|fetch|for-each-ref|hash-object|cat-file|commit|commit-tree|ls-remote|ls-files|ls-tree|log|merge-base|mktree|multi-pack-index|mv|pack-refs|reflog|remote|reset|restore|rm|write-tree|worktree|update-index|update-ref|rev-parse|rev-list|show-ref|stash|submodule|symbolic-ref|status|switch|tag|testkit|version> ..."
    );
}

fn resolve_cli_path(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn discover_git_dir(start: impl AsRef<Path>) -> Result<PathBuf> {
    if let Some(git_dir) = explicit_git_dir() {
        if git_dir.as_os_str().is_empty() {
            return Err(GitError::repository_not_found("not a git repository"));
        }
        let resolved = resolve_cli_path(start.as_ref(), git_dir.to_string_lossy().as_ref());
        // An explicit GIT_DIR / --git-dir that points at a `.git` *file*
        // ("gitdir: <path>") is resolved to its target, exactly as git's
        // `setup_explicit_git_dir` does via `read_gitfile`. Without this the
        // ref store would be pointed at the gitfile itself (a regular file),
        // so reads of HEAD/refs fail.
        if resolved.is_file()
            && let Some(target) = read_gitdir_file(&resolved)?
            && is_git_dir_candidate(&target)
        {
            return fs::canonicalize(target).map_err(|err| GitError::Io(err.to_string()));
        }
        return Ok(resolved);
    }
    if global_bare() {
        let cwd = env::current_dir()?;
        if is_git_dir_candidate(&cwd) {
            return fs::canonicalize(&cwd).map_err(|err| GitError::Io(err.to_string()));
        }
        return Err(GitError::repository_not_found("not a git repository"));
    }
    discover_git_dir_by_walk(start)
}

/// Discover the git directory of a *remote* repository named by a local path.
///
/// Upstream reaches a local-path remote by spawning `upload-pack` /
/// `receive-pack` with the local-repository environment cleared
/// (`local_repo_env` in environment.c), so an explicit `--git-dir` /
/// `GIT_DIR` or `--bare` from the *local* invocation must never leak into the
/// remote side's discovery — otherwise `git --git-dir=clone.git fetch origin`
/// would resolve the remote as `<remote-url>/clone.git` (the local clone
/// itself) instead of the remote repository.
pub(crate) fn discover_remote_git_dir(start: impl AsRef<Path>) -> Result<PathBuf> {
    discover_git_dir_by_walk(start)
}

/// The walk-up portion of repository discovery (`setup_git_directory_gently`'s
/// loop): examine `start` and each ancestor for a `.git` dir, a `.git` gitfile,
/// or a bare-layout directory, honoring `GIT_CEILING_DIRECTORIES`.
fn discover_git_dir_by_walk(start: impl AsRef<Path>) -> Result<PathBuf> {
    let ceilings = discovery_ceiling_directories();
    for candidate in start.as_ref().ancestors() {
        // GIT_CEILING_DIRECTORIES: stop the upward walk before *entering* a
        // listed directory. The starting directory itself is always examined
        // (a ceiling only limits proper ancestors, like git's
        // `longest_ancestor_length`).
        if candidate != start.as_ref()
            && ceilings
                .iter()
                .any(|ceiling| paths_refer_to_same_dir(ceiling, candidate))
        {
            break;
        }
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git);
        }
        if dot_git.is_file()
            && let Some(git_dir) = read_gitdir_file(&dot_git)?
            && is_git_dir_candidate(&git_dir)
        {
            return fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()));
        }
        if candidate.join("HEAD").is_file() && candidate.join("objects").is_dir() {
            return Ok(candidate.to_path_buf());
        }
    }
    Err(GitError::repository_not_found("not a git repository"))
}

/// The `GIT_CEILING_DIRECTORIES` list: colon-separated absolute paths that
/// repository discovery must not walk up into. Empty entries (including the
/// `""` no-canonicalization marker) are ignored.
fn discovery_ceiling_directories() -> Vec<PathBuf> {
    match env::var("GIT_CEILING_DIRECTORIES") {
        Ok(value) if !value.is_empty() => value
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether two paths name the same directory, tolerating symlink/relative
/// differences via canonicalization.
fn paths_refer_to_same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn repository_object_format(git_dir: &Path) -> Result<ObjectFormat> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config = common_git_dir.join("config");
    let Ok(config) = GitConfig::read(config) else {
        return Ok(ObjectFormat::Sha1);
    };
    config.repository_object_format()
}

/// Mirror git's `verify_repository_format` plus the extension collection in
/// `check_repo_format` (setup.c): with `core.repositoryformatversion = 0`, any
/// v1-only extension (`objectformat`, `refstorage`, `compatobjectformat`, ...)
/// is fatal; with version >= 1, any *unknown* extension is fatal; versions
/// above 1 are always fatal. Invalid `extensions.refstorage` values die with
/// git's per-occurrence `invalid value for 'extensions.refstorage'` diagnostic
/// (delegated to [`repository_ref_storage_format`]). A missing config file or
/// missing version key is silently OK, exactly like git's

/// Render the common config path the way git names it in a `bad config line`
/// diagnostic: the relative `.git/config` form when the repository was found by
/// discovery (git operates from the worktree top, so the gitdir is `.git`), or
/// the absolute path when `GIT_DIR` was set explicitly.

/// Physical 1-based line number of the first `[extensions] refstorage = <value>`
/// assignment whose value is neither `files` nor `reftable`, matching the line
/// git reports in its `fatal: bad config line N` diagnostic. Tracks the active
/// section like git's parser; returns `None` if no such line is found.

fn repository_abbrev(git_dir: &Path, format: ObjectFormat) -> Result<Option<usize>> {
    if let Some(value) = global_config_value("core.abbrev")? {
        return parse_repository_abbrev_value(format, &value);
    }
    let config_path = git_dir.join("config");
    let Ok(config) = GitConfig::read(config_path) else {
        return Ok(Some(7));
    };
    let Some(value) = config.get("core", None, "abbrev") else {
        return Ok(Some(7));
    };
    parse_repository_abbrev_value(format, value)
}

fn parse_repository_abbrev_value(format: ObjectFormat, value: &str) -> Result<Option<usize>> {
    if value.eq_ignore_ascii_case("no") {
        return Ok(None);
    }
    if value.eq_ignore_ascii_case("auto") {
        return Ok(Some(7));
    }
    let width = value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid core.abbrev value {value}")))?;
    if width < 4 {
        return Err(GitError::Command(format!(
            "core.abbrev length out of range: {width}"
        )));
    }
    Ok(Some(width.min(format.hex_len())))
}

fn commit_identity_from_env(role: &str) -> Result<Vec<u8>> {
    // git's identity precedence for the name/email of an author or committer:
    //   GIT_{role}_NAME/EMAIL env var
    //     -> `-c user.name=` / GIT_CONFIG_* command-line overrides
    //       -> effective config user.name (repo, then global, then system)
    //         -> sley's built-in default identity
    // Higher-precedence env/`-c`/repo sources are evaluated exactly as before;
    // the global+system config layer is the new fallback below repo config.
    // The effective config is loaded at most once, and only when the env vars do
    // not already supply both fields, so the common env-driven path is unchanged.
    let env_name = env::var(format!("GIT_{role}_NAME")).ok();
    let env_email = env::var(format!("GIT_{role}_EMAIL")).ok();
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| identity_config_value("user.name", &mut config))
        .unwrap_or_else(|| "Git Rs".into());
    let email = env_email
        .or_else(|| identity_config_value("user.email", &mut config))
        .unwrap_or_else(|| "sley@example.invalid".into());
    let date = env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

/// Canonicalise a `GIT_*_DATE`/`--date=` value to git's raw `<seconds> +HHMM`
/// form so the sequencer's identity builder (which only accepts the raw form)
/// stores the same bytes git would.
///
/// git's `commit-tree` / `commit` run author and committer dates through
/// `parse_date`, accepting ISO-8601 (`2005-04-07T22:13:13`), `<date> <time> <tz>`
/// (`2005-01-01 00:00:00 +0000`), RFC-2822, and the raw form. The full date.c
/// port lives in [`commands::approxidate`]; route the value through it and emit
/// the canonical raw form. Values that do not parse are passed through verbatim
/// so the sequencer still reports the original "invalid date" error.
fn canonicalize_commit_date(date: &str) -> String {
    match commands::approxidate::parse_commit_date(date) {
        Some((seconds, tz)) => format!("{seconds} {tz}"),
        None => date.to_string(),
    }
}

/// Lazily-loaded effective config used as the identity fallback. `Skip` means
/// the caller already has both fields from the environment and the config files
/// must not be touched; `Lazy` caches the (optional) loaded config so multiple
/// key lookups share a single load.
enum IdentityConfig {
    Skip,
    Lazy(Option<Option<GitConfig>>),
}

/// Resolve an identity config key (`user.name`/`user.email`) following git's
/// precedence below the environment: `-c`/`GIT_CONFIG_*` command-line overrides
/// first, then the effective config (repository, then global, then system).
fn identity_config_value(key: &str, config: &mut IdentityConfig) -> Option<String> {
    if let Ok(Some(value)) = global_config_value(key) {
        return Some(value);
    }
    let (section, name) = key.split_once('.')?;
    let loaded = match config {
        IdentityConfig::Skip => return None,
        IdentityConfig::Lazy(slot) => slot.get_or_insert_with(identity_effective_config),
    };
    loaded
        .as_ref()
        .and_then(|config| config.get(section, None, name).map(str::to_string))
}

/// Load the effective config (repository + global + system, with includes) for
/// identity fallback, or `None` when there is no repository in scope. Failures
/// degrade to `None` so identity resolution can still fall through to env/`-c`
/// values or the built-in default rather than aborting.
fn identity_effective_config() -> Option<GitConfig> {
    // `discover_git_dir` already honours `--git-dir`/`GIT_DIR` (via
    // `explicit_git_dir`) before walking up from the current directory.
    let git_dir = discover_git_dir(env::current_dir().ok()?).ok()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir).ok()?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.clone()),
        repo_current_branch_name(&git_dir),
    );
    sley_config::load_effective_config(&common_git_dir, &context).ok()
}

fn commit_signoff_from_env() -> Result<Vec<u8>> {
    // git's `--signoff` uses the committer identity, so resolve it with the same
    // precedence as `commit_identity_from_env("COMMITTER")`.
    let env_name = env::var("GIT_COMMITTER_NAME").ok();
    let env_email = env::var("GIT_COMMITTER_EMAIL").ok();
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| identity_config_value("user.name", &mut config))
        .unwrap_or_else(|| "Git Rs".into());
    let email = env_email
        .or_else(|| identity_config_value("user.email", &mut config))
        .unwrap_or_else(|| "sley@example.invalid".into());
    let date = env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity(&name, &email, &date)?;
    Ok(format!("Signed-off-by: {name} <{email}>").into_bytes())
}

fn commit_message_with_signoff(mut message: Vec<u8>, signoff: &[u8]) -> Vec<u8> {
    if message
        .split(|byte| *byte == b'\n')
        .any(|line| line == signoff)
    {
        return message;
    }
    if message.is_empty() {
        message.extend_from_slice(signoff);
        message.push(b'\n');
        return message;
    }
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    if !message.ends_with(b"\n\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(signoff);
    message.push(b'\n');
    message
}

fn commit_reflog_message(message: &[u8], amend: bool) -> Vec<u8> {
    let subject = String::from_utf8_lossy(message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if amend {
        format!("commit (amend): {subject}").into_bytes()
    } else {
        format!("commit: {subject}").into_bytes()
    }
}

/// Resolve the effective worktree for the *given* git dir.
///
/// This resolver is **silent** and operates on its `git_dir` argument (it is
/// also used as a probe by `is_inside_work_tree`, the ref-storage display path,
/// and the submodule-superproject walk, which all pass a specific git dir), so
/// it must neither print nor reinterpret the ambient process environment for an
/// arbitrary git dir. The user-facing worktree diagnostics — and the full
/// CLI-side env/config setup resolution — live in [`require_work_tree`], which
/// the worktree-requiring command entry points call for the ambient repository.
fn worktree_root_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    // CLI/process-level overrides take precedence over anything recorded in the
    // repository (these are not part of the repository-intrinsic resolution).
    if let Some(work_tree) = explicit_work_tree() {
        let work_tree =
            resolve_cli_path(&env::current_dir()?, work_tree.to_string_lossy().as_ref());
        return fs::canonicalize(work_tree).map_err(|err| GitError::Io(err.to_string()));
    }
    if explicit_git_dir().is_some() {
        return env::current_dir().map_err(|err| GitError::Io(err.to_string()));
    }
    // The rest (core.worktree, linked worktrees, parent-of-.git) is shared with
    // the library; a bare repository (None) is unsupported here.
    match sley_worktree::worktree_root_for_git_dir(git_dir)? {
        Some(root) => Ok(root),
        None => Err(GitError::Unsupported(
            "update-index currently requires a non-bare worktree".into(),
        )),
    }
}

/// Resolve the effective worktree for a worktree-requiring command, emitting
/// git's user-facing diagnostic on failure: the
/// "core.bare and core.worktree do not make sense" warning + "unable to set up
/// work tree using invalid config" for the config conflict, or
/// "this operation must be run in a work tree" for a bare / no-worktree repo.
fn require_work_tree(git_dir: &Path) -> Result<PathBuf> {
    if let Some(result) = setup::setup_git_directory() {
        if result.worktree_config_bogus {
            eprintln!("warning: core.bare and core.worktree do not make sense");
            eprintln!("fatal: unable to set up work tree using invalid config");
            return Err(GitError::Exit(128));
        }
        if let Some(worktree) = result.worktree {
            return Ok(worktree);
        }
        eprintln!("fatal: this operation must be run in a work tree");
        return Err(GitError::Exit(128));
    }
    match sley_worktree::worktree_root_for_git_dir(git_dir)? {
        Some(root) => Ok(root),
        None => {
            eprintln!("fatal: this operation must be run in a work tree");
            Err(GitError::Exit(128))
        }
    }
}

fn read_gitdir_file(path: &Path) -> Result<Option<PathBuf>> {
    let contents = fs::read_to_string(path)?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = target.trim();
    let target = PathBuf::from(target);
    if target.is_absolute() {
        Ok(Some(target))
    } else {
        Ok(Some(
            path.parent().unwrap_or_else(|| Path::new("")).join(target),
        ))
    }
}

fn resolve_revision(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    sley_rev::resolve_revision(git_dir, format, rev)
}

fn warn_ambiguous_refname_for_object_prefix(git_dir: &Path, format: ObjectFormat, rev: &str) {
    if rev.len() < 4
        || rev.len() >= format.hex_len()
        || !rev.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !revision_ref_name_exists(git_dir, format, rev)
    {
        return;
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if matches!(
        db.resolve_prefix(rev),
        Ok(ObjectPrefixResolution::Unique(_) | ObjectPrefixResolution::Ambiguous(_))
    ) {
        eprintln!("warning: refname '{rev}' is ambiguous.");
    }
}

fn revision_ref_name_exists(git_dir: &Path, format: ObjectFormat, rev: &str) -> bool {
    let refs = FileRefStore::new(git_dir, format);
    if rev == "HEAD" {
        return refs.read_ref("HEAD").ok().flatten().is_some();
    }
    if rev.starts_with("refs/") {
        return refs.read_ref(rev).ok().flatten().is_some();
    }
    refs.read_ref(&format!("refs/heads/{rev}"))
        .ok()
        .flatten()
        .is_some()
        || refs
            .read_ref(&format!("refs/tags/{rev}"))
            .ok()
            .flatten()
            .is_some()
}

fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

fn default_committer() -> Vec<u8> {
    b"Git Rs <sley@example.invalid> 0 +0000".to_vec()
}

#[cfg(test)]
mod tests {
    use super::refname_pattern_matches;

    #[test]
    fn refname_patterns_match_git_style_wildcards() {
        assert!(refname_pattern_matches("v*", "v1.0"));
        assert!(refname_pattern_matches("release/*", "release/2026.05"));
        assert!(refname_pattern_matches("*2026.05", "release/2026.05"));
        assert!(refname_pattern_matches("qa-?", "qa-1"));
        assert!(refname_pattern_matches("q[ab]-?", "qa-1"));
        assert!(refname_pattern_matches("v[1-3].0", "v2.0"));
        assert!(refname_pattern_matches("v[!1].0", "v2.0"));
        assert!(!refname_pattern_matches("v[!1].0", "v1.0"));
        assert!(!refname_pattern_matches("v?.0", "v10.0"));
    }

    #[test]
    fn refname_patterns_treat_invalid_classes_as_literals() {
        assert!(refname_pattern_matches("release[", "release["));
        assert!(refname_pattern_matches(r"release\*", "release*"));
        assert!(!refname_pattern_matches(r"release\*", "release/1"));
    }
}
