#![allow(
    dead_code,
    unused_assignments,
    unused_mut,
    unused_variables,
    clippy::all,
    clippy::unwrap_used
)]

use sley::{
    BString, GitConfig, GitError, Index, IndexEntry, ObjectFormat, ObjectId, RefPrecondition,
    ReferenceTarget as RefTarget, Result,
};
pub(crate) use sley::plumbing::{
    sley_config, sley_core, sley_diff_merge, sley_formats, sley_index, sley_object, sley_odb,
    sley_pack, sley_pretty, sley_refs, sley_rev, sley_remote, sley_worktree,
};
use sley::plumbing::sley_config::{ConfigBoolOrInt, ConfigEntry, ConfigSection};
use sley::plumbing::sley_core::DateMode;
use sley::plumbing::sley_formats::{
    Bundle, BundleCapability, BundlePrerequisite, BundleReference, CommitGraph,
    CommitGraphWriteEntry, InitOptions, RefStorageFormat, RepositoryBootstrap,
};
use sley::plumbing::sley_object::{
    Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntries, TreeEntry, TreeEntryRef,
    tree_entry_object_type,
};
use sley::plumbing::sley_odb::{
    FileObjectDatabase, LooseObjectIntegrity, ObjectPrefixResolution, ObjectReader, ObjectWriter,
    build_reachable_pack, collect_reachable_object_ids, grafted_parents, install_bundle_pack,
    install_reachable_pack, prune_unreachable_loose, repository_object_ids, repository_objects_dir,
};
use sley::plumbing::sley_pack::{MultiPackIndex, MultiPackIndexEntry, PackFile, PackIndex};
use sley_pathspec::{
    LsFilesPathFilter, PathspecAttributeCheck, PathspecAttributeState,
    parse_normalized_pathspec_element, pathspec_attrs_match_with, pathspec_filters_have_include,
    pathspec_filters_match, pathspec_filters_match_with,
};
use sley_protocol::{
    FetchHeadRecord, FetchRefUpdate, ProtocolVersion, ReceivePackCommand, ReceivePackPushRequest,
    RefAdvertisement, RefAdvertisementSet, UploadPackFeatures, parse_refspec, read_fetch_head,
    read_receive_pack_push_options, read_receive_pack_request, read_ref_advertisement_set,
    read_upload_pack_negotiation_request, read_upload_pack_request, refspec_map_source,
    write_receive_pack_report_status, write_ref_advertisement_set,
    write_upload_pack_packfile_response, write_upload_pack_raw_packfile_response,
};
pub(crate) use sley_ref_filter::*;
use sley::plumbing::sley_refs::{
    FileRefStore, PackRefDecision, Ref, RefTransactionHookUpdate, RefTransactionPhase, RefUpdate,
    ReferenceTransactionHook, ReflogEntry, branch_ref_name, check_refname_format, parse_packed_refs,
    resolve_ref_peeled, tag_ref_name, validate_ref_name, validate_symref_name, validate_symref_target,
};
use sley::plumbing::sley_remote::FetchOutcome;
pub(crate) use sley::plumbing::sley_rev::revlist::*;
use sley_transport::{RemoteTransport, RemoteUrl, parse_remote_url};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, BufWriter, IsTerminal, Read, Seek, SeekFrom, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};

pub(crate) fn collect_short_status(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<sley_worktree::ShortStatusEntry>> {
    collect_short_status_with_options(
        worktree_root,
        git_dir,
        format,
        sley_worktree::ShortStatusOptions::default(),
    )
}

pub(crate) fn collect_short_status_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: sley_worktree::ShortStatusOptions,
) -> Result<Vec<sley_worktree::ShortStatusEntry>> {
    let mut entries = Vec::new();
    sley_worktree::stream_short_status_with_options(
        worktree_root,
        git_dir,
        format,
        options,
        |entry| {
            entries.push(entry.to_owned_entry());
            Ok(sley_worktree::StreamControl::Continue)
        },
    )?;
    Ok(entries)
}

mod commands;
mod dispatch;
mod diff_render;
mod discovery;
mod global_options;
mod ownership;
mod remote;
mod repo_path;
mod repo_paths;
mod repository;
mod session;
mod setup;
mod trace2_cli;

pub(crate) use global_options::{
    apply_global_options, argv_bytes_from_os, argv_bytes_from_string, argv_string_from_bytes,
    core_big_file_threshold, effective_config_parameters_env, global_config_value,
    injected_config_parameters, GlobalConfigOverride, PathspecFlags, DEFAULT_BIG_FILE_THRESHOLD,
};
pub use global_options::argv_string_from_os;
pub(crate) use repo_paths::common_git_dir_for_git_dir;
pub(crate) use trace2_cli::{
    trace2_emit_def_params_at_depth, trace2_emit_def_params_once,
    trace2_emit_process_ancestry_at_depth, trace_reference_fsync_counter,
};

pub(crate) use diff_render::{
    DiffEntryRawRenderOptions, DiffEntryRenderContext, DiffEntryRenderModes, DiffEntryStatRenderOptions,
    DiffEntryStatSource, DiffLineStats, DiffRenderOptions, DiffPathspec, DiffStatEntryData,
    DiffStatOptions, DiffWorktreeCleanContext, WordDiffRequest, apply_diff_max_depth,
    apply_diff_order_file, apply_diff_pathspec, apply_submodule_ignore_filter,
    collect_diff_stat_entries, collect_diff_stat_entries_with_worktree_clean, collect_dirty_submodules,
    compile_ignore_matching_regexes, diff_entry_new_content, diff_entry_old_content,
    diff_entry_produces_output, diff_line_stats, diff_rename_limit_requires_integer_error,
    diff_stat_decimal_width, diff_stat_pprint_rename, diff_stat_totals, gitlink_diff_content,
    is_binary_content, is_gitlink_pair, parse_diff_max_depth, parse_dirstat_params,
    prefetch_via_configured_upload_pack, promisor_remote_names, read_blob,
    read_object_maybe_prefetch_promisor, render_diff_entries, render_tree_to_tree_patch,
    repo_path_to_path, reverse_diff_entries, reverse_diff_entry, submodule_diff_config,
    submodule_git_dir_for_path, validate_diff_rename_limit, write_diff_dirstat,
    write_diff_numstat_materialized_entry, write_diff_patch_entry, write_diff_raw_entry,
    write_diff_shortstat_materialized, write_diff_stat_materialized,
    write_diff_stat_materialized_with_widths, write_diff_stat_summary_line, write_diff_summary_entry,
};

pub(crate) use discovery::{
    is_git_dir_candidate, paths_refer_to_same_dir, read_gitdir_file, resolve_cli_path,
};

pub(crate) use sley::plumbing::sley_pretty::{
    CompiledLogFormat, FormatToken, LogFormatDialect, LogDescribeLookup, LogFormatContext,
    LogSignatureLookup, LogSignatureView, MailmapLookup, StashFormatContext, append_log_oid,
    commit_author_for_commit_encoding, commit_body, commit_encoding, commit_encoding_config,
    commit_object_message_and_optional_encoding,
    commit_encoding_header_from_config, commit_identity_name_email, commit_message_for_commit_encoding,
    commit_message_for_output, commit_message_has_invalid_utf8, commit_message_has_nul,
    commit_message_lines, commit_subject, commit_subject_bytes, emit_compiled_log_format,
    emit_compiled_log_format_limited_commit, emit_compiled_log_format_metadata,
    emit_compiled_log_format_metadata_with_message, emit_compiled_stash_format, emit_log_one_token,
    encoding_for_name, encoding_is_none, encoding_is_utf8, format_log_abbrev_oid,
    format_log_commit_header_oid, format_log_oid, format_subst_for_commit, format_trailers_from_commit,
    git_color_name_to_ansi, git_color_spec_to_ansi, log_email_local_part, log_output_encoding,
    log_pick_utf8, log_reencode_message, log_rewrap, log_sanitized_subject, presets,
};
pub(crate) use sley_options::validators::*;
pub(crate) use sley::plumbing::sley_rev::diff_options::{DiffFilter, DiffStatWidths, DirstatMode, DirstatOptions, SubmoduleIgnoreMode, diff_stat_count_option, diff_stat_parse_width_option, parse_diff_filter, parse_diff_rename_limit, parse_similarity_threshold, parse_submodule_ignore_mode};

pub(crate) use commands::args::{GitArgCursor, long_option_value};
pub(crate) use commands::cat_file::{cat_file_all_object_ids, cat_file_object_storage};
pub(crate) use commands::checkout::cmd_checkout;
pub(crate) use commands::config_cmd::{config_entry_name, has_unescaped_trailing_dollar};
pub(crate) use commands::merge_rebase::{
    MergePathResult, MergeTreeMap, commit_tree_oid, conclude_in_progress_merge,
    conclude_rebase_step_via_commit, head_commit_oid, merge_bases, merge_index_entry,
    merge_read_blob, merge_remove_worktree_file, merge_write_worktree_file,
    read_merge_message_from_file, rebase_in_progress, three_way_merge_trees,
};
pub(crate) use commands::remote::{
    read_repo_config, remote_exists, remote_names, repo_current_branch_name, write_repo_config,
};
pub(crate) use commands::status::cmd_status;
use commands::tag::tag_stripspace_message;
pub(crate) use repo_path::RepoPathBuf;
pub(crate) use repository::RepositoryContext;

pub fn run(args: Vec<String>) -> Result<()> {
    sley_core::set_original_cwd(env::current_dir().ok());
    let global = apply_global_options(&args)?;
    sley_core::trace2::touch();
    sley_core::trace2::start(global.args);
    trace2_emit_process_ancestry_at_depth(sley_core::trace2::depth(), &[]);
    trace2_emit_def_params_once();
    // `-c` / `--config-env` overrides are folded into the process
    // `GIT_CONFIG_PARAMETERS` env var during option parsing, so the single
    // `injected_config_parameters()` reader is the source of truth for every
    // config read; no separate global-override store is needed.
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    session::install_cli_session(session::CliSession::from_parsed_globals(
        cwd,
        global.git_dir.clone(),
        global.work_tree.clone(),
        global.attr_source.clone(),
        global.bare,
        global.replace_objects,
        global.lazy_fetch,
        global.pathspec_flags,
    ));
    // Emit git's GIT_TRACE_SETUP output (the env/config/gitfile discovery trace)
    // before dispatching. This is the CLI-side repository setup that
    // `sley::Repository::discover` deliberately leaves to this layer.
    if env::var_os("GIT_TRACE_SETUP").is_some()
        && let Some(setup_result) = setup::setup_git_directory()
    {
        setup::trace_repo_setup(&setup_result);
    }
    dispatch::dispatch_with_aliases(global.args, &global.config, 0)
}

pub(crate) fn with_local_repo_env_hidden<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    session::with_local_repo_env_hidden_session(f)
}


/// Effective default pathspec magic, folding in the global options *and* the
/// `GIT_*_PATHSPECS` environment variables (git reads both). Literal magic
/// (`--literal-pathspecs`/`--noglob-pathspecs`/`GIT_LITERAL_PATHSPECS`/
/// `GIT_NOGLOB_PATHSPECS`) suppresses glob magic.
pub(crate) fn effective_pathspec_flags() -> sley_worktree::PathspecMatchMagic {
    let mut flags = session::cli_session()
        .map(|session| session.pathspec_flags())
        .unwrap_or_default();
    if git_env_bool("GIT_LITERAL_PATHSPECS") {
        flags.literal = true;
        flags.literal_pathspecs = true;
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
        glob: flags.glob && !flags.literal && !flags.literal_pathspecs,
        icase: flags.icase,
        literal_pathspecs: flags.literal_pathspecs,
    }
}

pub(crate) fn attribute_checks_for_matching(
    checks: Vec<sley_worktree::AttributeCheck>,
) -> Vec<PathspecAttributeCheck> {
    checks
        .into_iter()
        .map(|check| PathspecAttributeCheck {
            attribute: check.attribute,
            state: check.state.map(|state| match state {
                sley_worktree::AttributeState::Set => PathspecAttributeState::Set,
                sley_worktree::AttributeState::Unset => PathspecAttributeState::Unset,
                sley_worktree::AttributeState::Value(value) => PathspecAttributeState::Value(value),
            }),
        })
        .collect()
}

fn git_env_bool(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

fn global_git_dir() -> Option<PathBuf> {
    session::cli_session().and_then(|session| session.git_dir_override())
}

fn global_work_tree() -> Option<PathBuf> {
    session::cli_session().and_then(|session| session.work_tree_override())
}

pub(crate) fn global_attr_source() -> Option<String> {
    session::cli_session().and_then(|session| session.attr_source())
}

fn environment_git_dir() -> Option<PathBuf> {
    if local_repo_env_hidden() {
        return None;
    }
    env::var_os("GIT_DIR").map(PathBuf::from)
}

pub(crate) fn explicit_git_dir() -> Option<PathBuf> {
    global_git_dir().or_else(environment_git_dir)
}

fn environment_work_tree() -> Option<PathBuf> {
    if local_repo_env_hidden() {
        return None;
    }
    env::var_os("GIT_WORK_TREE").map(PathBuf::from)
}

fn explicit_work_tree() -> Option<PathBuf> {
    global_work_tree().or_else(environment_work_tree)
}

pub(crate) fn global_bare() -> bool {
    let Some(session) = session::cli_session() else {
        return false;
    };
    if session.local_repo_env_hidden() {
        return false;
    }
    session.bare()
}

fn local_repo_env_hidden() -> bool {
    session::cli_session().is_some_and(|session| session.local_repo_env_hidden())
}

fn global_replace_objects() -> bool {
    session::cli_session()
        .map(|session| session.replace_objects())
        .unwrap_or(true)
        && env::var_os("GIT_NO_REPLACE_OBJECTS").is_none()
}

pub(crate) fn global_lazy_fetch_enabled() -> bool {
    session::cli_session()
        .map(|session| session.lazy_fetch())
        .unwrap_or(true)
        && env::var("GIT_NO_LAZY_FETCH")
            .map(|value| value == "0")
            .unwrap_or(true)
}

pub(crate) fn replace_objects_active(refs: &FileRefStore) -> Result<bool> {
    if !global_replace_objects() {
        return Ok(false);
    }
    refs.has_refs_with_prefix("refs/replace/")
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

fn init_config_value(
    key: &str,
    global_config: &[GlobalConfigOverride],
    config_git_dir: Option<&Path>,
) -> Result<Option<String>> {
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
    let context = match config_git_dir {
        Some(git_dir) => sley_config::ConfigIncludeContext::new(
            Some(sley_config::git_dir_for_include_context(git_dir)),
            sley_config::repo_current_branch_name(git_dir),
        ),
        None => sley_config::ConfigIncludeContext::new(None, None),
    };
    let mut config = sley_config::load_pre_dispatch_config(config_git_dir, &context)
        .map_err(report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    let base = match env::current_dir() {
        Ok(path) => path,
        Err(_) => PathBuf::from("."),
    };
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &base,
    )
    .map_err(report_config_setup_error)?;
    let (section, entry_key) = key
        .split_once('.')
        .ok_or_else(|| GitError::Command(format!("invalid config key {key}")))?;
    Ok(config.get(section, None, entry_key).map(str::to_owned))
}

/// `init.defaultBranch` from the global/injected config, used by `git clone`
/// when an empty/unborn remote leaves it to name the local default branch.
/// Looked up with no repository context (clone runs before the new repo's config
/// is relevant), so it consults injected `-c` overrides and the global config.
pub(crate) fn clone_init_default_branch_config() -> Result<Option<String>> {
    init_config_value("init.defaultBranch", &[], None)
}

pub(crate) fn clone_init_default_submodule_path_config() -> Result<bool> {
    Ok(
        init_config_value("init.defaultSubmodulePathConfig", &[], None)?
            .as_deref()
            .and_then(parse_config_bool)
            .unwrap_or(false),
    )
}

pub(crate) fn enable_submodule_path_config_extension(git_dir: &Path) -> Result<()> {
    let mut config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    set_config_value(&mut config, "core", None, "repositoryformatversion", "1");
    set_config_value(
        &mut config,
        "extensions",
        None,
        "submodulePathConfig",
        "true",
    );
    commands::remote::write_repo_config(git_dir, &config)
}

pub(crate) fn submodule_path_config_enabled(git_dir: &Path) -> bool {
    GitConfig::read(git_dir.join("config"))
        .ok()
        .and_then(|config| config.get_bool("extensions", None, "submodulePathConfig"))
        .unwrap_or(false)
}

pub(crate) fn report_config_setup_error(err: GitError) -> GitError {
    match err {
        GitError::InvalidFormat(message) => {
            if message == "relative config includes must come from files"
                || message.starts_with("exceeded maximum include depth")
            {
                eprintln!("fatal: {message}");
                return GitError::Exit(128);
            }
            if message
                == "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
            {
                eprintln!("fatal: {message}");
                return GitError::Exit(128);
            }
            if let Some((line, path)) = parse_bad_config_line_with_path(&message) {
                eprintln!("fatal: bad config line {line} in file {path}");
                return GitError::Exit(128);
            }
            if let Some(line) = parse_bad_config_line_without_path(&message) {
                eprintln!("fatal: bad config line {line}");
                return GitError::Exit(128);
            }
            GitError::InvalidFormat(message)
        }
        other => other,
    }
}

pub(crate) fn parse_bad_config_line_with_path(message: &str) -> Option<(&str, &str)> {
    let rest = message.strip_prefix("config line ")?;
    let (line, rest) = rest.split_once(" in file ")?;
    let path = match rest.rsplit_once(':') {
        Some((path, _detail)) => path,
        None => rest,
    };
    Some((line, path))
}

pub(crate) fn parse_bad_config_line_without_path(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("config line ")?;
    let (line, _detail) = rest.split_once(':')?;
    Some(line)
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
    let status = if include_untracked {
        collect_short_status(worktree_root, git_dir, format)?
    } else {
        collect_short_status_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::ShortStatusOptions {
                untracked_mode: sley_worktree::StatusUntrackedMode::None,
                ..Default::default()
            },
        )?
    };
    let mut actions = Vec::new();
    for entry in status {
        if entry.index == b'?' && entry.worktree == b'?' {
            if !include_untracked {
                continue;
            }
        } else if entry.worktree != b'M'
            && entry.worktree != b'T'
            && entry.worktree != b'D'
            && entry.worktree != b'A'
        {
            // A typechange (`T`) stages like a modification: the path is re-added
            // with its new worktree mode/content (the `else` Add branch below).
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
    let pathspec_text = pathspec.to_string_lossy();
    if sley_worktree::pathspec_is_glob(pathspec_text.as_bytes()) {
        let path_text = path.to_string_lossy();
        return sley_worktree::pathspec_item_matches(
            pathspec_text.as_bytes(),
            path_text.as_bytes(),
            sley_worktree::PathspecMatchMagic::default(),
        );
    }
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
    let mut current = *oid;
    let mut peeled = false;
    for _ in 0..16 {
        let object = db.read_object(&current)?;
        if object.object_type != ObjectType::Tag {
            return Ok(peeled.then_some(current));
        }
        let tag = Tag::parse_ref(format, &object.body)?;
        let target = db.read_object(&tag.object)?;
        if target.object_type != tag.object_type {
            return Ok(None);
        }
        current = tag.object;
        peeled = true;
    }
    Ok(None)
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

fn parse_reflog_expire_time(value: &str, option: &str) -> Result<i64> {
    // git's `parse_expiry_date`: "never"/"false" never expire; "all"/"now" expire
    // everything (TIME_MAX — by definition a reflog records only the past, so
    // "now" means "drop it all").
    match value {
        "all" | "now" => return Ok(i64::MAX),
        "never" | "false" => return Ok(i64::MIN),
        _ => {}
    }
    // Try the strict explicit-timestamp parser first; fall back to git's fuzzy
    // approxidate so relative forms ("2.weeks.ago", "yesterday", ...) work.
    if let Some(ts) = parse_reflog_expire_date(value) {
        return Ok(ts);
    }
    if let Some(ts) = crate::commands::approxidate::parse_approxidate(value) {
        return Ok(ts);
    }
    eprintln!("fatal: invalid timestamp '{value}' given to '{option}'");
    Err(GitError::Exit(128))
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
    if let Ok(git_dir) = session::cli_git_dir()
        && let Ok(format) = repository_object_format(&git_dir)
    {
        if let Ok(Some(refname)) =
            sley_rev::resolve_revision_symbolic_full_name(&git_dir, format, value)
        {
            return Ok(refname);
        }
        let store = FileRefStore::new(&git_dir, format);
        if store.read_ref(&format!("refs/{value}"))?.is_some() {
            return Ok(format!("refs/{value}"));
        }
    }
    branch_ref_name(value)
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

fn check_ignore_tracked_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<(BTreeSet<Vec<u8>>, Vec<Vec<u8>>)> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok((BTreeSet::new(), Vec::new()));
    };
    let mut tracked = BTreeSet::new();
    let mut gitlinks = Vec::new();
    for entry in index.entries {
        let path = entry.path.into_bytes();
        if sley_index::is_gitlink(entry.mode) {
            gitlinks.push(path.clone());
        }
        tracked.insert(path);
    }
    Ok((tracked, gitlinks))
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
                return None;
            }
            // Git unquotes C-style quoted pathspecs read in LF mode (e.g.
            // `"file\101.t"` -> `fileA.t`); with `--pathspec-file-nul` the bytes
            // are taken verbatim, so a leading quote stays literal.
            if !nul && entry.first() == Some(&b'"') {
                let mut unquoted = Vec::new();
                if commands::ref_command_stream::unquote_c_style(entry, &mut unquoted).is_some() {
                    return Some(PathBuf::from(
                        String::from_utf8_lossy(&unquoted).into_owned(),
                    ));
                }
            }
            Some(PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
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
    // git's "HEAD is now at" line re-encodes the subject from the commit's stored
    // `encoding` header to the log output encoding (i18n.logOutputEncoding, else
    // i18n.commitEncoding, else UTF-8) — t7102 cells 7/8. Write the result as raw
    // bytes since a non-UTF-8 output encoding (e.g. ISO8859-1) is not valid UTF-8.
    let config = read_repo_config(git_dir)?;
    let from = commit
        .encoding
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    let to = log_output_encoding(&config);
    let reencoded = log_reencode_message(commit.message, &from, &to);
    let subject = commit_subject_bytes(&reencoded);
    let mut stdout = io::stdout().lock();
    write!(
        stdout,
        "HEAD is now at {} ",
        format_log_abbrev_oid(commit_oid)
    )?;
    stdout.write_all(subject)?;
    writeln!(stdout)?;
    Ok(())
}

/// Git clean file selection without `-d` or pathspecs: a worktree-root file is
/// always eligible; a file in a subdirectory is eligible only when its immediate
/// parent directory contains tracked content (otherwise the file lives in a
/// wholly-untracked directory that Git would only remove under `-d`). This holds

fn checkout_create_or_reset_branch(
    git_dir: &Path,
    start_git_dir: &Path,
    format: ObjectFormat,
    branch: &str,
    start: &str,
    force: bool,
    create_reflog: bool,
    committer: Vec<u8>,
) -> Result<bool> {
    let store = FileRefStore::new(git_dir, format);
    if branch == "HEAD" || branch == "@" {
        eprintln!("fatal: '{branch}' is not a valid branch name");
        return Err(GitError::Exit(128));
    }
    let name = branch_ref_name(branch)?;
    let existing = store.read_ref(&name)?;
    if existing.is_some() && !force {
        eprintln!("fatal: a branch named '{branch}' already exists");
        return Err(GitError::Exit(128));
    }
    // The start point (often the implicit "HEAD") is resolved against the
    // worktree the command runs from — `git worktree add` from a linked
    // worktree branches off *that* worktree's HEAD.
    let start_oid = match resolve_checkout_start_oid(start_git_dir, format, start) {
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
    let db = FileObjectDatabase::from_git_dir(start_git_dir, format);
    let start_oid = sley_rev::peel_to_commit(&db, format, &start_oid)?;
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
        let reflog = store
            .should_write_reflog_for_update(&name, create_reflog)?
            .then(|| ReflogEntry {
                old_oid: ObjectId::null(format),
                new_oid: start_oid,
                committer,
                message: format!("branch: Created from {start}").into_bytes(),
            });
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(start_oid),
            reflog,
        });
        tx.commit()?;
        Ok(false)
    }
}

fn resolve_checkout_start_oid(
    git_dir: &Path,
    format: ObjectFormat,
    start: &str,
) -> Result<Option<ObjectId>> {
    if let Some(oid) = resolve_checkout_merge_base_start_oid(git_dir, format, start)? {
        return Ok(Some(oid));
    }
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

fn resolve_checkout_merge_base_start_oid(
    git_dir: &Path,
    format: ObjectFormat,
    start: &str,
) -> Result<Option<ObjectId>> {
    let Some((left, right)) = start.split_once("...") else {
        return Ok(None);
    };
    if right.contains("...") {
        return Ok(None);
    }
    let left = if left.is_empty() { "HEAD" } else { left };
    let right = if right.is_empty() { "HEAD" } else { right };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let left = sley_rev::peel_to_commit(&db, format, &resolve_revision(git_dir, format, left)?)?;
    let right = sley_rev::peel_to_commit(&db, format, &resolve_revision(git_dir, format, right)?)?;
    let bases = sley_rev::merge_bases(git_dir, format, &db, &left, &right)?;
    match bases.as_slice() {
        [base] => Ok(Some(*base)),
        [] => {
            eprintln!("fatal: no merge base found");
            Err(GitError::Exit(128))
        }
        _ => {
            eprintln!("fatal: multiple merge bases found");
            Err(GitError::Exit(128))
        }
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

fn write_object_id_hex<W: Write + ?Sized>(
    writer: &mut W,
    oid: &ObjectId,
    width: Option<usize>,
) -> Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hex_len = oid.format().hex_len();
    let width = width
        .map(|width| width.clamp(4, hex_len))
        .unwrap_or(hex_len);
    let mut out = [0u8; 64];
    for (index, byte) in oid.as_bytes().iter().copied().enumerate() {
        out[index * 2] = HEX[(byte >> 4) as usize];
        out[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    writer.write_all(&out[..width])?;
    Ok(())
}

fn print_tree_with_prefix(
    db: Option<&FileObjectDatabase>,
    format: ObjectFormat,
    body: &[u8],
    prefix: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = BufWriter::with_capacity(128 * 1024, stdout.lock());
    let mut path = prefix.to_vec();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        if options.tree_only && tree_entry_object_type(entry.mode()) == ObjectType::Blob {
            continue;
        }
        let path_len = path.len();
        path.extend_from_slice(entry.name);
        print_tree_entry_to_writer(&mut stdout, db, &entry, &path, options)?;
        path.truncate(path_len);
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
        write!(writer, "{:06o} {} ", entry.mode(), object_type.as_str())?;
        write_tree_oid(writer, entry.oid(), options)?;
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
        write_status_quoted_path(writer, path, false)?;
    }
    Ok(())
}

fn write_tree_oid(
    writer: &mut impl Write,
    oid: &ObjectId,
    options: TreePrintOptions<'_>,
) -> Result<()> {
    write_object_id_hex(writer, oid, options.oid_abbrev)
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
    if let Some((_, size)) = db.read_object_header(oid)? {
        return Ok(size.to_string());
    }
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
                if !nul && entry.first() == Some(&b'"') {
                    let mut unquoted = Vec::new();
                    if commands::ref_command_stream::unquote_c_style(entry, &mut unquoted).is_some()
                    {
                        return Some(PathBuf::from(
                            String::from_utf8_lossy(&unquoted).into_owned(),
                        ));
                    }
                }
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

fn patch_validate_unified_context(value: &str, short: bool) -> Result<()> {
    commit_validate_unified_context(value, short)?;
    if git_count_value_is_negative(value) {
        eprintln!("fatal: '--unified' cannot be negative");
        return Err(GitError::Exit(128));
    }
    Ok(())
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

fn patch_validate_inter_hunk_context(value: &str) -> Result<()> {
    commit_validate_inter_hunk_context(value)?;
    if git_count_value_is_negative(value) {
        eprintln!("fatal: '--inter-hunk-context' cannot be negative");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn commit_inter_hunk_context_expects_numerical_value_error() -> Result<()> {
    eprintln!("error: option `inter-hunk-context' expects a numerical value");
    Err(GitError::Exit(129))
}

fn git_count_value_is_negative(value: &str) -> bool {
    let number = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    number.trim_start().starts_with('-')
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

/// The resolved commit-message cleanup mode (git's `enum
/// commit_msg_cleanup_mode`). The raw `--cleanup`/`commit.cleanup` arg plus
/// whether an editor runs resolve to one of these via
/// [`resolve_commit_cleanup_mode`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitCleanupMode {
    /// `verbatim` → `COMMIT_MSG_CLEANUP_NONE`: no cleanup at all.
    Verbatim,
    /// `whitespace` (and the non-editor default) → `COMMIT_MSG_CLEANUP_SPACE`:
    /// strip trailing whitespace, squash blank-line runs, drop leading/trailing
    /// blanks. Comment lines are preserved.
    Whitespace,
    /// `strip` (and the editor default) → `COMMIT_MSG_CLEANUP_ALL`: whitespace
    /// cleanup plus dropping comment lines.
    Strip,
    /// `scissors` (with an editor) → `COMMIT_MSG_CLEANUP_SCISSORS`: truncate at
    /// the scissors line, then whitespace cleanup (comments preserved).
    Scissors,
}

/// Resolve the raw `--cleanup`/`commit.cleanup` argument (or its absence) to a
/// concrete [`CommitCleanupMode`], honouring git's editor-dependent defaults
/// (`get_cleanup_mode`): `default`/absent → `ALL` with an editor else `SPACE`;
/// `scissors` → `SCISSORS` with an editor else `SPACE`. Unknown values are
/// rejected earlier by [`validate_commit_cleanup_mode`], so we treat them as the
/// default here.
fn resolve_commit_cleanup_mode(arg: Option<&str>, use_editor: bool) -> CommitCleanupMode {
    let editor_default = if use_editor {
        CommitCleanupMode::Strip
    } else {
        CommitCleanupMode::Whitespace
    };
    match arg {
        None | Some("default") => editor_default,
        Some("verbatim") => CommitCleanupMode::Verbatim,
        Some("whitespace") => CommitCleanupMode::Whitespace,
        Some("strip") => CommitCleanupMode::Strip,
        Some("scissors") => {
            if use_editor {
                CommitCleanupMode::Scissors
            } else {
                CommitCleanupMode::Whitespace
            }
        }
        Some(_) => editor_default,
    }
}

/// Apply a resolved cleanup mode to a message (git's `cleanup_message`):
///   * SCISSORS (or `verbose`) truncates the message at the scissors line.
///   * Any mode other than NONE/Verbatim runs `strbuf_stripspace`, additionally
///     dropping comment lines under `Strip` (ALL).
fn commit_cleanup_message(
    mut message: Vec<u8>,
    mode: CommitCleanupMode,
    comment_char: &str,
    verbose: bool,
) -> Vec<u8> {
    if verbose || mode == CommitCleanupMode::Scissors {
        let end = commit_locate_scissors(&message, comment_char);
        message.truncate(end);
    }
    match mode {
        CommitCleanupMode::Verbatim => message,
        CommitCleanupMode::Strip => commit_stripspace_message(&message, Some(comment_char)),
        CommitCleanupMode::Whitespace | CommitCleanupMode::Scissors => {
            commit_stripspace_message(&message, None)
        }
    }
}

fn commit_stripspace_message(message: &[u8], comment_char: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pending_blank = false;
    let comment = comment_char.map(str::as_bytes);
    for raw_line in message.split(|byte| *byte == b'\n') {
        let line = commit_trim_trailing_space(raw_line);
        if comment.is_some_and(|prefix| line.starts_with(prefix)) {
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

fn commit_trim_trailing_space(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r'))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    &line[..end]
}

/// git's `wt_status_locate_end` over a byte message: the offset of the scissors
/// ("cut") line, or the message length when none is present. Everything from the
/// scissors line on is below the cut and is dropped by SCISSORS/verbose cleanup.
fn commit_locate_scissors(message: &[u8], comment_char: &str) -> usize {
    const CUT_BODY: &[u8] = b"------------------------ >8 ------------------------\n";
    // pattern head (no leading newline): "<comment> <cut_body>"
    let mut head = comment_char.as_bytes().to_vec();
    head.push(b' ');
    head.extend_from_slice(CUT_BODY);
    if message.starts_with(&head) {
        return 0;
    }
    // full pattern: "\n<comment> <cut_body>"
    let mut pattern = vec![b'\n'];
    pattern.extend_from_slice(&head);
    if pattern.len() > message.len() {
        return message.len();
    }
    match message
        .windows(pattern.len())
        .position(|w| w == pattern.as_slice())
    {
        Some(p) => (p + 1).min(message.len()),
        None => message.len(),
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

/// Split `git diff`'s positional arguments into leading revisions (resolved to
/// tree oids) and the remaining pathspec arguments.
///
/// Accepts `diff <rev>`, `diff <rev> <rev>`, `diff <rev>..<rev>`, and
/// `diff <rev>...<rev>` (symmetric, from the merge base) before any `-- <path>`
/// separator. A leading argument is treated as a revision only while it resolves
/// as one; the first argument that fails to resolve — and everything after it — is
/// a pathspec, so ordinary file paths keep working without an explicit `--`.


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
                if sley_refs::validate_ref_name(&name).is_err() {
                    return Ok(None);
                }
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
        paths.insert(
            head_ref.to_string(),
            canonical.to_string_lossy().into_owned(),
        );
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
            paths.insert(
                refname.to_string(),
                canonical.to_string_lossy().into_owned(),
            );
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
    let push_remote = for_each_ref_push_remote(config, branch)?;
    let remote_name = push_remote.name.clone();
    // The display name is exposed by `%(push:remotename)` even when the push
    // destination itself does not resolve, so compute it up front and keep it
    // on every return path (git's branch_get_push reports the remote regardless).
    let display_remote = remote_display_name(push_remote);
    if remote_name == "." {
        return Some(ForEachRefPush {
            refname: None,
            remote: display_remote,
            remote_ref: None,
        });
    }
    // An explicit push refspec (remote.<name>.push) takes precedence over
    // push.default — mirrors `remote->push.nr` in git's branch_get_push_1.
    if let Some(push) = config.get("remote", Some(remote_name.as_str()), "push") {
        if let Some(remote_ref) = map_remote_push_refspec(push, refname) {
            let tracking = map_remote_tracking_ref(config, &remote_name, &remote_ref);
            return Some(ForEachRefPush {
                refname: tracking,
                remote: display_remote,
                remote_ref: Some(remote_ref),
            });
        }
        return Some(ForEachRefPush {
            refname: None,
            remote: display_remote,
            remote_ref: None,
        });
    }
    // Otherwise resolve the destination through push.default, exactly as
    // git's branch_get_push_1 switch does.
    let push_default = config.get("push", None, "default").unwrap_or("simple");
    let tracking = match push_default {
        "nothing" => None,
        // matching/current push the branch's own ref through the push remote's
        // fetch refspec (tracking_for_push_dest on branch->refname).
        "matching" | "current" => map_remote_tracking_ref(config, &remote_name, refname),
        // upstream uses the branch's configured upstream destination.
        "upstream" => for_each_ref_upstream(config, refname).map(|up| up.refname),
        // simple/unspecified (the default): the push destination must equal the
        // upstream destination, otherwise there is no single 'simple' target and
        // %(push) is empty (the remote name is still reported).
        _ => {
            let up = for_each_ref_upstream(config, refname).map(|up| up.refname);
            let cur = map_remote_tracking_ref(config, &remote_name, refname);
            match (up, cur) {
                (Some(up), Some(cur)) if up == cur => Some(cur),
                _ => None,
            }
        }
    };
    Some(ForEachRefPush {
        refname: tracking,
        remote: display_remote,
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
    git_dir: &Path,
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
    for_each_ref_ahead_behind(git_dir, db, format, oid, &upstream_oid)
}

fn for_each_ref_ahead_behind_with_diagnostic(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ForEachRefTrack>> {
    let Ok(local_commit) = sley_rev::peel_to_commit(db, format, oid) else {
        if let Ok(object) = db.read_object(oid) {
            eprintln!(
                "error: object {} is a {}, not a commit",
                oid,
                object.object_type.as_str()
            );
        }
        return Ok(None);
    };
    let Ok(target_commit) = sley_rev::peel_to_commit(db, format, target) else {
        return Ok(None);
    };
    let (ahead, behind) =
        sley_rev::ahead_behind_counts(git_dir, format, db, &local_commit, &target_commit)?;
    Ok(Some(ForEachRefTrack {
        ahead,
        behind,
        gone: false,
    }))
}

fn for_each_ref_ahead_behind(
    git_dir: &Path,
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
    let (ahead, behind) =
        sley_rev::ahead_behind_counts(git_dir, format, db, &local_commit, &target_commit)?;
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

fn for_each_ref_validate_tag_pointer(
    tag_oid: &ObjectId,
    contents: &ForEachRefContents<'_>,
    target_oid: &ObjectId,
    target: &sley_object::EncodedObject,
) -> Result<()> {
    if contents
        .tag_object_type
        .is_some_and(|object_type| object_type != target.object_type)
    {
        eprintln!("error: bad tag pointer to {target_oid} in {tag_oid}");
        return Err(GitError::Exit(128));
    }
    Ok(())
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
    // %(signature*) verification of the ref object and its peeled tag target.
    signature: Option<commands::signing::GpgVerification>,
    peeled_signature: Option<commands::signing::GpgVerification>,
    mailmap: &'a commands::utility::Mailmap,
    // All ref names in the store + `core.warnambiguousrefs`, for the
    // `:short` atoms' shorten_unambiguous_ref resolution.
    ref_names: &'a std::collections::HashSet<String>,
    warn_ambiguous_refs: bool,
}

impl ForEachRefFormatContext<'_> {
    /// Shorten a fully-qualified refname to its unambiguous abbreviation, the
    /// way git's `%(refname:short)` / `%(symref:short)` / `%(upstream:short)` do.
    fn shorten_ref(&self, refname: &str) -> String {
        sley_ref_filter::shorten_unambiguous_ref(refname, self.warn_ambiguous_refs, |candidate| {
            self.ref_names.contains(candidate)
        })
    }
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

/// Emit one `%(signature[:opt])` (or `%(*signature[:opt])`) sub-field from a
/// verified signature, mirroring git's `grab_signature` field mapping. `option`
/// is the placeholder text after `signature` — `""` for the bare atom, or
/// `":grade"`, `":key"`, … for the typed sub-fields.
fn write_for_each_ref_signature(
    stdout: &mut impl Write,
    verification: &commands::signing::GpgVerification,
    option: &str,
) -> Result<()> {
    match option.strip_prefix(':').unwrap_or("") {
        // The bare atom prints gpg's human-readable verification output.
        "" => stdout.write_all(&commands::signing::bare_signature_output(verification))?,
        // grade: 'G'/'U'/'B'/'E'/'N' — git downgrades a good-but-untrusted
        // signature to 'U', which pretty_code already encodes.
        "grade" => stdout.write_all(&[verification.pretty_code()])?,
        "key" => stdout.write_all(verification.key.as_bytes())?,
        "signer" => stdout.write_all(verification.signer.as_bytes())?,
        "fingerprint" => stdout.write_all(verification.fingerprint.as_bytes())?,
        "primarykeyfingerprint" => stdout.write_all(verification.primary_fingerprint.as_bytes())?,
        "trustlevel" => stdout.write_all(verification.trust.as_bytes())?,
        _ => {}
    }
    Ok(())
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
                    stdout.write_all(context.shorten_ref(context.refname).as_bytes())?
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
                "worktreepath" => {
                    stdout.write_all(context.worktree_path.unwrap_or("").as_bytes())?
                }
                "symref" => stdout.write_all(context.symref.unwrap_or("").as_bytes())?,
                "symref:short" => stdout.write_all(
                    context
                        .symref
                        .map(|symref| context.shorten_ref(symref))
                        .unwrap_or_default()
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
                        .map(|upstream| context.shorten_ref(&upstream.refname))
                        .unwrap_or_default()
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
                        .map(|refname| context.shorten_ref(refname))
                        .unwrap_or_default()
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
                "signature"
                | "signature:grade"
                | "signature:key"
                | "signature:signer"
                | "signature:fingerprint"
                | "signature:primarykeyfingerprint"
                | "signature:trustlevel" => {
                    if let Some(signature) = context.signature.as_ref() {
                        write_for_each_ref_signature(
                            stdout,
                            signature,
                            &placeholder["signature".len()..],
                        )?;
                    }
                }
                "*signature"
                | "*signature:grade"
                | "*signature:key"
                | "*signature:signer"
                | "*signature:fingerprint"
                | "*signature:primarykeyfingerprint"
                | "*signature:trustlevel" => {
                    if let Some(signature) = context.peeled_signature.as_ref() {
                        write_for_each_ref_signature(
                            stdout,
                            signature,
                            &placeholder["*signature".len()..],
                        )?;
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
                "authordate" | "*authordate" | "committerdate" | "*committerdate"
                | "taggerdate" | "*taggerdate" | "creatordate" | "*creatordate" => {
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
                        stdout.write_all(
                            for_each_ref_lstrip_name(context.refname, count).as_bytes(),
                        )?;
                    } else if let Some(value) = other.strip_prefix("refname:rstrip=") {
                        let count = parse_for_each_ref_strip_count(value)?;
                        stdout.write_all(
                            for_each_ref_rstrip_name(context.refname, count).as_bytes(),
                        )?;
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
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
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
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
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
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
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
                        let width =
                            for_each_ref_oid_atom_width(arg, other, context.objectname_abbrev)?;
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
                    } else if let Some(result) =
                        for_each_ref_try_trailers_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_email_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_name_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(result) = for_each_ref_try_date_atom(stdout, other, context)
                    {
                        result?;
                    } else if let Some(rev) = other.strip_prefix("ahead-behind:") {
                        let target = resolve_revision(context.git_dir, context.format, rev)?;
                        if let Some(track) = for_each_ref_ahead_behind_with_diagnostic(
                            context.git_dir,
                            context.db,
                            context.format,
                            context.oid,
                            &target,
                        )? {
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
                    } else if let Some((peeled, opts)) = for_each_ref_describe_atom(other) {
                        // %(describe[:opts]) / %(*describe[:opts]) reuse the same
                        // describe engine as log's %(describe); git treats describe
                        // failures as an empty placeholder.
                        let spec = for_each_ref_parse_describe_opts(opts)?;
                        let target = if peeled {
                            context.peeled_object.as_ref().map(|object| object.oid)
                        } else {
                            Some(*context.oid)
                        };
                        if let Some(target) = target
                            && let Some(text) = crate::commands::describe::describe_for_format(
                                context.git_dir,
                                context.format,
                                context.db,
                                &target,
                                spec.tags,
                                spec.abbrev,
                                &spec.matches,
                                &spec.excludes,
                            )?
                        {
                            stdout.write_all(text.as_bytes())?;
                        }
                    } else if other.starts_with("HEAD:") {
                        // git's head_atom_parser: %(HEAD) takes no arguments.
                        eprintln!("fatal: %(HEAD) does not take arguments");
                        return Err(GitError::Exit(128));
                    } else if let Some(arg) = other
                        .strip_prefix("subject:")
                        .or_else(|| other.strip_prefix("*subject:"))
                    {
                        // The only valid %(subject) arg is `sanitize` (matched
                        // above); anything else is rejected like git's
                        // subject_atom_parser.
                        eprintln!("fatal: unrecognized %(subject) argument: {arg}");
                        return Err(GitError::Exit(128));
                    } else {
                        return Err(GitError::Command(format!(
                            "unsupported for-each-ref format placeholder %({other})"
                        )));
                    }
                }
            }
            Ok(())
        },
    )
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
                    stdout.write_all(context.shorten_ref(refname).as_bytes())?
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
                    None => write_object_id_hex(stdout, oid, None)?,
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
                    write_for_each_ref_identity_date_mode(stdout, identity, mode)?
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
fn setup_for_each_ref_email_options(
    arg: &str,
) -> std::result::Result<ForEachRefEmailOptions, String> {
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
        Some(arg) => match setup_for_each_ref_email_options(arg) {
            Ok(options) => options,
            Err(bad_arg) => {
                let name = atom.strip_prefix('*').unwrap_or(atom);
                eprintln!("fatal: unrecognized %({name}) argument: {bad_arg}");
                return Some(Err(GitError::Exit(128)));
            }
        },
        None => ForEachRefEmailOptions::default(),
    };
    Some(for_each_ref_write_email(
        stdout, context, peeled, role, options,
    ))
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
        None => sley_pretty::ForEachRefTrailerOptions::default(),
        Some(arg) => match sley_pretty::parse_for_each_ref_trailer_options(arg) {
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
                sley_pretty::format_trailers_from_commit(trailer_src, &options);
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
    let Some(mode) = DateMode::parse_atom_modifier(arg) else {
        let name = atom.strip_prefix('*').unwrap_or(atom);
        eprintln!(
            "fatal: unrecognized %({name}) argument: {}",
            arg.unwrap_or("")
        );
        return Some(Err(GitError::Exit(128)));
    };
    Some((|| -> Result<()> {
        if let Some(identity) = for_each_ref_typed_identity(context, peeled, role)
            && let Some(value) = for_each_ref_identity_date(identity, &mode)
        {
            stdout.write_all(value.as_bytes())?;
        }
        Ok(())
    })())
}

/// Recognize the `%(describe)` family. Returns `(peeled, opts)` where `peeled`
/// is set for the deref form `%(*describe…)` and `opts` is whatever follows the
/// colon (empty when there is none). Returns `None` for non-describe atoms.
fn for_each_ref_describe_atom(placeholder: &str) -> Option<(bool, &str)> {
    let (peeled, rest) = match placeholder.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, placeholder),
    };
    if rest == "describe" {
        Some((peeled, ""))
    } else {
        rest.strip_prefix("describe:").map(|opts| (peeled, opts))
    }
}

/// Parse `%(describe:opts)` like git's `describe_atom_parser`: walk the
/// comma-separated options, and on the first unrecognized token report
/// `unrecognized %(describe) argument: <bad-token-through-end>` (git keeps the
/// rest of the string, not just the offending token).
fn for_each_ref_parse_describe_opts(opts: &str) -> Result<sley_pretty::DescribeSpec> {
    let mut spec = sley_pretty::DescribeSpec::default();
    let mut rest = opts;
    while !rest.is_empty() {
        let (part, next) = match rest.split_once(',') {
            Some((part, next)) => (part, next),
            None => (rest, ""),
        };
        if part == "tags" {
            spec.tags = true;
        } else if let Some(value) = part.strip_prefix("abbrev=") {
            match value.parse::<usize>() {
                Ok(width) => spec.abbrev = Some(width),
                Err(_) => return Err(for_each_ref_bad_describe_arg(rest)),
            }
        } else if let Some(value) = part.strip_prefix("match=") {
            spec.matches.push(value.to_string());
        } else if let Some(value) = part.strip_prefix("exclude=") {
            spec.excludes.push(value.to_string());
        } else {
            return Err(for_each_ref_bad_describe_arg(rest));
        }
        rest = next;
    }
    Ok(spec)
}

fn for_each_ref_bad_describe_arg(bad: &str) -> GitError {
    eprintln!("fatal: unrecognized %(describe) argument: {bad}");
    GitError::Exit(128)
}

/// For an oid atom like `tree:short` / `parent:short=7`, return the option
/// argument (`short` or `short=7`) when `placeholder` is exactly `atom:<arg>`.
fn for_each_ref_oid_atom_arg<'a>(placeholder: &'a str, atom: &str) -> Option<&'a str> {
    let rest = placeholder.strip_prefix(atom)?;
    rest.strip_prefix(':')
}

/// Parse the `short`/`short=N` argument of an oid atom into an abbreviation
/// width, mirroring git's `oid_atom_parser` validation. A bare `short` resolves
/// to the repository's `DEFAULT_ABBREV` (git's `O_SHORT` case), supplied by the
/// caller via `default_abbrev`; `short=N` overrides it.
fn for_each_ref_oid_atom_width(
    arg: &str,
    atom: &str,
    default_abbrev: Option<usize>,
) -> Result<Option<usize>> {
    if arg == "short" {
        Ok(default_abbrev)
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
    attributes: Option<sley_worktree::StandardAttributeMatcher>,
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
        let (relative, pathspec_cwd) = match cwd.strip_prefix(&root) {
            Ok(relative) => (relative, cwd.as_path()),
            Err(_) => (Path::new(""), root.as_path()),
        };
        let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
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
            let parse_arg = normalize_absolute_cli_pathspec(&root, pathspec_cwd, arg)?;
            let element = parse_normalized_pathspec_element(&prefix, &parse_arg, magic)?;
            // Under literal magic, wildcard characters carry no special meaning.
            let is_glob =
                !element.magic().literal && sley_worktree::pathspec_is_glob(element.pattern());
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                pathspec_cwd.join(arg_path)
            };
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                recursive: arg == "." || arg.ends_with('/') || absolute.is_dir(),
                is_glob,
                element,
                matched: Cell::new(false),
            });
        }
        let needs_attrs = filters
            .iter()
            .any(|filter| !filter.element.attr_requirements().is_empty());
        let attributes = if needs_attrs {
            Some(sley_worktree::StandardAttributeMatcher::from_worktree_root(
                &root,
            )?)
        } else {
            None
        };
        Ok(Self {
            prefix,
            full_name,
            filters,
            attributes,
        })
    }

    fn untracked_pathspecs(&self) -> Vec<sley_worktree::UntrackedPathspecFilter> {
        self.filters
            .iter()
            .filter(|filter| !filter.is_exclude())
            .map(|filter| sley_worktree::UntrackedPathspecFilter {
                path: filter.element.pattern().to_vec(),
                recursive: filter.recursive,
                is_glob: filter.is_glob,
            })
            .collect()
    }

    fn display(&self, path: &[u8]) -> Option<Vec<u8>> {
        if !self.matches(path) {
            return None;
        }
        if self.full_name || self.prefix.is_empty() {
            return Some(path.to_vec());
        }
        // git renders the matched path relative to the cwd prefix (which it
        // treats as ending in '/'), emitting `../` for each prefix component
        // not shared with `path` — not "up to root then the full path".
        let mut prefix = self.prefix.clone();
        prefix.push(b'/');
        Some(relative_path_bytes(path, &prefix))
    }

    fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return self.path_in_default_scope(path);
        }
        let attrs = self.attributes.as_ref();
        let matched = pathspec_filters_match_with(&self.filters, path, |filter, path| {
            filter.matches(path)
                && pathspec_attrs_match_with(&filter.element, |requested| {
                    attribute_checks_for_matching(
                        attrs
                            .map(|matcher| matcher.attributes_for_path(path, requested, false))
                            .unwrap_or_default(),
                    )
                })
        });
        matched
            && (pathspec_filters_have_include(&self.filters) || self.path_in_default_scope(path))
    }

    fn path_in_default_scope(&self, path: &[u8]) -> bool {
        self.full_name
            || self.prefix.is_empty()
            || path
                .strip_prefix(self.prefix.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
                .is_some_and(|rest| !rest.is_empty())
    }

    fn exit_if_unmatched(&self) -> Result<()> {
        let mut has_unmatched = false;
        for filter in &self.filters {
            if !filter.is_exclude() && !filter.matched.get() {
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

fn normalize_absolute_cli_pathspec(root: &Path, cwd: &Path, arg: &str) -> Result<String> {
    let path = Path::new(arg);
    if !path.is_absolute() {
        return Ok(arg.to_string());
    }
    let absolute = fs::canonicalize(path)?;
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| GitError::InvalidPath(format!("pathspec {arg} is outside worktree")))?;
    let repo_path = relative.to_string_lossy().replace('\\', "/");
    if repo_path.is_empty() {
        return Ok(":/".to_string());
    }
    if cwd == root {
        return Ok(repo_path);
    }
    Ok(format!(":(top){repo_path}"))
}

fn path_component_count(path: &[u8]) -> usize {
    path.split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .count()
}

/// Render `input` relative to `prefix`, a faithful byte-level port of git's
/// `relative_path()` (path.c) for the POSIX, both-relative case (no DOS drive).
/// `prefix` is the cwd prefix and must end with `/` when non-empty, matching
/// git's `cmd_prefix`. Emits `../` for each `prefix` component not shared with
/// `input`, then the unshared tail of `input`.
fn relative_path_bytes(input: &[u8], prefix: &[u8]) -> Vec<u8> {
    let in_len = input.len();
    let prefix_len = prefix.len();
    if in_len == 0 {
        return b"./".to_vec();
    }
    if prefix_len == 0 {
        return input.to_vec();
    }
    let is_sep = |byte: u8| byte == b'/';
    let mut i = 0usize;
    let mut j = 0usize;
    let mut prefix_off = 0usize;
    let mut in_off = 0usize;
    while i < prefix_len && j < in_len && prefix[i] == input[j] {
        if is_sep(prefix[i]) {
            while i < prefix_len && is_sep(prefix[i]) {
                i += 1;
            }
            while j < in_len && is_sep(input[j]) {
                j += 1;
            }
            prefix_off = i;
            in_off = j;
        } else {
            i += 1;
            j += 1;
        }
    }

    if i >= prefix_len && prefix_off < prefix_len {
        if j >= in_len {
            in_off = in_len;
        } else if is_sep(input[j]) {
            while j < in_len && is_sep(input[j]) {
                j += 1;
            }
            in_off = j;
        } else {
            i = prefix_off;
        }
    } else if j >= in_len && in_off < in_len && i < prefix_len && is_sep(prefix[i]) {
        while i < prefix_len && is_sep(prefix[i]) {
            i += 1;
        }
        in_off = in_len;
    }

    let input = &input[in_off..];
    if i >= prefix_len {
        if input.is_empty() {
            return b"./".to_vec();
        }
        return input.to_vec();
    }

    let mut out = Vec::new();
    while i < prefix_len {
        if is_sep(prefix[i]) {
            out.extend_from_slice(b"../");
            while i < prefix_len && is_sep(prefix[i]) {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    if !is_sep(prefix[prefix_len - 1]) {
        out.extend_from_slice(b"../");
    }
    out.extend_from_slice(input);
    out
}

fn log_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn log_option_requires_value_error(option: &str) -> GitError {
    eprintln!("error: option `{option}' requires a value");
    GitError::Exit(129)
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
            && bytes[tz_start + 1..]
                .iter()
                .all(|byte| byte.is_ascii_digit())
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

/// `git log -S`/`-G` with no value: parse-options "switch requires a value"
/// (exit 129). `kind` is the single letter (`S`/`G`).
fn log_pickaxe_requires_value_error(kind: &str) -> GitError {
    eprintln!("error: switch `{kind}' requires a value");
    GitError::Exit(129)
}

/// `git log -S ""`/`-G ""` with an empty value (exit 129).
fn log_pickaxe_empty_error(kind: &str) -> GitError {
    eprintln!("error: -{kind} requires a non-empty argument");
    GitError::Exit(129)
}

/// Combining multiple pickaxe kinds (`-S`/`-G`/`--find-object`) — git rejects
/// with exit 128.
fn log_pickaxe_kinds_conflict_error() -> GitError {
    eprintln!("fatal: options '-G', '-S', and '--find-object' cannot be used together");
    GitError::Exit(128)
}

/// `-G` with `--pickaxe-regex` (exit 128).
fn log_pickaxe_g_regex_conflict_error() -> GitError {
    eprintln!(
        "fatal: options '-G' and '--pickaxe-regex' cannot be used together, use '--pickaxe-regex' with '-S'"
    );
    GitError::Exit(128)
}

/// `--pickaxe-all` with `--find-object` (exit 128).
fn log_pickaxe_all_objfind_conflict_error() -> GitError {
    eprintln!(
        "fatal: options '--pickaxe-all' and '--find-object' cannot be used together, use '--pickaxe-all' with '-G' and '-S'"
    );
    GitError::Exit(128)
}

fn log_date_mode(value: &str) -> Result<DateMode> {
    match DateMode::parse(value) {
        Some(mode) => Ok(mode),
        None => {
            log_unknown_date_format(value)?;
            unreachable!("log_unknown_date_format always returns an error")
        }
    }
}

fn log_unknown_date_format(value: &str) -> Result<()> {
    eprintln!("fatal: unknown date format {value}");
    Err(GitError::Exit(128))
}

pub(crate) fn log_parse_diff_algorithm(value: &str) -> sley_diff_merge::DiffAlgorithm {
    match value {
        "minimal" => sley_diff_merge::DiffAlgorithm::Minimal,
        "patience" => sley_diff_merge::DiffAlgorithm::Patience,
        "histogram" => sley_diff_merge::DiffAlgorithm::Histogram,
        _ => sley_diff_merge::DiffAlgorithm::Myers,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogDecorationMode {
    Off,
    Short,
    Full,
}

/// A single normalized decoration ref-filter pattern. git's
/// `normalize_glob_ref`: a pattern not starting with `refs/` (and not `HEAD`)
/// is prefixed with `refs/`; a trailing `/` is stripped; the pattern matches
/// either as a glob (`wildmatch`) or, when it has no glob metacharacters, as a
/// path-prefix (`refs/foo` matches `refs/foo` and `refs/foo/...`).
#[derive(Debug, Clone)]
struct DecorationPattern {
    normalized: String,
    is_glob: bool,
}

impl DecorationPattern {
    fn new(pattern: &str) -> Self {
        let mut normalized = String::new();
        if !pattern.starts_with("refs/") && pattern != "HEAD" {
            normalized.push_str("refs/");
        }
        normalized.push_str(pattern);
        while normalized.ends_with('/') {
            normalized.pop();
        }
        let is_glob = pattern.bytes().any(|b| matches!(b, b'*' | b'?' | b'['));
        DecorationPattern {
            normalized,
            is_glob,
        }
    }

    fn matches(&self, refname: &str) -> bool {
        if self.is_glob {
            sley_pathspec::wildmatch(self.normalized.as_bytes(), refname.as_bytes(), 0)
        } else {
            // Prefix match: refname == pattern, or refname starts with
            // "pattern/".
            match refname.strip_prefix(&self.normalized) {
                Some(rest) => rest.is_empty() || rest.starts_with('/'),
                None => false,
            }
        }
    }
}

/// Decoration ref filter mirroring git's `decoration_filter` / `ref_filter_match`.
#[derive(Debug, Clone, Default)]
pub(crate) struct DecorationFilter {
    include: Vec<DecorationPattern>,
    exclude: Vec<DecorationPattern>,
    exclude_config: Vec<DecorationPattern>,
}

impl DecorationFilter {
    pub(crate) fn new(include: &[String], exclude: &[String], exclude_config: &[String]) -> Self {
        DecorationFilter {
            include: include.iter().map(|p| DecorationPattern::new(p)).collect(),
            exclude: exclude.iter().map(|p| DecorationPattern::new(p)).collect(),
            exclude_config: exclude_config
                .iter()
                .map(|p| DecorationPattern::new(p))
                .collect(),
        }
    }

    /// Whether `refname` survives the filter (git `ref_filter_match`): explicit
    /// excludes first, then include-only (any include patterns ⇒ refname must
    /// match one), then config excludes, else keep.
    fn matches(&self, refname: &str) -> bool {
        if self.exclude.iter().any(|p| p.matches(refname)) {
            return false;
        }
        if !self.include.is_empty() {
            return self.include.iter().any(|p| p.matches(refname));
        }
        if self.exclude_config.iter().any(|p| p.matches(refname)) {
            return false;
        }
        true
    }
}

pub(crate) fn log_decoration_map(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    mode: LogDecorationMode,
    filter: &DecorationFilter,
) -> Result<HashMap<ObjectId, Vec<String>>> {
    let store = FileRefStore::new(git_dir, format);
    let head_ref = store.current_branch_ref()?;
    let mut decorations = HashMap::<ObjectId, Vec<String>>::new();
    // Git stores decorations in a per-object linked list by prepending each ref
    // as refs_for_each_ref() visits sorted refs; the rendered order is therefore
    // reverse ref iteration order. HEAD is loaded after refs, so it prepends over
    // all ordinary names and can collapse with the branch it points at.
    let mut head_decoration: Option<(ObjectId, String)> = None;
    let mut head_branch_shown_inline = false;
    if let Some(head_target) = store.read_ref("HEAD")? {
        let head_kept = filter.matches("HEAD");
        match head_target {
            RefTarget::Symbolic(name) => {
                if let Some(RefTarget::Direct(oid)) = store.read_ref(&name)?
                    && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
                {
                    let branch_kept = filter.matches(&name);
                    if head_kept && branch_kept {
                        let label = log_decoration_ref_name(&name, mode);
                        head_decoration = Some((commit, format!("HEAD -> {label}")));
                        head_branch_shown_inline = true;
                    } else if head_kept {
                        head_decoration = Some((commit, "HEAD".to_string()));
                    }
                }
            }
            RefTarget::Direct(oid) => {
                if head_kept && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) {
                    head_decoration = Some((commit, "HEAD".to_string()));
                }
            }
        }
    }
    for reference in store.list_refs()? {
        if head_branch_shown_inline && head_ref.as_deref() == Some(reference.name.as_str()) {
            continue;
        }
        if !filter.matches(&reference.name) {
            continue;
        }
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) else {
            continue;
        };
        let label = log_decoration_label(&reference.name, mode);
        decorations.entry(commit).or_default().insert(0, label);
    }
    if let Some((commit, label)) = head_decoration {
        decorations.entry(commit).or_default().insert(0, label);
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

pub(crate) fn commit_author_identity(raw: &[u8]) -> String {
    // Split the ident git's way (tolerant of broken emails / missing dates) and
    // re-join as `Name <email>`, exactly as pretty.c's pp_user_info renders the
    // Author:/Committer: line. A line with no `<…>` pair falls back to the raw
    // bytes.
    let Some(fields) = sley_core::split_ident_line(raw) else {
        return String::from_utf8_lossy(raw).into_owned();
    };
    let mut identity = String::new();
    identity.push_str(&String::from_utf8_lossy(fields.name));
    identity.push_str(" <");
    identity.push_str(&String::from_utf8_lossy(fields.email));
    identity.push('>');
    identity
}

/// `commit_author_identity` with an optional mailmap pass — the default/medium/
/// full pretty formats route the whole `Name <email>` through the mailmap when
/// `git log --use-mailmap`/`log.mailmap` is active (git's `pp_user_info`). When
/// `mailmap` is `None` (or empty) this is identical to `commit_author_identity`.
pub(crate) fn commit_identity_mailmapped(
    raw: &[u8],
    mailmap: Option<&commands::utility::Mailmap>,
) -> String {
    let identity = commit_author_identity(raw);
    let Some(mailmap) = mailmap.filter(|m| !m.is_empty()) else {
        return identity;
    };
    // Split `Name <email>` (commit_author_identity already trimmed the date).
    let (name, email) = match identity.rsplit_once(" <") {
        Some((name, rest)) => (name, rest.strip_suffix('>').unwrap_or(rest)),
        None => return identity,
    };
    let (name, email) = mailmap.map_user(name, email);
    format!("{name} <{email}>")
}

#[derive(Debug)]
struct SimpleLogRegex {
    alternatives: Vec<SimpleLogRegexAlternative>,
    /// `--perl-regexp` patterns compile through the full grep regex engine in
    /// PCRE mode instead of the simple BRE subset above.
    perl: Option<sley_grep::Regex>,
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
        Self::parse_with_diagnostic_verbosity(
            pattern,
            error_context,
            mode,
            sley_grep::RegexDiagnosticVerbosity::from_env(),
        )
    }

    fn parse_with_diagnostic_verbosity(
        pattern: &str,
        error_context: &'static str,
        mode: SimpleLogRegexMode,
        diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
    ) -> Result<Self> {
        if pattern.is_empty() {
            return Ok(Self {
                alternatives: vec![SimpleLogRegexAlternative {
                    anchor_start: false,
                    anchor_end: false,
                    tokens: Vec::new(),
                }],
                perl: None,
            });
        }
        if let SimpleLogRegexMode::Perl = mode {
            let regex =
                sley_grep::Regex::compile(pattern, sley_grep::RegexMode::Pcre, false, false)?;
            return Ok(Self {
                alternatives: Vec::new(),
                perl: Some(regex),
            });
        }
        let alternatives = match mode {
            SimpleLogRegexMode::Basic => split_log_regex_alternatives(pattern)
                .into_iter()
                .map(|alternative| {
                    SimpleLogRegexAlternative::parse(
                        alternative,
                        error_context,
                        diagnostic_verbosity,
                    )
                })
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
    fn parse(
        pattern: &str,
        error_context: &'static str,
        diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
    ) -> Result<Self> {
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
                    let (class, consumed) = parse_simple_log_regex_class(
                        &bytes[idx + 1..],
                        pattern,
                        error_context,
                        diagnostic_verbosity,
                    )?;
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
    parse_log_filter_patterns_with_diagnostic_verbosity(
        patterns,
        mode,
        sley_grep::RegexDiagnosticVerbosity::Verbose,
    )
}

fn parse_log_filter_patterns_with_diagnostic_verbosity(
    patterns: &[LogFilterPattern],
    mode: SimpleLogRegexMode,
    diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
) -> Result<Vec<SimpleLogRegex>> {
    patterns
        .iter()
        .map(|pattern| {
            SimpleLogRegex::parse_with_diagnostic_verbosity(
                &pattern.pattern,
                pattern.error_context,
                mode,
                diagnostic_verbosity,
            )
        })
        .collect()
}

fn log_grep_pattern_kind_from_config(
    config: &GitConfig,
    current: sley_grep::PatternKind,
    explicit: bool,
) -> sley_grep::PatternKind {
    if explicit {
        return current;
    }
    match config
        .get("grep", None, "patterntype")
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("fixed") => sley_grep::PatternKind::Fixed,
        Some("basic") => sley_grep::PatternKind::Basic,
        Some("extended") => sley_grep::PatternKind::Extended,
        Some("perl") => sley_grep::PatternKind::Perl,
        _ => current,
    }
}

fn compile_log_message_grep_matcher(
    patterns: &[String],
    kind: sley_grep::PatternKind,
    ignore_case: bool,
) -> Result<Option<sley_grep::GrepMatcher>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    sley_grep::GrepMatcher::compile_with_error_context(
        sley_grep::GrepCompileConfig {
            patterns,
            kind,
            ignore_case,
            word: false,
            line_regexp: false,
            diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity::Verbose,
        },
        "command line",
    )
    .map(Some)
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
    diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
) -> Result<(SimpleLogRegexClass, usize)> {
    let mut end = None;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b']' && idx > 0 {
            end = Some(idx);
            break;
        }
    }
    let Some(end) = end else {
        return log_regex_unterminated_class_error(
            bytes,
            pattern,
            error_context,
            diagnostic_verbosity,
        );
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
    _class_bytes: &[u8],
    pattern: &str,
    error_context: &str,
    diagnostic_verbosity: sley_grep::RegexDiagnosticVerbosity,
) -> Result<(SimpleLogRegexClass, usize)> {
    Err(sley_grep::report_regex_compile_error(
        error_context,
        pattern,
        diagnostic_verbosity,
        sley_grep::RegexDiagnosticDetail::UnbalancedBrackets,
    ))
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


// W31: CLI adapters for sley-pretty log format traits.
struct CliMailmapAdapter<'a>(&'a commands::utility::Mailmap);
impl sley_pretty::MailmapLookup for CliMailmapAdapter<'_> {
    fn map_user(&self, name: &str, email: &str) -> (String, String) {
        self.0.map_user(name, email)
    }
}

pub(crate) struct CliLogSignatureContext<'a> {
    pub git_dir: &'a Path,
    pub db: &'a FileObjectDatabase,
    pub config: &'a GitConfig,
    pub source_tag_signatures: &'a HashMap<ObjectId, commands::signing::GpgVerification>,
}

struct CliLogSignatureAdapter<'a>(&'a CliLogSignatureContext<'a>);
impl sley_pretty::LogSignatureLookup for CliLogSignatureAdapter<'_> {
    fn verification_for_oid(&self, oid: &ObjectId) -> Result<sley_pretty::LogSignatureView> {
        if let Some(v) = self.0.source_tag_signatures.get(oid) {
            return Ok(cli_log_signature_view(v));
        }
        let object = self.0.db.read_object(oid)?;
        let Some((payload, signature)) = commands::signing::commit_signature_payload(&object.body)
        else {
            return Ok(sley_pretty::LogSignatureView {
                trust: "undefined".into(),
                pretty_code: b'N',
                ..Default::default()
            });
        };
        Ok(cli_log_signature_view(&commands::signing::verify_payload(
            self.0.git_dir,
            Some(self.0.config),
            &payload,
            &signature,
        )?))
    }
}

fn cli_log_signature_view(v: &commands::signing::GpgVerification) -> sley_pretty::LogSignatureView {
    sley_pretty::LogSignatureView {
        trust: v.trust.clone(),
        signer: v.signer.clone(),
        key: v.key.clone(),
        fingerprint: v.fingerprint.clone(),
        primary_fingerprint: v.primary_fingerprint.clone(),
        pretty_code: v.pretty_code(),
        bare_output: commands::signing::bare_signature_output(v),
    }
}

pub(crate) struct CliLogDescribeContext<'a> {
    pub git_dir: &'a Path,
    pub db: &'a FileObjectDatabase,
    pub format: ObjectFormat,
}

struct CliLogDescribeAdapter<'a>(&'a CliLogDescribeContext<'a>);
impl sley_pretty::LogDescribeLookup for CliLogDescribeAdapter<'_> {
    fn describe_oid(&self, oid: &ObjectId, spec: &sley_pretty::DescribeSpec) -> Result<String> {
        Ok(commands::describe::describe_for_format(
            self.0.git_dir,
            self.0.format,
            self.0.db,
            oid,
            spec.tags,
            spec.abbrev,
            &spec.matches,
            &spec.excludes,
        )?
        .unwrap_or_default())
    }
}

fn source_tag_signatures_for_revision_tips(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    tips: &[sley_rev::RevisionTip],
) -> Result<HashMap<ObjectId, commands::signing::GpgVerification>> {
    let mut signatures = HashMap::new();
    for tip in tips {
        let object = db.read_object(&tip.oid)?;
        if object.object_type != ObjectType::Tag {
            continue;
        }
        let commit = match sley_rev::peel_to_commit(db, format, &tip.oid) {
            Ok(c) => c,
            Err(e) if tip.from_ref_selector => {
                let _ = e;
                continue;
            }
            Err(e) => return Err(e),
        };
        let Some((payload, signature)) = commands::signing::tag_signature_payload(&object.body)
        else {
            continue;
        };
        signatures.insert(
            commit,
            commands::signing::verify_payload(git_dir, Some(config), payload, signature)?,
        );
    }
    Ok(signatures)
}

fn print_log_format(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: LogFormatContext<'_>,
) -> Result<usize> {
    let mut line = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(
        record,
        compiled,
        &context,
        &mut line,
        0..compiled.tokens.len(),
    )?;
    let out = log_reencode_message(&line, "UTF-8", context.output_encoding);
    let emitted = out.len();
    io::stdout().write_all(&out)?;
    io::stdout().flush()?;
    Ok(emitted)
}

pub(crate) fn print_stash_compiled_format(
    entry: &ReflogEntry,
    index: usize,
    commit: &Commit,
    compiled: &CompiledLogFormat,
    abbrev_len: Option<usize>,
    date_mode: &DateMode,
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
fn rev_parse_symbolic_full_name(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<Option<String>> {
    sley_rev::resolve_revision_symbolic_full_name(git_dir, format, rev)
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
    status_quote_path_full(path, quote_space, true)
}

/// Like [`status_quote_path`] but parameterized by git's `quote_path_fully`
/// (`core.quotePath`): when `quote_path_fully` is false, bytes `>= 0x80` are
/// emitted verbatim instead of octal-escaped, so a UTF-8 path with no other
/// quote-forcing byte comes through raw (matching `quote_c_style` with
/// `core.quotePath=false`). Control bytes, `0x7f`, `"` and `\` are still quoted.
pub(crate) fn status_quote_path_full(
    path: &[u8],
    quote_space: bool,
    quote_path_fully: bool,
) -> String {
    if !status_path_needs_quotes_full(path, quote_space, quote_path_fully) {
        return String::from_utf8_lossy(path).into_owned();
    }
    let mut out: Vec<u8> = vec![b'"'];
    for &byte in path {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x20..=0x7e => out.push(byte),
            0x80..=0xff if !quote_path_fully => out.push(byte),
            _ => out.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    out.push(b'"');
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn write_status_quoted_path(
    writer: &mut impl Write,
    path: &[u8],
    quote_space: bool,
) -> Result<()> {
    if !status_path_needs_quotes(path, quote_space) {
        writer.write_all(path)?;
        return Ok(());
    }
    writer.write_all(b"\"")?;
    for &byte in path {
        match byte {
            b'"' => writer.write_all(br#"\""#)?,
            b'\\' => writer.write_all(br#"\\"#)?,
            b'\n' => writer.write_all(br#"\n"#)?,
            b'\t' => writer.write_all(br#"\t"#)?,
            0x20..=0x7e => writer.write_all(&[byte])?,
            _ => write!(writer, "\\{byte:03o}")?,
        }
    }
    writer.write_all(b"\"")?;
    Ok(())
}

fn status_path_needs_quotes(path: &[u8], quote_space: bool) -> bool {
    status_path_needs_quotes_full(path, quote_space, true)
}

fn status_path_needs_quotes_full(path: &[u8], quote_space: bool, quote_path_fully: bool) -> bool {
    path.iter().any(|&byte| {
        byte == b'"'
            || byte == b'\\'
            || byte == b'\n'
            || byte == b'\t'
            || byte < 0x20
            || byte == 0x7f
            || (quote_path_fully && byte >= 0x80)
            || (quote_space && byte == b' ')
    })
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
        return parse_repository_abbrev_value(git_dir, format, &value);
    }
    let config_path = git_dir.join("config");
    let Ok(config) = GitConfig::read(config_path) else {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
    };
    let Some(value) = config.get("core", None, "abbrev") else {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
    };
    parse_repository_abbrev_value(git_dir, format, value)
}

fn repository_abbrev_from_config(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Option<usize>> {
    if let Some(value) = global_config_value("core.abbrev")? {
        return parse_repository_abbrev_value(git_dir, format, &value);
    }
    let Some(value) = config.get("core", None, "abbrev") else {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
    };
    parse_repository_abbrev_value(git_dir, format, value)
}

fn parse_repository_abbrev_value(
    git_dir: &Path,
    format: ObjectFormat,
    value: &str,
) -> Result<Option<usize>> {
    if value.eq_ignore_ascii_case("no") {
        return Ok(None);
    }
    if value.eq_ignore_ascii_case("auto") {
        return Ok(Some(repository_auto_abbrev_width(git_dir, format)?));
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

fn repository_auto_abbrev_width(git_dir: &Path, format: ObjectFormat) -> Result<usize> {
    let object_count = repository_approx_object_count(git_dir, format)?;
    if object_count == 0 {
        return Ok(7.min(format.hex_len()));
    }
    let bits = u64::BITS as usize - object_count.saturating_sub(1).leading_zeros() as usize;
    Ok(((bits + 1) / 2).max(7).min(format.hex_len()))
}

fn repository_approx_object_count(git_dir: &Path, format: ObjectFormat) -> Result<u64> {
    let pack_dir = repository_objects_dir(git_dir).join("pack");
    if let Some(packed_count) =
        multi_pack_index_object_count(&pack_dir.join("multi-pack-index"), format)?
    {
        return Ok(packed_count);
    }
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(0);
    };
    let mut count = 0u64;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new("idx")) {
            continue;
        }
        count = count.saturating_add(u64::from(pack_index_object_count(&path)?));
    }
    Ok(count)
}

fn multi_pack_index_object_count(path: &Path, format: ObjectFormat) -> Result<Option<u64>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut header = [0u8; 12];
    file.read_exact(&mut header).map_err(|_| {
        GitError::InvalidFormat(format!("multi-pack-index {} is too short", path.display()))
    })?;
    if &header[..4] != b"MIDX" {
        return Err(GitError::InvalidFormat(format!(
            "missing multi-pack-index signature in {}",
            path.display()
        )));
    }
    let version = header[4];
    if version != 1 && version != 2 {
        return Err(GitError::Unsupported(format!(
            "multi-pack-index version {version}"
        )));
    }
    let expected_hash_id = match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    };
    let hash_id = header[5];
    if u32::from(hash_id) != expected_hash_id {
        return Err(GitError::InvalidFormat(format!(
            "multi-pack-index hash id {hash_id} does not match {}",
            format.name()
        )));
    }
    let chunk_count = header[6] as usize;
    let base_midx_count = header[7];
    if base_midx_count != 0 {
        return Err(GitError::Unsupported(format!(
            "multi-pack-index base count {base_midx_count}"
        )));
    }

    let mut lookup = vec![0u8; (chunk_count + 1).saturating_mul(12)];
    file.read_exact(&mut lookup).map_err(|_| {
        GitError::InvalidFormat(format!(
            "truncated multi-pack-index chunk lookup in {}",
            path.display()
        ))
    })?;
    let mut oid_fanout_offset = None;
    for chunk in lookup.chunks_exact(12).take(chunk_count) {
        if &chunk[..4] == b"OIDF" {
            oid_fanout_offset = Some(u64::from_be_bytes([
                chunk[4], chunk[5], chunk[6], chunk[7], chunk[8], chunk[9], chunk[10], chunk[11],
            ]));
            break;
        }
    }
    let Some(oid_fanout_offset) = oid_fanout_offset else {
        return Err(GitError::InvalidFormat(format!(
            "multi-pack-index {} missing OIDF chunk",
            path.display()
        )));
    };
    file.seek(SeekFrom::Start(oid_fanout_offset + 255 * 4))?;
    let mut count = [0u8; 4];
    file.read_exact(&mut count).map_err(|_| {
        GitError::InvalidFormat(format!(
            "truncated multi-pack-index OIDF chunk in {}",
            path.display()
        ))
    })?;
    Ok(Some(u64::from(u32::from_be_bytes(count))))
}

fn pack_index_object_count(path: &Path) -> Result<u32> {
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; 8 + 256 * 4];
    file.read_exact(&mut header[..8]).map_err(|_| {
        GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
    })?;
    let fanout_offset = if header[..8].starts_with(&[0xff, b't', b'O', b'c']) {
        file.read_exact(&mut header[8..]).map_err(|_| {
            GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
        })?;
        8
    } else {
        file.read_exact(&mut header[8..256 * 4]).map_err(|_| {
            GitError::InvalidFormat(format!("pack index {} is too short", path.display()))
        })?;
        0
    };
    let offset = fanout_offset + 255 * 4;
    Ok(u32::from_be_bytes([
        header[offset],
        header[offset + 1],
        header[offset + 2],
        header[offset + 3],
    ]))
}

fn commit_identity_from_env(role: &str) -> Result<Vec<u8>> {
    // git's identity precedence for the name/email of an author or committer:
    //   GIT_{role}_NAME/EMAIL env var
    //     -> `-c {author,committer}.name=` / GIT_CONFIG_* command-line overrides
    //       -> effective config {author,committer}.name/email
    //         -> effective config user.name/email
    //           -> sley's built-in default identity
    // Higher-precedence env/`-c`/repo sources are evaluated exactly as before;
    // the global+system config layer is the new fallback below repo config.
    // The effective config is loaded at most once, and only when the env vars do
    // not already supply both fields, so the common env-driven path is unchanged.
    let env_name = env::var_os(format!("GIT_{role}_NAME")).map(argv_bytes_from_os);
    let env_email = env::var_os(format!("GIT_{role}_EMAIL")).map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role(role, "name", &mut config).map(String::into_bytes)
        })
        .or_else(|| identity_default_value("Git Rs", &mut config).map(String::into_bytes));
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role(role, "email", &mut config).map(String::into_bytes)
        })
        .or_else(|| {
            identity_default_value("sley@example.invalid", &mut config).map(String::into_bytes)
        });
    let (Some(name), Some(email)) = (name, email) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name(role, &name, &email)?;
    let date = env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
}

/// Like [`commit_identity_from_env`] but with the date forced to `date_override`
/// (any form [`canonicalize_commit_date`] accepts), keeping the env/config
/// name+email resolution unchanged. Used by `git am
/// --committer-date-is-author-date`, which keeps the environment committer
/// name/email but substitutes the author date.
fn commit_identity_from_env_with_date(role: &str, date_override: &str) -> Result<Vec<u8>> {
    let env_name = env::var_os(format!("GIT_{role}_NAME")).map(argv_bytes_from_os);
    let env_email = env::var_os(format!("GIT_{role}_EMAIL")).map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role(role, "name", &mut config).map(String::into_bytes)
        })
        .or_else(|| identity_default_value("Git Rs", &mut config).map(String::into_bytes));
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role(role, "email", &mut config).map(String::into_bytes)
        })
        .or_else(|| {
            identity_default_value("sley@example.invalid", &mut config).map(String::into_bytes)
        });
    let (Some(name), Some(email)) = (name, email) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name(role, &name, &email)?;
    let date = canonicalize_commit_date(date_override);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
}

fn committer_identity_for_reflog() -> Result<Vec<u8>> {
    let env_name = env::var_os("GIT_COMMITTER_NAME").map(argv_bytes_from_os);
    let env_email = env::var_os("GIT_COMMITTER_EMAIL").map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "name", &mut config).map(String::into_bytes)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| b"Git Rs".to_vec());
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "email", &mut config)
                .map(String::into_bytes)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| b"sley@example.invalid".to_vec());
    let date = env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
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
    if date.is_empty() {
        return default_commit_date();
    }
    match commands::approxidate::parse_commit_date(date) {
        Some((seconds, tz)) => format!("{seconds} {tz}"),
        None => date.to_string(),
    }
}

fn default_commit_date() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    format!("{seconds} +0000")
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

fn identity_config_value_for_role(
    role: &str,
    field: &str,
    config: &mut IdentityConfig,
) -> Option<String> {
    let role_key = match role {
        "AUTHOR" => Some(format!("author.{field}")),
        "COMMITTER" => Some(format!("committer.{field}")),
        _ => None,
    };
    role_key
        .as_deref()
        .and_then(|key| identity_config_value(key, config))
        .or_else(|| identity_config_value(&format!("user.{field}"), config))
}

fn identity_default_value(value: &str, config: &mut IdentityConfig) -> Option<String> {
    if identity_use_config_only(config) {
        None
    } else {
        Some(value.to_string())
    }
}

fn identity_use_config_only(config: &mut IdentityConfig) -> bool {
    identity_config_value("user.useconfigonly", config)
        .as_deref()
        .and_then(sley_config::parse_config_bool)
        .unwrap_or(false)
}

fn identity_use_config_only_error<T>() -> Result<T> {
    eprintln!("fatal: no email was given and auto-detection is disabled");
    Err(GitError::Exit(128))
}

fn validate_commit_identity_name(role: &str, name: &[u8], email: &[u8]) -> Result<()> {
    if name.is_empty() {
        print_identity_unknown_hint(role);
        eprintln!(
            "fatal: empty ident name (for <{}>) not allowed",
            String::from_utf8_lossy(email)
        );
        return Err(GitError::Exit(128));
    }
    if !name.iter().any(|byte| !commit_identity_name_crud(*byte)) {
        eprintln!(
            "fatal: name consists only of disallowed characters: {}",
            String::from_utf8_lossy(name)
        );
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn commit_identity_name_crud(byte: u8) -> bool {
    matches!(
        byte,
        0..=32 | b',' | b':' | b';' | b'<' | b'>' | b'"' | b'\\' | b'\''
    )
}

fn print_identity_unknown_hint(role: &str) {
    match role {
        "AUTHOR" => eprintln!("Author identity unknown"),
        "COMMITTER" => eprintln!("Committer identity unknown"),
        _ => {}
    }
}

/// Load the effective config (repository + global + system, with includes) for
/// identity fallback, or `None` when there is no repository in scope. Failures
/// degrade to `None` so identity resolution can still fall through to env/`-c`
/// values or the built-in default rather than aborting.
fn identity_effective_config() -> Option<GitConfig> {
    // `cli_git_dir` already honours `--git-dir`/`GIT_DIR` (via
    // `explicit_git_dir`) before walking up from the current directory.
    let git_dir = session::cli_git_dir().ok()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir).ok()?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.clone()),
        repo_current_branch_name(&git_dir),
    );
    let mut config = sley_config::load_effective_config(&common_git_dir, &context).ok()?;
    // Layer the command-line `-c`/`--config-env` overrides on top, so reads like
    // `mailmap.blob`/`mailmap.file` see the same values `git config` would (the
    // CLI cannot push `-c` into the process env, so reconstruct it here).
    let parameters_env = effective_config_parameters_env();
    if let Ok(parameters) = sley_config::injected_config_parameters(parameters_env.as_deref()) {
        let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            &base,
        );
    }
    Some(config)
}

fn commit_signoff_from_env() -> Result<Vec<u8>> {
    // git's `--signoff` uses the committer identity, so resolve it with the same
    // precedence as `commit_identity_from_env("COMMITTER")`.
    let env_name = env::var_os("GIT_COMMITTER_NAME").map(argv_bytes_from_os);
    let env_email = env::var_os("GIT_COMMITTER_EMAIL").map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "name", &mut config).map(String::into_bytes)
        })
        .or_else(|| identity_default_value("Git Rs", &mut config).map(String::into_bytes));
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "email", &mut config)
                .map(String::into_bytes)
        })
        .or_else(|| {
            identity_default_value("sley@example.invalid", &mut config).map(String::into_bytes)
        });
    let (Some(name), Some(email)) = (name, email) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name("COMMITTER", &name, &email)?;
    let date = env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)?;
    let mut out = b"Signed-off-by: ".to_vec();
    out.extend_from_slice(&name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&email);
    out.push(b'>');
    Ok(out)
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
    if let Some(setup) = setup::setup_git_directory()
        && let Some(worktree) = setup.worktree.as_ref()
        && (explicit_git_dir().is_some()
            || explicit_work_tree().is_some()
            || setup_matches_git_dir(&setup, git_dir))
    {
        return Ok(worktree.clone());
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

fn setup_matches_git_dir(setup: &setup::SetupResult, git_dir: &Path) -> bool {
    let setup_git_dir = Path::new(&setup.git_dir);
    let setup_git_dir = if setup_git_dir.is_absolute() {
        setup_git_dir.to_path_buf()
    } else {
        setup.cwd.join(setup_git_dir)
    };
    paths_refer_to_same_dir(&setup_git_dir, git_dir)
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

fn resolve_revision(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    sley_rev::resolve_revision(git_dir, format, rev)
}

fn resolve_revision_commitish(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    if is_short_hex_object_prefix(format, rev) {
        return sley_rev::resolve_short_object_id(
            git_dir,
            format,
            rev,
            sley_rev::ObjectDisambiguation::Commitish,
        )?
        .into_result(rev);
    }
    sley_rev::resolve_revision(git_dir, format, rev)
}

fn resolve_revision_treeish(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    if is_short_hex_object_prefix(format, rev) {
        return sley_rev::resolve_short_object_id(
            git_dir,
            format,
            rev,
            sley_rev::ObjectDisambiguation::Treeish,
        )?
        .into_result(rev);
    }
    sley_rev::resolve_revision(git_dir, format, rev)
}

fn is_short_hex_object_prefix(format: ObjectFormat, rev: &str) -> bool {
    rev.len() >= 4
        && rev.len() < format.hex_len()
        && rev.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn warn_ambiguous_refname_for_object_prefix(git_dir: &Path, format: ObjectFormat, rev: &str) {
    if rev.len() < 4
        || rev.len() > format.hex_len()
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
    use super::{count_line_diff, refname_pattern_matches};

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

    #[test]
    fn diff_stat_line_count_fast_paths_are_exact() {
        let mut many_new = String::new();
        for idx in 0..1024 {
            many_new.push_str(&format!("new line {idx}\n"));
        }
        assert_eq!(
            count_line_diff(b"old line\n", many_new.as_bytes()),
            (1024, 1)
        );

        let mut old = String::from("shared prefix\n");
        let mut new = String::from("shared prefix\n");
        for idx in 0..1024 {
            old.push_str(&format!("old middle {idx}\n"));
            new.push_str(&format!("new middle {idx}\n"));
        }
        old.push_str("shared suffix\n");
        new.push_str("shared suffix\n");
        assert_eq!(
            count_line_diff(old.as_bytes(), new.as_bytes()),
            (1024, 1024)
        );
    }
}
