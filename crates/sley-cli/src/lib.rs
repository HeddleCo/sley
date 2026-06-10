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

mod commands;
mod log_format;
mod remote;
mod repo_path;
mod repository;

pub(crate) use log_format::{CompiledLogFormat, FormatToken, LogFormatDialect, presets};

pub(crate) use commands::args::{GitArgCursor, long_option_value};
pub(crate) use commands::cat_file::{cat_file_all_object_ids, cat_file_object_storage};
pub(crate) use commands::config_cmd::{config_entry_name, has_unescaped_trailing_dollar};
pub(crate) use commands::merge_rebase::{
    MergePathResult, MergeTreeMap, commit_tree_oid, conclude_in_progress_merge,
    conclude_rebase_step_via_commit, head_commit_oid, merge_bases, merge_index_entry,
    merge_is_regular_file, merge_read_blob, merge_remove_worktree_file, merge_write_worktree_file,
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
        "init" => cmd_init(&args[1..], global_config),
        "add" => cmd_add(&args[1..]),
        "archive" => cmd_archive(&args[1..]),
        "branch" => commands::branch::cmd_branch(&args[1..]),
        "bundle" => cmd_bundle(&args[1..]),
        "hash-object" => commands::hash_object::cmd_hash_object(&args[1..]),
        "index-pack" => commands::pack::cmd_index_pack(&args[1..]),
        "pack-objects" => commands::pack_objects::cmd_pack_objects(&args[1..]),
        "cat-file" => commands::cat_file::cmd_cat_file(&args[1..]),
        "checkout" => commands::workspace::cmd_checkout(&args[1..]),
        "check-attr" => commands::attrs::cmd_check_attr(&args[1..]),
        "check-ignore" => commands::attrs::cmd_check_ignore(&args[1..]),
        "check-mailmap" => commands::utility::cmd_check_mailmap(&args[1..]),
        "check-ref-format" => commands::utility::cmd_check_ref_format(&args[1..]),
        "clean" => cmd_clean(&args[1..]),
        "clone" => commands::remote_cmds::cmd_clone(&args[1..]),
        "config" => commands::config_cmd::cmd_config(&args[1..]),
        "count-objects" => commands::pack::cmd_count_objects(&args[1..]),
        "gc" => commands::pack::cmd_gc(&args[1..]),
        "maintenance" => commands::pack::cmd_maintenance(&args[1..]),
        "repack" => commands::pack::cmd_repack(&args[1..]),
        "apply" => cmd_apply(&args[1..]),
        "commit" => commands::workspace::cmd_commit(&args[1..]),
        "commit-graph" => cmd_commit_graph(&args[1..]),
        "commit-tree" => cmd_commit_tree(&args[1..]),
        "diff" => commands::diff::cmd_diff(&args[1..]),
        "fetch" => commands::remote_cmds::cmd_fetch(&args[1..]),
        "for-each-ref" => commands::for_each_ref::cmd_for_each_ref(&args[1..]),
        "fsck" => cmd_fsck(&args[1..]),
        "get-tar-commit-id" => commands::utility::cmd_get_tar_commit_id(&args[1..]),
        "ls-remote" => commands::remote_cmds::cmd_ls_remote(&args[1..]),
        "ls-files" => commands::index::cmd_ls_files(&args[1..]),
        "ls-tree" => commands::index::cmd_ls_tree(&args[1..]),
        "log" => commands::log::cmd_log(&args[1..]),
        "merge" => commands::merge_rebase::cmd_merge(&args[1..]),
        "merge-base" => commands::merge_rebase::cmd_merge_base(&args[1..]),
        "pull" => commands::merge_rebase::cmd_pull(&args[1..]),
        "rebase" => commands::merge_rebase::cmd_rebase(&args[1..]),
        "cherry-pick" => commands::merge_rebase::cmd_cherry_pick(&args[1..]),
        "revert" => commands::merge_rebase::cmd_revert(&args[1..]),
        "mktree" => commands::index::cmd_mktree(&args[1..]),
        "multi-pack-index" => commands::pack::cmd_multi_pack_index(&args[1..]),
        "mv" => cmd_mv(&args[1..]),
        "pack-refs" => commands::pack::cmd_pack_refs(&args[1..]),
        "prune" => commands::pack::cmd_prune(&args[1..]),
        "prune-packed" => cmd_prune_packed(&args[1..]),
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
        "replace" => cmd_replace(&args[1..]),
        "rerere" => cmd_rerere(&args[1..]),
        "reset" => commands::workspace::cmd_reset(&args[1..]),
        "restore" => commands::workspace::cmd_restore(&args[1..]),
        "rm" => cmd_rm(&args[1..]),
        "show-ref" => commands::refs::cmd_show_ref(&args[1..]),
        "show-index" => commands::utility::cmd_show_index(&args[1..]),
        "stripspace" => commands::utility::cmd_stripspace(&args[1..]),
        "stash" => commands::stash::cmd_stash(&args[1..]),
        "submodule" => cmd_submodule(&args[1..]),
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

fn cmd_archive(args: &[String]) -> Result<()> {
    let mut format_name = "tar";
    let mut prefix = Vec::new();
    let mut output = None;
    let mut treeish = None;
    let mut pathspecs = Vec::new();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            if treeish.is_none() {
                treeish = Some(arg.as_str());
            } else {
                pathspecs.push(arg.as_bytes().to_vec());
            }
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--format" => {
                format_name = iter
                    .next()
                    .map(String::as_str)
                    .ok_or_else(|| GitError::Command("archive --format requires a value".into()))?;
            }
            "--prefix" => {
                prefix = iter
                    .next()
                    .ok_or_else(|| GitError::Command("archive --prefix requires a value".into()))?
                    .as_bytes()
                    .to_vec();
            }
            "-o" | "--output" => {
                output = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("archive --output requires a value".into())
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--format=") => {
                format_name = &value["--format=".len()..];
            }
            value if value.starts_with("--prefix=") => {
                prefix = value.as_bytes()["--prefix=".len()..].to_vec();
            }
            value if value.starts_with("--output=") => {
                output = Some(value["--output=".len()..].to_string());
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported archive option {value}"
                )));
            }
            value => {
                if treeish.is_none() {
                    treeish = Some(value);
                } else {
                    pathspecs.push(value.as_bytes().to_vec());
                }
            }
        }
    }
    if format_name != "tar" {
        return Err(GitError::Command(format!(
            "archive currently supports --format=tar, not {format_name}"
        )));
    }
    let treeish = treeish.ok_or_else(|| GitError::Command("archive requires a tree-ish".into()))?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let current_prefix = worktree_prefix(&cwd, &git_dir)?.into_bytes();
    let pathspecs = archive_pathspecs_for_current_prefix(&current_prefix, pathspecs);
    let oid = resolve_revision(&git_dir, format, treeish)?;
    let object = db.read_object(&oid)?;
    let (tree_oid, mtime, commit_id) = match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body)?;
            let mtime = commit_graph_commit_time_from_committer(commit.committer)?;
            (commit.tree, mtime, Some(oid))
        }
        ObjectType::Tree => (oid, current_unix_seconds().max(0) as u64, None),
        ObjectType::Tag => {
            let tree_oid = sley_rev::peel_to_tree(&db, format, &oid)?;
            (tree_oid, current_unix_seconds().max(0) as u64, None)
        }
        other => {
            return Err(GitError::InvalidObject(format!(
                "expected tree-ish {oid}, found {}",
                other.as_str()
            )));
        }
    };
    let options = sley_archive::TarArchiveOptions {
        prefix,
        strip_prefix: current_prefix,
        mtime,
        commit_id,
        pathspecs,
    };
    if let Some(path) = output {
        let mut file = fs::File::create(path)?;
        handle_archive_result(sley_archive::write_tar_archive(
            &mut file, &db, format, &tree_oid, options,
        ))
    } else {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        handle_archive_result(sley_archive::write_tar_archive(
            &mut lock, &db, format, &tree_oid, options,
        ))?;
        lock.flush()?;
        Ok(())
    }
}

fn handle_archive_result(result: Result<()>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(GitError::InvalidPath(message)) if message.starts_with("pathspec ") => {
            eprintln!("fatal: {message}");
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn archive_pathspecs_for_current_prefix(
    current_prefix: &[u8],
    pathspecs: Vec<Vec<u8>>,
) -> Vec<Vec<u8>> {
    if current_prefix.is_empty() {
        return pathspecs;
    }
    if pathspecs.is_empty() {
        return vec![
            current_prefix
                .strip_suffix(b"/")
                .unwrap_or(current_prefix)
                .to_vec(),
        ];
    }
    pathspecs
        .into_iter()
        .map(|pathspec| {
            let pathspec = pathspec.strip_prefix(b"./").unwrap_or(&pathspec);
            let mut full = Vec::with_capacity(current_prefix.len() + pathspec.len());
            full.extend_from_slice(current_prefix);
            full.extend_from_slice(pathspec);
            full
        })
        .collect()
}

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
fn effective_config_parameters_env() -> Option<String> {
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
fn injected_config_parameters() -> Result<Vec<sley_config::ConfigParameter>> {
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
fn init_repo_is_implicitly_bare(cwd: &Path) -> Result<bool> {
    // Determine the effective git directory git would inspect.
    if let Some(git_dir) = environment_git_dir() {
        return Ok(guess_repository_type(&git_dir, cwd));
    }
    // No GIT_DIR: git_dir defaults to ".git". Only a linked-worktree gitfile (whose
    // target has a `commondir`) redirects the inspection to the common repository;
    // a plain separate-git-dir gitfile does not.
    let dot_git = cwd.join(".git");
    if dot_git.is_file()
        && let Some(target) = read_gitdir_file(&dot_git)?
        && target.join("commondir").is_file()
    {
        let common = common_git_dir_for_git_dir(&target)?;
        return Ok(guess_repository_type(&common, cwd));
    }
    // Otherwise git_dir is ".git", which guess_repository_type treats as non-bare.
    Ok(false)
}

/// Mirror of git's `guess_repository_type()` (builtin/init-db.c): decide whether a
/// git directory path implies a bare repository.
fn guess_repository_type(git_dir: &Path, cwd: &Path) -> bool {
    // "GIT_DIR=. git init" — and "GIT_DIR=$(pwd) git init" — are always bare.
    if git_dir == Path::new(".") {
        return true;
    }
    if git_dir == cwd {
        return true;
    }
    // "GIT_DIR=.git" or "GIT_DIR=something/.git" is usually NOT bare.
    if git_dir == Path::new(".git") {
        return false;
    }
    if git_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".git")
    {
        return false;
    }
    // Otherwise it is often bare. At this point git is just guessing.
    true
}

fn cmd_init(args: &[String], global_config: &[GlobalConfigOverride]) -> Result<()> {
    let mut bare = global_bare();
    // git distinguishes an *explicitly requested* bare repo (`--bare`/global
    // `--bare`) from one merely *guessed* from the environment. The former pairs
    // with `--separate-git-dir` as "cannot be used together"; the latter as
    // "incompatible with bare repository". Track the explicit signal separately
    // from the `.git`-suffix path heuristic applied further down.
    let mut bare_explicit = global_bare();
    let mut object_format = None::<String>;
    let mut ref_format = None::<Option<String>>;
    let mut initial_branch = None::<String>;
    let mut initial_branch_explicit = false;
    let mut quiet = false;
    let mut path = PathBuf::from(".");
    let mut path_given = false;
    let mut template = None::<Option<String>>;
    let mut template_config = true;
    let mut separate_git_dir = None::<String>;
    let mut shared_repository = None::<Option<String>>;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--bare" => {
                bare = true;
                bare_explicit = true;
            }
            "-q" | "--quiet" => quiet = true,
            "-s" | "--shared" => shared_repository = Some(Some("group".into())),
            "--no-shared" => shared_repository = Some(None),
            "-b" | "--initial-branch" => {
                initial_branch = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .to_string(),
                );
                initial_branch_explicit = true;
            }
            "--object-format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-format requires a value".into()))?;
                object_format = Some(value.to_string());
            }
            "--template" => {
                template = Some(Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--template requires a value".into()))?
                        .to_string(),
                ));
                template_config = true;
            }
            "--no-template" => {
                template = Some(None);
                template_config = false;
            }
            "--separate-git-dir" => {
                separate_git_dir = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("--separate-git-dir requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--no-separate-git-dir" => separate_git_dir = None,
            "--ref-format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--ref-format requires a value".into()))?;
                ref_format = Some(Some(value.to_string()));
            }
            "--no-ref-format" => ref_format = Some(None),
            value if value.starts_with("--initial-branch=") => {
                initial_branch = Some(
                    value
                        .strip_prefix("--initial-branch=")
                        .ok_or_else(|| {
                            GitError::Command("--initial-branch requires a value".into())
                        })?
                        .to_string(),
                );
                initial_branch_explicit = true;
            }
            value if value.starts_with("--object-format=") => {
                let value = value
                    .strip_prefix("--object-format=")
                    .ok_or_else(|| GitError::Command("--object-format requires a value".into()))?;
                object_format = Some(value.to_string());
            }
            value if value.starts_with("--template=") => {
                template = Some(Some(
                    value
                        .strip_prefix("--template=")
                        .ok_or_else(|| GitError::Command("--template requires a value".into()))?
                        .to_string(),
                ));
                template_config = true;
            }
            value if value.starts_with("--separate-git-dir=") => {
                separate_git_dir = Some(
                    value
                        .strip_prefix("--separate-git-dir=")
                        .ok_or_else(|| {
                            GitError::Command("--separate-git-dir requires a value".into())
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--shared=") => {
                shared_repository = Some(Some(
                    value
                        .strip_prefix("--shared=")
                        .ok_or_else(|| GitError::Command("--shared requires a value".into()))?
                        .to_string(),
                ));
            }
            value if value.starts_with("--ref-format=") => {
                ref_format = Some(Some(
                    value
                        .strip_prefix("--ref-format=")
                        .ok_or_else(|| GitError::Command("--ref-format requires a value".into()))?
                        .to_string(),
                ));
            }
            value => {
                path = PathBuf::from(value);
                path_given = true;
            }
        }
    }

    if !bare
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".git"))
    {
        bare = true;
    }

    // Mirror refs.c `repo_default_branch_name`: an explicit `--initial-branch`
    // wins; otherwise `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME` (when non-empty),
    // then `init.defaultBranch`, then "master" (which triggers the
    // `advice.defaultBranchName` hint, emitted after a successful fresh init).
    // A name sourced from the env/config default dies with git's
    // `invalid branch name: init.defaultBranch = <name>`; an explicit
    // `--initial-branch` dies with `invalid initial branch name: '<name>'`
    // (init-db.c).
    let mut branch_defaulted = false;
    let initial_branch = match initial_branch {
        Some(branch) => {
            if check_refname_format(&format!("refs/heads/{branch}"), false).is_err() {
                eprintln!("fatal: invalid initial branch name: '{branch}'");
                return Err(GitError::Exit(128));
            }
            branch
        }
        None => {
            let default_name = env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME")
                .ok()
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || init_config_value("init.defaultBranch", global_config),
                    |name| Ok(Some(name)),
                )?
                .filter(|value| !value.is_empty());
            match default_name {
                Some(name) => {
                    if check_refname_format(&format!("refs/heads/{name}"), false).is_err() {
                        eprintln!("fatal: invalid branch name: init.defaultBranch = {name}");
                        return Err(GitError::Exit(128));
                    }
                    name
                }
                None => {
                    branch_defaulted = true;
                    "master".to_string()
                }
            }
        }
    };

    let cwd = env::current_dir()?;
    let worktree = resolve_cli_path(&cwd, path.to_string_lossy().as_ref());
    let separate_git_dir = separate_git_dir.map(|value| resolve_cli_path(&cwd, &value));

    if separate_git_dir.is_some() {
        if bare_explicit {
            // init-db.c: `real_git_dir && is_bare_repository_cfg == 1` where the
            // `1` came from the `--bare` option.
            eprintln!("fatal: options '--bare' and '--separate-git-dir' cannot be used together");
            return Err(GitError::Exit(128));
        }
        // init-db.c later sets `is_bare_repository_cfg = guess_repository_type(git_dir)`
        // when bare was not explicit, then rejects `--separate-git-dir` against an
        // implicitly-bare repository (e.g. `GIT_DIR=.`, or inside a linked worktree
        // whose common repository is bare).
        if init_repo_is_implicitly_bare(&cwd)? {
            eprintln!("fatal: --separate-git-dir incompatible with bare repository");
            return Err(GitError::Exit(128));
        }
    }

    // init-db.c: GIT_WORK_TREE (or --work-tree) only makes sense together with
    // GIT_DIR and without an explicit `--bare`. After chdir'ing into the target
    // directory, `--bare` pins GIT_DIR to that directory (overwriting the
    // environment when a directory argument was given); the effective git dir
    // then comes from GIT_DIR and its *string* form drives the bare guess.
    let env_git_dir = explicit_git_dir();
    let env_work_tree = explicit_work_tree();
    if env_work_tree.is_some() && (bare_explicit || env_git_dir.is_none()) {
        eprintln!(
            "fatal: GIT_WORK_TREE (or --work-tree=<directory>) not allowed without specifying GIT_DIR (or --git-dir=<directory>)"
        );
        return Err(GitError::Exit(128));
    }

    let mut worktree = worktree;
    let mut git_dir_override = None::<PathBuf>;
    let mut core_worktree = None::<String>;
    // Re-initializing from *inside* a linked worktree operates on the shared
    // repository: git's setup discovers the common git dir and the *main*
    // worktree, so `init --separate-git-dir` there relocates the common dir and
    // repoints the main worktree's `.git` (init-db.c works on the discovered
    // repository, not the linked-worktree admin dir). Redirect `worktree` to the
    // main worktree root before bootstrap so `.git` resolves to the common dir.
    if !bare && env_git_dir.is_none() && env_work_tree.is_none() {
        let dot_git = worktree.join(".git");
        if dot_git.is_file()
            && let Some(admin_dir) = read_gitdir_file(&dot_git)?
            && admin_dir.join("commondir").is_file()
        {
            let common = common_git_dir_for_git_dir(&admin_dir)?;
            if let Some(main_root) = common.parent() {
                worktree = main_root.to_path_buf();
            }
        }
    }
    if bare_explicit {
        // `--bare` without a directory argument leaves an existing GIT_DIR in
        // charge of where the (bare) repository lives.
        if !path_given && let Some(raw) = env_git_dir.clone() {
            git_dir_override = Some(resolve_cli_path(&worktree, raw.to_string_lossy().as_ref()));
        }
    } else if let Some(raw) = env_git_dir.clone()
        && separate_git_dir.is_none()
        && !bare
    {
        let git_dir_abs = resolve_cli_path(&worktree, raw.to_string_lossy().as_ref());
        if guess_repository_type(&raw, &worktree) {
            match env_work_tree.clone() {
                // Guessed-bare git dir + GIT_WORK_TREE: the repository is
                // *non*-bare after all; record `core.worktree` (init-db.c sets
                // the work tree, so `create_default_files` writes it).
                Some(raw_work_tree) => {
                    let work_tree_abs =
                        resolve_cli_path(&worktree, raw_work_tree.to_string_lossy().as_ref());
                    let work_tree_abs =
                        fs::canonicalize(&work_tree_abs).unwrap_or(work_tree_abs);
                    if git_dir_abs != work_tree_abs.join(".git") {
                        core_worktree = Some(work_tree_abs.to_string_lossy().into_owned());
                    }
                    git_dir_override = Some(git_dir_abs);
                    worktree = work_tree_abs;
                }
                // Plain guessed-bare GIT_DIR (e.g. `GIT_DIR=dir.git git init`):
                // a bare repository at that directory.
                None => {
                    git_dir_override = Some(git_dir_abs);
                    bare = true;
                }
            }
        } else {
            // Non-bare guess (".git" or "…/.git"): the work tree is the git
            // dir's parent (or the target directory), unless GIT_WORK_TREE
            // overrides it.
            let work_tree_abs = match env_work_tree.clone() {
                Some(raw_work_tree) => {
                    let resolved =
                        resolve_cli_path(&worktree, raw_work_tree.to_string_lossy().as_ref());
                    fs::canonicalize(&resolved).unwrap_or(resolved)
                }
                None => git_dir_abs
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| worktree.clone()),
            };
            if git_dir_abs != work_tree_abs.join(".git") {
                core_worktree = Some(work_tree_abs.to_string_lossy().into_owned());
            }
            git_dir_override = Some(git_dir_abs);
            worktree = work_tree_abs;
        }
    }

    let (object_format, object_format_explicit) =
        resolve_init_object_format(object_format, global_config)?;
    let (ref_storage, ref_storage_explicit) =
        resolve_init_ref_storage(ref_format, global_config)?;
    let shared_repository = resolve_init_shared_repository(shared_repository, global_config, bare)?;
    let template_dir = resolve_init_template_dir(template, template_config, global_config, &cwd)?;

    let layout = RepositoryBootstrap::init(InitOptions {
        worktree,
        git_dir_override,
        core_worktree,
        object_format,
        object_format_explicit,
        bare,
        initial_branch: initial_branch.clone(),
        template_dir,
        copy_template_config: template_config,
        separate_git_dir,
        shared_repository,
        ref_storage,
        ref_storage_explicit,
    })
    .map_err(|err| match err {
        // Bootstrap reports fatal init failures (e.g. reinitializing with a different
        // object/ref format) as `GitError::Command`; git prints these as `fatal: <msg>`
        // and exits 128.
        GitError::Command(message) => {
            eprintln!("fatal: {message}");
            GitError::Exit(128)
        }
        other => other,
    })?;

    if branch_defaulted && !quiet && !layout.reinitialized {
        emit_default_branch_advice(&initial_branch, global_config)?;
    }
    if layout.reinitialized && initial_branch_explicit {
        eprintln!("warning: re-init: ignored --initial-branch={initial_branch}");
    }
    if !quiet {
        let git_dir = fs::canonicalize(&layout.git_dir)?;
        let action = if layout.reinitialized {
            "Reinitialized existing"
        } else {
            "Initialized empty"
        };
        println!("{action} Git repository in {}/", git_dir.to_string_lossy());
    }
    Ok(())
}

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
fn emit_default_branch_advice(
    branch: &str,
    global_config: &[GlobalConfigOverride],
) -> Result<()> {
    if let Ok(value) = env::var("GIT_ADVICE") {
        if !parse_config_bool(&value).unwrap_or(!value.is_empty()) {
            return Ok(());
        }
    }
    if init_config_bool("advice.defaultBranchName", global_config)? == Some(false) {
        return Ok(());
    }
    // `color.advice`: "always" colours unconditionally; "never"/false disables;
    // "auto"/true/unset colour only when stderr is a terminal (color.c
    // `git_config_colorbool` + `want_color_stderr`).
    let colored = match init_config_value("color.advice", global_config)?.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("always") => true,
        Some(value) if value.eq_ignore_ascii_case("never") => false,
        Some(value) if value.eq_ignore_ascii_case("auto") => stderr_is_terminal(),
        Some(value) => match parse_config_bool(value) {
            Some(false) => false,
            _ => stderr_is_terminal(),
        },
        None => stderr_is_terminal(),
    };
    let (color, reset) = if colored { ("\x1b[33m", "\x1b[m") } else { ("", "") };
    // The advice body already ends without a trailing newline; the
    // `Disable this message ...` instruction line was appended above with the
    // leading blank line git's `turn_off_instructions` carries.
    let body = DEFAULT_BRANCH_NAME_ADVICE.replacen("{}", branch, 1);
    for line in body.split('\n') {
        let sep = if line.is_empty() { "" } else { " " };
        eprintln!("{color}hint:{sep}{line}{reset}");
    }
    Ok(())
}

fn stderr_is_terminal() -> bool {
    use std::io::IsTerminal;
    io::stderr().is_terminal()
}

/// Resolve the object format for a *fresh* init, returning the chosen format and
/// whether it was specified explicitly on the command line.
///
/// Mirrors git's `repository_format_configure` precedence: an explicit
/// `--object-format` wins (and a bad value is fatal); otherwise `GIT_DEFAULT_HASH`
/// is consulted (also fatal on a bad value); otherwise the `init.defaultObjectFormat`
/// config default is used (a bad value here only warns and falls back to sha1). The
/// reinitialize-with-different-hash guard is applied later in
/// [`RepositoryBootstrap::init`], once the existing repository format is known.
fn resolve_init_object_format(
    cli_format: Option<String>,
    global_config: &[GlobalConfigOverride],
) -> Result<(ObjectFormat, bool)> {
    // git reads the config defaults FIRST (setup.c `read_default_format_config`),
    // so an invalid `init.defaultObjectFormat` warns even when the command line
    // or `GIT_DEFAULT_HASH` ends up choosing the format.
    let config_format = match init_config_value("init.defaultObjectFormat", global_config)? {
        Some(value) => match value.parse::<ObjectFormat>() {
            Ok(format) => Some(format),
            Err(_) => {
                eprintln!("warning: unknown hash algorithm '{value}'");
                None
            }
        },
        None => None,
    };
    if let Some(value) = cli_format {
        return Ok((parse_init_object_format(&value)?, true));
    }
    if let Ok(hash) = env::var("GIT_DEFAULT_HASH") {
        if !hash.is_empty() {
            return Ok((parse_init_object_format(&hash)?, false));
        }
    }
    if let Some(format) = config_format {
        return Ok((format, false));
    }
    Ok((ObjectFormat::Sha1, false))
}

/// Parse an object-format name the way git's `init` does: an unrecognised value is a
/// `fatal: unknown hash algorithm '<value>'` with exit status 128.
fn parse_init_object_format(value: &str) -> Result<ObjectFormat> {
    value.parse::<ObjectFormat>().map_err(|_| {
        eprintln!("fatal: unknown hash algorithm '{value}'");
        GitError::Exit(128)
    })
}

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
fn resolve_init_ref_storage(
    cli_ref_format: Option<Option<String>>,
    global_config: &[GlobalConfigOverride],
) -> Result<(RefStorageFormat, bool)> {
    // git reads the config defaults FIRST (setup.c `read_default_format_config`),
    // so an invalid `init.defaultRefFormat` warns even when the command line or
    // `GIT_DEFAULT_REF_FORMAT` ends up choosing the format.
    let config_format = match init_config_value("init.defaultRefFormat", global_config)? {
        Some(value) if value.is_empty() => Some(RefStorageFormat::Files),
        Some(value) => match RefStorageFormat::parse(&value) {
            Ok(format) => Some(format),
            Err(_) => {
                eprintln!("warning: unknown ref storage format '{value}'");
                None
            }
        },
        None => None,
    };
    if let Some(value) = cli_ref_format {
        return Ok((parse_init_ref_storage(value.as_deref().unwrap_or(""))?, true));
    }
    if let Ok(value) = env::var("GIT_DEFAULT_REF_FORMAT") {
        return Ok((parse_init_ref_storage(&value)?, false));
    }
    if let Some(format) = config_format {
        return Ok((format, false));
    }
    if init_config_bool("feature.experimental", global_config)?.unwrap_or(false) {
        return Ok((RefStorageFormat::Reftable, false));
    }
    Ok((RefStorageFormat::Files, false))
}

fn parse_init_ref_storage(value: &str) -> Result<RefStorageFormat> {
    RefStorageFormat::parse(value).map_err(|err| match err {
        GitError::Command(message) => {
            eprintln!("fatal: {message}");
            GitError::Exit(128)
        }
        other => other,
    })
}

fn resolve_init_shared_repository(
    cli_shared: Option<Option<String>>,
    global_config: &[GlobalConfigOverride],
    bare: bool,
) -> Result<Option<String>> {
    if let Some(value) = cli_shared {
        return Ok(value);
    }
    if bare {
        return Ok(None);
    }
    init_config_value("core.sharedRepository", global_config)
}

fn resolve_init_template_dir(
    cli_template: Option<Option<String>>,
    template_config: bool,
    global_config: &[GlobalConfigOverride],
    cwd: &Path,
) -> Result<Option<PathBuf>> {
    let _ = template_config;
    match cli_template {
        Some(None) => Ok(None),
        Some(Some(path)) => {
            if path.is_empty() {
                Ok(Some(PathBuf::new()))
            } else {
                Ok(Some(resolve_cli_path(cwd, &path)))
            }
        }
        None => {
            if let Some(path) = init_config_value("init.templatedir", global_config)? {
                let expanded = sley_config::expand_user_path(&path);
                Ok(Some(if expanded.is_absolute() {
                    expanded
                } else {
                    cwd.join(expanded)
                }))
            } else if let Ok(path) = env::var("GIT_TEMPLATE_DIR") {
                if path.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(resolve_cli_path(cwd, &path)))
                }
            } else {
                Ok(default_init_template_dir())
            }
        }
    }
}

fn default_init_template_dir() -> Option<PathBuf> {
    let output = ProcessCommand::new("git")
        .arg("--exec-path")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let exec_path = String::from_utf8_lossy(&output.stdout);
    let candidate = PathBuf::from(exec_path.trim()).join("../share/git-core/templates");
    candidate.canonicalize().ok().filter(|path| path.is_dir())
}

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

fn init_config_bool(key: &str, global_config: &[GlobalConfigOverride]) -> Result<Option<bool>> {
    init_config_value(key, global_config).map(|value| value.as_deref().and_then(parse_config_bool))
}

fn parse_config_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn cmd_add(args: &[String]) -> Result<()> {
    let mut paths = Vec::new();
    let mut dry_run = false;
    let mut verbose = false;
    let mut update = false;
    let mut all = false;
    let mut ignore_removal = false;
    let mut ignore_missing = false;
    let mut chmod = None;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut parsing_options = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-u" | "--update" => update = true,
            "--no-update" => update = false,
            "-A" | "--all" | "--no-ignore-removal" => {
                all = true;
                ignore_removal = false;
            }
            "--ignore-removal" | "--no-all" => {
                all = false;
                ignore_removal = true;
            }
            "--ignore-missing" => ignore_missing = true,
            "--no-ignore-missing" => ignore_missing = false,
            "--chmod" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--chmod requires a value".into()))?;
                chmod = Some(parse_add_chmod(value)?);
            }
            "--no-chmod" => chmod = None,
            value if value.starts_with("--chmod=") => {
                let value = value
                    .strip_prefix("--chmod=")
                    .expect("prefix checked by match guard");
                chmod = Some(parse_add_chmod(value)?);
            }
            "--ignore-errors" | "--no-ignore-errors" | "--sparse" | "--no-sparse" => {}
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| matches!(option, b'A' | b'n' | b'u' | b'v')) =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'A' => all = true,
                        b'n' => dry_run = true,
                        b'u' => update = true,
                        b'v' => verbose = true,
                        _ => unreachable!("add short-option group was filtered"),
                    }
                }
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                paths.push(PathBuf::from(value));
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    if ignore_missing && !dry_run {
        eprintln!("fatal: the option '--ignore-missing' requires '--dry-run'");
        return Err(GitError::Exit(128));
    }
    if paths.is_empty() && !update && !all {
        eprintln!("Nothing specified, nothing added.");
        eprintln!("hint: Maybe you wanted to say 'git add .'?");
        eprintln!(
            "hint: Disable this message with \"git config set advice.addEmptyPathspec false\""
        );
        return Ok(());
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    if update || all {
        let actions = resolve_add_update_actions(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            paths,
            all,
            ignore_missing,
        )?;
        if dry_run {
            print_add_actions(&worktree_root, &actions)?;
            return Ok(());
        }
        let action_paths = actions
            .iter()
            .map(AddAction::path)
            .cloned()
            .collect::<Vec<_>>();
        if !action_paths.is_empty() {
            let config = read_repo_config(&git_dir)?;
            sley_worktree::update_index_paths_filtered(
                &worktree_root,
                git_dir,
                format,
                &action_paths,
                sley_worktree::UpdateIndexOptions {
                    add: true,
                    remove: true,
                    force_remove: false,
                    chmod,
                    info_only: false,
                    ignore_skip_worktree_entries: false,
                },
                &config,
            )?;
        }
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    let actions = resolve_add_regular_actions(
        &cwd,
        &worktree_root,
        &git_dir,
        format,
        paths,
        AddRegularOptions {
            chmod,
            ignore_removal,
            ignore_missing,
        },
    )?;
    if dry_run {
        print_add_actions(&worktree_root, &actions)?;
        return Ok(());
    }
    let action_paths = actions
        .iter()
        .map(AddAction::path)
        .cloned()
        .collect::<Vec<_>>();
    if !action_paths.is_empty() {
        let config = read_repo_config(&git_dir)?;
        sley_worktree::update_index_paths_filtered(
            &worktree_root,
            git_dir,
            format,
            &action_paths,
            sley_worktree::UpdateIndexOptions {
                add: true,
                remove: true,
                force_remove: false,
                chmod,
                info_only: false,
                ignore_skip_worktree_entries: false,
            },
            &config,
        )?;
    }
    if verbose {
        print_add_actions(&worktree_root, &actions)?;
    }
    Ok(())
}

fn parse_add_chmod(value: &str) -> Result<bool> {
    match value {
        "+x" => Ok(true),
        "-x" => Ok(false),
        _ => {
            eprintln!("fatal: --chmod param '{value}' must be either -x or +x");
            Err(GitError::Exit(128))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AddAction {
    Add(PathBuf),
    Remove(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AddRegularOptions {
    chmod: Option<bool>,
    ignore_removal: bool,
    ignore_missing: bool,
}

impl AddAction {
    fn path(&self) -> &PathBuf {
        match self {
            Self::Add(path) | Self::Remove(path) => path,
        }
    }
}

fn resolve_add_regular_actions(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: Vec<PathBuf>,
    options: AddRegularOptions,
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
    let mut actions = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in sley_worktree::short_status(worktree_root, git_dir, format)? {
        let actionable = (entry.index == b'?' && entry.worktree == b'?')
            || entry.worktree == b'M'
            || entry.worktree == b'D';
        if !actionable {
            continue;
        }
        let path = worktree_root.join(
            std::str::from_utf8(&entry.path)
                .map_err(|err| GitError::InvalidPath(err.to_string()))?,
        );
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
        if entry.worktree == b'D' && options.ignore_removal {
            continue;
        }
        if seen.insert(path.clone()) {
            let action = if entry.worktree == b'D' {
                AddAction::Remove(path)
            } else {
                AddAction::Add(path)
            };
            actions.push(action);
        }
    }
    if options.chmod.is_some() {
        for (_, pathspec, _) in &pathspecs {
            for path in resolve_add_paths(cwd, worktree_root, vec![pathspec.clone()])? {
                if seen.insert(path.clone()) {
                    actions.push(AddAction::Add(path));
                }
            }
        }
    }
    for ((display, _, _), matched) in pathspecs.iter().zip(matched) {
        if !matched && !options.ignore_missing {
            eprintln!(
                "fatal: pathspec '{}' did not match any files",
                display.to_string_lossy()
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(actions)
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

fn resolve_add_paths(
    cwd: &Path,
    worktree_root: &Path,
    paths: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut resolved = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if absolute.is_dir() {
            collect_add_files(worktree_root, &absolute, &mut resolved)?;
        } else {
            resolved.insert(absolute);
        }
    }
    Ok(resolved.into_iter().collect())
}

fn collect_add_files(
    worktree_root: &Path,
    directory: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path == worktree_root.join(".git") {
            continue;
        }
        if path.is_dir() {
            collect_add_files(worktree_root, &path, out)?;
        } else {
            out.insert(path);
        }
    }
    Ok(())
}

fn print_add_actions(worktree_root: &Path, actions: &[AddAction]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for action in actions {
        let path = action.path();
        let display = path.strip_prefix(worktree_root).unwrap_or(path);
        let verb = match action {
            AddAction::Add(_) => "add",
            AddAction::Remove(_) => "remove",
        };
        writeln!(
            stdout,
            "{verb} '{}'",
            display.to_string_lossy().replace('\\', "/")
        )?;
    }
    Ok(())
}

fn cmd_clean(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut force = false;
    let mut force_was_mentioned = false;
    let mut directories = false;
    let mut include_ignored = false;
    let mut quiet = false;
    let mut excludes = Vec::new();
    let mut path_args = Vec::new();
    let mut parsing_options = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            path_args.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-f" | "--force" | "-ff" => {
                force = true;
                force_was_mentioned = true;
            }
            "--no-force" => {
                force = false;
                force_was_mentioned = true;
            }
            "-d" => directories = true,
            "-x" => include_ignored = true,
            "-e" | "--exclude" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clean --exclude requires a value".into()))?;
                excludes.push(value.to_string());
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--no-interactive" => {}
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|byte| matches!(byte, b'f' | b'd' | b'n' | b'q' | b'x')) =>
            {
                dry_run |= value.contains('n');
                if value.contains('f') {
                    force = true;
                    force_was_mentioned = true;
                }
                directories |= value.contains('d');
                include_ignored |= value.contains('x');
                quiet |= value.contains('q');
            }
            "--" => parsing_options = false,
            value if value.starts_with("--exclude=") => {
                let value = value
                    .strip_prefix("--exclude=")
                    .ok_or_else(|| GitError::Command("clean --exclude requires a value".into()))?;
                excludes.push(value.to_string());
            }
            value => path_args.push(value.to_string()),
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let config = read_repo_config(&git_dir)?;
    let require_force = config
        .get_bool("clean", None, "requireForce")
        .unwrap_or(true);
    if !dry_run && !force && require_force {
        if force_was_mentioned {
            eprintln!("fatal: clean.requireForce is true and -f not given: refusing to clean");
        } else {
            eprintln!(
                "fatal: clean.requireForce defaults to true and neither -i, -n, nor -f given; refusing to clean"
            );
        }
        return Err(GitError::Exit(128));
    }
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let pathspec = LsFilesPathspec::new(&cwd, &worktree_root, false, &path_args)?;
    let paths = clean_targets(
        &worktree_root,
        &git_dir,
        format,
        directories,
        include_ignored,
        &pathspec,
        &excludes,
    )?;
    let mut stdout = io::stdout();
    for target in paths {
        let display = String::from_utf8_lossy(&target.display);
        if dry_run {
            writeln!(stdout, "Would remove {display}")?;
            continue;
        }
        if !quiet {
            writeln!(stdout, "Removing {display}")?;
        }
        let mut filesystem_path = target.path;
        if filesystem_path.ends_with(b"/") {
            filesystem_path.pop();
        }
        let relative = std::str::from_utf8(&filesystem_path)
            .map_err(|err| GitError::InvalidPath(err.to_string()))?;
        let absolute = worktree_root.join(relative);
        if target.is_dir {
            fs::remove_dir_all(absolute)?;
        } else {
            fs::remove_file(absolute)?;
        }
    }
    Ok(())
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

enum ApplyAction {
    Write {
        path: Vec<u8>,
        mode: u32,
        content: Vec<u8>,
    },
    Remove {
        path: Vec<u8>,
    },
}

fn cmd_apply(args: &[String]) -> Result<()> {
    let mut check = false;
    let mut files = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--apply" | "--stat" | "--numstat" | "--summary" | "-q" | "--quiet" | "--recount"
            | "--allow-empty" | "--unsafe-paths" => {}
            "-R" | "--reverse" => {
                return Err(GitError::Unsupported(
                    "apply --reverse is not supported yet".into(),
                ));
            }
            "-3" | "--3way" | "--index" | "--cached" => {
                return Err(GitError::Unsupported(format!(
                    "apply {arg} is not supported yet"
                )));
            }
            "-p" | "-C" | "--whitespace" | "--directory" | "--exclude" | "--include" => {
                iter.next();
            }
            "--" => {
                files.extend(iter.by_ref().map(|value| value.to_string()));
                break;
            }
            value
                if value.starts_with("-p")
                    || value.starts_with("--whitespace=")
                    || value.starts_with("--directory=")
                    || value.starts_with("--exclude=")
                    || value.starts_with("--include=") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported apply option {value}"
                )));
            }
            value => files.push(value.to_string()),
        }
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let mut input = Vec::new();
    if files.is_empty() {
        io::stdin().read_to_end(&mut input)?;
    } else {
        for file in &files {
            input.extend_from_slice(&fs::read(file)?);
        }
    }
    let patches = sley_diff_merge::parse_unified_patch(&input)?;

    // Phase 1: compute every result first (git applies a patch atomically).
    let mut actions = Vec::new();
    for patch in &patches {
        let base = if patch.is_new {
            Vec::new()
        } else if let Some(old) = patch.old_path.as_deref().or(patch.new_path.as_deref()) {
            let rel = std::str::from_utf8(old)
                .map_err(|_| GitError::InvalidFormat("non-utf8 patch path".into()))?;
            fs::read(worktree_root.join(rel)).unwrap_or_default()
        } else {
            Vec::new()
        };
        let content = match sley_diff_merge::apply_file_patch(&base, patch) {
            sley_diff_merge::ApplyOutcome::Applied(content) => content,
            sley_diff_merge::ApplyOutcome::Rejected => {
                let name = patch
                    .new_path
                    .as_deref()
                    .or(patch.old_path.as_deref())
                    .unwrap_or(b"");
                eprintln!("error: patch failed: {}", String::from_utf8_lossy(name));
                return Err(GitError::Exit(1));
            }
        };
        if patch.is_delete {
            if let Some(old) = &patch.old_path {
                actions.push(ApplyAction::Remove { path: old.clone() });
            }
        } else {
            let mode = patch.new_mode.or(patch.old_mode).unwrap_or(0o100644);
            let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone()) else {
                return Err(GitError::InvalidFormat("patch missing target path".into()));
            };
            actions.push(ApplyAction::Write {
                path: target,
                mode,
                content,
            });
            if patch.is_rename
                && let Some(old) = &patch.old_path
            {
                actions.push(ApplyAction::Remove { path: old.clone() });
            }
        }
    }

    if check {
        return Ok(());
    }
    // Phase 2: materialize.
    for action in actions {
        match action {
            ApplyAction::Write {
                path,
                mode,
                content,
            } => merge_write_worktree_file(&worktree_root, &path, &content, mode)?,
            ApplyAction::Remove { path } => merge_remove_worktree_file(&worktree_root, &path)?,
        }
    }
    Ok(())
}

fn cmd_fsck(args: &[String]) -> Result<()> {
    let mut progress = true;
    let mut report_dangling = true;
    let mut report_unreachable = false;
    for arg in args {
        match arg.as_str() {
            "--no-progress" => progress = false,
            "--progress" => progress = true,
            "--dangling" => report_dangling = true,
            "--no-dangling" => report_dangling = false,
            "--unreachable" => report_unreachable = true,
            "--no-unreachable" => report_unreachable = false,
            "--full" | "--strict" | "--connectivity-only" | "--name-objects" => {}
            value => {
                return Err(GitError::Command(format!(
                    "fsck currently supports --no-progress and basic object connectivity; unsupported option {value}"
                )));
            }
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let roots = fsck_root_oids(&git_dir, format)?;
    let mut object_ids = repository_object_ids(&git_dir, format)?;
    // Mirror builtin/fsck.c `fsck_loose`: probe every loose object file before the
    // connectivity walk, reporting corrupt or mismatched ones at `error:` level on
    // stderr (with git's path-form spelling) and excluding them from the object set
    // so they neither parse nor surface as dangling.
    let objects_dir_display = fsck_objects_dir_display(&git_dir, &cwd);
    let mut bad_loose = HashSet::new();
    for oid in db.loose().object_ids()? {
        let hex = oid.to_hex();
        let display_path = format!("{objects_dir_display}/{}/{}", &hex[..2], &hex[2..]);
        match db.loose().verify_object(&oid, &display_path)? {
            None | Some(LooseObjectIntegrity::Ok) => {}
            Some(LooseObjectIntegrity::HashMismatch { actual }) => {
                eprintln!("error: {actual}: hash-path mismatch, found at: {display_path}");
                bad_loose.insert(oid);
            }
            Some(LooseObjectIntegrity::Corrupt) => {
                eprintln!("error: {oid}: object corrupt or missing: {display_path}");
                bad_loose.insert(oid);
            }
        }
    }
    let loose_errors = !bad_loose.is_empty();
    object_ids.retain(|oid| !bad_loose.contains(oid));
    if roots.is_empty() && progress {
        eprintln!("notice: No default references");
    }
    let report = sley_fsck::fsck_objects_with_options(
        &db,
        format,
        roots,
        object_ids,
        sley_fsck::FsckOptions {
            report_dangling,
            report_unreachable,
        },
    );
    for notice in &report.notices {
        println!("{}", notice.message);
    }
    for issue in &report.issues {
        println!("{}", issue.message);
    }
    if !report.is_ok() {
        Err(GitError::Exit(10))
    } else if loose_errors {
        // builtin/fsck.c exits with its `errors_found` bitmask; a corrupt or
        // misplaced loose object sets ERROR_OBJECT (= 1).
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

/// The directory prefix git uses when printing loose-object paths from fsck:
/// `$GIT_DIR/objects` with GIT_DIR's textual (often relative) value — `./objects`
/// when the cwd IS the git dir (a bare repository), `.git/objects` at a worktree
/// root. sley's discovery yields an absolute git dir, so reconstruct the relative
/// spelling for those shapes and fall back to the absolute path.
fn fsck_objects_dir_display(git_dir: &Path, cwd: &Path) -> String {
    if git_dir == cwd {
        return "./objects".to_string();
    }
    if let Ok(relative) = git_dir.strip_prefix(cwd) {
        return format!("{}/objects", relative.display());
    }
    format!("{}/objects", git_dir.display())
}

fn fsck_root_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let store = FileRefStore::new(git_dir, format);
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    if let Some(target) = store.read_ref("HEAD")? {
        let reference = Ref {
            name: "HEAD".to_string(),
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push(oid);
        }
    }
    for reference in store.list_refs()? {
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push(oid);
        }
    }
    Ok(roots)
}

fn pack_refs_peeled_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let peeled = sley_rev::peel_tags(db, format, oid)?;
    Ok((peeled != *oid).then_some(peeled))
}

#[derive(Debug)]
enum ReplaceMode {
    Create { object: String, replacement: String },
    List { pattern: Option<String> },
    Delete { objects: Vec<String> },
}

#[derive(Debug)]
struct ReplaceOptions {
    force: bool,
    format: ReplaceListFormat,
    mode: ReplaceMode,
}

#[derive(Debug, Clone, Copy)]
enum ReplaceListFormat {
    Short,
    Medium,
    Long,
}

fn cmd_replace(args: &[String]) -> Result<()> {
    let options = parse_replace_options(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    match options.mode {
        ReplaceMode::List { pattern } => {
            replace_list(&store, &db, format, pattern.as_deref(), options.format)
        }
        ReplaceMode::Delete { objects } => {
            replace_delete(&store, &common_git_dir, format, &objects)
        }
        ReplaceMode::Create {
            object,
            replacement,
        } => replace_create(
            &store,
            &db,
            &common_git_dir,
            format,
            &object,
            &replacement,
            options.force,
        ),
    }
}

fn parse_replace_options(args: &[String]) -> Result<ReplaceOptions> {
    let mut force = false;
    let mut format = ReplaceListFormat::Short;
    let mut list = false;
    let mut delete = false;
    let mut unsupported_mode = None::<&str>;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                positional.extend(iter.cloned());
                break;
            }
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-l" | "--list" => list = true,
            "-d" | "--delete" => delete = true,
            "-e" | "--edit" => unsupported_mode = Some("--edit"),
            "-g" | "--graft" => unsupported_mode = Some("--graft"),
            "--convert-graft-file" => unsupported_mode = Some("--convert-graft-file"),
            "--raw" | "--no-raw" => {}
            "--format" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `format' requires a value");
                    return Err(GitError::Exit(129));
                };
                format = parse_replace_list_format(value)?;
            }
            "--no-format" => format = ReplaceListFormat::Short,
            value if let Some(value) = long_option_value(value, "format") => {
                format = parse_replace_list_format(value)?;
            }
            value if value.starts_with("--no-force=") => {
                eprintln!("error: option `no-force' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return replace_usage();
            }
            value if value.starts_with('-') && value.len() > 1 => {
                for option in value[1..].chars() {
                    match option {
                        'f' => force = true,
                        'l' => list = true,
                        'd' => delete = true,
                        'e' => unsupported_mode = Some("--edit"),
                        'g' => unsupported_mode = Some("--graft"),
                        other => {
                            eprintln!("error: unknown switch `{other}'");
                            return replace_usage();
                        }
                    }
                }
            }
            value => positional.push(value.to_string()),
        }
    }
    if let Some(mode) = unsupported_mode {
        return Err(GitError::Unsupported(format!("replace {mode}")));
    }
    if delete {
        if positional.is_empty() {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::Delete {
                objects: positional,
            },
        });
    }
    if list || positional.len() <= 1 {
        if positional.len() > 1 {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::List {
                pattern: positional.pop(),
            },
        });
    }
    if positional.len() == 2 {
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::Create {
                object: positional.remove(0),
                replacement: positional.remove(0),
            },
        });
    }
    replace_usage()
}

fn parse_replace_list_format(value: &str) -> Result<ReplaceListFormat> {
    match value {
        "short" => Ok(ReplaceListFormat::Short),
        "medium" => Ok(ReplaceListFormat::Medium),
        "long" => Ok(ReplaceListFormat::Long),
        other => {
            eprintln!("error: invalid replace format '{other}'");
            eprintln!("valid formats are 'short', 'medium' and 'long'");
            Err(GitError::Exit(255))
        }
    }
}

fn replace_list(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    object_format: ObjectFormat,
    pattern: Option<&str>,
    format: ReplaceListFormat,
) -> Result<()> {
    for reference in store.list_refs()? {
        let Some(object) = reference.name.strip_prefix("refs/replace/") else {
            continue;
        };
        if pattern.is_some_and(|pattern| !refname_pattern_matches(pattern, object)) {
            continue;
        }
        let RefTarget::Direct(replacement) = reference.target else {
            continue;
        };
        match format {
            ReplaceListFormat::Short => println!("{object}"),
            ReplaceListFormat::Medium => println!("{object} -> {replacement}"),
            ReplaceListFormat::Long => {
                let object_type = replace_object_type(db, object_format, object)?;
                let replacement_type = db
                    .read_object_header(&replacement)?
                    .map(|(object_type, _)| object_type.as_str())
                    .unwrap_or("unknown");
                println!("{object} ({object_type}) -> {replacement} ({replacement_type})");
            }
        }
    }
    Ok(())
}

fn replace_delete(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    objects: &[String],
) -> Result<()> {
    let mut failed = false;
    for object in objects {
        let oid = match ObjectId::from_hex(format, object) {
            Ok(oid) => oid,
            Err(_) => match resolve_revision(git_dir, format, object) {
                Ok(oid) => oid,
                Err(_) => {
                    eprintln!("error: failed to resolve '{object}' as a valid ref");
                    failed = true;
                    continue;
                }
            },
        };
        let name = format!("refs/replace/{oid}");
        match store.delete_ref(&name) {
            Ok(_) => println!("Deleted replace ref '{oid}'"),
            Err(_) => {
                eprintln!("error: replace ref '{oid}' not found");
                failed = true;
            }
        }
    }
    if failed {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

fn replace_create(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    object: &str,
    replacement: &str,
    force: bool,
) -> Result<()> {
    let object_oid = resolve_revision(git_dir, format, object)?;
    let replacement_oid = resolve_revision(git_dir, format, replacement)?;
    let object_type = db
        .read_object_header(&object_oid)?
        .map(|(object_type, _)| object_type)
        .ok_or_else(|| GitError::object_not_found(object_oid))?;
    let replacement_type = db
        .read_object_header(&replacement_oid)?
        .map(|(object_type, _)| object_type)
        .ok_or_else(|| GitError::object_not_found(replacement_oid))?;
    if object_type != replacement_type {
        eprintln!("error: Objects must be of the same type.");
        eprintln!(
            "'{object}' points to a replaced object of type '{}'",
            object_type.as_str()
        );
        eprintln!(
            "while '{replacement}' points to a replacement object of type '{}'.",
            replacement_type.as_str()
        );
        return Err(GitError::Exit(255));
    }
    let name = format!("refs/replace/{object_oid}");
    let precondition = if force {
        RefPrecondition::Any
    } else {
        RefPrecondition::MustNotExist
    };
    let mut tx = store.transaction();
    tx.update_to(
        name.clone(),
        RefTarget::Direct(replacement_oid),
        precondition,
        None,
    );
    match tx.commit() {
        Ok(()) => Ok(()),
        Err(_) if !force => {
            eprintln!("error: replace ref '{name}' already exists");
            Err(GitError::Exit(255))
        }
        Err(err) => Err(err),
    }
}

fn replace_object_type(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    object: &str,
) -> Result<&'static str> {
    let oid = ObjectId::from_hex(format, object)?;
    Ok(db
        .read_object_header(&oid)?
        .map(|(object_type, _)| object_type.as_str())
        .unwrap_or("unknown"))
}

fn replace_usage<T>() -> Result<T> {
    eprintln!("usage: git replace [-f] <object> <replacement>");
    eprintln!("   or: git replace [-f] --edit <object>");
    eprintln!("   or: git replace [-f] --graft <commit> [<parent>...]");
    eprintln!("   or: git replace [-f] --convert-graft-file");
    eprintln!("   or: git replace -d <object>...");
    eprintln!("   or: git replace [--format=<format>] [-l [<pattern>]]");
    eprintln!();
    eprintln!("    -l, --list            list replace refs");
    eprintln!("    -d, --delete          delete replace refs");
    eprintln!("    -e, --edit            edit existing object");
    eprintln!("    -g, --graft           change a commit's parents");
    eprintln!("    --convert-graft-file  convert existing graft file");
    eprintln!("    -f, --[no-]force      replace the ref if it exists");
    eprintln!("    --[no-]raw            do not pretty-print contents for --edit");
    eprintln!("    --[no-]format <format>");
    eprintln!("                          use this format");
    eprintln!();
    Err(GitError::Exit(129))
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
fn stash_tree_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, (u32, ObjectId)>> {
    let mut entries = BTreeMap::new();
    collect_stash_tree_entry_map(db, format, tree_oid, Vec::new(), &mut entries)?;
    Ok(entries)
}

fn collect_stash_tree_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut BTreeMap<Vec<u8>, (u32, ObjectId)>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {}, found {}",
            tree_oid,
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            collect_stash_tree_entry_map(db, format, &entry.oid, path, entries)?;
        } else {
            entries.insert(path, (entry.mode, entry.oid));
        }
    }
    Ok(())
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

fn cmd_prune_packed(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut positional = 0usize;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-q" | "--quiet" | "--no-quiet" => {}
            "--" => {
                positional += iter.count();
                break;
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return prune_packed_usage();
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown switch `{}'", value.trim_start_matches('-'));
                return prune_packed_usage();
            }
            _ => positional += 1,
        }
    }
    if positional > 0 {
        eprintln!("fatal: too many arguments");
        eprintln!();
        return prune_packed_usage();
    }

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let objects_dir = repository_objects_dir(&git_dir);
    let packed = prune_packed_object_ids(&objects_dir.join("pack"), format)?;
    if packed.is_empty() {
        return Ok(());
    }
    for (oid, path) in prune_packed_loose_object_paths(&objects_dir, format)? {
        if !packed.contains(&oid) {
            continue;
        }
        if dry_run {
            println!("rm -f {}", prune_packed_display_path(&path)?);
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn prune_packed_usage<T>() -> Result<T> {
    eprintln!("usage: git prune-packed [-n | --dry-run] [-q | --quiet]");
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    dry run");
    eprintln!("    -q, --[no-]quiet      be quiet");
    eprintln!();
    Err(GitError::Exit(129))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeRrEntry {
    hash: String,
    variant: u32,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerereSubcommand {
    Clear,
    Forget,
    Status,
}

#[derive(Debug)]
struct RerereOptions {
    subcommand: Option<RerereSubcommand>,
    paths: Vec<String>,
}

fn cmd_rerere(args: &[String]) -> Result<()> {
    let options = parse_rerere_options(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    match options.subcommand {
        None => Ok(()),
        Some(RerereSubcommand::Status) => rerere_status(&git_dir),
        Some(RerereSubcommand::Clear) => rerere_clear(&git_dir),
        Some(RerereSubcommand::Forget) => rerere_forget(&git_dir, &options.paths),
    }
}

fn parse_rerere_options(args: &[String]) -> Result<RerereOptions> {
    let mut autoupdate = None;
    let mut subcommand = None;
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--rerere-autoupdate" => autoupdate = Some(true),
            "--no-rerere-autoupdate" => autoupdate = Some(false),
            value if value.starts_with("--no-rerere-autoupdate=") => {
                eprintln!("error: option `no-rerere-autoupdate' takes no value");
                return rerere_usage();
            }
            value if value.starts_with("--rerere-autoupdate=") => {
                eprintln!("error: option `rerere-autoupdate' takes no value");
                return rerere_usage();
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return rerere_usage();
            }
            "clear" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Clear),
            "forget" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Forget),
            "status" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Status),
            _ if subcommand.is_none() => return rerere_usage(),
            value => paths.push(value.to_string()),
        }
    }
    if matches!(subcommand, Some(RerereSubcommand::Forget)) && paths.is_empty() {
        eprintln!("warning: 'git rerere forget' without paths is deprecated");
    }
    let _ = autoupdate;
    Ok(RerereOptions { subcommand, paths })
}

fn rerere_usage<T>() -> Result<T> {
    eprintln!("usage: git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]");
    eprintln!();
    eprintln!("    --[no-]rerere-autoupdate");
    eprintln!("                          register clean resolutions in index");
    eprintln!();
    Err(GitError::Exit(129))
}

fn is_rerere_enabled(git_dir: &Path) -> Result<bool> {
    let config = read_repo_config(git_dir)?;
    if let Some(value) = config.get("rerere", None, "enabled") {
        return Ok(matches!(value, "true" | "1" | "yes" | "on"));
    }
    Ok(git_dir.join("rr-cache").is_dir())
}

fn read_merge_rr(git_dir: &Path) -> Result<Vec<MergeRrEntry>> {
    let path = git_dir.join("MERGE_RR");
    let Ok(data) = fs::read(&path) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for record in data
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(GitError::Command("corrupt MERGE_RR".into()));
        };
        let id = std::str::from_utf8(&record[..tab])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let (hash, variant) = parse_merge_rr_id(id)?;
        entries.push(MergeRrEntry {
            hash,
            variant,
            path: path.to_string(),
        });
    }
    Ok(entries)
}

fn parse_merge_rr_id(id: &str) -> Result<(String, u32)> {
    let Some(dot) = id.find('.') else {
        return Ok((id.to_string(), 0));
    };
    let hash = &id[..dot];
    let variant = id[dot + 1..]
        .parse::<u32>()
        .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
    Ok((hash.to_string(), variant))
}

fn rerere_cache_file_path(cache_dir: &Path, variant: u32, name: &str) -> PathBuf {
    if variant == 0 {
        cache_dir.join(name)
    } else {
        cache_dir.join(format!("{name}.{variant}"))
    }
}

fn rerere_has_resolution(rr_cache: &Path, entry: &MergeRrEntry) -> bool {
    let cache_dir = rr_cache.join(&entry.hash);
    rerere_cache_file_path(&cache_dir, entry.variant, "preimage").is_file()
        && rerere_cache_file_path(&cache_dir, entry.variant, "postimage").is_file()
}

fn remove_rr_cache_entry(rr_cache: &Path, entry: &MergeRrEntry) -> Result<()> {
    let cache_dir = rr_cache.join(&entry.hash);
    if !cache_dir.is_dir() {
        return Ok(());
    }
    for file in fs::read_dir(&cache_dir)? {
        let path = file?.path();
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    match fs::remove_dir(&cache_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(GitError::Io(err.to_string())),
    }
    Ok(())
}

fn rerere_status(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    for entry in read_merge_rr(git_dir)? {
        println!("{}", entry.path);
    }
    Ok(())
}

fn rerere_clear(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    for entry in read_merge_rr(git_dir)? {
        if !rerere_has_resolution(&rr_cache, &entry) {
            remove_rr_cache_entry(&rr_cache, &entry)?;
        }
    }
    let merge_rr = git_dir.join("MERGE_RR");
    if merge_rr.is_file() {
        fs::remove_file(merge_rr)?;
    }
    Ok(())
}

fn rerere_path_matches(path: &str, pattern: &str) -> bool {
    path == pattern || path.ends_with(&format!("/{pattern}"))
}

fn rerere_forget(git_dir: &Path, paths: &[String]) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    if paths.is_empty() {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    let entries = read_merge_rr(git_dir)?;
    for pattern in paths {
        let mut matched = false;
        for entry in entries
            .iter()
            .filter(|entry| rerere_path_matches(&entry.path, pattern))
        {
            matched = true;
            let cache_dir = rr_cache.join(&entry.hash);
            let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
            if !postimage.is_file() {
                eprintln!("error: no remembered resolution for '{pattern}'");
                continue;
            }
            fs::remove_file(&postimage)?;
            if let Ok(thisimage) = fs::read(rerere_cache_file_path(
                &cache_dir,
                entry.variant,
                "thisimage",
            )) {
                fs::write(
                    rerere_cache_file_path(&cache_dir, entry.variant, "preimage"),
                    thisimage,
                )?;
                eprintln!("Updated preimage for '{pattern}'");
            }
            eprintln!("Forgot resolution for '{pattern}'");
        }
        if !matched {
            eprintln!("error: no remembered resolution for '{pattern}'");
        }
    }
    Ok(())
}

fn prune_packed_object_ids(pack_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let mut packed = HashSet::new();
    if !pack_dir.exists() {
        return Ok(packed);
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        packed.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(packed)
}

fn prune_packed_loose_object_paths(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<(ObjectId, PathBuf)>> {
    let mut objects = Vec::new();
    if !objects_dir.exists() {
        return Ok(objects);
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let fanout = entry.file_name();
        let Some(fanout) = fanout.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let suffix = object_entry.file_name();
            let Some(suffix) = suffix.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let oid = ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?;
            objects.push((oid, object_entry.path()));
        }
    }
    objects.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(objects)
}

fn prune_packed_display_path(path: &Path) -> Result<String> {
    let cwd = env::current_dir()?;
    let display = path.strip_prefix(&cwd).unwrap_or(path);
    Ok(display.to_string_lossy().replace('\\', "/"))
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

fn cmd_rm(args: &[String]) -> Result<()> {
    let mut paths = Vec::new();
    let mut recursive = false;
    let mut quiet = false;
    let mut cached = false;
    let mut force = false;
    let mut dry_run = false;
    let mut ignore_unmatch = false;
    let mut parsing_options = true;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "-r" | "-R" | "--recursive" => recursive = true,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--cached" => cached = true,
            "--no-cached" => cached = false,
            "--ignore-unmatch" => ignore_unmatch = true,
            "--no-ignore-unmatch" => ignore_unmatch = false,
            "--sparse" | "--no-sparse" => {}
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| matches!(option, b'r' | b'R' | b'f' | b'n' | b'q')) =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'r' | b'R' => recursive = true,
                        b'f' => force = true,
                        b'n' => dry_run = true,
                        b'q' => quiet = true,
                        _ => unreachable!("rm short-option group was filtered"),
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported rm option {value}")));
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                paths.push(PathBuf::from(value));
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    if paths.is_empty() {
        eprintln!("fatal: No pathspec was given. Which files should I remove?");
        return Err(GitError::Exit(128));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let resolved_paths = paths
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .collect::<Vec<_>>();
    let result = sley_worktree::remove_index_and_worktree_paths(
        worktree_root,
        git_dir,
        format,
        &resolved_paths,
        sley_worktree::RemoveOptions {
            recursive,
            cached,
            force,
            dry_run,
            ignore_unmatch,
        },
    )?;
    if !quiet {
        let mut stdout = io::stdout().lock();
        for path in result.removed {
            writeln!(stdout, "rm '{}'", String::from_utf8_lossy(&path))?;
        }
    }
    Ok(())
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

fn cmd_mv(args: &[String]) -> Result<()> {
    let mut paths = Vec::new();
    let mut force = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut skip_errors = false;
    let mut parsing_options = true;
    for arg in args {
        if !parsing_options {
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-k" => skip_errors = true,
            "--sparse" | "--no-sparse" => {}
            value if value.starts_with('-') && !value.starts_with("--") && value.len() > 2 => {
                for flag in value[1..].bytes() {
                    match flag {
                        b'f' => force = true,
                        b'n' => dry_run = true,
                        b'v' => verbose = true,
                        b'k' => skip_errors = true,
                        other => {
                            return Err(GitError::Command(format!(
                                "unsupported mv option -{}",
                                other as char
                            )));
                        }
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported mv option {value}")));
            }
            value => paths.push(PathBuf::from(value)),
        }
    }
    if paths.len() < 2 {
        return Err(GitError::Command(
            "mv currently supports <source>... <destination>".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let destination = if paths[paths.len() - 1].is_absolute() {
        paths[paths.len() - 1].clone()
    } else {
        cwd.join(&paths[paths.len() - 1])
    };
    if paths.len() > 2 && !destination.is_dir() {
        eprintln!(
            "fatal: destination '{}' is not a directory",
            destination.display()
        );
        return Err(GitError::Exit(128));
    }

    let mut results = Vec::new();
    for source in &paths[..paths.len() - 1] {
        let source = if source.is_absolute() {
            source.clone()
        } else {
            cwd.join(source)
        };
        let result = sley_worktree::move_index_and_worktree_path(
            &worktree_root,
            &git_dir,
            format,
            &source,
            &destination,
            sley_worktree::MoveOptions {
                force,
                dry_run,
                skip_errors,
            },
        )?;
        let fatal = result.fatal.is_some();
        results.push(result);
        if dry_run && fatal {
            break;
        }
    }
    if dry_run {
        for result in &results {
            let source = String::from_utf8_lossy(&result.source);
            let destination = String::from_utf8_lossy(&result.destination);
            println!("Checking rename of '{source}' to '{destination}'");
            for detail in &result.details {
                let source = String::from_utf8_lossy(&detail.source);
                let destination = String::from_utf8_lossy(&detail.destination);
                println!("Checking rename of '{source}' to '{destination}'");
            }
        }
        if let Some(fatal) = results.iter().find_map(|result| result.fatal.as_deref()) {
            eprintln!("{fatal}");
            return Err(GitError::Exit(128));
        }
    }
    if dry_run || verbose {
        for result in &results {
            if result.skipped {
                continue;
            }
            let source = String::from_utf8_lossy(&result.source);
            let destination = String::from_utf8_lossy(&result.destination);
            println!("Renaming {source} to {destination}");
            for detail in &result.details {
                if detail.skipped {
                    continue;
                }
                let source = String::from_utf8_lossy(&detail.source);
                let destination = String::from_utf8_lossy(&detail.destination);
                println!("Renaming {source} to {destination}");
            }
        }
    }
    Ok(())
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

struct CleanTarget {
    path: Vec<u8>,
    display: Vec<u8>,
    is_dir: bool,
}

fn clean_targets(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    directories: bool,
    include_ignored: bool,
    pathspec: &LsFilesPathspec,
    excludes: &[String],
) -> Result<Vec<CleanTarget>> {
    let has_pathspec = !pathspec.filters.is_empty();
    // Git treats any pathspec as `-d` for selection purposes.
    let effective_directories = directories || has_pathspec;
    let index = sley_worktree::read_repository_index(git_dir, format)?;

    let mut paths = if effective_directories {
        sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory: true,
                no_empty_directory: false,
                preserve_ignored_directories: directories,
                exclude_standard: !include_ignored,
                ignored_only: false,
                exclude_patterns: Vec::new(),
                exclude_per_directory: Vec::new(),
                pathspecs: pathspec.untracked_pathspecs(),
            },
        )?
    } else {
        sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory: false,
                no_empty_directory: false,
                preserve_ignored_directories: false,
                exclude_standard: !include_ignored,
                ignored_only: false,
                exclude_patterns: Vec::new(),
                exclude_per_directory: Vec::new(),
                pathspecs: pathspec.untracked_pathspecs(),
            },
        )?
    };

    // Without `-d` (and without a pathspec, which Git treats as `-d`), the
    // non-directory walk lists every untracked file. Git only removes a file in
    // a subdirectory when that directory contains tracked content; an untracked
    // file inside a wholly-untracked directory needs `-d`. The directory walk
    // already encodes this selection (it rolls wholly-untracked directories up
    // to `dir/` and only descends into directories with tracked/ignored content),
    // so the retain must run only on the non-directory walk's flat output.
    if !effective_directories {
        paths.retain(|path| {
            path.ends_with(b"/") || clean_untracked_file_eligible(path, index.as_ref())
        });
    }

    if has_pathspec {
        paths = clean_collapse_untracked_paths(paths);
    }

    let mut targets = Vec::new();
    for path in paths {
        let is_dir = path.ends_with(b"/");
        let Some(display) = pathspec.display(&path) else {
            continue;
        };
        if clean_target_is_excluded(&path, excludes) {
            continue;
        }
        targets.push(CleanTarget {
            path,
            display,
            is_dir,
        });
    }

    targets.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(targets)
}

/// Git clean file selection without `-d` or pathspecs: a worktree-root file is
/// always eligible; a file in a subdirectory is eligible only when its immediate
/// parent directory contains tracked content (otherwise the file lives in a
/// wholly-untracked directory that Git would only remove under `-d`). This holds
/// regardless of `-x` or whether the repository has any commits yet.
fn clean_untracked_file_eligible(path: &[u8], index: Option<&Index>) -> bool {
    if !path.iter().any(|byte| *byte == b'/') {
        return true;
    }
    let Some(index) = index else {
        return false;
    };
    clean_path_parent(path).is_some_and(|parent| clean_index_has_tracked_under(index, parent))
}

fn clean_index_has_tracked_under(index: &Index, directory: &[u8]) -> bool {
    let mut prefix = directory.to_vec();
    prefix.push(b'/');
    index
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes().starts_with(&prefix))
}

fn clean_path_parent(path: &[u8]) -> Option<&[u8]> {
    let slash = path.iter().rposition(|byte| *byte == b'/')?;
    if slash == 0 {
        return None;
    }
    Some(&path[..slash])
}

/// Match git `correct_untracked_entries` for pathspec-driven clean.
fn clean_collapse_untracked_paths(paths: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    // The directory walk already encodes Git's `--directory` rollup: a
    // wholly-untracked directory named by a pathspec is emitted as `dir/`, while
    // untracked files inside a partially-tracked directory are listed
    // individually. The only post-processing left is dropping a file entry that
    // is already subsumed by a rolled-up parent directory entry.
    let mut sorted = paths;
    sorted.sort();
    let mut kept = BTreeSet::new();
    for path in &sorted {
        if sorted.iter().any(|other| {
            other != path && other.ends_with(b"/") && clean_directory_contains_path(other, path)
        }) {
            continue;
        }
        kept.insert(path.clone());
    }
    kept.into_iter().collect()
}

fn clean_target_is_excluded(path: &[u8], excludes: &[String]) -> bool {
    excludes
        .iter()
        .any(|pattern| clean_exclude_pattern_matches(pattern, path))
}

fn clean_exclude_pattern_matches(pattern: &str, path: &[u8]) -> bool {
    let pattern = pattern.trim_end_matches('/');
    let path = String::from_utf8_lossy(path);
    let normalized = path.trim_end_matches('/');
    let candidate = if pattern.contains('/') {
        normalized
    } else {
        normalized.rsplit('/').next().unwrap_or(normalized)
    };
    if pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        refname_pattern_matches(pattern, candidate)
    } else {
        candidate == pattern
    }
}

fn clean_directory_contains_path(directory: &[u8], path: &[u8]) -> bool {
    directory.strip_suffix(b"/").is_some_and(|directory| {
        path.strip_prefix(directory)
            .and_then(|rest| rest.strip_prefix(b"/"))
            .is_some()
    })
}

fn cmd_bundle(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Err(GitError::Command(
            "bundle requires <create|verify|list-heads|unbundle>".into(),
        ));
    };
    match subcommand {
        "create" => cmd_bundle_create(&args[1..]),
        "verify" => cmd_bundle_verify(&args[1..]),
        "list-heads" => cmd_bundle_list_heads(&args[1..]),
        "unbundle" => cmd_bundle_unbundle(&args[1..]),
        other => Err(GitError::Command(format!(
            "unsupported bundle subcommand {other}"
        ))),
    }
}

fn cmd_commit_graph(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Err(GitError::Command(
            "commit-graph requires <write|verify>".into(),
        ));
    };
    match subcommand {
        "write" => cmd_commit_graph_write(&args[1..]),
        "verify" => cmd_commit_graph_verify(&args[1..]),
        other => Err(GitError::Command(format!(
            "unsupported commit-graph subcommand {other}"
        ))),
    }
}

fn cmd_commit_graph_write(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut reachable = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--reachable" => reachable = true,
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" | "--no-progress" => {}
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "commit-graph write option {other}"
                )));
            }
        }
    }
    if !reachable {
        return Ok(());
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    let graph = commit_graph_for_reachable_refs(&git_dir, &object_dir, format)?;
    let graph_dir = object_dir.join("info");
    fs::create_dir_all(&graph_dir)?;
    fs::write(graph_dir.join("commit-graph"), graph)?;
    Ok(())
}

fn cmd_commit_graph_verify(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" | "--no-progress" => {}
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "commit-graph verify option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    let graph_path = object_dir.join("info").join("commit-graph");
    if graph_path.exists() {
        CommitGraph::parse(&fs::read(graph_path)?, format)?;
        return Ok(());
    }
    let chain_path = object_dir
        .join("info")
        .join("commit-graphs")
        .join("commit-graph-chain");
    if chain_path.exists() {
        return verify_split_commit_graph_chain(&chain_path, format);
    }
    Err(GitError::not_found("commit-graph"))
}

fn verify_split_commit_graph_chain(chain_path: &Path, format: ObjectFormat) -> Result<()> {
    let chain_dir = chain_path
        .parent()
        .ok_or_else(|| GitError::InvalidPath("commit-graph chain path has no parent".into()))?;
    let chain_bytes = fs::read(chain_path)?;
    let text = std::str::from_utf8(&chain_bytes)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut graph_hashes = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        graph_hashes.push(ObjectId::from_hex(format, line)?);
    }
    if graph_hashes.is_empty() {
        return Err(GitError::InvalidFormat(
            "commit-graph chain is empty".into(),
        ));
    }
    for (idx, expected_hash) in graph_hashes.iter().enumerate() {
        let graph_path = chain_dir.join(format!("graph-{expected_hash}.graph"));
        let graph = CommitGraph::parse(&fs::read(&graph_path)?, format)?;
        if &graph.checksum != expected_hash {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph {} checksum is {}, expected {expected_hash}",
                graph_path.display(),
                graph.checksum
            )));
        }
        if graph.base_graph_count as usize != graph.base_graphs.len() {
            return Err(GitError::InvalidFormat(
                "commit-graph BASE count does not match parsed base list".into(),
            ));
        }
        if graph.base_graph_count as usize > idx {
            return Err(GitError::InvalidFormat(
                "commit-graph has more base graphs than previous chain entries".into(),
            ));
        }
        if !graph.base_graphs.is_empty() {
            let expected_bases = &graph_hashes[idx - graph.base_graphs.len()..idx];
            if graph.base_graphs != expected_bases {
                return Err(GitError::InvalidFormat(
                    "commit-graph BASE hashes do not match chain order".into(),
                ));
            }
        }
    }
    Ok(())
}

fn commit_graph_for_reachable_refs(
    git_dir: &Path,
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<u8>> {
    let db = FileObjectDatabase::new(object_dir, format);
    let store = FileRefStore::new(git_dir, format);
    let mut starts = Vec::new();
    let mut seen_starts = HashSet::new();
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        if let Ok(commit) = sley_rev::peel_to_commit(&db, format, &oid)
            && seen_starts.insert(commit)
        {
            starts.push(commit);
        }
    }
    if let Ok(head) = resolve_revision(git_dir, format, "HEAD")
        && let Ok(commit) = sley_rev::peel_to_commit(&db, format, &head)
        && seen_starts.insert(commit)
    {
        starts.push(commit);
    }
    let records = sley_rev::walk_commits(&db, format, starts)?;
    let record_map = records
        .iter()
        .map(|record| (record.oid, record))
        .collect::<HashMap<_, _>>();
    let mut generation_cache = HashMap::new();
    let mut entries = Vec::with_capacity(records.len());
    for record in &records {
        entries.push(CommitGraphWriteEntry {
            oid: record.oid,
            tree: record.commit.tree,
            parents: record.parents.clone(),
            generation: commit_graph_generation(&record.oid, &record_map, &mut generation_cache)?,
            commit_time: commit_graph_commit_time(&record.commit)?,
        });
    }
    CommitGraph::write(format, &entries)
}

fn commit_graph_generation(
    oid: &ObjectId,
    records: &HashMap<ObjectId, &sley_rev::CommitRecord>,
    cache: &mut HashMap<ObjectId, u32>,
) -> Result<u32> {
    if let Some(generation) = cache.get(oid) {
        return Ok(*generation);
    }
    let record = records
        .get(oid)
        .ok_or_else(|| GitError::InvalidObject(format!("commit {oid} missing from walk")))?;
    let generation = if record.parents.is_empty() {
        1
    } else {
        record
            .parents
            .iter()
            .map(|parent| commit_graph_generation(parent, records, cache))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit generation overflow".into()))?
    };
    cache.insert(*oid, generation);
    Ok(generation)
}

fn commit_graph_commit_time(commit: &Commit) -> Result<u64> {
    commit_graph_commit_time_from_committer(&commit.committer)
}

fn commit_graph_commit_time_from_committer(committer: &[u8]) -> Result<u64> {
    let committer =
        std::str::from_utf8(committer).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let Some((before_tz, _tz)) = committer.rsplit_once(' ') else {
        return Err(GitError::InvalidFormat(
            "commit committer is missing timezone".into(),
        ));
    };
    let Some((_identity, timestamp)) = before_tz.rsplit_once(' ') else {
        return Err(GitError::InvalidFormat(
            "commit committer is missing timestamp".into(),
        ));
    };
    timestamp
        .parse::<u64>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))
}

fn cmd_bundle_create(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut path = None;
    let mut all = false;
    let mut revs = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" if path.is_none() => quiet = true,
            "--all" if path.is_some() => all = true,
            _ if path.is_none() => path = Some(arg),
            _ => revs.push(arg.clone()),
        }
    }
    let _ = quiet;
    let Some(path) = path else {
        return Err(GitError::Command("bundle create requires <file>".into()));
    };
    if !all && revs.is_empty() {
        return Err(GitError::Unsupported(
            "bundle create currently supports --all or explicit <rev> [^<rev>...]".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let selection = if all {
        bundle_all_revision_selection(&git_dir, format, &revs)?
    } else {
        bundle_revision_selection(&git_dir, format, &revs)?
    };
    if selection.references.is_empty() {
        return Err(GitError::Command("Refusing to create empty bundle.".into()));
    }
    let excluded = collect_reachable_object_ids(&db, format, selection.excludes)?;
    let Some(pack) = build_reachable_pack(&db, format, selection.starts, &excluded)? else {
        eprintln!("fatal: Refusing to create empty bundle.");
        return Err(GitError::Exit(128));
    };
    let bundle = Bundle {
        version: if format == ObjectFormat::Sha1 { 2 } else { 3 },
        format,
        capabilities: Vec::new(),
        prerequisites: selection.prerequisites,
        references: selection.references,
        pack: pack.pack,
    };
    fs::write(path, bundle.write()?)?;
    Ok(())
}

fn cmd_bundle_verify(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" if path.is_none() => quiet = true,
            _ if path.is_none() => path = Some(arg),
            _ => {
                return Err(GitError::Command(
                    "bundle verify requires [-q|--quiet] <file>".into(),
                ));
            }
        }
    }
    let Some(path) = path else {
        return Err(GitError::Command("bundle verify requires <file>".into()));
    };
    let cwd = env::current_dir()?;
    let git_dir = match discover_git_dir(&cwd) {
        Ok(git_dir) => git_dir,
        Err(_) => {
            eprintln!("error: need a repository to verify a bundle");
            return Err(GitError::Exit(1));
        }
    };
    let format = repository_object_format(&git_dir)?;
    let bundle = Bundle::parse(&fs::read(path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    verify_bundle_prerequisites_for_cli(&bundle, &db)?;
    if !quiet {
        print_bundle_verify_details(&bundle)?;
    }
    eprintln!("{path} is okay");
    Ok(())
}

fn cmd_bundle_list_heads(args: &[String]) -> Result<()> {
    let Some(path) = args.first() else {
        return Err(GitError::Command(
            "bundle list-heads requires <file>".into(),
        ));
    };
    let refs = &args[1..];
    let bundle = Bundle::parse_standalone(&fs::read(path)?)?;
    print_bundle_refs(&bundle.references, refs)
}

fn cmd_bundle_unbundle(args: &[String]) -> Result<()> {
    let mut progress = false;
    let mut path = None;
    let mut refs = Vec::new();
    for arg in args {
        if arg == "--progress" && path.is_none() {
            progress = true;
        } else if path.is_none() {
            path = Some(arg);
        } else {
            refs.push(arg.clone());
        }
    }
    let _ = progress;
    let Some(path) = path else {
        return Err(GitError::Command("bundle unbundle requires <file>".into()));
    };
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let bundle = Bundle::parse(&fs::read(path)?, format)?;
    let prerequisite_reader = FileObjectDatabase::from_git_dir(&git_dir, format);
    let database = FileObjectDatabase::from_git_dir(&git_dir, format);
    let result = install_bundle_pack(&bundle, &prerequisite_reader, &database)?;
    print_bundle_refs(&result.references, &refs)
}

fn print_bundle_refs(refs: &[BundleReference], filters: &[String]) -> Result<()> {
    for reference in refs {
        if filters.is_empty() || filters.iter().any(|filter| filter == &reference.name) {
            println!("{} {}", reference.oid, reference.name);
        }
    }
    Ok(())
}

fn print_bundle_verify_details(bundle: &Bundle) -> Result<()> {
    match bundle.references.len() {
        1 => println!("The bundle contains this ref:"),
        count => println!("The bundle contains these {count} refs:"),
    }
    print_bundle_refs(&bundle.references, &[])?;
    match bundle.prerequisites.len() {
        0 => println!("The bundle records a complete history."),
        1 => {
            println!("The bundle requires this ref:");
            print_bundle_prerequisites(bundle)?;
        }
        count => {
            println!("The bundle requires these {count} refs:");
            print_bundle_prerequisites(bundle)?;
        }
    }
    println!(
        "The bundle uses this hash algorithm: {}",
        bundle.format.name()
    );
    Ok(())
}

fn verify_bundle_prerequisites_for_cli(bundle: &Bundle, db: &FileObjectDatabase) -> Result<()> {
    let mut missing = Vec::new();
    for prerequisite in &bundle.prerequisites {
        match db.read_object(&prerequisite.oid) {
            Ok(object) => {
                let actual = object.object_id(bundle.format)?;
                if actual != prerequisite.oid {
                    return Err(GitError::InvalidObject(format!(
                        "bundle prerequisite {} hashes to {actual}",
                        prerequisite.oid
                    )));
                }
            }
            Err(GitError::NotFound(_)) => missing.push(prerequisite),
            Err(err) => return Err(err),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    eprintln!("error: Repository lacks these prerequisite commits:");
    for prerequisite in missing {
        eprintln!("error: {} ", prerequisite.oid);
    }
    Err(GitError::Exit(1))
}

fn print_bundle_prerequisites(bundle: &Bundle) -> Result<()> {
    for prerequisite in &bundle.prerequisites {
        println!("{} ", prerequisite.oid);
    }
    Ok(())
}

fn bundle_all_references(git_dir: &Path, format: ObjectFormat) -> Result<Vec<BundleReference>> {
    let store = FileRefStore::new(git_dir, format);
    let mut references = Vec::new();
    for reference in store.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target {
            references.push(BundleReference {
                oid,
                name: reference.name,
            });
        }
    }
    if let Ok(oid) = resolve_revision(git_dir, format, "HEAD") {
        references.push(BundleReference {
            oid,
            name: "HEAD".into(),
        });
    }
    Ok(references)
}

struct BundleCreateSelection {
    references: Vec<BundleReference>,
    prerequisites: Vec<BundlePrerequisite>,
    starts: Vec<ObjectId>,
    excludes: Vec<ObjectId>,
}

fn bundle_all_revision_selection(
    git_dir: &Path,
    format: ObjectFormat,
    revs: &[String],
) -> Result<BundleCreateSelection> {
    let references = bundle_all_references(git_dir, format)?;
    let mut starts = references
        .iter()
        .map(|reference| reference.oid)
        .collect::<Vec<_>>();
    let mut prerequisites = Vec::new();
    let mut excludes = Vec::new();
    for rev in revs {
        if let Some(excluded) = rev.strip_prefix('^') {
            if excluded.is_empty() {
                return Err(GitError::Command(
                    "bundle create excludes require a revision".into(),
                ));
            }
            let oid = resolve_revision(git_dir, format, excluded)?;
            prerequisites.push(BundlePrerequisite {
                oid,
                comment: Vec::new(),
            });
            excludes.push(oid);
        } else {
            starts.push(resolve_revision(git_dir, format, rev)?);
        }
    }
    Ok(BundleCreateSelection {
        references,
        prerequisites,
        starts,
        excludes,
    })
}

fn bundle_revision_selection(
    git_dir: &Path,
    format: ObjectFormat,
    revs: &[String],
) -> Result<BundleCreateSelection> {
    let mut references = Vec::new();
    let mut prerequisites = Vec::new();
    let mut starts = Vec::new();
    let mut excludes = Vec::new();
    for rev in revs {
        if let Some(excluded) = rev.strip_prefix('^') {
            if excluded.is_empty() {
                return Err(GitError::Command(
                    "bundle create excludes require a revision".into(),
                ));
            }
            let oid = resolve_revision(git_dir, format, excluded)?;
            prerequisites.push(BundlePrerequisite {
                oid,
                comment: Vec::new(),
            });
            excludes.push(oid);
        } else {
            let oid = resolve_revision(git_dir, format, rev)?;
            references.push(BundleReference {
                oid,
                name: rev.clone(),
            });
            starts.push(oid);
        }
    }
    Ok(BundleCreateSelection {
        references,
        prerequisites,
        starts,
        excludes,
    })
}

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

fn cmd_commit_tree(args: &[String]) -> Result<()> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut message_chunks = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" => {
                let Some(parent) = iter.next() else {
                    return commit_tree_parent_requires_value_error();
                };
                parents.push(parent.to_string());
            }
            value if value.starts_with("-p") && value.len() > 2 => {
                parents.push(value[2..].to_string());
            }
            "-m" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                let mut chunk = value.as_bytes()[2..].to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            "-F" => {
                let Some(path) = iter.next() else {
                    return commit_tree_file_requires_value_error();
                };
                message_chunks.push(read_commit_message_file(path)?);
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                message_chunks.push(read_commit_message_file(&value[2..])?);
            }
            "--no-gpg-sign" => {}
            value if tree.is_none() => tree = Some(value.to_string()),
            value if !value.starts_with('-') => return commit_tree_requires_one_tree_error(),
            value => {
                return Err(GitError::Command(format!(
                    "unexpected commit-tree argument {value}"
                )));
            }
        }
    }
    let Some(tree) = tree else {
        return commit_tree_requires_one_tree_error();
    };
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let tree = ObjectId::from_hex(format, &tree)?;
    let parents = parents
        .iter()
        .map(|parent| ObjectId::from_hex(format, parent))
        .collect::<Result<Vec<_>>>()?;
    let message = if message_chunks.is_empty() {
        let mut message = Vec::new();
        io::stdin().read_to_end(&mut message)?;
        message
    } else {
        commit_message_from_prepared_chunks(&message_chunks)
    };
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer,
            message,
        },
    )?;
    println!("{oid}");
    Ok(())
}

fn commit_tree_parent_requires_value_error() -> Result<()> {
    eprintln!("error: switch `p' requires a value");
    Err(GitError::Exit(129))
}

fn commit_tree_file_requires_value_error() -> Result<()> {
    eprintln!("error: switch `F' requires a value");
    Err(GitError::Exit(129))
}

fn commit_tree_requires_one_tree_error() -> Result<()> {
    eprintln!("fatal: must give exactly one tree");
    Err(GitError::Exit(128))
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
    stdout: &mut io::Stdout,
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
    stdout: &mut io::Stdout,
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
    if zero {
        "0".repeat(zero_width)
    } else {
        oid.map(|oid| {
            let hex = oid.to_hex();
            let width = abbrev.unwrap_or(hex.len()).min(hex.len());
            hex[..width].to_string()
        })
        .unwrap_or_else(|| "0".repeat(zero_width))
    }
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
}

fn write_diff_patch_entry(
    stdout: &mut io::Stdout,
    entry: &sley_diff_merge::NameStatusEntry,
    options: DiffPatchOptions<'_>,
) -> Result<()> {
    let old_content = diff_entry_old_content(entry, options.db)?;
    let new_content = diff_entry_new_content(
        entry,
        options.db,
        options.worktree_root,
        options.use_worktree_new,
    )?;
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
    if !content_changed {
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
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            writeln!(stdout, "--- /dev/null")?;
        }
        _ => {
            writeln!(stdout, "--- {old_header_path}")?;
        }
    }
    match entry.status {
        sley_diff_merge::NameStatus::Deleted => {
            writeln!(stdout, "+++ /dev/null")?;
        }
        _ => {
            writeln!(stdout, "+++ {header_path}")?;
        }
    }
    write_diff_full_file_hunk(stdout, old_content.as_deref(), new_content.as_deref())?;
    Ok(())
}

fn write_diff_binary_patch_entry(
    stdout: &mut io::Stdout,
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
    stdout: &mut io::Stdout,
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

fn write_diff_full_file_hunk(
    stdout: &mut io::Stdout,
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
) -> Result<()> {
    let old_lines = old_content.map(diff_lines).unwrap_or_default();
    let new_lines = new_content.map(diff_lines).unwrap_or_default();
    writeln!(
        stdout,
        "@@ -{} +{} @@",
        diff_hunk_range(old_lines.len()),
        diff_hunk_range(new_lines.len())
    )?;
    write_diff_lcs_lines(stdout, &old_lines, &new_lines)?;
    Ok(())
}

fn diff_hunk_range(line_count: usize) -> String {
    match line_count {
        0 => "0,0".to_string(),
        1 => "1".to_string(),
        _ => format!("1,{line_count}"),
    }
}

fn write_diff_lcs_lines(
    stdout: &mut io::Stdout,
    old_lines: &[&[u8]],
    new_lines: &[&[u8]],
) -> Result<()> {
    let mut lengths = vec![vec![0; new_lines.len() + 1]; old_lines.len() + 1];
    for old_idx in (0..old_lines.len()).rev() {
        for new_idx in (0..new_lines.len()).rev() {
            lengths[old_idx][new_idx] = if old_lines[old_idx] == new_lines[new_idx] {
                lengths[old_idx + 1][new_idx + 1] + 1
            } else {
                lengths[old_idx + 1][new_idx].max(lengths[old_idx][new_idx + 1])
            };
        }
    }
    let mut old_idx = 0;
    let mut new_idx = 0;
    while old_idx < old_lines.len() && new_idx < new_lines.len() {
        if old_lines[old_idx] == new_lines[new_idx] {
            write_diff_patch_line(stdout, b' ', old_lines[old_idx])?;
            old_idx += 1;
            new_idx += 1;
        } else if lengths[old_idx + 1][new_idx] >= lengths[old_idx][new_idx + 1] {
            write_diff_patch_line(stdout, b'-', old_lines[old_idx])?;
            old_idx += 1;
        } else {
            write_diff_patch_line(stdout, b'+', new_lines[new_idx])?;
            new_idx += 1;
        }
    }
    while old_idx < old_lines.len() {
        write_diff_patch_line(stdout, b'-', old_lines[old_idx])?;
        old_idx += 1;
    }
    while new_idx < new_lines.len() {
        write_diff_patch_line(stdout, b'+', new_lines[new_idx])?;
        new_idx += 1;
    }
    Ok(())
}

fn write_diff_patch_line(stdout: &mut io::Stdout, prefix: u8, line: &[u8]) -> Result<()> {
    stdout.write_all(&[prefix])?;
    stdout.write_all(line)?;
    if !line.ends_with(b"\n") {
        stdout.write_all(b"\n\\ No newline at end of file\n")?;
    }
    Ok(())
}

fn write_diff_numstat_entry(
    stdout: &mut io::Stdout,
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
            let old_path = status_quote_path(old_path, false);
            let path = status_quote_path(&entry.path, false);
            writeln!(stdout, "{old_path} => {path}")?;
        } else {
            let path = status_quote_path(&entry.path, false);
            writeln!(stdout, "{path}")?;
        }
    }
    Ok(())
}

fn write_diff_numstat_counts(stdout: &mut io::Stdout, stats: DiffLineStats) -> Result<()> {
    match stats {
        DiffLineStats::Binary => write!(stdout, "-\t-\t")?,
        DiffLineStats::Text { inserted, deleted } => write!(stdout, "{inserted}\t{deleted}\t")?,
    }
    Ok(())
}

fn write_diff_shortstat(
    stdout: &mut io::Stdout,
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
    write!(
        stdout,
        " {} {} changed",
        entries.len(),
        plural(entries.len(), "file", "files")
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

fn write_diff_stat(
    stdout: &mut io::Stdout,
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    options: DiffStatOptions,
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
    let displayed_rows = if let Some(count) = stat_count {
        rows.len().min(count)
    } else {
        rows.len()
    };
    let path_width = rows
        .iter()
        .take(displayed_rows)
        .map(|row| row.path.len())
        .max()
        .unwrap_or(0);
    let has_binary_rows = rows
        .iter()
        .take(displayed_rows)
        .any(|row| matches!(row.stats, DiffStatStats::Binary { .. }));
    let count_width = rows
        .iter()
        .take(displayed_rows)
        .filter_map(|row| match row.stats {
            DiffStatStats::Text { inserted, deleted } => Some(inserted + deleted),
            DiffStatStats::Binary { .. } => None,
        })
        .map(|count| count.to_string().len())
        .max()
        .unwrap_or(1);
    let count_width = if has_binary_rows {
        count_width.max(3)
    } else {
        count_width
    };
    for row in rows.iter().take(displayed_rows) {
        match row.stats {
            DiffStatStats::Binary {
                old_size,
                new_size,
                unchanged,
            } => {
                if unchanged {
                    // git prints just `Bin` (no ` N -> M bytes`) when the binary
                    // blob is identical on both sides -- e.g. a pure mode change.
                    writeln!(stdout, " {:path_width$} | Bin", row.path)?;
                } else {
                    let old_size = color_stat_deleted(&old_size.to_string(), color);
                    let new_size = color_stat_inserted(&new_size.to_string(), color);
                    writeln!(
                        stdout,
                        " {:path_width$} | Bin {old_size} -> {new_size} bytes",
                        row.path
                    )?;
                }
            }
            DiffStatStats::Text { inserted, deleted } => {
                let count = inserted + deleted;
                if count == 0 {
                    writeln!(stdout, " {:path_width$} | {count:>count_width$}", row.path)?;
                } else {
                    let graph = diff_stat_graph(inserted, deleted, color);
                    writeln!(
                        stdout,
                        " {:path_width$} | {count:>count_width$} {graph}",
                        row.path
                    )?;
                }
            }
        }
    }
    if displayed_rows < rows.len() {
        writeln!(stdout, " ...")?;
    }
    write_diff_shortstat(stdout, entries, db, worktree_root, use_worktree_new)
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
        format!(
            "{} => {}",
            status_quote_path(old_path, false),
            status_quote_path(&entry.path, false)
        )
    } else {
        status_quote_path(&entry.path, false)
    };
    if compact_summary && let Some(summary) = diff_compact_summary_label(entry) {
        path.push(' ');
        path.push_str(summary);
    }
    path
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

fn diff_stat_graph(inserted: usize, deleted: usize, color: bool) -> String {
    let mut graph = String::with_capacity(inserted + deleted);
    if inserted > 0 {
        let pluses = std::iter::repeat_n('+', inserted).collect::<String>();
        graph.push_str(&color_stat_inserted(&pluses, color));
    }
    if deleted > 0 {
        let minuses = std::iter::repeat_n('-', deleted).collect::<String>();
        graph.push_str(&color_stat_deleted(&minuses, color));
    }
    graph
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

fn count_line_diff(old: &[u8], new: &[u8]) -> (usize, usize) {
    let old_lines = diff_lines(old);
    let new_lines = diff_lines(new);
    let common = lcs_len(&old_lines, &new_lines);
    (new_lines.len() - common, old_lines.len() - common)
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

fn lcs_len(left: &[&[u8]], right: &[&[u8]]) -> usize {
    let mut previous = vec![0; right.len() + 1];
    let mut current = vec![0; right.len() + 1];
    for left_line in left {
        for (idx, right_line) in right.iter().enumerate() {
            current[idx + 1] = if left_line == right_line {
                previous[idx] + 1
            } else {
                previous[idx + 1].max(current[idx])
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(0);
    }
    previous[right.len()]
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
        self.filters.iter().any(|filter| filter.matches(path))
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

fn version_sort_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut left_idx = 0;
    let mut right_idx = 0;
    while left_idx < left.len() && right_idx < right.len() {
        let left_digit = left[left_idx].is_ascii_digit();
        let right_digit = right[right_idx].is_ascii_digit();
        if left_digit && right_digit {
            let left_start = left_idx;
            let right_start = right_idx;
            while left_idx < left.len() && left[left_idx].is_ascii_digit() {
                left_idx += 1;
            }
            while right_idx < right.len() && right[right_idx].is_ascii_digit() {
                right_idx += 1;
            }
            let ordering = version_sort_number_cmp(
                &left[left_start..left_idx],
                &right[right_start..right_idx],
            );
            if !ordering.is_eq() {
                return ordering;
            }
        } else {
            let ordering = left[left_idx].cmp(&right[right_idx]);
            if !ordering.is_eq() {
                return ordering;
            }
            left_idx += 1;
            right_idx += 1;
        }
    }
    left.len().cmp(&right.len())
}

fn version_sort_number_cmp(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    let left_trimmed = trim_version_sort_leading_zeroes(left);
    let right_trimmed = trim_version_sort_leading_zeroes(right);
    left_trimmed
        .len()
        .cmp(&right_trimmed.len())
        .then_with(|| left_trimmed.cmp(right_trimmed))
        .then_with(|| left.len().cmp(&right.len()))
}

fn trim_version_sort_leading_zeroes(value: &[u8]) -> &[u8] {
    let first_non_zero = value
        .iter()
        .position(|byte| *byte != b'0')
        .unwrap_or(value.len().saturating_sub(1));
    &value[first_non_zero..]
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
    let Some(upstream_target) = store.read_ref(upstream)? else {
        return Ok(None);
    };
    let upstream_ref = sley_refs::Ref {
        name: upstream.to_string(),
        target: upstream_target,
    };
    let Some((upstream_oid, _)) = resolve_for_each_ref_target(store, &upstream_ref)? else {
        return Ok(None);
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
    Ok(Some(ForEachRefTrack { ahead, behind }))
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
    write_for_each_ref_format(stdout, format_spec, context.quote, |stdout, atom| {
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
            "upstream:track,nobracket" => {
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
            "push:track,nobracket" => {
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
                if let Some(contents) = &context.contents {
                    stdout.write_all(commit_subject(&contents.message).as_bytes())?;
                }
            }
            "*subject" | "*contents:subject" => {
                if let Some(message) = context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_ref())
                {
                    stdout.write_all(commit_subject(message).as_bytes())?;
                }
            }
            "contents:body" => {
                if let Some(contents) = &context.contents {
                    stdout.write_all(commit_body(&contents.message))?;
                }
            }
            "*contents:body" => {
                if let Some(message) = context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_ref())
                {
                    stdout.write_all(commit_body(message))?;
                }
            }
            "body" => {
                if let Some(contents) = &context.contents {
                    stdout.write_all(commit_body(&contents.message))?;
                }
            }
            "*body" => {
                if let Some(message) = context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_ref())
                {
                    stdout.write_all(commit_body(message))?;
                }
            }
            "contents" => {
                if let Some(contents) = &context.contents {
                    stdout.write_all(&contents.message)?;
                }
            }
            "*contents" => {
                if let Some(message) = context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_ref())
                {
                    stdout.write_all(message)?;
                }
            }
            "contents:size" => {
                if let Some(contents) = &context.contents {
                    write!(stdout, "{}", contents.message.len())?;
                }
            }
            "*contents:size" => {
                if let Some(message) = context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.message.as_ref())
                {
                    write!(stdout, "{}", message.len())?;
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
            "authorname" => write_for_each_ref_identity_name(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.author.as_deref()),
            )?,
            "*authorname" => write_for_each_ref_identity_name(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.author.as_deref()),
            )?,
            "authoremail" => write_for_each_ref_identity_email(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.author.as_deref()),
            )?,
            "*authoremail" => write_for_each_ref_identity_email(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.author.as_deref()),
            )?,
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
            "committername" => write_for_each_ref_identity_name(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.committer.as_deref()),
            )?,
            "*committername" => write_for_each_ref_identity_name(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.committer.as_deref()),
            )?,
            "committeremail" => write_for_each_ref_identity_email(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.committer.as_deref()),
            )?,
            "*committeremail" => write_for_each_ref_identity_email(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.committer.as_deref()),
            )?,
            "tagger" => write_for_each_ref_identity(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tagger.as_deref()),
            )?,
            "*tagger" => write_for_each_ref_identity(stdout, None)?,
            "taggername" => write_for_each_ref_identity_name(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tagger.as_deref()),
            )?,
            "*taggername" => write_for_each_ref_identity_name(stdout, None)?,
            "taggeremail" => write_for_each_ref_identity_email(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tagger.as_deref()),
            )?,
            "*taggeremail" => write_for_each_ref_identity_email(stdout, None)?,
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
            "authordate" => write_for_each_ref_identity_date(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.author.as_deref()),
            )?,
            "*authordate" => write_for_each_ref_identity_date(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.author.as_deref()),
            )?,
            "committerdate" => write_for_each_ref_identity_date(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.committer.as_deref()),
            )?,
            "*committerdate" => write_for_each_ref_identity_date(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.committer.as_deref()),
            )?,
            "taggerdate" => write_for_each_ref_identity_date(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tagger.as_deref()),
            )?,
            "*taggerdate" => write_for_each_ref_identity_date(stdout, None)?,
            "creatordate" => write_for_each_ref_identity_date(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.creator.as_deref()),
            )?,
            "*creatordate" => write_for_each_ref_identity_date(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.creator.as_deref()),
            )?,
            "authordate:raw" => write_for_each_ref_identity_date_raw(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.author.as_deref()),
            )?,
            "*authordate:raw" => write_for_each_ref_identity_date_raw(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.author.as_deref()),
            )?,
            "committerdate:raw" => write_for_each_ref_identity_date_raw(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.committer.as_deref()),
            )?,
            "*committerdate:raw" => write_for_each_ref_identity_date_raw(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.committer.as_deref()),
            )?,
            "taggerdate:raw" => write_for_each_ref_identity_date_raw(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.tagger.as_deref()),
            )?,
            "*taggerdate:raw" => write_for_each_ref_identity_date_raw(stdout, None)?,
            "creatordate:raw" => write_for_each_ref_identity_date_raw(
                stdout,
                context
                    .contents
                    .as_ref()
                    .and_then(|contents| contents.creator.as_deref()),
            )?,
            "*creatordate:raw" => write_for_each_ref_identity_date_raw(
                stdout,
                context
                    .peeled_object
                    .as_ref()
                    .and_then(|peeled| peeled.creator.as_deref()),
            )?,
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
                } else if let Some((identity, mode)) = for_each_ref_date_modifier(other, context) {
                    write_for_each_ref_identity_date_mode(stdout, identity, mode)?;
                } else if let Some((identity, mode)) = for_each_ref_email_modifier(other, context) {
                    write_for_each_ref_identity_email_mode(stdout, identity, mode)?;
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

fn for_each_ref_date_modifier<'a>(
    placeholder: &str,
    context: &'a ForEachRefFormatContext<'_>,
) -> Option<(Option<&'a [u8]>, ForEachRefDateMode)> {
    let (atom, modifier) = placeholder.split_once(':')?;
    let mode = match modifier {
        "raw" => ForEachRefDateMode::Raw,
        "unix" => ForEachRefDateMode::Unix,
        "short" => ForEachRefDateMode::Short,
        "iso" | "iso8601" => ForEachRefDateMode::Iso,
        "iso8601-strict" => ForEachRefDateMode::IsoStrict,
        "rfc2822" => ForEachRefDateMode::Rfc2822,
        _ => return None,
    };
    let contents = context.contents.as_ref();
    let identity = match atom {
        "authordate" => contents.and_then(|contents| contents.author.as_deref()),
        "committerdate" => contents.and_then(|contents| contents.committer.as_deref()),
        "taggerdate" => contents.and_then(|contents| contents.tagger.as_deref()),
        "creatordate" => contents.and_then(|contents| contents.creator.as_deref()),
        "*authordate" => context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.author.as_deref()),
        "*committerdate" => context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.committer.as_deref()),
        "*taggerdate" => None,
        "*creatordate" => context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.creator.as_deref()),
        _ => return None,
    };
    Some((identity, mode))
}

fn for_each_ref_email_modifier<'a>(
    placeholder: &str,
    context: &'a ForEachRefFormatContext<'_>,
) -> Option<(Option<&'a [u8]>, ForEachRefEmailMode)> {
    let (atom, modifier) = placeholder.split_once(':')?;
    let mode = match modifier {
        "trim" => ForEachRefEmailMode::Trim,
        "localpart" => ForEachRefEmailMode::LocalPart,
        _ => return None,
    };
    let contents = context.contents.as_ref();
    let identity = match atom {
        "authoremail" => contents.and_then(|contents| contents.author.as_deref()),
        "committeremail" => contents.and_then(|contents| contents.committer.as_deref()),
        "taggeremail" => contents.and_then(|contents| contents.tagger.as_deref()),
        "*authoremail" => context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.author.as_deref()),
        "*committeremail" => context
            .peeled_object
            .as_ref()
            .and_then(|peeled| peeled.committer.as_deref()),
        "*taggeremail" => None,
        _ => return None,
    };
    Some((identity, mode))
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
        Ok(Self {
            prefix,
            full_name,
            filters,
            cwd_depth,
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
            if filter.matches(path) {
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
    fn matches(&self, path: &[u8]) -> bool {
        sley_worktree::untracked_pathspec_matches(
            &sley_worktree::UntrackedPathspecFilter {
                path: self.path.clone(),
                recursive: self.recursive,
                is_glob: self.is_glob,
            },
            path,
        )
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
    Topo,
    Date,
}

fn rev_list_topo_order(records: Vec<&sley_rev::CommitRecord>) -> Vec<&sley_rev::CommitRecord> {
    rev_list_ready_order(records, |idx| idx)
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
}

#[derive(Debug, Clone, Copy)]
enum SimpleLogRegexMode {
    Basic,
    Fixed,
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
        let alternatives = match mode {
            SimpleLogRegexMode::Basic => split_log_regex_alternatives(pattern)
                .into_iter()
                .map(|alternative| SimpleLogRegexAlternative::parse(alternative, error_context))
                .collect::<Result<Vec<_>>>()?,
            SimpleLogRegexMode::Fixed => vec![SimpleLogRegexAlternative::parse_fixed(pattern)],
        };
        Ok(Self { alternatives })
    }

    fn is_match(&self, value: &str, ignore_case: bool) -> bool {
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
    io::stdout().write_all(&line)?;
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
    let LogFormatContext {
        abbrev_len,
        decorations,
        marker,
        dialect,
        source,
        date_mode,
    } = *context;
    let (author_name, author_email) = commit_identity_name_email(&record.commit.author);
    let (committer_name, committer_email) = commit_identity_name_email(&record.commit.committer);
    let author_timestamp = commit_identity_timestamp(&record.commit.author);
    let committer_timestamp = commit_identity_timestamp(&record.commit.committer);

    for token in &compiled.tokens[token_range] {
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
                write!(out, "{}", commit_subject(&record.commit.message))
                    .map_err(io::Error::from)?;
            }
            FormatToken::SanitizedSubject => {
                write!(out, "{}", log_sanitized_subject(&record.commit.message))
                    .map_err(io::Error::from)?;
            }
            FormatToken::Encoding => {
                write!(out, "{}", commit_encoding(&record.commit)).map_err(io::Error::from)?;
            }
            FormatToken::NoteName if dialect == LogFormatDialect::Log => {}
            FormatToken::NoteName => out.extend_from_slice(b"%N"),
            FormatToken::RevisionSource if dialect == LogFormatDialect::Log => {
                if let Some(source) = source {
                    out.extend_from_slice(source.as_bytes());
                }
            }
            FormatToken::RevisionSource => out.extend_from_slice(b"%S"),
            FormatToken::ColorParen | FormatToken::ColorName(_) => {}
            FormatToken::Body => out.extend_from_slice(commit_body(&record.commit.message)),
            FormatToken::FullMessage => out.extend_from_slice(&record.commit.message),
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
            | FormatToken::CommitterDateRfc2822 => {}
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

fn cmd_submodule(args: &[String]) -> Result<()> {
    let mut index = 0;
    let mut quiet = false;
    while matches!(args.get(index).map(String::as_str), Some("--quiet" | "-q")) {
        quiet = true;
        index += 1;
    }
    if matches!(args.get(index).map(String::as_str), Some("status")) {
        index += 1;
        return cmd_submodule_status(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("init")) {
        index += 1;
        return cmd_submodule_init(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("deinit")) {
        index += 1;
        return cmd_submodule_deinit(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("sync")) {
        index += 1;
        return cmd_submodule_sync(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("absorbgitdirs")) {
        index += 1;
        return cmd_submodule_absorbgitdirs(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("foreach")) {
        index += 1;
        return cmd_submodule_foreach(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("summary")) {
        index += 1;
        return cmd_submodule_summary(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("set-branch")) {
        index += 1;
        return cmd_submodule_set_branch(&args[index..], quiet);
    }
    if matches!(args.get(index).map(String::as_str), Some("set-url")) {
        index += 1;
        return cmd_submodule_set_url(&args[index..], quiet);
    }
    cmd_submodule_status(&args[index..], quiet)
}

#[derive(Debug)]
struct SubmoduleStatusOptions<'a> {
    cached: bool,
    quiet: bool,
    recursive: bool,
    paths: Vec<&'a str>,
}

#[derive(Debug)]
struct SubmoduleConfigEntry {
    name: String,
    path: String,
    url: Option<String>,
    update: Option<String>,
}

#[derive(Debug)]
struct SubmoduleStatusEntry {
    path: String,
    display_path: String,
}

fn cmd_submodule_status(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_status_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodules(&cwd, &worktree_root, submodules, &options.paths)?;
    if quiet || options.quiet {
        return Ok(());
    }
    let index = read_repository_index(&git_dir, format)?;
    for submodule in selected {
        print_submodule_status_tree(
            &cwd,
            &worktree_root,
            &index,
            &submodule,
            options.cached,
            options.recursive,
        )?;
    }
    Ok(())
}

fn parse_submodule_status_options(args: &[String]) -> Result<SubmoduleStatusOptions<'_>> {
    let mut cached = false;
    let mut quiet = false;
    let mut recursive = false;
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--cached" => cached = true,
            "--quiet" | "-q" => quiet = true,
            "--recursive" => recursive = true,
            "--no-recursive" => return submodule_usage(),
            value if value.starts_with('-') => {
                return submodule_usage();
            }
            value => paths.push(value),
        }
    }
    Ok(SubmoduleStatusOptions {
        cached,
        quiet,
        recursive,
        paths,
    })
}

fn submodule_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git submodule [--quiet] [--cached]\n   or: git submodule [--quiet] add [-b <branch>] [-f|--force] [--name <name>] [--reference <repository>] [--] <repository> [<path>]\n   or: git submodule [--quiet] status [--cached] [--recursive] [--] [<path>...]\n   or: git submodule [--quiet] init [--] [<path>...]\n   or: git submodule [--quiet] deinit [-f|--force] (--all| [--] <path>...)\n   or: git submodule [--quiet] update [--init [--filter=<filter-spec>]] [--remote] [-N|--no-fetch] [-f|--force] [--checkout|--merge|--rebase] [--[no-]recommend-shallow] [--reference <repository>] [--recursive] [--[no-]single-branch] [--] [<path>...]\n   or: git submodule [--quiet] set-branch (--default|--branch <branch>) [--] <path>\n   or: git submodule [--quiet] set-url [--] <path> <newurl>\n   or: git submodule [--quiet] summary [--cached|--files] [--summary-limit <n>] [commit] [--] [<path>...]\n   or: git submodule [--quiet] foreach [--recursive] <command>\n   or: git submodule [--quiet] sync [--recursive] [--] [<path>...]\n   or: git submodule [--quiet] absorbgitdirs [--] [<path>...]"
    );
    Err(GitError::Exit(1))
}

fn cmd_submodule_init(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet) = parse_submodule_init_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    let mut config = read_repo_config(&git_dir)?;
    let mut changed = false;
    for submodule in selected {
        if config
            .get("submodule", Some(&submodule.name), "url")
            .is_some()
        {
            continue;
        }
        let Some(url) = &submodule.url else {
            continue;
        };
        let url = resolve_submodule_init_url(&worktree_root, &config, url);
        set_submodule_config_value(&mut config, &submodule.name, "active", "true");
        set_submodule_config_value(&mut config, &submodule.name, "url", &url);
        if let Some(update) = &submodule.update {
            set_submodule_config_value(&mut config, &submodule.name, "update", update);
        }
        if !quiet {
            eprintln!(
                "Submodule '{}' ({}) registered for path '{}'",
                submodule.name, url, submodule.path
            );
        }
        changed = true;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

fn cmd_submodule_deinit(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_deinit_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = if options.all {
        submodules.iter().collect::<Vec<_>>()
    } else {
        if options.paths.is_empty() {
            eprintln!("fatal: Use '--all' if you really want to deinitialize all submodules");
            return Err(GitError::Exit(128));
        }
        filter_submodule_configs(&cwd, &worktree_root, &submodules, &options.paths)?
    };
    let mut config = read_repo_config(&git_dir)?;
    let mut changed = false;
    for submodule in selected {
        let Some(url) = config
            .get("submodule", Some(&submodule.name), "url")
            .map(str::to_string)
            .or_else(|| submodule.url.clone())
        else {
            continue;
        };
        if !options.force && submodule_worktree_has_local_changes(&worktree_root, submodule)? {
            eprintln!("error: the following file has local modifications:");
            eprintln!("    {}", submodule.path);
            eprintln!("(use --cached to keep the file, or -f to force removal)");
            eprintln!(
                "fatal: Submodule work tree '{}' contains local modifications; use '-f' to discard them",
                submodule.path
            );
            return Err(GitError::Exit(128));
        }
        clear_submodule_worktree(&worktree_root.join(&submodule.path))?;
        remove_submodule_config_section(&mut config, &submodule.name);
        if !options.quiet {
            println!("Cleared directory '{}'", submodule.path);
            println!(
                "Submodule '{}' ({}) unregistered for path '{}'",
                submodule.name, url, submodule.path
            );
        }
        changed = true;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

fn cmd_submodule_sync(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet, _recursive) = parse_submodule_sync_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    let mut config = read_repo_config(&git_dir)?;
    let mut changed = false;
    for submodule in selected {
        if config
            .get("submodule", Some(&submodule.name), "url")
            .is_none()
        {
            continue;
        }
        let Some(url) = &submodule.url else {
            continue;
        };
        let url = resolve_submodule_sync_url(&worktree_root, &config, url);
        set_submodule_config_value(&mut config, &submodule.name, "url", &url);
        if !quiet {
            println!("Synchronizing submodule url for '{}'", submodule.path);
        }
        changed = true;
    }
    if changed {
        write_repo_config(&git_dir, &config)?;
    }
    Ok(())
}

fn cmd_submodule_absorbgitdirs(args: &[String], quiet: bool) -> Result<()> {
    let (paths, quiet) = parse_submodule_absorbgitdirs_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let selected = filter_submodule_configs(&cwd, &worktree_root, &submodules, &paths)?;
    for submodule in selected {
        absorb_submodule_git_dir(&git_dir, &worktree_root, submodule, quiet)?;
    }
    Ok(())
}

fn cmd_submodule_foreach(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_foreach_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let submodules = read_submodule_configs(&worktree_root)?;
    let index = read_repository_index(&git_dir, format)?;
    run_submodule_foreach_tree(&cwd, &worktree_root, &index, &submodules, &options)
}

fn cmd_submodule_summary(args: &[String], quiet: bool) -> Result<()> {
    let options = parse_submodule_summary_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    if options.quiet || options.summary_limit == Some(0) {
        return Ok(());
    }
    let submodules = read_submodule_configs(&worktree_root)?;
    let index = read_repository_index(&git_dir, format)?;
    let selected = select_submodules_for_summary(&cwd, &worktree_root, &submodules, &options);
    for submodule in selected {
        print_submodule_summary(
            &cwd,
            &git_dir,
            &worktree_root,
            &index,
            submodule,
            options.cached,
            options.summary_limit,
        )?;
    }
    Ok(())
}

fn cmd_submodule_set_url(args: &[String], quiet: bool) -> Result<()> {
    let (path, new_url, quiet) = parse_submodule_set_url_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let gitmodules_path = worktree_root.join(".gitmodules");
    let mut gitmodules = GitConfig::read(&gitmodules_path)?;
    let Some(name) = submodule_name_for_exact_path(&gitmodules, path) else {
        eprintln!("fatal: no submodule mapping found in .gitmodules for path '{path}'");
        return Err(GitError::Exit(128));
    };
    set_submodule_config_value(&mut gitmodules, &name, "url", new_url);
    fs::write(&gitmodules_path, gitmodules.to_canonical_bytes())?;

    let mut config = read_repo_config(&git_dir)?;
    if config.get("submodule", Some(&name), "url").is_some() {
        set_submodule_config_value(&mut config, &name, "url", new_url);
        write_repo_config(&git_dir, &config)?;
        if !quiet {
            println!("Synchronizing submodule url for '{path}'");
        }
    }
    Ok(())
}

enum SubmoduleSetBranchAction<'a> {
    Branch(&'a str),
    Default,
}

fn cmd_submodule_set_branch(args: &[String], quiet: bool) -> Result<()> {
    let (path, action, _quiet) = parse_submodule_set_branch_options(args, quiet)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let gitmodules_path = worktree_root.join(".gitmodules");
    let mut gitmodules = GitConfig::read(&gitmodules_path)?;
    let Some(name) = submodule_name_for_exact_path(&gitmodules, path) else {
        eprintln!("fatal: no submodule mapping found in .gitmodules for path '{path}'");
        return Err(GitError::Exit(128));
    };
    match action {
        SubmoduleSetBranchAction::Branch(branch) => {
            set_submodule_config_value(&mut gitmodules, &name, "branch", branch);
        }
        SubmoduleSetBranchAction::Default => {
            unset_submodule_config_value(&mut gitmodules, &name, "branch");
        }
    }
    fs::write(&gitmodules_path, gitmodules.to_canonical_bytes())?;
    Ok(())
}

fn parse_submodule_init_options(args: &[String], mut quiet: bool) -> Result<(Vec<&str>, bool)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet))
}

struct SubmoduleDeinitOptions<'a> {
    all: bool,
    force: bool,
    quiet: bool,
    paths: Vec<&'a str>,
}

struct SubmoduleForeachOptions {
    command: String,
    quiet: bool,
    recursive: bool,
}

struct SubmoduleSummaryOptions {
    cached: bool,
    quiet: bool,
    summary_limit: Option<isize>,
    positionals: Vec<String>,
}

fn parse_submodule_deinit_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleDeinitOptions<'_>> {
    let mut all = false;
    let mut force = false;
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--all" => all = true,
            "--quiet" | "-q" => quiet = true,
            "-f" | "--force" => force = true,
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok(SubmoduleDeinitOptions {
        all,
        force,
        quiet,
        paths,
    })
}

fn parse_submodule_set_branch_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(&str, SubmoduleSetBranchAction<'_>, bool)> {
    let mut branch = None;
    let mut default = false;
    let mut values = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            values.push(arg.as_str());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--default" | "-d" => default = true,
            "--branch" | "-b" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                branch = Some(value.as_str());
            }
            value if let Some(value) = value.strip_prefix("--branch=") => {
                branch = Some(value);
            }
            "--no-default" | "--no-branch" => return submodule_usage(),
            value if value.starts_with('-') => return submodule_usage(),
            value => values.push(value),
        }
        index += 1;
    }
    if branch.is_none() && !default {
        eprintln!("fatal: --branch or --default required");
        return Err(GitError::Exit(128));
    }
    if branch.is_some() && default {
        eprintln!("fatal: options '--branch' and '--default' cannot be used together");
        return Err(GitError::Exit(128));
    }
    match (values.as_slice(), branch, default) {
        ([path], Some(branch), false) => {
            Ok((path, SubmoduleSetBranchAction::Branch(branch), quiet))
        }
        ([path], None, true) => Ok((path, SubmoduleSetBranchAction::Default, quiet)),
        _ => submodule_set_branch_usage(),
    }
}

fn submodule_set_branch_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git submodule set-branch [-q|--quiet] (-d|--default) <path>\n   or: git submodule set-branch [-q|--quiet] (-b|--branch) <branch> <path>\n\n    -d, --[no-]default    set the default tracking branch to master\n    -b, --[no-]branch <branch>\n                          set the default tracking branch\n"
    );
    Err(GitError::Exit(129))
}

fn parse_submodule_set_url_options(args: &[String], mut quiet: bool) -> Result<(&str, &str, bool)> {
    let mut values = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            values.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--no-quiet" => quiet = false,
            value if value.starts_with('-') => return submodule_set_url_usage(),
            value => values.push(value),
        }
    }
    match values.as_slice() {
        [path, new_url] => Ok((path, new_url, quiet)),
        _ => submodule_set_url_usage(),
    }
}

fn submodule_set_url_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git submodule set-url [--quiet] <path> <newurl>\n\n    -q, --[no-]quiet      suppress output for setting url of a submodule\n"
    );
    Err(GitError::Exit(129))
}

fn parse_submodule_sync_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool, bool)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    let mut recursive = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--recursive" => recursive = true,
            "--no-recursive" => return submodule_usage(),
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet, recursive))
}

fn parse_submodule_absorbgitdirs_options(
    args: &[String],
    mut quiet: bool,
) -> Result<(Vec<&str>, bool)> {
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            value if value.starts_with('-') => return submodule_usage(),
            value => paths.push(value),
        }
    }
    Ok((paths, quiet))
}

fn parse_submodule_foreach_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleForeachOptions> {
    let mut recursive = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--quiet" | "-q" => {
                quiet = true;
                index += 1;
            }
            "--recursive" => {
                recursive = true;
                index += 1;
            }
            "--" => {
                index += 1;
                break;
            }
            value if value.starts_with('-') => return submodule_usage(),
            _ => break,
        }
    }
    Ok(SubmoduleForeachOptions {
        command: args[index..].join(" "),
        quiet,
        recursive,
    })
}

fn parse_submodule_summary_options(
    args: &[String],
    mut quiet: bool,
) -> Result<SubmoduleSummaryOptions> {
    let mut cached = false;
    let mut files = false;
    let mut summary_limit = None;
    let mut positionals = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if positional_only {
            positionals.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--quiet" | "-q" => quiet = true,
            "--cached" => cached = true,
            "--files" => files = true,
            "--summary-limit" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return submodule_usage();
                };
                summary_limit = Some(parse_submodule_summary_limit(value)?);
            }
            value if let Some(value) = value.strip_prefix("--summary-limit=") => {
                summary_limit = Some(parse_submodule_summary_limit(value)?);
            }
            value if value.starts_with('-') => return submodule_usage(),
            value => positionals.push(value.to_string()),
        }
        index += 1;
    }
    if cached && files {
        eprintln!("fatal: options '--cached' and '--files' cannot be used together");
        return Err(GitError::Exit(128));
    }
    Ok(SubmoduleSummaryOptions {
        cached,
        quiet,
        summary_limit,
        positionals,
    })
}

fn parse_submodule_summary_limit(value: &str) -> Result<isize> {
    value.parse::<isize>().map_err(|_| {
        eprintln!(
            "error: option `summary-limit' expects an integer value with an optional k/m/g suffix"
        );
        GitError::Exit(129)
    })
}

fn submodule_name_for_exact_path(config: &GitConfig, path: &str) -> Option<String> {
    config
        .sections
        .iter()
        .filter(|section| section.name == "submodule")
        .find(|section| {
            section
                .entries
                .iter()
                .rev()
                .find(|entry| entry.key == "path")
                .and_then(|entry| entry.value.as_deref())
                == Some(path)
        })
        .and_then(|section| section.subsection.clone())
}

fn filter_submodule_configs<'a>(
    cwd: &Path,
    worktree_root: &Path,
    submodules: &'a [SubmoduleConfigEntry],
    paths: &[&str],
) -> Result<Vec<&'a SubmoduleConfigEntry>> {
    if paths.is_empty() {
        return Ok(submodules.iter().collect());
    }
    let mut selected = Vec::new();
    for path in paths {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, path);
        let matching = submodules
            .iter()
            .filter(|submodule| submodule_path_matches_pathspec(&submodule.path, &normalized))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(1));
        }
        selected.extend(matching);
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    Ok(selected)
}

fn select_submodules_for_summary<'a>(
    cwd: &Path,
    worktree_root: &Path,
    submodules: &'a [SubmoduleConfigEntry],
    options: &SubmoduleSummaryOptions,
) -> Vec<&'a SubmoduleConfigEntry> {
    if options.positionals.is_empty() {
        return submodules.iter().collect();
    }
    let mut selected = Vec::new();
    for path in &options.positionals {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, path);
        selected.extend(
            submodules
                .iter()
                .filter(|submodule| submodule_path_matches_pathspec(&submodule.path, &normalized)),
        );
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    selected
}

fn resolve_submodule_init_url(worktree_root: &Path, config: &GitConfig, url: &str) -> String {
    if !(url.starts_with("../") || url.starts_with("./")) {
        return url.to_string();
    }
    let base = config
        .get("remote", Some("origin"), "url")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| path.parent().map(|parent| parent.join(url)))
        .unwrap_or_else(|| {
            eprintln!(
                "warning: could not look up configuration 'remote.origin.url'. Assuming this repository is its own authoritative upstream."
            );
            worktree_root.join(url)
        });
    normalize_lexical_path(&base).display().to_string()
}

fn resolve_submodule_sync_url(worktree_root: &Path, config: &GitConfig, url: &str) -> String {
    if !(url.starts_with("../") || url.starts_with("./")) {
        return url.to_string();
    }
    let base = config
        .get("remote", Some("origin"), "url")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| path.parent().map(|parent| parent.join(url)))
        .unwrap_or_else(|| worktree_root.join(url));
    normalize_lexical_path(&base).display().to_string()
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

fn set_submodule_config_value(config: &mut GitConfig, name: &str, key: &str, value: &str) {
    set_config_value(config, "submodule", Some(name), key, value);
}

fn unset_submodule_config_value(config: &mut GitConfig, name: &str, key: &str) {
    let Some(section) =
        config.sections.iter_mut().rev().find(|section| {
            section.name == "submodule" && section.subsection.as_deref() == Some(name)
        })
    else {
        return;
    };
    section
        .entries
        .retain(|entry| !entry.key.eq_ignore_ascii_case(key));
}

fn remove_submodule_config_section(config: &mut GitConfig, name: &str) {
    config.sections.retain(|section| {
        !(section.name == "submodule" && section.subsection.as_deref() == Some(name))
    });
}

fn submodule_worktree_has_local_changes(
    worktree_root: &Path,
    submodule: &SubmoduleConfigEntry,
) -> Result<bool> {
    let submodule_root = worktree_root.join(&submodule.path);
    if !submodule_root.exists() {
        return Ok(false);
    }
    let Ok((git_dir, _)) = submodule_head(&submodule_root) else {
        return Ok(false);
    };
    let format = repository_object_format(&git_dir)?;
    let Some(index) = read_repository_index(&git_dir, format)? else {
        return submodule_worktree_has_entries(&submodule_root);
    };
    let mut tracked = BTreeSet::new();
    for entry in &index.entries {
        tracked.insert(String::from_utf8_lossy(&entry.path).into_owned());
        let path = submodule_root.join(String::from_utf8_lossy(&entry.path).as_ref());
        if !path.exists() {
            return Ok(true);
        }
        if entry.mode == 0o100644 || entry.mode == 0o100755 {
            let body = fs::read(&path)?;
            let oid = sley_core::object_id_for_bytes(format, "blob", &body)?;
            if oid != entry.oid {
                return Ok(true);
            }
        }
    }
    submodule_worktree_has_untracked_entries(&submodule_root, &submodule_root, &tracked)
}

fn absorb_submodule_git_dir(
    git_dir: &Path,
    worktree_root: &Path,
    submodule: &SubmoduleConfigEntry,
    quiet: bool,
) -> Result<()> {
    let submodule_root = worktree_root.join(&submodule.path);
    let dot_git = submodule_root.join(".git");
    if !dot_git.is_dir() {
        return Ok(());
    }
    let modules_git_dir = git_dir.join("modules").join(&submodule.path);
    let Some(parent) = modules_git_dir.parent() else {
        return Err(GitError::InvalidPath(format!(
            "invalid submodule gitdir path {}",
            modules_git_dir.display()
        )));
    };
    fs::create_dir_all(parent)?;
    let from_display = fs::canonicalize(&dot_git)?;
    let to_display = if modules_git_dir.exists() {
        fs::canonicalize(&modules_git_dir)?
    } else {
        fs::canonicalize(parent)?.join(
            modules_git_dir
                .file_name()
                .ok_or_else(|| GitError::InvalidPath("invalid submodule gitdir".into()))?,
        )
    };
    if !quiet {
        eprintln!("Migrating git directory of '{}' from", submodule.path);
        eprintln!("'{}' to", from_display.display());
        eprintln!("'{}'", to_display.display());
    }
    fs::rename(&dot_git, &modules_git_dir)?;

    let gitdir_link = relative_path_from_absolute_components(&submodule_root, &modules_git_dir)?;
    fs::write(&dot_git, format!("gitdir: {gitdir_link}\n"))?;

    let mut config = read_repo_config(&modules_git_dir)?;
    let worktree = relative_path_from_absolute_components(&modules_git_dir, &submodule_root)?;
    set_config_value(&mut config, "core", None, "worktree", &worktree);
    write_repo_config(&modules_git_dir, &config)?;
    Ok(())
}

fn run_submodule_foreach_tree(
    cwd: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodules: &[SubmoduleConfigEntry],
    options: &SubmoduleForeachOptions,
) -> Result<()> {
    let selected = filter_submodule_configs(cwd, worktree_root, submodules, &[])?;
    for submodule in selected {
        let submodule_root = worktree_root.join(&submodule.path);
        let Ok((submodule_git_dir, _)) = submodule_head(&submodule_root) else {
            continue;
        };
        run_submodule_foreach_command(cwd, worktree_root, index, submodule, options)?;
        if options.recursive {
            let nested_configs = read_submodule_configs(&submodule_root)?;
            let nested_format = repository_object_format(&submodule_git_dir)?;
            let nested_index = read_repository_index(&submodule_git_dir, nested_format)?;
            run_submodule_foreach_tree(
                cwd,
                &submodule_root,
                &nested_index,
                &nested_configs,
                options,
            )?;
        }
    }
    Ok(())
}

fn run_submodule_foreach_command(
    cwd: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleConfigEntry,
    options: &SubmoduleForeachOptions,
) -> Result<()> {
    let submodule_root = worktree_root.join(&submodule.path);
    let display_path = display_submodule_path(cwd, worktree_root, &submodule.path)?;
    let sha1 = submodule_index_oid(index, &submodule.path)
        .map(|oid| oid.to_string())
        .unwrap_or_default();
    if !options.quiet {
        println!("Entering '{display_path}'");
    }
    let output = ProcessCommand::new("sh")
        .arg("-c")
        .arg(&options.command)
        .current_dir(&submodule_root)
        .env("name", &submodule.name)
        .env("sm_path", &submodule.path)
        .env("displaypath", &display_path)
        .env("sha1", &sha1)
        .env("toplevel", worktree_root)
        .output()
        .map_err(|err| GitError::Io(err.to_string()))?;
    io::stdout().write_all(&output.stdout)?;
    io::stderr().write_all(&output.stderr)?;
    if output.status.success() {
        return Ok(());
    }
    eprintln!("fatal: run_command returned non-zero status for {display_path}");
    eprintln!(".");
    Err(GitError::Exit(128))
}

fn print_submodule_summary(
    cwd: &Path,
    git_dir: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleConfigEntry,
    cached: bool,
    summary_limit: Option<isize>,
) -> Result<()> {
    let Some(index_oid) = submodule_index_oid(index, &submodule.path) else {
        return Ok(());
    };
    let submodule_root = worktree_root.join(&submodule.path);
    let Ok((submodule_git_dir, head_oid)) = submodule_head(&submodule_root) else {
        return Ok(());
    };
    let old_oid = if cached {
        let format = repository_object_format(git_dir)?;
        let Some(head_index_oid) = submodule_head_tree_oid(git_dir, format, &submodule.path)?
        else {
            return Ok(());
        };
        head_index_oid
    } else {
        index_oid
    };
    let new_oid = if cached { index_oid } else { head_oid };
    if new_oid == old_oid {
        return Ok(());
    }
    let format = repository_object_format(&submodule_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&submodule_git_dir, format);
    let (marker, commits) = submodule_summary_commits(&db, format, &old_oid, &new_oid)?;
    if commits.is_empty() {
        return Ok(());
    }
    let display_path = display_submodule_path(cwd, worktree_root, &submodule.path)?;
    println!(
        "* {} {}...{} ({}):",
        display_path,
        format_log_abbrev_oid(&old_oid),
        format_log_abbrev_oid(&new_oid),
        commits.len()
    );
    let limit = summary_limit
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(commits.len());
    for (_, commit) in commits.iter().take(limit) {
        println!("  {marker} {}", commit_subject(&commit.message));
    }
    println!();
    Ok(())
}

fn submodule_head_tree_oid(
    git_dir: &Path,
    format: ObjectFormat,
    path: &str,
) -> Result<Option<ObjectId>> {
    let Ok(head_oid) = resolve_revision(git_dir, format, "HEAD") else {
        return Ok(None);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&head_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            head_oid,
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let tree_object = db.read_object(&commit.tree)?;
    if tree_object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {}, found {}",
            commit.tree,
            tree_object.object_type.as_str()
        )));
    }
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let Some(entry) = find_tree_entry(&db, format, &tree_object.body, &components)? else {
        return Ok(None);
    };
    if entry.mode != 0o160000 {
        return Ok(None);
    }
    Ok(Some(entry.oid))
}

fn submodule_summary_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    index_oid: &ObjectId,
    head_oid: &ObjectId,
) -> Result<(char, Vec<(ObjectId, Commit)>)> {
    let forward = submodule_summary_forward_commits(db, format, index_oid, head_oid)?;
    if !forward.is_empty() {
        return Ok(('>', forward));
    }
    let reverse = submodule_summary_forward_commits(db, format, head_oid, index_oid)?;
    Ok(('<', reverse))
}

fn submodule_summary_forward_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
) -> Result<Vec<(ObjectId, Commit)>> {
    let old_ancestors = ancestor_depths(db, format, old_oid)?;
    let mut commits = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([*new_oid]);
    while let Some(oid) = pending.pop_front() {
        if old_ancestors.contains_key(&oid) || !seen.insert(oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse(format, &object.body)?;
        pending.extend(commit.parents.iter().copied());
        commits.push((oid, commit));
    }
    Ok(commits)
}

fn submodule_index_oid(index: &Option<Index>, path: &str) -> Option<ObjectId> {
    let path = path.as_bytes();
    index
        .as_ref()?
        .entries
        .iter()
        .find(|entry| entry.mode == 0o160000 && entry.path == path)
        .map(|entry| entry.oid)
}

fn submodule_worktree_has_entries(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
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

fn clear_submodule_worktree(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn read_submodule_configs(worktree_root: &Path) -> Result<Vec<SubmoduleConfigEntry>> {
    let path = worktree_root.join(".gitmodules");
    let Ok(config) = GitConfig::read(path) else {
        return Ok(Vec::new());
    };
    let mut submodules = Vec::new();
    for section in config.sections {
        if section.name != "submodule" {
            continue;
        }
        let Some(name) = section.subsection.clone() else {
            continue;
        };
        let path = section
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == "path")
            .and_then(|entry| entry.value.clone());
        let url = section
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == "url")
            .and_then(|entry| entry.value.clone());
        let update = section
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == "update")
            .and_then(|entry| entry.value.clone());
        if let Some(path) = path {
            submodules.push(SubmoduleConfigEntry {
                name,
                path,
                url,
                update,
            });
        }
    }
    Ok(submodules)
}

fn filter_submodules(
    cwd: &Path,
    worktree_root: &Path,
    submodules: Vec<SubmoduleConfigEntry>,
    paths: &[&str],
) -> Result<Vec<SubmoduleStatusEntry>> {
    if paths.is_empty() {
        let mut selected = submodules
            .into_iter()
            .map(|submodule| {
                let display_path = display_submodule_path(cwd, worktree_root, &submodule.path)?;
                Ok(SubmoduleStatusEntry {
                    path: submodule.path,
                    display_path,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        selected.sort_by(|left, right| left.path.cmp(&right.path));
        return Ok(selected);
    }
    let mut selected = Vec::new();
    for path in paths {
        let normalized = normalize_submodule_pathspec(cwd, worktree_root, path);
        let matching = submodules
            .iter()
            .filter(|submodule| submodule_path_matches_pathspec(&submodule.path, &normalized))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(1));
        }
        for submodule in matching {
            selected.push(SubmoduleStatusEntry {
                path: submodule.path.clone(),
                display_path: display_submodule_path(cwd, worktree_root, &submodule.path)?,
            });
        }
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    selected.dedup_by(|left, right| left.path == right.path);
    Ok(selected)
}

fn submodule_path_matches_pathspec(path: &str, pathspec: &str) -> bool {
    pathspec.is_empty()
        || path == pathspec
        || path
            .strip_prefix(pathspec)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn normalize_submodule_pathspec(cwd: &Path, worktree_root: &Path, path: &str) -> String {
    let path = path.trim_end_matches('/');
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let root = fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    lexical_relative_path(&root, &absolute).unwrap_or_else(|| {
        path.to_string_lossy()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    })
}

fn display_submodule_path(cwd: &Path, worktree_root: &Path, path: &str) -> Result<String> {
    let absolute = fs::canonicalize(worktree_root)?.join(path);
    relative_path_from_absolute(cwd, &absolute).map(|path| path.trim_end_matches('/').to_string())
}

fn lexical_relative_path(root: &Path, target: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in target.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::Normal(value) => parts.push(value.to_os_string()),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                parts.clear();
                parts.push(component.as_os_str().to_os_string());
            }
        }
    }
    let normalized = parts.into_iter().collect::<PathBuf>();
    let relative = normalized.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn read_repository_index(git_dir: &Path, format: ObjectFormat) -> Result<Option<Index>> {
    sley_worktree::read_repository_index(git_dir, format)
}

fn print_submodule_status_tree(
    cwd: &Path,
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleStatusEntry,
    cached: bool,
    recursive: bool,
) -> Result<()> {
    print_submodule_status(worktree_root, index, submodule, cached)?;
    if !recursive {
        return Ok(());
    }
    let submodule_root = worktree_root.join(&submodule.path);
    let Ok((git_dir, _)) = submodule_head(&submodule_root) else {
        return Ok(());
    };
    let nested_configs = read_submodule_configs(&submodule_root)?;
    let nested = filter_submodules(cwd, &submodule_root, nested_configs, &[])?;
    let nested_format = repository_object_format(&git_dir)?;
    let nested_index = read_repository_index(&git_dir, nested_format)?;
    for nested_submodule in nested {
        print_submodule_status_tree(
            cwd,
            &submodule_root,
            &nested_index,
            &nested_submodule,
            cached,
            recursive,
        )?;
    }
    Ok(())
}

fn print_submodule_status(
    worktree_root: &Path,
    index: &Option<Index>,
    submodule: &SubmoduleStatusEntry,
    cached: bool,
) -> Result<()> {
    let path_bytes = submodule.path.as_bytes();
    let cached_oid = index
        .as_ref()
        .and_then(|index| {
            index
                .entries
                .iter()
                .find(|entry| entry.mode == 0o160000 && entry.path == path_bytes)
        })
        .map(|entry| entry.oid);
    let Some(cached_oid) = cached_oid else {
        return Ok(());
    };

    let submodule_root = worktree_root.join(&submodule.path);
    let submodule_head = submodule_head(&submodule_root).ok();
    let prefix = if submodule_head.is_none() {
        '-'
    } else if submodule_head
        .as_ref()
        .is_some_and(|(_, oid)| oid != &cached_oid)
    {
        '+'
    } else {
        ' '
    };
    let output_oid = if cached {
        cached_oid
    } else {
        submodule_head
            .as_ref()
            .map(|(_, oid)| *oid)
            .unwrap_or(cached_oid)
    };
    let suffix = submodule_status_suffix(
        submodule_head
            .as_ref()
            .map(|(git_dir, _)| git_dir.as_path()),
        &output_oid,
    )?;
    println!("{prefix}{output_oid} {}{suffix}", submodule.display_path);
    Ok(())
}

fn submodule_head(submodule_root: &Path) -> Result<(PathBuf, ObjectId)> {
    let dot_git = submodule_root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let Some(git_dir) = read_gitdir_file(&dot_git)? else {
            return Err(GitError::not_found("submodule gitdir"));
        };
        git_dir
    } else {
        return Err(GitError::not_found("submodule gitdir"));
    };
    let format = repository_object_format(&git_dir)?;
    let oid = sley_rev::resolve_revision(&git_dir, format, "HEAD")?;
    Ok((git_dir, oid))
}

fn submodule_status_suffix(git_dir: Option<&Path>, oid: &ObjectId) -> Result<String> {
    let Some(git_dir) = git_dir else {
        return Ok(String::new());
    };
    let format = repository_object_format(git_dir)?;
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    for reference in refs
        .iter()
        .filter(|reference| reference.name.starts_with("refs/tags/"))
    {
        if let Some((target_oid, _)) = resolve_for_each_ref_target(&store, reference)?
            && target_oid == *oid
        {
            return Ok(format!(" ({})", display_submodule_ref(&reference.name)));
        }
    }
    if let Some(RefTarget::Symbolic(target)) = store.read_ref("HEAD")?
        && let Some(target_oid) = resolve_ref_to_oid(&store, &target)?
        && target_oid == *oid
    {
        return Ok(format!(" ({})", display_submodule_ref(&target)));
    }
    for reference in refs {
        if reference.name.starts_with("refs/tags/") {
            continue;
        }
        if let Some((target_oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && target_oid == *oid
        {
            return Ok(format!(" ({})", display_submodule_ref(&reference.name)));
        }
    }
    Ok(String::new())
}

fn resolve_ref_to_oid(store: &FileRefStore, name: &str) -> Result<Option<ObjectId>> {
    resolve_ref_peeled(store, name)
}

fn display_submodule_ref(name: &str) -> String {
    if let Some(tag) = name.strip_prefix("refs/tags/") {
        return tag.to_string();
    }
    name.strip_prefix("refs/").unwrap_or(name).to_string()
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

fn status_quote_path(path: &[u8], quote_space: bool) -> String {
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
        return Ok(resolve_cli_path(
            start.as_ref(),
            git_dir.to_string_lossy().as_ref(),
        ));
    }
    if global_bare() {
        let cwd = env::current_dir()?;
        if is_git_dir_candidate(&cwd) {
            return fs::canonicalize(&cwd).map_err(|err| GitError::Io(err.to_string()));
        }
        return Err(GitError::repository_not_found("not a git repository"));
    }
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
    sley_sequencer::format_commit_identity(&name, &email, &date)
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
