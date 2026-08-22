#![allow(
    // Git-compatible command handlers deliberately keep their option/state
    // inputs visible; bundling them would obscure the upstream call shape.
    clippy::too_many_arguments,
    clippy::type_complexity,
    // Complete literals retain `..Default::default()` so additions to shared
    // option structs do not silently become mandatory in every CLI renderer.
    clippy::needless_update,
    // Generic I/O/iterator adapters keep explicit conversions to make the
    // surrounding Result error type clear even when inference makes them no-ops.
    clippy::useless_conversion,
    // Large command modules keep focused unit tests next to the private helper
    // they exercise instead of forcing all tests to the end of the file.
    clippy::items_after_test_module
)]

use sley::plumbing::sley_config::{ConfigBoolOrInt, ConfigEntry, ConfigSection};
use sley::plumbing::sley_core::DateMode;
use sley::plumbing::sley_formats::{
    Bundle, BundleCapability, BundlePrerequisite, BundleReference, CommitGraph,
    CommitGraphWriteEntry, InitOptions, RefStorageFormat, RepositoryBootstrap,
};
use sley::plumbing::sley_object::{
    Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntries, TreeEntry, tree_entry_object_type,
};
use sley::plumbing::sley_odb::{
    FileObjectDatabase, LooseObjectIntegrity, ObjectPrefixResolution, ObjectReader, ObjectWriter,
    build_reachable_pack, collect_reachable_object_ids, grafted_parents, install_bundle_pack,
    install_reachable_pack, prune_unreachable_loose, repository_object_ids, repository_objects_dir,
};
use sley::plumbing::sley_pack::{MultiPackIndex, MultiPackIndexEntry, PackFile, PackIndex};
use sley::plumbing::sley_refs::{
    FileRefStore, PackRefDecision, Ref, RefTransactionHookUpdate, RefTransactionPhase, RefUpdate,
    ReferenceTransactionHook, ReflogEntry, branch_ref_name, check_refname_format,
    parse_packed_refs, resolve_ref_peeled, tag_ref_name,
    validate_ref_name, validate_symref_name, validate_symref_target,
};
use sley::plumbing::sley_remote::FetchOutcome;
use sley::{
    BString, GitConfig, GitError, Index, IndexEntry, ObjectFormat, ObjectId, RefPrecondition,
    ReferenceTarget as RefTarget, Result,
};
use sley_protocol::{
    FetchHeadRecord, ProtocolVersion, ReceivePackCommand, RefAdvertisement, RefAdvertisementSet,
    UploadPackFeatures, parse_refspec, read_fetch_head, read_receive_pack_push_options,
    read_receive_pack_request, read_ref_advertisement_set, read_upload_pack_negotiation_request,
    read_upload_pack_request, refspec_map_source, write_ref_advertisement_set,
};
use sley_transport::{RemoteTransport, RemoteUrl, parse_remote_url};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;

mod checkout_reset;
mod cli_misc;
mod command_synopsis;
mod commands;
mod commit_identity;
mod commit_message;
mod diff_render;
mod discovery;
mod dispatch;
mod for_each_ref_helpers;
mod global_options;
mod init_config;
mod interrupt_cancel;
mod log_cli;
mod ls_files_pathspec;
mod ownership;
mod reflog_parse;
mod remote;
mod repo_helpers;
mod repo_path;
mod repo_paths;
mod repository;
mod revision;
mod scalar;
mod session;
mod session_globals;
mod setup;
mod status_format;
mod trace2_cli;
mod tree_print;

pub(crate) use sley::plumbing::sley_rev::revlist::*;
pub(crate) use sley::plumbing::{
    sley_config, sley_core, sley_diff_merge, sley_index, sley_object, sley_odb, sley_pack,
    sley_pretty, sley_refs, sley_remote, sley_rev, sley_worktree,
};
pub(crate) use sley_options::validators::*;
pub(crate) use sley_ref_filter::*;

pub use global_options::argv_string_from_os;
pub(crate) use global_options::{
    DEFAULT_BIG_FILE_THRESHOLD, GlobalConfigOverride, PathspecFlags, apply_global_options,
    argv_bytes_from_os, argv_bytes_from_string, argv_string_from_bytes, core_big_file_threshold,
    effective_config_parameters_env, global_config_value, injected_config_parameters,
};
pub(crate) use repo_paths::common_git_dir_for_git_dir;
pub use scalar::run_scalar;
pub(crate) use trace2_cli::{
    trace_reference_fsync_counter, trace2_emit_def_params_at_depth, trace2_emit_def_params_once,
    trace2_emit_process_ancestry_at_depth,
};

pub(crate) use diff_render::{
    DiffEntryRawRenderOptions, DiffEntryRenderContext, DiffEntryRenderModes,
    DiffEntryStatRenderOptions, DiffEntryStatSource, DiffLineStats, DiffPathspec,
    DiffRenderOptions, DiffStatEntryData, DiffStatOptions, DiffWorktreeCleanContext,
    WordDiffRequest, apply_diff_max_depth, apply_diff_order_file, apply_diff_pathspec,
    apply_submodule_ignore_filter, collect_diff_stat_entries,
    collect_diff_stat_entries_with_worktree_clean, collect_dirty_submodules,
    compile_ignore_matching_regexes, diff_entry_new_content, diff_entry_old_content,
    diff_entry_produces_output, diff_line_stats, diff_rename_limit_requires_integer_error,
    diff_stat_decimal_width, diff_stat_pprint_rename, diff_stat_totals, gitlink_diff_content,
    is_binary_content, is_gitlink_pair, parse_diff_max_depth, parse_dirstat_params,
    prefetch_diff_entry_blobs, prefetch_promisor_objects, prefetch_via_configured_upload_pack,
    promisor_remote_names, read_blob, read_object_maybe_prefetch_promisor, render_diff_entries,
    render_tree_to_tree_patch, repo_path_to_path, reverse_diff_entries, reverse_diff_entry,
    submodule_diff_config_with_config, submodule_git_dir_for_path, validate_diff_rename_limit,
    write_diff_dirstat, write_diff_numstat_materialized_entry, write_diff_patch_entry,
    write_diff_raw_entry, write_diff_shortstat_materialized, write_diff_stat_materialized,
    write_diff_stat_materialized_with_widths, write_diff_stat_summary_line,
    write_diff_summary_entry,
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
    log_date_requires_value_error, log_decoration_map, log_grep_filters_match,
    log_grep_pattern_kind_from_config, log_grep_requires_value_error,
    log_option_requires_value_error, log_option_takes_no_value_error, log_parse_age,
    log_parse_date_cutoff, log_parse_diff_algorithm, log_pickaxe_all_objfind_conflict_error,
    log_pickaxe_empty_error, log_pickaxe_g_regex_conflict_error, log_pickaxe_kinds_conflict_error,
    log_pickaxe_requires_value_error, parse_log_filter_patterns,
    parse_log_filter_patterns_with_diagnostic_verbosity, print_log_decorations, print_log_format,
    print_stash_compiled_format, source_tag_signatures_for_revision_tips,
};
pub(crate) use repo_helpers::{
    repository_abbrev, repository_abbrev_from_config, repository_object_format, worktree_prefix,
    worktree_root_for_git_dir,
};

pub(crate) use sley::plumbing::sley_pretty::{
    CompiledLogFormat, FormatToken, LogDescribeLookup, LogFormatContext, LogFormatDialect,
    MailmapLookup, StashFormatContext, append_log_oid, commit_author_for_commit_encoding,
    commit_body, commit_encoding, commit_encoding_config, commit_encoding_header_from_config,
    commit_identity_name_email, commit_message_for_commit_encoding, commit_message_for_output,
    commit_message_has_invalid_utf8, commit_message_has_nul, commit_message_lines,
    commit_object_message_and_optional_encoding, commit_subject, commit_subject_bytes,
    emit_compiled_log_format, emit_compiled_log_format_limited_commit,
    emit_compiled_log_format_metadata, emit_compiled_stash_format, emit_log_one_token,
    encoding_for_name, encoding_is_none, encoding_is_utf8, format_log_abbrev_oid,
    format_log_commit_header_oid, format_log_oid, git_color_name_to_ansi, git_color_spec_to_ansi,
    log_output_encoding, log_reencode_message, log_rewrap, presets, try_git_color_spec_to_ansi,
};
pub(crate) use sley::plumbing::sley_rev::diff_options::{
    DiffFilter, DiffStatWidths, DirstatMode, DirstatOptions, SubmoduleIgnoreMode,
    diff_stat_count_option, diff_stat_parse_width_option, parse_diff_filter,
    parse_similarity_threshold, parse_submodule_ignore_mode,
};

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
    read_repo_config, read_repo_config_on_disk, remote_exists, remote_names,
    repo_current_branch_name, write_repo_config,
};
pub(crate) use commands::status::cmd_status;
use commands::tag::tag_stripspace_message;
pub(crate) use repo_path::RepoPathBuf;
pub(crate) use repository::RepositoryContext;

pub(crate) use checkout_reset::*;
pub(crate) use commit_identity::*;
pub(crate) use commit_message::*;
pub(crate) use revision::*;
pub(crate) use session_globals::*;
pub(crate) use status_format::*;
pub(crate) use tree_print::*;

pub(crate) use cli_misc::{
    AddAction, add_path_matches, check_ignore_tracked_paths, count_objects_human_bytes,
    current_unix_seconds, delete_symbolic_ref, pack_refs_peeled_oid, parse_abbrev,
    read_pathspecs_from_file, read_repository_index, resolve_add_update_actions,
    resolve_ref_to_oid, set_config_value, show_ref_filter_matches,
    submodule_worktree_has_untracked_entries, write_check_attr_state,
};
pub(crate) use init_config::{
    DEFAULT_BRANCH_NAME_ADVICE, clone_init_default_branch_config,
    clone_init_default_submodule_path_config, enable_submodule_path_config_extension,
    init_config_value, parse_bad_config_line_with_path, parse_bad_config_line_without_path,
    parse_config_bool, report_config_setup_error, submodule_path_config_enabled,
};
pub(crate) use ls_files_pathspec::{
    LsFilesPathspec, index_entry_stage, normalize_absolute_cli_pathspec, normalize_lexical_path,
    path_component_count, relative_path_from_absolute, relative_path_from_absolute_components,
};
pub(crate) use reflog_parse::{
    parse_reflog_count, parse_reflog_expire_date, parse_reflog_expire_time,
    parse_reflog_max_parent_count, parse_reflog_min_parent_count, parse_reflog_skip_count,
    reflog_reference_name,
};
pub(crate) use sley::plumbing::sley_refs::refname_pattern_matches_case;

pub(crate) fn refname_pattern_matches(pattern: &str, name: &str) -> bool {
    refname_pattern_matches_case(pattern, name, false)
}

pub(crate) fn short_oid(hex: &str) -> &str {
    &hex[..hex.len().min(7)]
}

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

pub fn run(args: Vec<String>) -> Result<()> {
    sley_core::set_original_cwd(env::current_dir().ok());
    let global = apply_global_options(&args)?;
    // `--namespace` overrides `GIT_NAMESPACE` for this process (git uses setenv;
    // the workspace forbids `env::set_var`, so a process-local override is used).
    if let Some(namespace) = global.namespace.clone() {
        sley_core::set_git_namespace_override(Some(namespace));
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cli_session = session::CliSession::from_parsed_globals(
        cwd,
        global.git_dir.clone(),
        global.work_tree.clone(),
        global.attr_source.clone(),
        global.bare,
        global.replace_objects,
        global.lazy_fetch,
        global.pathspec_flags,
    );
    sley_core::trace2::touch();
    sley_core::trace2::start(global.args);
    trace2_emit_process_ancestry_at_depth(sley_core::trace2::depth(), &[]);
    trace2_emit_def_params_once(&cli_session);
    // `-c` / `--config-env` overrides are folded into the process
    // `GIT_CONFIG_PARAMETERS` env var during option parsing, so the single
    // `injected_config_parameters()` reader is the source of truth for every
    // config read; no separate global-override store is needed.
    // Emit git's GIT_TRACE_SETUP output (the env/config/gitfile discovery trace)
    // before dispatching. This is the CLI-side repository setup that
    // `sley::Repository::discover` deliberately leaves to this layer.
    if env::var_os("GIT_TRACE_SETUP").is_some()
        && let Some(setup_result) = setup::setup_git_directory(&cli_session)
    {
        setup::trace_repo_setup(&setup_result);
    }
    // git's `precompose_argv_prefix`: once the repo config is known, convert
    // NFD path arguments to NFC when `core.precomposeunicode` is true. Command
    // name (argv[0]) is left alone; options and pathspecs are normalized.
    let mut dispatch_args: Vec<String> = global.args.to_vec();
    match cli_session.repository_snapshot() {
        Ok(snapshot) => {
            sley_core::activate_precompose_unicode(snapshot.config.get_bool(
                "core",
                None,
                "precomposeunicode",
            ));
            if dispatch_args.len() > 1 {
                sley_core::precompose_argv_if_needed(&mut dispatch_args[1..]);
            }
        }
        // Commands such as `config --global` remain valid without a repository;
        // their command-specific config reader owns any later diagnostic.
        Err(GitError::NotFound(_)) => {}
        // Repository bootstrap is the first authoritative parse of invocation
        // config. Stop here after a malformed `-c` (whose parser has already
        // emitted Git's diagnostic) instead of dispatching and parsing it again.
        Err(err) => return Err(report_config_setup_error(err)),
    }
    dispatch::dispatch_with_aliases(&cli_session, &dispatch_args, &global.config, 0)
}

#[cfg(test)]
mod tests {
    use crate::diff_render::count_line_diff;

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
