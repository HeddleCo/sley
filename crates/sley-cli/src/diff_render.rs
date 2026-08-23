//! CLI glue for diff output rendering.
//!
//! The porcelain tiers live in `sley-diff-merge::porcelain`; the promisor
//! lazy-fetch boundary lives in `sley-remote`; submodule log/range helpers
//! live in `sley-submodule::history`. What remains here cannot sink without a
//! dependency cycle or process-spawning CLI plumbing: userdiff/textconv, the
//! worktree-bound submodule inline-diff renderer, and thin prelude
//! delegations so command modules compile unchanged.

use crate::{
    BString, DEFAULT_BIG_FILE_THRESHOLD, GitConfig, GitError, ObjectFormat, ObjectId, Result,
    commit_encoding, commit_subject, core_big_file_threshold, log_reencode_message,
    normalize_absolute_cli_pathspec, read_repo_config, repository_object_format,
    sley_diff_merge, sley_pretty, sley_remote, sley_rev, sley_worktree,
};
use sley::plumbing::sley_object::{Commit, EncodedObject};
use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectReader};
use sley_diff_merge::porcelain::{LazyObjectFetch, PatchDriver, PatchUserdiff, SubmodulePatchRender};
pub(crate) use sley_diff_merge::porcelain::DiffRenderOptions;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) use sley_diff_merge::porcelain::{
    LineStats as DiffLineStats, StatEntry as DiffStatEntryData, StatOptions as DiffStatOptions,
    DiffEntryRawRenderOptions, DiffEntryRenderContext, DiffEntryRenderModes,
    DiffEntryStatRenderOptions, DiffEntryStatSource, DiffPathspec, WordDiffRequest,
    apply_diff_max_depth, apply_diff_order_file, apply_diff_pathspec,
    apply_submodule_ignore_filter, collect_diff_stat_entries,
    collect_diff_stat_entries_with_worktree_clean, compile_ignore_matching_regexes,
    diff_entry_new_content, diff_entry_old_content, diff_entry_produces_output, diff_line_stats,
    diff_rename_limit_requires_integer_error, diff_stat_decimal_width, diff_stat_totals,
    gitlink_diff_content, is_binary_content, is_gitlink_pair, parse_diff_max_depth, read_blob,
    parse_dirstat_params, render_diff_entries, repo_path_to_path, reverse_diff_entries,
    reverse_diff_entry, validate_diff_rename_limit, write_diff_dirstat, DiffWorktreeCleanContext,
    write_diff_numstat_materialized_entry, write_diff_patch_entry, write_diff_raw_entry,
    write_diff_shortstat_materialized, write_diff_stat_summary_line, write_diff_summary_entry,
};

struct CliDiffRenderServices;

/// Host display/terminal services for the porcelain stat writers.
pub(crate) fn cli_render_services() -> &'static dyn sley_diff_merge::porcelain::RenderServices {
    &CliDiffRenderServices
}

impl sley_diff_merge::porcelain::RenderServices for CliDiffRenderServices {
    fn display_width(&self, rendered: &str) -> i64 {
        sley_strbuf_expand::strwidth(rendered.as_bytes()) as i64
    }

    fn terminal_columns(&self) -> i64 {
        sley_pretty::term_columns()
    }
}

// Lazy-fetch (promisor) seam

fn load_repo_config_for_promisor(git_dir: &Path) -> Option<GitConfig> {
    read_repo_config(git_dir).ok()
}

struct CliDiffLazyFetch;

impl LazyObjectFetch for CliDiffLazyFetch {
    fn read_object_maybe_prefetch(
        &self,
        db: &FileObjectDatabase,
        oid: &ObjectId,
    ) -> Result<std::sync::Arc<EncodedObject>> {
        sley_remote::read_object_maybe_prefetch_promisor(db, oid, &load_repo_config_for_promisor)
    }

    fn prefetch_entry_blobs(
        &self,
        db: &FileObjectDatabase,
        entries: &[sley_diff_merge::NameStatusEntry],
        new_side_is_worktree: bool,
    ) -> Result<()> {
        sley_remote::prefetch_diff_entry_blobs(
            db,
            entries,
            new_side_is_worktree,
            &load_repo_config_for_promisor,
        )
    }
}

static CLI_DIFF_LAZY_FETCH: CliDiffLazyFetch = CliDiffLazyFetch;

/// The lazy-fetch hook corresponding to a `lazy_fetch` flag: `None` disables
/// promisor hydration exactly like the former `lazy_fetch == false`.
pub(crate) fn diff_lazy_fetch(enabled: bool) -> Option<&'static dyn LazyObjectFetch> {
    enabled.then_some(&CLI_DIFF_LAZY_FETCH as &'static dyn LazyObjectFetch)
}

pub(crate) fn read_object_maybe_prefetch_promisor(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<std::sync::Arc<EncodedObject>> {
    match diff_lazy_fetch(lazy_fetch) {
        Some(fetch) => fetch.read_object_maybe_prefetch(db, oid),
        None => Ok(db.read_object(oid)?),
    }
}

/// Promisor remotes to consult for a lazy fetch, in Git's
/// `promisor_remote_get_direct` order.
pub(crate) fn promisor_remote_names(config: &GitConfig) -> Vec<String> {
    sley_remote::promisor_remote_names(config)
}

/// Batch-prefetch every missing blob referenced by the queued diff entries.
pub(crate) fn prefetch_diff_entry_blobs(
    db: &FileObjectDatabase,
    entries: &[sley_diff_merge::NameStatusEntry],
    new_side_is_worktree: bool,
    lazy_fetch: bool,
) -> Result<()> {
    if let Some(fetch) = diff_lazy_fetch(lazy_fetch) {
        return fetch.prefetch_entry_blobs(db, entries, new_side_is_worktree);
    }
    Ok(())
}

pub(crate) fn prefetch_promisor_objects(
    db: &FileObjectDatabase,
    oids: &[ObjectId],
    lazy_fetch: bool,
) -> Result<()> {
    if !lazy_fetch {
        return Ok(());
    }
    sley_remote::prefetch_promisor_objects(db, oids, &load_repo_config_for_promisor)
}

pub(crate) fn prefetch_via_configured_upload_pack(command: &str, repository: &str) -> Result<bool> {
    sley_remote::prefetch_via_configured_upload_pack(command, repository)
}

// Stat writer wrappers (host display services injected)

pub(crate) fn write_diff_stat_materialized_with_widths(
    stdout: &mut dyn Write,
    entries: &[DiffStatEntryData<'_>],
    options: DiffStatOptions,
    widths: sley_rev::diff_options::DiffStatWidths,
) -> Result<()> {
    sley_diff_merge::porcelain::write_diff_stat_materialized_with_widths(
        stdout,
        entries,
        options,
        widths,
        &CliDiffRenderServices,
    )
}

pub(crate) fn write_diff_stat_materialized(
    stdout: &mut dyn Write,
    entries: &[DiffStatEntryData<'_>],
    options: DiffStatOptions,
    config: Option<&GitConfig>,
) -> Result<()> {
    sley_diff_merge::porcelain::write_diff_stat_materialized(
        stdout,
        entries,
        options,
        config,
        &CliDiffRenderServices,
    )
}

// Config/worktree seams for the moved filters

/// Resolved `core.bigfilethreshold` for patch binary detection.
pub(crate) fn diff_big_file_threshold(db: &FileObjectDatabase) -> u64 {
    core_big_file_threshold(db.objects_dir().parent()).unwrap_or(DEFAULT_BIG_FILE_THRESHOLD)
}

pub(crate) fn submodule_diff_config_with_config(
    git_dir: &Path,
    worktree_root: Option<&Path>,
    cli: Option<sley_rev::diff_options::SubmoduleIgnoreMode>,
    repo_config: Option<&GitConfig>,
) -> sley_diff_merge::porcelain::SubmoduleDiffConfig {
    sley_diff_merge::porcelain::submodule_diff_config_with_config(
        git_dir,
        worktree_root,
        cli,
        repo_config,
        &load_repo_config_for_promisor,
    )
}

struct CliSubmoduleDirtSource;

impl sley_diff_merge::porcelain::SubmoduleDirtSource for CliSubmoduleDirtSource {
    fn index_gitlinks(
        &self,
        git_dir: &Path,
        format: ObjectFormat,
    ) -> Result<Option<Vec<sley_diff_merge::IndexGitlinkEntry>>> {
        let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
            return Ok(None);
        };
        Ok(Some(
            index
                .entries
                .iter()
                .filter(|entry| entry.mode == 0o160000)
                .map(|entry| sley_diff_merge::IndexGitlinkEntry {
                    path: BString::from_bytes(entry.path.as_bytes()),
                    oid: entry.oid,
                })
                .collect(),
        ))
    }

    fn submodule_dirt(&self, sub_root: &Path) -> u8 {
        sley_worktree::submodule_dirt(sub_root)
    }
}

pub(crate) fn collect_dirty_submodules(
    entries: &mut Vec<sley_diff_merge::NameStatusEntry>,
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    config: &sley_diff_merge::porcelain::SubmoduleDiffConfig,
    precomputed_gitlinks: Option<&[sley_diff_merge::IndexGitlinkEntry]>,
) -> Result<HashMap<Vec<u8>, u8>> {
    sley_diff_merge::porcelain::collect_dirty_submodules(
        entries,
        git_dir,
        format,
        worktree_root,
        config,
        precomputed_gitlinks,
        &CliSubmoduleDirtSource,
    )
}

pub(crate) fn render_tree_to_tree_patch(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    lazy_fetch: bool,
) -> Result<Vec<u8>> {
    sley_diff_merge::porcelain::render_tree_to_tree_patch(
        db,
        format,
        old_tree,
        new_tree,
        diff_lazy_fetch(lazy_fetch),
        diff_big_file_threshold(db),
    )
}

pub(crate) fn cli_submodule_render() -> Option<&'static dyn SubmodulePatchRender> {
    Some(&CLI_SUBMODULE_PATCH_RENDER)
}

// Pathspec construction (CLI pathspec-normalization front end)

/// Build a [`DiffPathspec`] from CLI path arguments, normalizing absolute
/// paths and subdirectory prefixes the way the ls-files front end does.
pub(crate) fn diff_pathspec_new(
    cwd: &Path,
    worktree_root: &Path,
    path_args: &[String],
    magic: sley_worktree::PathspecMatchMagic,
) -> Result<DiffPathspec> {
    use std::cell::Cell;

    let root = fs::canonicalize(worktree_root)?;
    let cwd = fs::canonicalize(cwd)?;
    let relative = cwd.strip_prefix(&root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", cwd.display()))
    })?;
    let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
    let mut filters = Vec::new();
    for arg in path_args {
        let parse_arg = normalize_absolute_cli_pathspec(&root, &cwd, arg)?;
        let element =
            sley_pathspec::parse_normalized_pathspec_element(&prefix, &parse_arg, magic)?;
        let arg_path = Path::new(arg);
        let absolute = if arg_path.is_absolute() {
            arg_path.to_path_buf()
        } else {
            cwd.join(arg_path)
        };
        let recursive = arg == "." || arg.ends_with('/') || absolute.is_dir();
        filters.push(sley_pathspec::LsFilesPathFilter {
            original: arg.clone(),
            recursive,
            is_glob: !element.magic().literal && sley_pathspec::pathspec_is_glob(element.pattern()),
            element,
            matched: Cell::new(false),
        });
    }
    Ok(DiffPathspec::from_filters(filters))
}

// Worktree clean-filter context construction

/// A boxed worktree clean-filter implementation tied to the borrows it closes
/// over (config, attribute matcher, object database).
pub(crate) type CliCleanApply<'a> =
    Box<dyn Fn(&[u8], &[u8], Option<&ObjectId>) -> Result<Vec<u8>> + 'a>;

/// Build the engine's clean-filter seam from CLI-side handles.
pub(crate) fn make_clean_apply<'a>(
    db: &'a FileObjectDatabase,
    config: &'a GitConfig,
    attributes: &'a sley_worktree::WorktreeAttributes,
) -> CliCleanApply<'a> {
    Box::new(move |path: &[u8], content: &[u8], index_blob: Option<&ObjectId>| {
        // Honour has_crlf_in_index so text=auto does not strip CRLF when the
        // recorded (old/index) blob already has CRLF — otherwise unstaged
        // diffs show mixed endings (`-a\r` / `+b`) and break apply
        // round-trips (t4124).
        let index_blob = match index_blob {
            Some(oid) => sley_worktree::SafeCrlfIndexBlob::Lookup { odb: db, oid: *oid },
            None => sley_worktree::SafeCrlfIndexBlob::None,
        };
        attributes.apply_clean_filter_respecting_index(config, path, content, index_blob)
    })
}

/// Borrow a boxed clean-filter closure as the engine's context type.
pub(crate) fn clean_context<'a>(apply: &'a CliCleanApply<'a>) -> DiffWorktreeCleanContext<'a> {
    DiffWorktreeCleanContext {
        apply_clean: apply.as_ref(),
    }
}


// Userdiff / textconv seam

impl PatchUserdiff for crate::commands::userdiff::UserdiffResolver {
    fn patch_driver_for_path(&self, path: &[u8]) -> Result<Option<PatchDriver>> {
        Ok(self.driver_for_path(path)?.map(|driver| PatchDriver {
            funcname: driver.funcname.clone(),
            word_regex: driver.word_regex.clone(),
            binary: driver.binary,
            textconv: driver.textconv.clone(),
        }))
    }

    fn patch_config_word_regex(&self) -> Option<Vec<u8>> {
        self.config_word_regex()
    }

    fn patch_run_textconv(&self, command: &str, content: &[u8]) -> Result<Option<Vec<u8>>> {
        crate::commands::userdiff::run_textconv(command, content)
    }
}

// ---------------------------------------------------------------------------
// Submodule inline-diff rendering (stays CLI-local)
//
// The `Log`/`Diff` submodule renderers depend on `sley-rev` (merge bases,
// commit walks) and `sley-worktree` (index reads, dirt probes); both crates
// sit above `sley-submodule` in the dependency graph (worktree depends on
// submodule), so sinking this tier into `sley-submodule` would create cycles.
// It therefore remains at the CLI layer and plugs into the engine's
// `SubmodulePatchRender` seam below.
// ---------------------------------------------------------------------------

static CLI_SUBMODULE_PATCH_RENDER: CliSubmodulePatchRenderer = CliSubmodulePatchRenderer;

struct CliSubmodulePatchRenderer;

impl SubmodulePatchRender for CliSubmodulePatchRenderer {
    fn write_submodule_patch(
        &self,
        out: &mut dyn Write,
        entry: &sley_diff_merge::NameStatusEntry,
        options: &DiffRenderOptions<'_>,
    ) -> Result<()> {
        write_submodule_patch_entry(out, entry, *options)
    }
}

fn visible_submodule_dirt(
    entry: &sley_diff_merge::NameStatusEntry,
    options: &DiffRenderOptions<'_>,
) -> u8 {
    options
        .submodule_dirt
        .and_then(|dirty| dirty.get(&entry.path[..]).copied())
        .unwrap_or(0)
}

fn database_git_dir(db: &FileObjectDatabase) -> Option<PathBuf> {
    let objects = db.objects_dir();
    (objects.file_name()? == "objects").then(|| objects.parent().map(Path::to_path_buf))?
}

pub(crate) fn submodule_git_dir_for_path(
    parent_db: &FileObjectDatabase,
    sub_root: &Path,
    path: &[u8],
) -> Option<PathBuf> {
    sley_diff_merge::gitlink_git_dir(sub_root).or_else(|| {
        let git_dir = database_git_dir(parent_db)?;
        let modules_dir = git_dir.join("modules").join(repo_path_to_path(path));
        modules_dir.is_dir().then_some(modules_dir)
    })
}

fn write_submodule_patch_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    options: DiffRenderOptions<'_>,
) -> Result<()> {
    let old_is_gitlink = entry.old_mode == Some(0o160000);
    let new_is_gitlink = entry.new_mode == Some(0o160000);
    if old_is_gitlink && entry.new_mode.is_some() && !new_is_gitlink {
        let sub_entry = sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Deleted,
            path: entry.path.clone(),
            old_path: None,
            old_mode: entry.old_mode,
            new_mode: None,
            old_oid: entry.old_oid,
            new_oid: None,
        };
        write_submodule_patch_entry(stdout, &sub_entry, options)?;
        let blob_entry = sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Added,
            path: entry.path.clone(),
            old_path: None,
            old_mode: None,
            new_mode: entry.new_mode,
            old_oid: None,
            new_oid: entry.new_oid,
        };
        return write_diff_patch_entry(
            stdout,
            &blob_entry,
            DiffRenderOptions {
                binary: false,
                submodule_format: sley_rev::diff_options::SubmoduleDiffFormat::Short,
                ..options
            },
        );
    }
    if !old_is_gitlink && entry.old_mode.is_some() && new_is_gitlink {
        let blob_entry = sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Deleted,
            path: entry.path.clone(),
            old_path: None,
            old_mode: entry.old_mode,
            new_mode: None,
            old_oid: entry.old_oid,
            new_oid: None,
        };
        write_diff_patch_entry(
            stdout,
            &blob_entry,
            DiffRenderOptions {
                binary: false,
                submodule_format: sley_rev::diff_options::SubmoduleDiffFormat::Short,
                ..options
            },
        )?;
        let sub_entry = sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Added,
            path: entry.path.clone(),
            old_path: None,
            old_mode: None,
            new_mode: entry.new_mode,
            old_oid: None,
            new_oid: entry.new_oid,
        };
        return write_submodule_patch_entry(stdout, &sub_entry, options);
    }

    let dirt = visible_submodule_dirt(entry, &options);
    let path = String::from_utf8_lossy(&entry.path);
    if dirt & sley_worktree::DIRTY_SUBMODULE_UNTRACKED != 0 {
        writeln!(stdout, "Submodule {path} contains untracked content")?;
    }
    if dirt & sley_worktree::DIRTY_SUBMODULE_MODIFIED != 0 {
        writeln!(stdout, "Submodule {path} contains modified content")?;
    }

    let old_oid = entry
        .old_oid
        .filter(|_| entry.old_mode == Some(0o160000))
        .unwrap_or_else(|| ObjectId::null(options.format));
    let new_oid = sley_submodule::history::new_gitlink_oid(
        entry,
        options.db,
        options.worktree_root,
        options.use_worktree_new,
    )?
    .filter(|_| entry.new_mode == Some(0o160000))
    .unwrap_or_else(|| ObjectId::null(options.format));

    let diff_dirty_only =
        options.submodule_format == sley_rev::diff_options::SubmoduleDiffFormat::Diff
            && dirt & sley_worktree::DIRTY_SUBMODULE_MODIFIED != 0;
    if old_oid == new_oid && !diff_dirty_only {
        return Ok(());
    }

    let sub_root = options
        .worktree_root
        .map(|root| root.join(repo_path_to_path(&entry.path)));
    let sub_git_dir = sub_root
        .as_deref()
        .and_then(|root| submodule_git_dir_for_path(options.db, root, &entry.path));
    let (sub_format, sub_db) = match sub_git_dir.as_deref() {
        Some(git_dir) => match repository_object_format(git_dir) {
            Ok(format) => (
                Some(format),
                Some(FileObjectDatabase::from_git_dir(git_dir, format)),
            ),
            Err(_) => (None, None),
        },
        None => (None, None),
    };

    let old_present = sub_db
        .as_ref()
        .is_some_and(|db| old_oid.is_null() || sley_submodule::history::submodule_commit_tree(db, &old_oid).is_ok());
    let new_present = sub_db
        .as_ref()
        .is_some_and(|db| new_oid.is_null() || sley_submodule::history::submodule_commit_tree(db, &new_oid).is_ok());
    if old_oid == new_oid && diff_dirty_only {
        if let (Some(sub_db), Some(sub_format)) = (sub_db.as_ref(), sub_format) {
            write_submodule_inline_diff(
                stdout, entry, options, sub_db, sub_format, &old_oid, &new_oid, dirt,
            )?;
        }
        return Ok(());
    }
    let message = if old_oid.is_null() {
        Some("(new submodule)")
    } else if new_oid.is_null() {
        Some("(submodule deleted)")
    } else if sub_db.is_none() || !old_present || !new_present {
        Some("(commits not present)")
    } else {
        None
    };
    let (range, rewind) = if message == Some("(commits not present)") {
        ("...", false)
    } else {
        sley_submodule::history::submodule_range_marker(
            sub_git_dir.as_deref(),
            sub_db.as_ref(),
            sub_format,
            &old_oid,
            &new_oid,
        )?
    };
    let old_abbrev = submodule_abbrev(&old_oid);
    let new_abbrev = submodule_abbrev(&new_oid);
    match message {
        Some(message) => {
            writeln!(
                stdout,
                "Submodule {path} {old_abbrev}{range}{new_abbrev} {message}"
            )?;
        }
        None if rewind => {
            writeln!(
                stdout,
                "Submodule {path} {old_abbrev}{range}{new_abbrev} (rewind):"
            )?;
        }
        None => {
            writeln!(stdout, "Submodule {path} {old_abbrev}{range}{new_abbrev}:")?;
        }
    }

    let Some(sub_db) = sub_db.as_ref() else {
        return Ok(());
    };
    let Some(sub_format) = sub_format else {
        return Ok(());
    };
    if message == Some("(commits not present)") || !old_present || !new_present {
        return Ok(());
    }

    match options.submodule_format {
        sley_rev::diff_options::SubmoduleDiffFormat::Log => {
            sley_submodule::history::write_submodule_log(
                stdout,
                sub_git_dir.as_deref(),
                sub_db,
                sub_format,
                &old_oid,
                &new_oid,
                &submodule_commit_subject,
            )?;
        }
        sley_rev::diff_options::SubmoduleDiffFormat::Diff => {
            write_submodule_inline_diff(
                stdout, entry, options, sub_db, sub_format, &old_oid, &new_oid, dirt,
            )?;
        }
        sley_rev::diff_options::SubmoduleDiffFormat::Short => {}
    }
    Ok(())
}

fn submodule_abbrev(oid: &ObjectId) -> String {
    oid.to_hex()[..oid.abbrev_hex_len(7)].to_string()
}

fn submodule_commit_subject(commit: &Commit) -> String {
    let encoding = commit_encoding(commit);
    let message = log_reencode_message(&commit.message, &encoding, "UTF-8");
    commit_subject(&message)
}


fn nested_submodule_options<'a>(
    options: &DiffRenderOptions<'a>,
    sub_db: &'a FileObjectDatabase,
    sub_format: ObjectFormat,
    sub_root: Option<&'a Path>,
    use_worktree_new: bool,
    src_prefix: &'a str,
    dst_prefix: &'a str,
    dirt: &'a HashMap<Vec<u8>, u8>,
) -> DiffRenderOptions<'a> {
    DiffRenderOptions {
        binary: false,
        anchors: &[],
        allow_textconv: false,
        db: sub_db,
        lazy_fetch: options.lazy_fetch,
        worktree_root: sub_root,
        use_worktree_new,
        format: sub_format,
        abbrev: options.abbrev,
        src_prefix,
        dst_prefix,
        context: options.context,
        userdiff: None,
        funcname: None,
        colors: options.colors,
        word_diff: None,
        line_indicators: sley_diff_merge::render::LineIndicators::default(),
        suppress_blank_empty: false,
        no_index_contents: None,
        submodule_format: sley_rev::diff_options::SubmoduleDiffFormat::Diff,
        submodule_dirt: Some(dirt),
        ws_error: None,
        color_moved: None,
        interhunk: options.interhunk,
        ws_ignore: sley_diff_merge::WsIgnore::default(),
        diff_algorithm: options.diff_algorithm,
        ignore_blank_lines: false,
        ignore_regexes: &[],
        line_ranges: None,
        indent_heuristic: options.indent_heuristic,
        big_file_threshold: options.big_file_threshold,
        submodule_render: Some(&CLI_SUBMODULE_PATCH_RENDER),
    }
}

fn write_submodule_inline_diff(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    options: DiffRenderOptions<'_>,
    sub_db: &FileObjectDatabase,
    sub_format: ObjectFormat,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    dirt: u8,
) -> Result<()> {
    let old_tree = sley_submodule::history::submodule_commit_tree(sub_db, old_oid)?;
    let new_tree = sley_submodule::history::submodule_commit_tree(sub_db, new_oid)?;
    let entries = if old_oid.is_null() {
        sley_diff_merge::diff_name_status_empty_tree_with_options(
            sub_db,
            sub_format,
            &new_tree,
            sley_diff_merge::DiffNameStatusOptions::default(),
        )?
    } else {
        sley_diff_merge::diff_name_status_trees_with_options(
            sub_db,
            sub_format,
            &old_tree,
            &new_tree,
            sley_diff_merge::DiffNameStatusOptions::default(),
        )?
    };
    let sub_path = String::from_utf8_lossy(&entry.path);
    let src_prefix = format!("{}{}{}", options.src_prefix, sub_path, "/");
    let dst_prefix = format!("{}{}{}", options.dst_prefix, sub_path, "/");
    let nested_worktree_root = options
        .worktree_root
        .map(|root| root.join(repo_path_to_path(&entry.path)));
    if dirt & sley_worktree::DIRTY_SUBMODULE_MODIFIED != 0
        && let Some(sub_root) = nested_worktree_root.as_deref()
    {
        let Some(sub_git_dir) = submodule_git_dir_for_path(options.db, sub_root, &entry.path)
        else {
            return Ok(());
        };
        let submodule_dirt = submodule_collect_patch_dirt(sub_root, &sub_git_dir, sub_format)?;
        let dirty_entries = sley_diff_merge::diff_name_status_tree_worktree_with_options(
            sub_root,
            &sub_git_dir,
            sub_format,
            &old_tree,
            sley_diff_merge::DiffNameStatusOptions::default(),
        )?;
        for dirty_entry in &dirty_entries {
            write_diff_patch_entry(
                stdout,
                dirty_entry,
                nested_submodule_options(
                    &options,
                    sub_db,
                    sub_format,
                    Some(sub_root),
                    true,
                    &src_prefix,
                    &dst_prefix,
                    &submodule_dirt,
                ),
            )?;
        }
        return Ok(());
    }
    let nested_dirt = match nested_worktree_root.as_deref() {
        Some(root) => {
            let git_dir = submodule_git_dir_for_path(options.db, root, &entry.path);
            match git_dir.as_deref() {
                Some(git_dir) => submodule_collect_patch_dirt(root, git_dir, sub_format)?,
                None => HashMap::new(),
            }
        }
        None => HashMap::new(),
    };
    for sub_entry in &entries {
        write_diff_patch_entry(
            stdout,
            sub_entry,
            nested_submodule_options(
                &options,
                sub_db,
                sub_format,
                nested_worktree_root.as_deref(),
                false,
                &src_prefix,
                &dst_prefix,
                &nested_dirt,
            ),
        )?;
    }
    Ok(())
}

fn submodule_collect_patch_dirt(
    sub_root: &Path,
    sub_git_dir: &Path,
    format: ObjectFormat,
) -> Result<HashMap<Vec<u8>, u8>> {
    let Some(index) = sley_worktree::read_repository_index(sub_git_dir, format)? else {
        return Ok(HashMap::new());
    };
    let mut dirt = HashMap::new();
    for entry in index.entries.iter().filter(|entry| entry.mode == 0o160000) {
        let path = entry.path.as_bytes();
        let submodule_root = sub_root.join(repo_path_to_path(path));
        let bits = sley_worktree::submodule_dirt(&submodule_root);
        if bits != 0 {
            dirt.insert(path.to_vec(), bits);
        }
    }
    Ok(dirt)
}

