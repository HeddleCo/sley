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
mod for_each_ref_helpers;
mod global_options;
mod log_cli;
mod ownership;
mod repo_helpers;
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

pub(crate) use for_each_ref_helpers::*;
pub(crate) use log_cli::{
    CliLogDescribeAdapter, CliLogDescribeContext, CliLogSignatureAdapter, CliLogSignatureContext,
    CliMailmapAdapter, DecorationFilter, LogDecorationMode, LogFilterPattern, SimpleLogRegex,
    SimpleLogRegexMode, commit_author_identity, commit_identity_mailmapped,
    compile_log_message_grep_matcher, log_author_filters_match, log_author_requires_value_error,
    log_committer_filters_match, log_committer_requires_value_error, log_date_mode,
    log_date_requires_value_error, log_days_from_civil, log_decoration_map, log_grep_filters_match,
    log_grep_pattern_kind_from_config, log_grep_requires_value_error, log_option_requires_value_error,
    log_option_takes_no_value_error, log_parse_age, log_parse_date_cutoff, log_parse_date_ymd,
    log_parse_diff_algorithm, log_parse_time_hms, log_parse_timezone_offset_seconds,
    log_pickaxe_all_objfind_conflict_error, log_pickaxe_empty_error, log_pickaxe_g_regex_conflict_error,
    log_pickaxe_kinds_conflict_error, log_pickaxe_requires_value_error, parse_log_filter_patterns,
    parse_log_filter_patterns_with_diagnostic_verbosity, print_log_decorations, print_log_format,
    print_stash_compiled_format, source_tag_signatures_for_revision_tips,
};
pub(crate) use repo_helpers::{
    repository_abbrev, repository_abbrev_from_config, repository_object_format,
    worktree_prefix, worktree_root_for_git_dir,
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
