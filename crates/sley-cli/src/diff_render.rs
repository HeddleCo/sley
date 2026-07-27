//! Diff output rendering helpers (`write_diff_*`, stat/dirstat, pathspec filtering).
#![allow(clippy::expect_used)]

use crate::commands;
use crate::commands::remote::read_repo_config;
use crate::session;
use crate::{
    BString, DEFAULT_BIG_FILE_THRESHOLD, GitConfig, GitError, ObjectFormat, ObjectId, Result,
    commit_encoding, commit_subject, core_big_file_threshold, log_reencode_message,
    normalize_absolute_cli_pathspec, repository_object_format, sley_config, sley_core,
    sley_diff_merge, sley_odb, sley_pretty, sley_remote, sley_rev, sley_worktree,
    status_quote_path, worktree_root_for_git_dir,
};
use sley::plumbing::sley_object::{Commit, EncodedObject, ObjectType};
use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectReader};
use sley::plumbing::sley_rev::diff_options::{
    DiffStatWidths, DirstatMode, DirstatOptions, SubmoduleIgnoreMode, parse_submodule_ignore_mode,
};
use sley_grep;
use sley_pathspec::{LsFilesPathFilter, parse_normalized_pathspec_element, pathspec_filters_match};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;

pub(crate) use sley_diff_merge::porcelain::{
    LineStats as DiffLineStats, StatEntry as DiffStatEntryData, StatOptions as DiffStatOptions,
};

struct CliDiffRenderServices;

impl sley_diff_merge::porcelain::RenderServices for CliDiffRenderServices {
    fn display_width(&self, rendered: &str) -> i64 {
        sley_strbuf_expand::strwidth(rendered.as_bytes()) as i64
    }
}

fn map_porcelain_render(
    result: std::result::Result<
        sley_diff_merge::porcelain::RenderOutcome,
        sley_diff_merge::porcelain::RenderError,
    >,
) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(sley_diff_merge::porcelain::RenderError::Output(error)) => Err(error.into()),
    }
}

pub(crate) fn write_diff_summary_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
) -> Result<()> {
    map_porcelain_render(sley_diff_merge::porcelain::render_summary_entry(
        stdout, entry,
    ))
}

pub(crate) fn write_diff_raw_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    z: bool,
    zero_worktree_oids: bool,
    abbrev: Option<usize>,
    format: ObjectFormat,
) -> Result<()> {
    map_porcelain_render(sley_diff_merge::porcelain::render_raw_entry(
        stdout,
        entry,
        sley_diff_merge::porcelain::RawOptions {
            nul_terminated: z,
            zero_new_oid: zero_worktree_oids,
            abbrev,
            object_format: format,
            print_hash_ellipsis: std::env::var("GIT_PRINT_SHA1_ELLIPSIS")
                .is_ok_and(|value| value == "yes"),
        },
    ))
}

#[derive(Clone, Copy)]
pub(crate) struct DiffWorktreeCleanContext<'a> {
    pub(crate) config: &'a GitConfig,
    pub(crate) attributes: &'a sley_worktree::WorktreeAttributes,
}

#[derive(Clone, Copy)]
pub(crate) struct DiffRenderOptions<'a> {
    pub(crate) db: &'a FileObjectDatabase,
    pub(crate) lazy_fetch: bool,
    pub(crate) worktree_root: Option<&'a Path>,
    pub(crate) use_worktree_new: bool,
    pub(crate) format: ObjectFormat,
    pub(crate) abbrev: usize,
    pub(crate) src_prefix: &'a str,
    pub(crate) dst_prefix: &'a str,
    /// Lines of hunk context (`-U<n>`); the porcelain default is 3.
    pub(crate) context: usize,
    /// Userdiff driver resolution (`diff=<driver>` attributes + config);
    /// `None` keeps the default funcname heuristic.
    pub(crate) userdiff: Option<&'a commands::userdiff::UserdiffResolver>,
    /// Explicit function-name heading pattern for `@@ @@` section headers.
    /// `None` falls back to `userdiff` resolution or the built-in default
    /// funcname resolver.
    pub(crate) funcname: Option<&'a commands::userdiff::CompiledFuncname>,
    /// ANSI palette when color output is enabled.
    pub(crate) colors: Option<&'a commands::diff_words::DiffColors>,
    /// Word-diff rendering request (mode + the command-line regex override).
    pub(crate) word_diff: Option<&'a WordDiffRequest<'a>>,
    /// Hunk body line indicators (` `, `-`, `+` by default).
    pub(crate) line_indicators: sley_diff_merge::render::LineIndicators,
    /// Omit the leading context marker on an otherwise-empty context line.
    pub(crate) suppress_blank_empty: bool,
    /// Preloaded file contents for `diff --no-index` (old, new), bypassing
    /// the object database / worktree reads.
    pub(crate) no_index_contents: Option<(Option<&'a [u8]>, Option<&'a [u8]>)>,
    /// Requested gitlink renderer. `Short` is the synthetic one-line patch;
    /// `Log` and `Diff` use submodule-native history and tree diff rendering.
    pub(crate) submodule_format: commands::diff_options::SubmoduleDiffFormat,
    /// Gitlink paths whose worktree-side submodule dirt is visible after
    /// `--ignore-submodules` filtering. The bitmask uses
    /// `sley_worktree::DIRTY_SUBMODULE_*`.
    pub(crate) submodule_dirt: Option<&'a HashMap<Vec<u8>, u8>>,
    /// Whitespace-error highlighting (`--ws-error-highlight` /
    /// `diff.wsErrorHighlight`) when color is enabled. `None` disables it.
    pub(crate) ws_error: Option<sley_diff_merge::render::WsErrorHighlight>,
    /// Moved-code coloring (`--color-moved`) when color is enabled and
    /// word-diff is disabled. `None` disables it.
    pub(crate) color_moved: Option<sley_diff_merge::render::ColorMoved>,
    /// Extra inter-hunk merge distance (`--inter-hunk-context`).
    pub(crate) interhunk: usize,
    /// Whitespace-ignore flags (`-w`, `-b`, `--ignore-space-at-eol`,
    /// `--ignore-cr-at-eol`) applied to the line comparison.
    pub(crate) ws_ignore: sley_diff_merge::WsIgnore,
    /// The line-diff algorithm (`--patience` / `--histogram` / default Myers).
    pub(crate) diff_algorithm: sley_diff_merge::DiffAlgorithm,
    /// `--ignore-blank-lines`: drop change groups whose lines are all blank.
    pub(crate) ignore_blank_lines: bool,
    /// `-I<regex>` / `--ignore-matching-lines`: drop change groups all of whose
    /// lines match one of these (compiled ERE) regexes.
    pub(crate) ignore_regexes: &'a [sley_grep::Regex],
    /// `log -L`: restrict the emitted hunks to these post-image line ranges.
    /// `None` (every non-line-log caller) renders the full patch.
    pub(crate) line_ranges: Option<&'a [sley_diff_merge::render::LineRange]>,
    /// `--indent-heuristic` / `diff.indentHeuristic`: shift slidable change
    /// groups to the most readable boundary. Enabled by default, matching git.
    pub(crate) indent_heuristic: bool,
    /// `--binary`: emit an applicable `GIT binary patch` block (literal-encoded,
    /// full index) for binary files instead of `Binary files … differ`.
    pub(crate) binary: bool,
    /// `--anchored=<text>` prefixes (git's patience anchors). Only consulted when
    /// `diff_algorithm` is patience; empty (the default) is plain patience.
    pub(crate) anchors: &'a [Vec<u8>],
    /// git's `DIFF_OPT_ALLOW_TEXTCONV`: when set, a regular-file side whose diff
    /// driver defines `diff.<d>.textconv` is converted to its text representation
    /// before binary detection and diffing. Enabled for porcelain patch output
    /// (`git diff`/`show`/`log -p`/`status -v`); off for plumbing (`diff-tree`,
    /// `diff-index`, `diff-files`) and patch generation (`format-patch`).
    pub(crate) allow_textconv: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DiffEntryRenderModes {
    pub(crate) raw: bool,
    pub(crate) numstat: bool,
    pub(crate) stat: bool,
    pub(crate) shortstat: bool,
    pub(crate) summary: bool,
    pub(crate) patch: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DiffEntryRawRenderOptions {
    pub(crate) z: bool,
    pub(crate) abbrev: Option<usize>,
    pub(crate) format: ObjectFormat,
}

pub(crate) enum DiffEntryStatSource<'a> {
    Materialized(&'a [DiffStatEntryData<'a>]),
}

pub(crate) struct DiffEntryStatRenderOptions<'a> {
    pub(crate) source: Option<DiffEntryStatSource<'a>>,
    pub(crate) z: bool,
    pub(crate) options: DiffStatOptions,
    pub(crate) widths: Option<DiffStatWidths>,
    pub(crate) config: Option<&'a GitConfig>,
}

pub(crate) struct DiffEntryRenderContext<'a> {
    pub(crate) raw: DiffEntryRawRenderOptions,
    pub(crate) stat: DiffEntryStatRenderOptions<'a>,
    pub(crate) after_stat: Option<&'a mut dyn FnMut(&mut dyn Write) -> Result<()>>,
    pub(crate) prefix_already_written: bool,
}

pub(crate) fn render_diff_entries<RawZero, PatchEntry>(
    stdout: &mut dyn Write,
    entries: &[sley_diff_merge::NameStatusEntry],
    modes: DiffEntryRenderModes,
    mut context: DiffEntryRenderContext<'_>,
    mut raw_zero_worktree_oids: RawZero,
    mut write_patch_entry: PatchEntry,
) -> Result<()>
where
    RawZero: FnMut(&sley_diff_merge::NameStatusEntry) -> bool,
    PatchEntry: FnMut(&mut dyn Write, &sley_diff_merge::NameStatusEntry) -> Result<()>,
{
    let mut wrote_prefix = context.prefix_already_written;
    if modes.raw {
        for entry in entries {
            write_diff_raw_entry(
                stdout,
                entry,
                context.raw.z,
                raw_zero_worktree_oids(entry),
                context.raw.abbrev,
                context.raw.format,
            )?;
        }
        wrote_prefix = true;
    }
    if modes.numstat {
        match context
            .stat
            .source
            .as_ref()
            .expect("stat source provided for numstat")
        {
            DiffEntryStatSource::Materialized(stat_entries) => {
                for entry in *stat_entries {
                    write_diff_numstat_materialized_entry(
                        stdout,
                        entry.entry,
                        entry.stats,
                        context.stat.z,
                    )?;
                }
            }
        }
        wrote_prefix = true;
    }
    if modes.stat {
        match context
            .stat
            .source
            .as_ref()
            .expect("stat source provided for diffstat")
        {
            DiffEntryStatSource::Materialized(stat_entries) => match context.stat.widths {
                Some(widths) => write_diff_stat_materialized_with_widths(
                    stdout,
                    stat_entries,
                    context.stat.options,
                    widths,
                )?,
                None => write_diff_stat_materialized(
                    stdout,
                    stat_entries,
                    context.stat.options,
                    context.stat.config,
                )?,
            },
        }
        wrote_prefix = true;
    }
    if modes.shortstat {
        match context
            .stat
            .source
            .as_ref()
            .expect("stat source provided for shortstat")
        {
            DiffEntryStatSource::Materialized(stat_entries) => {
                write_diff_shortstat_materialized(stdout, stat_entries)?;
            }
        }
        wrote_prefix = true;
    }
    if let Some(after_stat) = context.after_stat.as_mut() {
        after_stat(stdout)?;
    }
    if modes.summary {
        for entry in entries {
            write_diff_summary_entry(stdout, entry)?;
        }
        wrote_prefix = true;
    }
    if modes.patch {
        if wrote_prefix {
            writeln!(stdout)?;
        }
        for entry in entries {
            write_patch_entry(stdout, entry)?;
        }
    }
    Ok(())
}

/// A `--word-diff` request before per-file word-regex resolution.
pub(crate) struct WordDiffRequest<'a> {
    pub(crate) mode: commands::diff_words::WordDiffMode,
    /// `--word-diff-regex` / `--color-words=<re>` override.
    pub(crate) cli_regex: Option<&'a str>,
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

/// The resolved submodule-ignore configuration for a diff invocation.
/// Precedence per path (upstream run_diff_files / set_diffopt_flags_from_
/// submodule_config): an explicit `--ignore-submodules` wins over everything;
/// otherwise `submodule.<name>.ignore` in the repo config, then in
/// `.gitmodules`; otherwise `diff.ignoreSubmodules`; otherwise the implicit
/// untracked-ignoring default.
pub(crate) struct SubmoduleDiffConfig {
    cli: Option<SubmoduleIgnoreMode>,
    base: SubmoduleIgnoreMode,
    per_path: HashMap<Vec<u8>, SubmoduleIgnoreMode>,
}

impl SubmoduleDiffConfig {
    fn effective(&self, path: &[u8]) -> SubmoduleIgnoreMode {
        if let Some(mode) = self.cli {
            return mode;
        }
        if let Some(mode) = self.per_path.get(path) {
            return *mode;
        }
        self.base
    }
}

pub(crate) fn submodule_diff_config(
    git_dir: &Path,
    worktree_root: Option<&Path>,
    cli: Option<SubmoduleIgnoreMode>,
) -> SubmoduleDiffConfig {
    submodule_diff_config_with_config(git_dir, worktree_root, cli, None)
}

pub(crate) fn submodule_diff_config_with_config(
    git_dir: &Path,
    worktree_root: Option<&Path>,
    cli: Option<SubmoduleIgnoreMode>,
    repo_config: Option<&GitConfig>,
) -> SubmoduleDiffConfig {
    let loaded_config = repo_config
        .is_none()
        .then(|| read_repo_config(git_dir).ok())
        .flatten();
    let repo_config = repo_config.or(loaded_config.as_ref());
    let base = repo_config
        .and_then(|config| config.get("diff", None, "ignoresubmodules"))
        .and_then(parse_submodule_ignore_mode)
        .unwrap_or(SubmoduleIgnoreMode::Untracked);
    let mut per_path = HashMap::new();
    if let Some(root) = worktree_root
        && let Ok(gitmodules) = GitConfig::read(root.join(".gitmodules"))
    {
        for section in &gitmodules.sections {
            if section.name != "submodule" {
                continue;
            }
            let Some(name) = section.subsection.as_deref() else {
                continue;
            };
            let value_of = |key: &str| {
                section
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.key.eq_ignore_ascii_case(key))
                    .and_then(|entry| entry.value.as_deref())
            };
            let Some(path) = value_of("path") else {
                continue;
            };
            let ignore = repo_config
                .and_then(|config| config.get("submodule", Some(name), "ignore"))
                .or_else(|| value_of("ignore"));
            if let Some(mode) = ignore.and_then(parse_submodule_ignore_mode) {
                per_path.insert(path.as_bytes().to_vec(), mode);
            }
        }
    }
    SubmoduleDiffConfig {
        cli,
        base,
        per_path,
    }
}

/// Drop gitlink entries whose effective ignore mode is `all`.
pub(crate) fn apply_submodule_ignore_filter(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    config: &SubmoduleDiffConfig,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    entries
        .into_iter()
        .filter(|entry| {
            let gitlink = entry.old_mode == Some(0o160000) || entry.new_mode == Some(0o160000);
            !(gitlink && config.effective(&entry.path) == SubmoduleIgnoreMode::All)
        })
        .collect()
}

/// For a worktree-involved diff: find every staged gitlink whose submodule
/// tree is dirty under its effective ignore mode (the `-dirty` suffix set),
/// and append a Modified pair for dirty submodules whose checked-out commit
/// still matches the staged oid (which the map comparison alone would skip).
pub(crate) fn collect_dirty_submodules(
    entries: &mut Vec<sley_diff_merge::NameStatusEntry>,
    git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    config: &SubmoduleDiffConfig,
    precomputed_gitlinks: Option<&[sley_diff_merge::IndexGitlinkEntry]>,
) -> Result<HashMap<Vec<u8>, u8>> {
    if let Some(gitlinks) = precomputed_gitlinks {
        return collect_dirty_submodules_from_gitlinks(entries, worktree_root, config, gitlinks);
    }
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(HashMap::new());
    };
    let gitlinks = index
        .entries
        .iter()
        .filter(|entry| entry.mode == 0o160000)
        .map(|entry| sley_diff_merge::IndexGitlinkEntry {
            path: BString::from_bytes(entry.path.as_bytes()),
            oid: entry.oid,
        })
        .collect::<Vec<_>>();
    collect_dirty_submodules_from_gitlinks(entries, worktree_root, config, &gitlinks)
}

fn collect_dirty_submodules_from_gitlinks(
    entries: &mut Vec<sley_diff_merge::NameStatusEntry>,
    worktree_root: &Path,
    config: &SubmoduleDiffConfig,
    gitlinks: &[sley_diff_merge::IndexGitlinkEntry],
) -> Result<HashMap<Vec<u8>, u8>> {
    let mut dirty = HashMap::new();
    let mut injected = false;
    for entry in gitlinks {
        let path = entry.path.as_bytes();
        let mode = config.effective(path);
        if matches!(mode, SubmoduleIgnoreMode::All | SubmoduleIgnoreMode::Dirty) {
            continue;
        }
        let sub_root = worktree_root.join(repo_path_to_path(path));
        let dirt = sley_worktree::submodule_dirt(&sub_root);
        let counts = dirt & sley_worktree::DIRTY_SUBMODULE_MODIFIED != 0
            || (mode == SubmoduleIgnoreMode::None
                && dirt & sley_worktree::DIRTY_SUBMODULE_UNTRACKED != 0);
        if !counts {
            continue;
        }
        let visible_dirt = match mode {
            SubmoduleIgnoreMode::None => dirt,
            SubmoduleIgnoreMode::Untracked => dirt & !sley_worktree::DIRTY_SUBMODULE_UNTRACKED,
            SubmoduleIgnoreMode::Dirty | SubmoduleIgnoreMode::All => 0,
        };
        if visible_dirt == 0 {
            continue;
        }
        dirty.insert(path.to_vec(), visible_dirt);
        if !entries.iter().any(|existing| existing.path[..] == *path) {
            entries.push(sley_diff_merge::NameStatusEntry {
                status: sley_diff_merge::NameStatus::Modified,
                path: path.to_vec().into(),
                old_path: None,
                old_mode: Some(0o160000),
                new_mode: Some(0o160000),
                old_oid: Some(entry.oid),
                new_oid: Some(entry.oid),
            });
            injected = true;
        }
    }
    if injected {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
    }
    Ok(dirty)
}

/// Render the `git diff-tree -p`-equivalent patch between two trees, byte-for-byte
/// matching what `git show`/`log_tree_commit` writes. Used by the sequencer's
/// `make_patch` (`.git/rebase-merge/patch`) so the stopped-pick patch carries the
/// real `index <old>..<new> <mode>` line, collapsed single-line hunk headers
/// (`@@ -1 +1 @@`), and no spurious blank lines — exactly the renderer that drives
/// porcelain diff output, rather than a hand-rolled approximation.
///
/// Matches git `make_patch`: `DEFAULT_ABBREV` (7) index hashes, three lines of
/// context, no rename detection (`log_tree_commit` does not honor `diff.renames`),
/// `a/`/`b/` prefixes.
pub(crate) fn render_tree_to_tree_patch(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    lazy_fetch: bool,
) -> Result<Vec<u8>> {
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    // Batch-prefetch every blob the patch body will open (git's
    // `diff_queued_diff_prefetch` / `promisor_remote_get_direct`).
    prefetch_diff_entry_blobs(db, &entries, lazy_fetch)?;
    let mut out: Vec<u8> = Vec::new();
    for entry in &entries {
        write_diff_patch_entry(
            &mut out,
            entry,
            DiffRenderOptions {
                binary: false,
                anchors: &[],
                allow_textconv: false,
                db,
                lazy_fetch,
                worktree_root: None,
                use_worktree_new: false,
                format,
                abbrev: 7,
                src_prefix: "a/",
                dst_prefix: "b/",
                context: 3,
                userdiff: None,
                funcname: None,
                colors: None,
                word_diff: None,
                line_indicators: sley_diff_merge::render::LineIndicators::default(),
                suppress_blank_empty: false,
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                color_moved: None,
                interhunk: 0,
                ws_ignore: sley_diff_merge::WsIgnore::default(),
                diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
                ignore_blank_lines: false,
                ignore_regexes: &[],
                line_ranges: None,
                indent_heuristic: true,
            },
        )?;
    }
    Ok(out)
}

pub(crate) fn apply_diff_order_file(
    mut entries: Vec<sley_diff_merge::NameStatusEntry>,
    order_file: Option<&str>,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    let Some(order_file) = order_file else {
        return Ok(entries);
    };
    if order_file == "/dev/null" {
        return Ok(entries);
    }
    let path = Path::new(order_file);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let contents = fs::read(&path)?;
    let patterns: Vec<Vec<u8>> = contents
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(|line| line.to_vec())
        .collect();
    if patterns.is_empty() {
        return Ok(entries);
    }
    entries.sort_by_key(|entry| diff_order_rank(entry, &patterns).unwrap_or(usize::MAX));
    Ok(entries)
}

fn diff_order_rank(
    entry: &sley_diff_merge::NameStatusEntry,
    patterns: &[Vec<u8>],
) -> Option<usize> {
    patterns.iter().position(|pattern| {
        diff_order_pattern_matches(pattern, entry.path.as_bytes())
            || entry
                .old_path
                .as_ref()
                .is_some_and(|old| diff_order_pattern_matches(pattern, old.as_bytes()))
    })
}

fn diff_order_pattern_matches(pattern: &[u8], path: &[u8]) -> bool {
    pattern == path || diff_order_glob_matches(pattern, path)
}

fn diff_order_glob_matches(pattern: &[u8], text: &[u8]) -> bool {
    let mut p = 0usize;
    let mut t = 0usize;
    let mut star: Option<usize> = None;
    let mut match_after_star = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            match_after_star = t;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            match_after_star += 1;
            t = match_after_star;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Compile the `-I<regex>` (`--ignore-matching-lines`) patterns into ERE
/// matchers. A malformed pattern fails like git's `diff_opt_ignore_regex`:
/// `error: invalid regex given to -I: '<pat>'` and exit code 129.
pub(crate) fn compile_ignore_matching_regexes(
    patterns: &[String],
) -> Result<Vec<sley_grep::Regex>> {
    let mut compiled = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        match sley_grep::Regex::compile(pattern, sley_grep::RegexMode::Ere, false, false) {
            Ok(regex) => compiled.push(regex),
            Err(_) => {
                eprintln!("error: invalid regex given to -I: '{pattern}'");
                return Err(GitError::Exit(129));
            }
        }
    }
    Ok(compiled)
}

/// Whether a tree/index mode is a regular file (`S_ISREG`). Textconv acts only
/// on regular-file blobs, never on symlinks (`120000`) or gitlinks (`160000`).
fn diff_mode_is_regular_file(mode: Option<u32>) -> bool {
    matches!(mode, Some(m) if (m & 0o170000) == 0o100000)
}

pub(crate) fn write_diff_patch_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    options: DiffRenderOptions<'_>,
) -> Result<()> {
    // A symlink target that is an incomplete line is not a whitespace error:
    // git clears `WS_INCOMPLETE_LINE` when the new side is a symlink (diff.c
    // "symlink being an incomplete line is not a news"), so the `\ No newline at
    // end of file` marker is rendered in the context color rather than
    // highlighted. Applies before the typechange split below so the split's
    // symlink-creation half inherits the cleared rule.
    let mut options = options;
    if entry.new_mode == Some(0o120000)
        && let Some(ws_error) = options.ws_error.as_mut()
    {
        ws_error.rule &= !sley_diff_merge::ws::WS_INCOMPLETE_LINE;
    }
    // A filepair whose two sides have different file types (regular↔symlink,
    // regular↔gitlink, symlink↔gitlink) cannot be rendered as one textual diff.
    // git's `run_diff` (diff.c) splits it into a deletion of the old side
    // followed by a creation of the new side, each shown through the normal
    // add/delete patch path. The single `T` status survives in raw/name-status/
    // summary; only the patch body is split.
    if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
        && sley_diff_merge::is_type_change(old_mode, new_mode)
    {
        let deletion_path = entry.old_path.clone().unwrap_or_else(|| entry.path.clone());
        let deletion = sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Deleted,
            path: deletion_path,
            old_path: None,
            old_mode: Some(old_mode),
            new_mode: None,
            old_oid: entry.old_oid,
            new_oid: None,
        };
        let deletion_options = DiffRenderOptions {
            no_index_contents: options.no_index_contents.map(|(old, _)| (old, None)),
            ..options
        };
        write_diff_patch_entry(stdout, &deletion, deletion_options)?;
        let creation = sley_diff_merge::NameStatusEntry {
            status: sley_diff_merge::NameStatus::Added,
            path: entry.path.clone(),
            old_path: None,
            old_mode: None,
            new_mode: Some(new_mode),
            old_oid: None,
            new_oid: entry.new_oid,
        };
        let creation_options = DiffRenderOptions {
            no_index_contents: options.no_index_contents.map(|(_, new)| (None, new)),
            ..options
        };
        return write_diff_patch_entry(stdout, &creation, creation_options);
    }
    if is_gitlink_pair(entry)
        && options.submodule_format != commands::diff_options::SubmoduleDiffFormat::Short
        && options.no_index_contents.is_none()
    {
        return write_submodule_patch_entry(stdout, entry, options);
    }
    let (mut old_content, mut new_content) = match options.no_index_contents {
        Some((old, new)) => (old.map(<[u8]>::to_vec), new.map(<[u8]>::to_vec)),
        None => (
            diff_entry_old_content(entry, options.db, options.lazy_fetch)?,
            diff_entry_new_content(
                entry,
                options.db,
                options.worktree_root,
                options.use_worktree_new,
                None,
                options.lazy_fetch,
            )?,
        ),
    };
    // A dirty submodule's worktree side carries the `-dirty` suffix in the
    // synthesized "Subproject commit <oid>" content (diff.c
    // diff_populate_filespec with dirty_submodule set).
    if entry.new_mode == Some(0o160000)
        && options.use_worktree_new
        && options
            .submodule_dirt
            .is_some_and(|dirty| dirty.contains_key(&entry.path[..]))
        && let Some(content) = new_content.as_mut()
        && content.ends_with(b"\n")
    {
        content.truncate(content.len() - 1);
        content.extend_from_slice(b"-dirty\n");
    }
    let old_path = entry.old_path.as_deref().unwrap_or(&entry.path);
    let diff_old_path = diff_patch_prefixed_path(options.src_prefix, old_path);
    let diff_path = diff_patch_prefixed_path(options.dst_prefix, &entry.path);
    let old_header_path = diff_patch_file_header_path(options.src_prefix, old_path);
    let header_path = diff_patch_file_header_path(options.dst_prefix, &entry.path);
    let old_similarity_path = status_quote_path(old_path, false);
    let similarity_path = status_quote_path(&entry.path, false);
    let colors = options.colors;
    let (old_driver, new_driver) = match options.userdiff {
        Some(resolver) => (
            resolver.driver_for_path(old_path)?,
            resolver.driver_for_path(&entry.path)?,
        ),
        None => (None, None),
    };
    // Textconv (git's `fill_textconv`): for porcelain `-p` output, replace a
    // regular-file side's bytes with `diff.<driver>.textconv`'s output before
    // binary detection and diffing. The recorded blob oids (and thus the `index`
    // line) are unaffected; symlinks/gitlinks are never converted (not regular
    // files), and a textconv helper that fails leaves the side unconverted.
    if options.allow_textconv {
        if let Some(driver) = old_driver.as_ref()
            && let Some(command) = driver.textconv.as_deref()
            && diff_mode_is_regular_file(entry.old_mode)
            && let Some(content) = old_content.as_deref()
            && let Some(converted) = commands::userdiff::run_textconv(command, content)?
        {
            old_content = Some(converted);
        }
        if let Some(driver) = new_driver.as_ref()
            && let Some(command) = driver.textconv.as_deref()
            && diff_mode_is_regular_file(entry.new_mode)
            && let Some(content) = new_content.as_deref()
            && let Some(converted) = commands::userdiff::run_textconv(command, content)?
        {
            new_content = Some(converted);
        }
    }
    let binary_override = old_driver
        .as_ref()
        .and_then(|driver| driver.binary)
        .or_else(|| new_driver.as_ref().and_then(|driver| driver.binary));
    let big_file_threshold = core_big_file_threshold(options.db.objects_dir().parent())
        .unwrap_or(DEFAULT_BIG_FILE_THRESHOLD);
    let treat_as_binary = match binary_override {
        Some(binary) => binary,
        None => {
            old_content
                .as_deref()
                .is_some_and(|content| is_binary_or_large_content(content, big_file_threshold))
                || new_content
                    .as_deref()
                    .is_some_and(|content| is_binary_or_large_content(content, big_file_threshold))
        }
    };
    if treat_as_binary {
        return write_diff_binary_patch_entry(stdout, entry, old_content, new_content, options);
    }
    let content_changed = old_content.as_deref() != new_content.as_deref();
    // git's diffcore drops an *unmodified* pair (same content, same mode, not a
    // rename/copy) before formatting — `diff_flush` → `diff_unmodified_pair` — so
    // no `diff --git` header is emitted at all. A plain `Modified` entry whose
    // content and mode are unchanged is exactly such a pair: this happens for the
    // stat-dirty entries `git diff-files` reports (a `touch`ed or `reset
    // --no-refresh`-restored file shows `M` in raw/name-status but produces an
    // empty patch). Suppress the entry entirely to match git.
    let mode_unchanged = match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) => old_mode == new_mode,
        _ => true,
    };
    if matches!(entry.status, sley_diff_merge::NameStatus::Modified)
        && !content_changed
        && mode_unchanged
    {
        return Ok(());
    }
    // `--ignore-blank-lines` / `-w` / `-I<regex>` can erase every hunk of a
    // plain content modification. git then drops the whole file pair (the
    // header is emitted lazily on the first hunk via fn_out_consume), so a
    // file with no surviving hunks produces no `diff --git` block at all. A
    // mode change / rename / copy / add / delete still shows its header, so
    // restrict the suppression to a same-mode `Modified` pair with both sides
    // present. We pre-render the body (cheaply, no funcname/color/word-diff
    // since emptiness depends only on contents + ignore flags) to decide.
    let ignore_active = !options.ws_ignore.is_empty()
        || options.ignore_blank_lines
        || !options.ignore_regexes.is_empty();
    if ignore_active
        && content_changed
        && mode_unchanged
        && matches!(entry.status, sley_diff_merge::NameStatus::Modified)
        && old_content.is_some()
        && new_content.is_some()
    {
        let ignore_regexes = options.ignore_regexes;
        let regex_match = (!ignore_regexes.is_empty()).then_some(move |line: &[u8]| {
            ignore_regexes
                .iter()
                .any(|re| re.is_match_with_case(line, false))
        });
        let change_ignore = (options.ignore_blank_lines || !ignore_regexes.is_empty()).then(|| {
            sley_diff_merge::render::ChangeIgnore {
                ignore_blank_lines: options.ignore_blank_lines,
                regex_match: regex_match.as_ref().map(|f| f as &dyn Fn(&[u8]) -> bool),
            }
        });
        let mut probe_options = sley_diff_merge::render::HunkRenderOptions {
            context: options.context,
            interhunk: options.interhunk,
            ws_ignore: options.ws_ignore,
            algorithm: options.diff_algorithm,
            change_ignore: change_ignore.as_ref(),
            ..Default::default()
        };
        let mut probe = Vec::new();
        sley_diff_merge::render::render_hunks(
            &mut probe,
            old_content.as_deref(),
            new_content.as_deref(),
            &mut probe_options,
        );
        if probe.is_empty() {
            return Ok(());
        }
    }
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
        | sley_diff_merge::NameStatus::TypeChanged
        | sley_diff_merge::NameStatus::Renamed(_)
        | sley_diff_merge::NameStatus::Copied(_) => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                write_diff_meta_line(stdout, colors, &format!("old mode {old_mode:06o}"))?;
                write_diff_meta_line(stdout, colors, &format!("new mode {new_mode:06o}"))?;
            }
        }
        // Unmerged paths are surfaced via the raw/name-status `U` line, not a
        // patch hunk, so they carry no meta header here.
        sley_diff_merge::NameStatus::Unmerged => {}
    }
    write_diff_similarity_headers(&mut *stdout, entry, &old_similarity_path, &similarity_path)?;
    if !content_changed {
        return Ok(());
    }
    let no_index_stdin_add = options.no_index_contents.is_some()
        && matches!(entry.status, sley_diff_merge::NameStatus::Added)
        && entry.path.as_bytes() == b"-"
        && entry.old_oid.is_none()
        && entry.new_oid == Some(ObjectId::null(options.format));
    let no_index_stream_pair = options.no_index_contents.is_some()
        && entry.old_oid == Some(ObjectId::null(options.format))
        && entry.new_oid == Some(ObjectId::null(options.format));
    if !no_index_stdin_add && !no_index_stream_pair {
        write_diff_meta_line(
            stdout,
            colors,
            &format!(
                "index {}..{}{}",
                diff_patch_oid(
                    options.db,
                    entry.old_oid.as_ref(),
                    old_content.as_deref(),
                    options.format,
                    options.abbrev,
                ),
                diff_patch_oid(
                    options.db,
                    entry.new_oid.as_ref(),
                    new_content.as_deref(),
                    options.format,
                    options.abbrev,
                ),
                diff_patch_mode_suffix(entry)
            ),
        )?;
    }
    let empty_add_or_delete = matches!(
        entry.status,
        sley_diff_merge::NameStatus::Added | sley_diff_merge::NameStatus::Deleted
    ) && old_content.as_deref().unwrap_or_default().is_empty()
        && new_content.as_deref().unwrap_or_default().is_empty();
    if empty_add_or_delete {
        return Ok(());
    }
    // Build the hunk body before emitting the file headers. When whitespace
    // ignore suppresses all content hunks for a rename/copy/mode change, git
    // still emits the metadata through the index line, but does not print the
    // `---`/`+++` file headers.
    let funcname = options
        .funcname
        .or_else(|| {
            old_driver
                .as_ref()
                .and_then(|driver| driver.funcname.as_ref())
        })
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
                    sley_grep::Regex::compile_bytes(&spec, sley_grep::RegexMode::Ere, false, false)
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
    let mut heading = sley_diff_merge::format::heading_classifier(funcname);
    let mut word_diff_adapter = word_diff
        .as_ref()
        .map(sley_diff_merge::format::WordDiffAdapter::new);
    let ws_error = colors.and(options.ws_error);
    let ignore_regexes = options.ignore_regexes;
    let regex_match = (!ignore_regexes.is_empty()).then_some(move |line: &[u8]| {
        ignore_regexes
            .iter()
            .any(|re| re.is_match_with_case(line, false))
    });
    let change_ignore = (options.ignore_blank_lines || !ignore_regexes.is_empty()).then(|| {
        sley_diff_merge::render::ChangeIgnore {
            ignore_blank_lines: options.ignore_blank_lines,
            regex_match: regex_match.as_ref().map(|f| f as &dyn Fn(&[u8]) -> bool),
        }
    });
    let mut render_options = sley_diff_merge::render::HunkRenderOptions {
        context: options.context,
        interhunk: options.interhunk,
        heading: Some(&mut heading),
        colors: colors.map(sley_diff_merge::format::render_colors),
        word_diff: word_diff_adapter
            .as_mut()
            .map(|adapter| adapter as &mut dyn sley_diff_merge::render::HunkWordDiff),
        line_indicators: options.line_indicators,
        suppress_blank_empty: options.suppress_blank_empty,
        ws_error,
        color_moved: colors
            .and(options.color_moved)
            .filter(|_| word_diff.is_none()),
        ws_ignore: options.ws_ignore,
        algorithm: options.diff_algorithm,
        indent_heuristic: options.indent_heuristic,
        change_ignore: change_ignore.as_ref(),
        line_ranges: options.line_ranges,
        anchors: options.anchors,
        ..Default::default()
    };
    let mut hunks = Vec::new();
    sley_diff_merge::render::render_hunks(
        &mut hunks,
        old_content.as_deref(),
        new_content.as_deref(),
        &mut render_options,
    );
    if hunks.is_empty() {
        return Ok(());
    }
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
    stdout.write_all(&hunks)?;
    Ok(())
}

fn write_diff_binary_patch_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    old_content: Option<Vec<u8>>,
    new_content: Option<Vec<u8>>,
    options: DiffRenderOptions<'_>,
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
        | sley_diff_merge::NameStatus::TypeChanged
        | sley_diff_merge::NameStatus::Renamed(_)
        | sley_diff_merge::NameStatus::Copied(_) => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                writeln!(stdout, "old mode {old_mode:06o}")?;
                writeln!(stdout, "new mode {new_mode:06o}")?;
            }
        }
        // Unmerged paths carry no patch meta header.
        sley_diff_merge::NameStatus::Unmerged => {}
    }
    write_diff_similarity_headers(&mut *stdout, entry, &old_similarity_path, &similarity_path)?;
    if old_content.as_deref() == new_content.as_deref() {
        return Ok(());
    }
    // `--binary` implies `--full-index`: the binary apply requires full hex OIDs.
    let index_abbrev = if options.binary {
        options.format.hex_len()
    } else {
        options.abbrev
    };
    writeln!(
        stdout,
        "index {}..{}{}",
        diff_patch_oid(
            options.db,
            entry.old_oid.as_ref(),
            old_content.as_deref(),
            options.format,
            index_abbrev,
        ),
        diff_patch_oid(
            options.db,
            entry.new_oid.as_ref(),
            new_content.as_deref(),
            options.format,
            index_abbrev,
        ),
        diff_patch_mode_suffix(entry)
    )?;
    if options.binary {
        // Emit an applicable `GIT binary patch` block (forward then reverse hunk,
        // each literal-encoded). Round-trips through the apply binary codec.
        writeln!(stdout, "GIT binary patch")?;
        write_git_binary_hunk(stdout, new_content.as_deref().unwrap_or(b""))?;
        write_git_binary_hunk(stdout, old_content.as_deref().unwrap_or(b""))?;
        return Ok(());
    }
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

/// Emit one `literal <N>` binary hunk: the zlib-deflated content base85-encoded
/// in git's `emit_binary_diff_body` line layout (a length-byte + up to 52 bytes
/// per line), terminated by a blank line.
fn write_git_binary_hunk(stdout: &mut dyn Write, content: &[u8]) -> Result<()> {
    let deflated = deflate_zlib(content);
    writeln!(stdout, "literal {}", content.len())?;
    for chunk in deflated.chunks(52) {
        let mut line = Vec::with_capacity(1 + chunk.len() / 4 * 5 + 5);
        // Length byte: 'A'-'Z' for 1-26 bytes, 'a'-'z' for 27-52.
        let len = chunk.len();
        line.push(if len <= 26 {
            (len as u8) + b'A' - 1
        } else {
            (len as u8) - 26 + b'a' - 1
        });
        encode_base85_group(&mut line, chunk);
        stdout.write_all(&line)?;
        stdout.write_all(b"\n")?;
    }
    writeln!(stdout)?;
    Ok(())
}

/// base85-encode `data` (4 bytes → 5 chars, big-endian), git's `encode_85`.
fn encode_base85_group(out: &mut Vec<u8>, data: &[u8]) {
    const EN85: &[u8; 85] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
    let mut i = 0;
    while i < data.len() {
        let mut acc: u32 = 0;
        for shift in [24u32, 16, 8, 0] {
            if i < data.len() {
                acc |= (data[i] as u32) << shift;
                i += 1;
            } else {
                break;
            }
        }
        let mut group = [0u8; 5];
        let mut value = acc;
        for slot in group.iter_mut().rev() {
            *slot = EN85[(value % 85) as usize];
            value /= 85;
        }
        out.extend_from_slice(&group);
    }
}

/// Deflate `content` with a zlib header/trailer.
fn deflate_zlib(content: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write as _;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(9));
    let _ = encoder.write_all(content);
    encoder.finish().unwrap_or_default()
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
    db: &FileObjectDatabase,
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
    let mut width = abbrev.min(hex.len());
    // Patch index lines use `find_unique_abbrev`, not a blind prefix slice.
    // Only repository-backed OIDs participate: a no-index/worktree content
    // hash may not exist in the ODB and therefore has no repository collision
    // set to extend against.
    if let Some(oid) = oid.filter(|oid| !oid.is_null()) {
        while width < hex.len()
            && matches!(
                db.resolve_prefix(&hex[..width]),
                Ok(sley_odb::ObjectPrefixResolution::Ambiguous(_))
            )
        {
            width += 1;
        }
    }
    hex[..width].to_string()
}

fn diff_patch_mode_suffix(entry: &sley_diff_merge::NameStatusEntry) -> String {
    match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) if old_mode == new_mode => format!(" {old_mode:06o}"),
        _ => String::new(),
    }
}

pub(crate) fn write_diff_numstat_materialized_entry(
    stdout: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
    stats: DiffLineStats,
    z: bool,
) -> Result<()> {
    map_porcelain_render(sley_diff_merge::porcelain::render_numstat_entry(
        stdout, entry, stats, z,
    ))
}

pub(crate) fn write_diff_shortstat_materialized(
    stdout: &mut dyn Write,
    entries: &[DiffStatEntryData<'_>],
) -> Result<()> {
    map_porcelain_render(sley_diff_merge::porcelain::render_shortstat(
        stdout, entries,
    ))
}

/// git `decimal_width()`: columns needed to print `number` in decimal.
pub(crate) fn diff_stat_decimal_width(number: usize) -> i64 {
    sley_diff_merge::porcelain::decimal_width(number)
}

pub(crate) fn write_diff_stat_materialized_with_widths(
    stdout: &mut dyn Write,
    entries: &[DiffStatEntryData<'_>],
    options: DiffStatOptions,
    widths: DiffStatWidths,
) -> Result<()> {
    let stat_width = if widths.stat_width == -1 {
        sley_pretty::term_columns() - widths.line_prefix_width
    } else {
        widths.stat_width
    };
    map_porcelain_render(sley_diff_merge::porcelain::render_stat(
        stdout,
        entries,
        options,
        sley_diff_merge::porcelain::StatLayout {
            stat_width,
            name_width: widths.name_width,
            graph_width: widths.graph_width,
        },
        &CliDiffRenderServices,
    ))
}

pub(crate) fn write_diff_stat_materialized(
    stdout: &mut dyn Write,
    entries: &[DiffStatEntryData<'_>],
    options: DiffStatOptions,
    config: Option<&GitConfig>,
) -> Result<()> {
    let mut widths = DiffStatWidths::terminal();
    if let Some(config) = config {
        widths.resolve_config(config);
    } else {
        widths.resolve_config_defaults();
    }
    write_diff_stat_materialized_with_widths(stdout, entries, options, widths)
}

/// git `parse_dirstat_params()`: comma-separated `changes|lines|files|
/// cumulative|noncumulative|<limit>` parameters. Unknown parameters append to
/// `errors` (one line each) and are counted in the returned error total.
pub(crate) fn parse_dirstat_params(
    params: &str,
    options: &mut DirstatOptions,
    errors: &mut String,
) -> usize {
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
                let mut permille: i64 = param[..digits_end].parse::<i64>().unwrap_or(0) * 10;
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
pub(crate) fn write_diff_dirstat(
    stdout: &mut dyn Write,
    entries: &[sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    options: DirstatOptions,
    lazy_fetch: bool,
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
                    let old_content = diff_entry_old_content(entry, db, lazy_fetch)?;
                    let new_content = diff_entry_new_content(
                        entry,
                        db,
                        worktree_root,
                        use_worktree_new,
                        worktree_clean,
                        lazy_fetch,
                    )?;
                    match diff_line_stats(old_content.as_deref(), new_content.as_deref()) {
                        DiffLineStats::Binary { .. } => {
                            let bytes = old_content.as_ref().map_or(0, Vec::len)
                                + new_content.as_ref().map_or(0, Vec::len);
                            (bytes as u64).div_ceil(64)
                        }
                        DiffLineStats::Text { inserted, deleted } => (inserted + deleted) as u64,
                    }
                }
                DirstatMode::Changes => {
                    let old_content = diff_entry_old_content(entry, db, lazy_fetch)?;
                    let new_content = diff_entry_new_content(
                        entry,
                        db,
                        worktree_root,
                        use_worktree_new,
                        worktree_clean,
                        lazy_fetch,
                    )?;
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
        files.push(DirstatFile {
            name,
            changed: damage,
        });
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
pub(crate) fn write_diff_stat_summary_line(
    stdout: &mut dyn Write,
    files: usize,
    inserted: usize,
    deleted: usize,
) -> Result<()> {
    map_porcelain_render(sley_diff_merge::porcelain::render_stat_summary(
        stdout, files, inserted, deleted,
    ))
}

enum DiffBlobContent {
    Object(Arc<EncodedObject>),
    Owned(Vec<u8>),
}

impl DiffBlobContent {
    fn as_slice(&self) -> &[u8] {
        match self {
            DiffBlobContent::Object(object) => &object.body,
            DiffBlobContent::Owned(bytes) => bytes,
        }
    }
}

pub(crate) fn collect_diff_stat_entries<'a>(
    entries: &'a [sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    lazy_fetch: bool,
) -> Result<Vec<DiffStatEntryData<'a>>> {
    collect_diff_stat_entries_with_worktree_clean(
        entries,
        db,
        worktree_root,
        use_worktree_new,
        None,
        lazy_fetch,
    )
}

pub(crate) fn collect_diff_stat_entries_with_worktree_clean<'a>(
    entries: &'a [sley_diff_merge::NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<Vec<DiffStatEntryData<'a>>> {
    // Batch-hydrate every blob the stat pass will open so a partial clone does
    // one promisor negotiation rather than one per path (t4067).
    prefetch_diff_entry_blobs(db, entries, lazy_fetch)?;
    let mut stat_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let old_content = diff_entry_old_stat_content(entry, db, lazy_fetch)?;
        let stats = if entry.old_oid.is_some() && entry.old_oid == entry.new_oid {
            let old_bytes = old_content.as_ref().map(DiffBlobContent::as_slice);
            diff_line_stats(old_bytes, old_bytes)
        } else {
            let new_content = diff_entry_new_stat_content(
                entry,
                db,
                worktree_root,
                use_worktree_new,
                worktree_clean,
                lazy_fetch,
            )?;
            diff_line_stats(
                old_content.as_ref().map(DiffBlobContent::as_slice),
                new_content.as_ref().map(DiffBlobContent::as_slice),
            )
        };
        stat_entries.push(DiffStatEntryData { entry, stats });
    }
    Ok(stat_entries)
}

pub(crate) fn diff_stat_totals(entries: &[DiffStatEntryData<'_>]) -> (usize, usize) {
    sley_diff_merge::porcelain::stat_totals(entries)
}

/// git `pprint_rename()`: collapse a rename's common directory prefix and
/// suffix into braces — `dir/{old => new}/file` — falling back to the plain
/// `old => new` form when either side needs c-style quoting or when nothing
/// is shared.
pub(crate) fn diff_stat_pprint_rename(a: &[u8], b: &[u8], quote_path_fully: bool) -> String {
    sley_diff_merge::porcelain::pprint_rename(a, b, quote_path_fully)
}

/// The synthetic blob content git diffs a gitlink as: `Subproject commit
/// <oid>\n` (diff.c diff_populate_filespec), with an optional `-dirty` suffix
/// for a worktree-side submodule whose own tree has changes.
pub(crate) fn gitlink_diff_content(oid: &ObjectId, dirty: bool) -> Vec<u8> {
    let suffix = if dirty { "-dirty" } else { "" };
    format!("Subproject commit {oid}{suffix}\n").into_bytes()
}

pub(crate) fn is_gitlink_pair(entry: &sley_diff_merge::NameStatusEntry) -> bool {
    entry.old_mode == Some(0o160000) || entry.new_mode == Some(0o160000)
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
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
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
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
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
    let new_oid = diff_entry_new_gitlink_oid(
        entry,
        options.db,
        options.worktree_root,
        options.use_worktree_new,
    )?
    .filter(|_| entry.new_mode == Some(0o160000))
    .unwrap_or_else(|| ObjectId::null(options.format));

    let diff_dirty_only = options.submodule_format
        == commands::diff_options::SubmoduleDiffFormat::Diff
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
        .is_some_and(|db| old_oid.is_null() || submodule_commit_tree(db, &old_oid).is_ok());
    let new_present = sub_db
        .as_ref()
        .is_some_and(|db| new_oid.is_null() || submodule_commit_tree(db, &new_oid).is_ok());
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
        submodule_range_marker(
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
        commands::diff_options::SubmoduleDiffFormat::Log => {
            write_submodule_log(
                stdout,
                sub_git_dir.as_deref(),
                sub_db,
                sub_format,
                &old_oid,
                &new_oid,
            )?;
        }
        commands::diff_options::SubmoduleDiffFormat::Diff => {
            write_submodule_inline_diff(
                stdout, entry, options, sub_db, sub_format, &old_oid, &new_oid, dirt,
            )?;
        }
        commands::diff_options::SubmoduleDiffFormat::Short => {}
    }
    Ok(())
}

fn submodule_abbrev(oid: &ObjectId) -> String {
    oid.to_hex()[..oid.abbrev_hex_len(7)].to_string()
}

fn submodule_commit_tree(db: &FileObjectDatabase, oid: &ObjectId) -> Result<ObjectId> {
    if oid.is_null() {
        return Ok(ObjectId::empty_tree(db.object_format()));
    }
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!("expected commit {oid}")));
    }
    Ok(Commit::parse(db.object_format(), &object.body)?.tree)
}

fn submodule_range_marker(
    git_dir: Option<&Path>,
    db: Option<&FileObjectDatabase>,
    format: Option<ObjectFormat>,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
) -> Result<(&'static str, bool)> {
    let (Some(git_dir), Some(db), Some(format)) = (git_dir, db, format) else {
        return Ok(("...", false));
    };
    if old_oid.is_null() || new_oid.is_null() {
        return Ok(("...", false));
    }
    let bases = sley_rev::merge_bases(git_dir, format, db, old_oid, new_oid)?;
    let fast_forward = bases.iter().any(|base| base == old_oid);
    let rewind = bases.iter().any(|base| base == new_oid);
    Ok((if fast_forward || rewind { ".." } else { "..." }, rewind))
}

fn submodule_symmetric_records(
    db: &FileObjectDatabase,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
) -> Result<Vec<(char, sley_rev::CommitRecord)>> {
    if old_oid.is_null() || new_oid.is_null() {
        return Ok(Vec::new());
    }
    let left = sley_rev::walk_commits(db, db.object_format(), [*old_oid])?;
    let right = sley_rev::walk_commits(db, db.object_format(), [*new_oid])?;
    let left_set = left.iter().map(|record| record.oid).collect::<HashSet<_>>();
    let right_set = right
        .iter()
        .map(|record| record.oid)
        .collect::<HashSet<_>>();
    let mut marked = Vec::new();
    marked.extend(
        right
            .into_iter()
            .filter(|record| !left_set.contains(&record.oid))
            .map(|record| ('>', record)),
    );
    marked.extend(
        left.into_iter()
            .filter(|record| !right_set.contains(&record.oid))
            .map(|record| ('<', record)),
    );
    Ok(marked)
}

fn write_submodule_log(
    stdout: &mut dyn Write,
    git_dir: Option<&Path>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
) -> Result<()> {
    let Some(git_dir) = git_dir else {
        return Ok(());
    };
    let bases = if old_oid.is_null() || new_oid.is_null() {
        HashSet::new()
    } else {
        sley_rev::merge_bases(git_dir, format, db, old_oid, new_oid)?
            .into_iter()
            .collect()
    };
    for (marker, record) in submodule_symmetric_records(db, old_oid, new_oid)? {
        if bases.contains(&record.oid) {
            continue;
        }
        let subject = submodule_commit_subject(&record.commit);
        writeln!(stdout, "  {marker} {subject}")?;
    }
    Ok(())
}

fn submodule_commit_subject(commit: &Commit) -> String {
    let encoding = commit_encoding(commit);
    let message = log_reencode_message(&commit.message, &encoding, "UTF-8");
    commit_subject(&message)
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
    let old_tree = submodule_commit_tree(sub_db, old_oid)?;
    let new_tree = submodule_commit_tree(sub_db, new_oid)?;
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
                DiffRenderOptions {
                    binary: false,
                    anchors: &[],
                    allow_textconv: false,
                    db: sub_db,
                    lazy_fetch: options.lazy_fetch,
                    worktree_root: Some(sub_root),
                    use_worktree_new: true,
                    format: sub_format,
                    abbrev: options.abbrev,
                    src_prefix: &src_prefix,
                    dst_prefix: &dst_prefix,
                    context: options.context,
                    userdiff: None,
                    funcname: None,
                    colors: options.colors,
                    word_diff: None,
                    line_indicators: sley_diff_merge::render::LineIndicators::default(),
                    suppress_blank_empty: false,
                    no_index_contents: None,
                    submodule_format: commands::diff_options::SubmoduleDiffFormat::Diff,
                    submodule_dirt: Some(&submodule_dirt),
                    ws_error: None,
                    color_moved: None,
                    interhunk: options.interhunk,
                    ws_ignore: sley_diff_merge::WsIgnore::default(),
                    diff_algorithm: options.diff_algorithm,
                    ignore_blank_lines: false,
                    ignore_regexes: &[],
                    line_ranges: None,
                    indent_heuristic: options.indent_heuristic,
                },
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
            DiffRenderOptions {
                binary: false,
                anchors: &[],
                allow_textconv: false,
                db: sub_db,
                lazy_fetch: options.lazy_fetch,
                worktree_root: nested_worktree_root.as_deref(),
                use_worktree_new: false,
                format: sub_format,
                abbrev: options.abbrev,
                src_prefix: &src_prefix,
                dst_prefix: &dst_prefix,
                context: options.context,
                userdiff: None,
                funcname: None,
                colors: options.colors,
                word_diff: None,
                line_indicators: sley_diff_merge::render::LineIndicators::default(),
                suppress_blank_empty: false,
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Diff,
                submodule_dirt: Some(&nested_dirt),
                ws_error: None,
                color_moved: None,
                interhunk: options.interhunk,
                ws_ignore: sley_diff_merge::WsIgnore::default(),
                diff_algorithm: options.diff_algorithm,
                ignore_blank_lines: false,
                ignore_regexes: &[],
                line_ranges: None,
                indent_heuristic: options.indent_heuristic,
            },
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

/// Whether a name-status entry produces any visible diff output once the
/// whitespace-ignore (`-w`/`-b`/eol) and change-group-ignore
/// (`--ignore-blank-lines` / `-I<regex>`) flags are applied — git's
/// `DIFF_OPT_HAS_CHANGES`, which `--exit-code`/`--quiet` reflect. A
/// non-content change (add/delete/rename/copy/mode change) always counts; a
/// same-mode pure content modification counts only if a hunk survives the
/// ignore filters.
pub(crate) fn diff_entry_produces_output(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    interhunk: usize,
    context: usize,
    ws_ignore: sley_diff_merge::WsIgnore,
    ignore_blank_lines: bool,
    ignore_regexes: &[sley_grep::Regex],
    lazy_fetch: bool,
) -> Result<bool> {
    // Non-modification statuses, mode changes, and renames/copies always show.
    let mode_unchanged = match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) => old_mode == new_mode,
        _ => true,
    };
    if !matches!(entry.status, sley_diff_merge::NameStatus::Modified) || !mode_unchanged {
        return Ok(true);
    }
    let old_content = diff_entry_old_content(entry, db, lazy_fetch)?;
    let new_content = diff_entry_new_content(
        entry,
        db,
        worktree_root,
        use_worktree_new,
        worktree_clean,
        lazy_fetch,
    )?;
    if old_content.as_deref() == new_content.as_deref() {
        return Ok(false);
    }
    // Binary content always shows a (binary) change.
    if old_content.as_deref().is_some_and(is_binary_content)
        || new_content.as_deref().is_some_and(is_binary_content)
    {
        return Ok(true);
    }
    let regex_match = (!ignore_regexes.is_empty()).then_some(move |line: &[u8]| {
        ignore_regexes
            .iter()
            .any(|re| re.is_match_with_case(line, false))
    });
    let change_ignore = (ignore_blank_lines || !ignore_regexes.is_empty()).then(|| {
        sley_diff_merge::render::ChangeIgnore {
            ignore_blank_lines,
            regex_match: regex_match.as_ref().map(|f| f as &dyn Fn(&[u8]) -> bool),
        }
    });
    let mut probe_options = sley_diff_merge::render::HunkRenderOptions {
        context,
        interhunk,
        ws_ignore,
        algorithm: sley_diff_merge::DiffAlgorithm::Myers,
        change_ignore: change_ignore.as_ref(),
        ..Default::default()
    };
    let mut probe = Vec::new();
    sley_diff_merge::render::render_hunks(
        &mut probe,
        old_content.as_deref(),
        new_content.as_deref(),
        &mut probe_options,
    );
    Ok(!probe.is_empty())
}

fn diff_entry_old_stat_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    lazy_fetch: bool,
) -> Result<Option<DiffBlobContent>> {
    if entry.old_mode == Some(0o160000) {
        return Ok(entry
            .old_oid
            .as_ref()
            .map(|oid| DiffBlobContent::Owned(gitlink_diff_content(oid, false))));
    }
    entry
        .old_oid
        .as_ref()
        .map(|oid| read_blob_content(db, oid, lazy_fetch))
        .transpose()
}

fn diff_entry_new_stat_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<Option<DiffBlobContent>> {
    if entry.new_mode.is_none() {
        return Ok(None);
    }
    if entry.new_mode == Some(0o160000) {
        // A gitlink's content never comes from reading the path (it's a
        // directory): it is the recorded commit - the entry's oid, or for a
        // worktree comparison (where changed-path oids are unresolved) the
        // submodule's live HEAD, falling back to the old side's oid.
        let oid = match entry.new_oid {
            Some(oid) => Some(oid),
            None => match (use_worktree, worktree_root) {
                (true, Some(root)) => {
                    let sub_root = root.join(repo_path_to_path(&entry.path));
                    sley_diff_merge::gitlink_head_oid(&sub_root, db.object_format())
                        .or(entry.old_oid)
                }
                _ => entry.old_oid,
            },
        };
        return Ok(oid.map(|oid| DiffBlobContent::Owned(gitlink_diff_content(&oid, false))));
    }
    if use_worktree {
        return diff_entry_new_content(entry, db, worktree_root, true, worktree_clean, lazy_fetch)
            .map(|content| content.map(DiffBlobContent::Owned));
    }
    entry
        .new_oid
        .as_ref()
        .map(|oid| read_blob_content(db, oid, lazy_fetch))
        .transpose()
}

pub(crate) fn diff_entry_old_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    lazy_fetch: bool,
) -> Result<Option<Vec<u8>>> {
    if entry.old_mode == Some(0o160000) {
        return Ok(entry
            .old_oid
            .as_ref()
            .map(|oid| gitlink_diff_content(oid, false)));
    }
    entry
        .old_oid
        .as_ref()
        .map(|oid| read_blob(db, oid, lazy_fetch))
        .transpose()
}

pub(crate) fn diff_entry_new_content(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: bool,
) -> Result<Option<Vec<u8>>> {
    if entry.new_mode.is_none() {
        return Ok(None);
    }
    if entry.new_mode == Some(0o160000) {
        // A gitlink's content never comes from reading the path (it's a
        // directory): it is the recorded commit — the entry's oid, or for a
        // worktree comparison (where changed-path oids are unresolved) the
        // submodule's live HEAD, falling back to the old side's oid.
        let oid = match entry.new_oid {
            Some(oid) => Some(oid),
            None => match (use_worktree, worktree_root) {
                (true, Some(root)) => {
                    let sub_root = root.join(repo_path_to_path(&entry.path));
                    sley_diff_merge::gitlink_head_oid(&sub_root, db.object_format())
                        .or(entry.old_oid)
                }
                _ => entry.old_oid,
            },
        };
        return Ok(oid.map(|oid| gitlink_diff_content(&oid, false)));
    }
    if use_worktree {
        let root = worktree_root.ok_or_else(|| {
            GitError::Command("diff numstat requires a worktree for worktree comparisons".into())
        })?;
        let path = root.join(repo_path_to_path(&entry.path));
        // A worktree symlink's "content" is its target path bytes (git's
        // `diff_populate_filespec` uses `strbuf_readlink`), NOT the bytes of the
        // file it points at — so never dereference it with `fs::read`.
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Ok(Some(sley_diff_merge::symlink_target_bytes(&path)?));
            }
            Ok(_) => {
                let content = fs::read(path)?;
                return match worktree_clean {
                    Some(clean) => {
                        // Honour has_crlf_in_index so text=auto does not strip
                        // CRLF when the recorded (old/index) blob already has
                        // CRLF — otherwise unstaged diffs show mixed endings
                        // (`-a\r` / `+b`) and break apply round-trips (t4124).
                        let index_blob = match entry.old_oid {
                            Some(oid) => sley_worktree::SafeCrlfIndexBlob::Lookup { odb: db, oid },
                            None => sley_worktree::SafeCrlfIndexBlob::None,
                        };
                        clean
                            .attributes
                            .apply_clean_filter_respecting_index(
                                clean.config,
                                &entry.path,
                                &content,
                                index_blob,
                            )
                            .map(Some)
                    }
                    None => Ok(Some(content)),
                };
            }
            Err(_) => return Ok(None),
        }
    }
    entry
        .new_oid
        .as_ref()
        .map(|oid| read_blob(db, oid, lazy_fetch))
        .transpose()
}

fn diff_entry_new_gitlink_oid(
    entry: &sley_diff_merge::NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree: bool,
) -> Result<Option<ObjectId>> {
    if entry.new_mode != Some(0o160000) {
        return Ok(None);
    }
    Ok(match entry.new_oid {
        Some(oid) => Some(oid),
        None => match (use_worktree, worktree_root) {
            (true, Some(root)) => {
                let sub_root = root.join(repo_path_to_path(&entry.path));
                sley_diff_merge::gitlink_head_oid(&sub_root, db.object_format()).or(entry.old_oid)
            }
            _ => entry.old_oid,
        },
    })
}

pub(crate) fn validate_diff_rename_limit(value: &str) -> Result<()> {
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

pub(crate) fn diff_rename_limit_requires_integer_error() -> GitError {
    eprintln!("error: switch `l' expects an integer value with an optional k/m/g suffix");
    GitError::Exit(129)
}

fn read_blob_content(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<DiffBlobContent> {
    let object = read_object_maybe_prefetch_promisor(db, oid, lazy_fetch)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "diff expected blob object {oid}"
        )));
    }
    Ok(DiffBlobContent::Object(object))
}

pub(crate) fn read_object_maybe_prefetch_promisor(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<Arc<EncodedObject>> {
    let object = match db.read_object(oid) {
        Ok(object) => object,
        Err(err @ GitError::NotFound(_)) => {
            if !prefetch_local_promisor_object(db, oid, lazy_fetch)? {
                return Err(err);
            }
            db.read_object(oid)?
        }
        Err(err) => return Err(err),
    };
    Ok(object)
}

/// Promisor remotes to consult for a lazy fetch, in Git's
/// `promisor_remote_get_direct` order.
pub(crate) fn promisor_remote_names(config: &GitConfig) -> Vec<String> {
    sley_remote::configured_promisor_remote_names(config)
}

/// Non-gitlink blob OIDs referenced by `entries` (both sides). Unchanged paths
/// never appear in the queue, so same-OID skips (t4067 #3) fall out naturally.
pub(crate) fn collect_diff_entry_blob_oids(
    entries: &[sley_diff_merge::NameStatusEntry],
) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    let mut oids = Vec::new();
    for entry in entries {
        if entry.old_mode != Some(0o160000)
            && let Some(oid) = entry.old_oid
            && seen.insert(oid)
        {
            oids.push(oid);
        }
        if entry.new_mode != Some(0o160000)
            && let Some(oid) = entry.new_oid
            && seen.insert(oid)
        {
            oids.push(oid);
        }
    }
    oids
}

/// Batch-prefetch every missing blob referenced by the queued diff entries.
/// Mirrors git's `diff_queued_diff_prefetch` + `promisor_remote_get_direct`.
pub(crate) fn prefetch_diff_entry_blobs(
    db: &FileObjectDatabase,
    entries: &[sley_diff_merge::NameStatusEntry],
    lazy_fetch: bool,
) -> Result<()> {
    let oids = collect_diff_entry_blob_oids(entries);
    prefetch_promisor_objects(db, &oids, lazy_fetch)
}

/// Materialize the missing subset of `oids` in one request per configured
/// local/file promisor. Packet-trace identity is `fetch` for the duration of
/// each negotiation so `GIT_TRACE_PACKET` matches git's child-fetch process
/// (t4067, t1022).
pub(crate) fn prefetch_promisor_objects(
    db: &FileObjectDatabase,
    oids: &[ObjectId],
    lazy_fetch: bool,
) -> Result<()> {
    if !lazy_fetch || oids.is_empty() {
        return Ok(());
    }

    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    for oid in oids {
        if seen.insert(*oid) && !db.contains(oid)? {
            missing.push(*oid);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    let Some(git_dir) = database_git_dir(db) else {
        return Ok(());
    };
    let Ok(config) = read_repo_config(&git_dir) else {
        return Ok(());
    };
    let resolution_cwd =
        sley_worktree::worktree_root_for_git_dir(&git_dir)?.unwrap_or_else(|| git_dir.clone());

    // In-process upload-pack reuses this process's packet-trace identity; git's
    // promisor path forks `git fetch`, so traces show `fetch> done`. Match that.
    let _trace_identity = sley_protocol::scoped_packet_trace_identity("fetch");

    for remote_name in promisor_remote_names(&config) {
        if missing.is_empty() {
            break;
        }
        // Custom upload-pack is an arbitrary shell protocol; leave those to the
        // single-object fallback rather than inventing a stdin protocol here.
        if config
            .get("remote", Some(&remote_name), "uploadpack")
            .is_some()
        {
            continue;
        }
        let Some(url) = config.get("remote", Some(&remote_name), "url") else {
            continue;
        };
        let resolution = sley_remote::RemoteResolutionContext {
            cwd: &resolution_cwd,
            local_git_dir: Some(&git_dir),
            config: Some(&config),
        };
        let filter = config
            .get("remote", Some(&remote_name), "partialclonefilter")
            .and_then(sley_remote::pack_filter_from_spec)
            .or(Some(sley_odb::PackObjectFilter::BlobNone));
        let quiet = config.get_bool("promisor", None, "quiet").unwrap_or(false);
        trace2_promisor_fetch_child_start(&remote_name, quiet);
        let hydrated_ok = if let Ok(remote_git_dir) =
            sley_remote::resolve_local_remote_git_dir(resolution, url)
        {
            sley_remote::install_fetch_pack_via_local_upload_pack(
                &git_dir,
                &remote_git_dir,
                db.object_format(),
                missing.clone(),
                None,
                true,
                false,
                filter,
                None,
                false,
                None,
            )
            .is_ok()
        } else if sley_remote::remote_url_is_http(url).unwrap_or(false) {
            // Smart-HTTP promisor hydrate (t0410 #39): exact-want, no haves.
            let mut any = false;
            for oid in &missing {
                if hydrate_promisor_oid_via_http(
                    &git_dir,
                    db.object_format(),
                    url,
                    *oid,
                    filter.clone(),
                )
                .is_ok()
                {
                    any = true;
                }
            }
            any
        } else {
            false
        };
        if !hydrated_ok {
            continue;
        }

        db.refresh_read_cache();
        let before = missing.len();
        let mut still_missing = Vec::with_capacity(before);
        for oid in missing {
            if !db.contains(&oid)? {
                still_missing.push(oid);
            }
        }
        missing = still_missing;
        let hydrated = before - missing.len();
        if hydrated > 0 {
            sley_core::trace2::data("promisor", "fetch_count", hydrated as u64);
            sley_core::trace2::data("pack-objects", "written", hydrated as u64);
        }
    }
    Ok(())
}

/// Lazy-fetch one missing object from a smart-HTTP promisor remote.
///
/// Mirrors git's `promisor_remote_get_direct` over HTTP: exact-want, no haves,
/// installed as a promisor pack so subsequent fsck/rev-list still treat the
/// transfer as partial.
fn hydrate_promisor_oid_via_http(
    git_dir: &Path,
    format: ObjectFormat,
    url: &str,
    oid: ObjectId,
    _filter: Option<sley_odb::PackObjectFilter>,
) -> Result<()> {
    let remote = sley_transport::parse_remote_url(url)?;
    if !matches!(
        remote.transport,
        sley_transport::RemoteTransport::Http | sley_transport::RemoteTransport::Https
    ) {
        return Err(GitError::Unsupported(
            "promisor HTTP hydrate requires HTTP(S)".into(),
        ));
    }
    let client = sley_remote::new_http_client();
    let mut credentials = sley_remote::CredentialHelperProvider::new(None);
    let discovered = sley_remote::http_service_advertisements(
        &client,
        &remote,
        format,
        sley_protocol::GitService::UploadPack,
        &mut credentials,
        None,
    )?;
    let pack_request = sley_remote::HttpFetchPackRequest {
        client: &client,
        git_dir,
        format,
        remote: &remote,
        wants: vec![oid],
        haves: None,
        shallow: Vec::new(),
        deepen: None,
        promisor: true,
        max_input_size: None,
        // Omit a partial-clone filter on the wire: many HTTP remotes (including
        // t0410's plain smart-HTTP fixture) have not set `uploadpack.allowfilter`,
        // and exact-object hydration only needs the named wants (t0410 #39).
        // Local promisor fetches keep their blob:none filter separately.
        filter: None,
        deepen_since: None,
        deepen_not: Vec::new(),
        deepen_relative: false,
        git_protocol: Some("version=2"),
        post_buffer: 1 << 20,
        omit_haves: true,
    };
    let mut progress = sley_remote::SilentProgress;
    if let Some(handshake) = discovered.handshake.as_ref() {
        sley_remote::install_fetch_pack_via_http_protocol_v2_fetch(
            pack_request,
            handshake,
            &mut credentials,
            &mut progress,
            sley_core::CancelFlag::never(),
        )?;
    } else {
        sley_remote::install_fetch_pack_via_http_upload_pack(
            pack_request,
            &mut credentials,
            &mut progress,
            sley_core::CancelFlag::never(),
        )?;
    }
    Ok(())
}

fn prefetch_local_promisor_object(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<bool> {
    if !lazy_fetch {
        return Ok(false);
    }
    // Prefer the batched path when a single oid is requested so packet identity
    // and fetch_count accounting stay consistent with multi-oid callers.
    let before = db.contains(oid).unwrap_or(false);
    if before {
        return Ok(false);
    }
    prefetch_promisor_objects(db, &[*oid], true)?;
    if db.contains(oid).unwrap_or(false) {
        return Ok(true);
    }
    // Fallback: custom remote.<name>.uploadpack (not handled by the batch path).
    let Some(git_dir) = database_git_dir(db) else {
        return Ok(false);
    };
    let Ok(config) = read_repo_config(&git_dir) else {
        return Ok(false);
    };
    for remote_name in promisor_remote_names(&config) {
        let Some(url) = config.get("remote", Some(&remote_name), "url") else {
            continue;
        };
        let quiet = config.get_bool("promisor", None, "quiet").unwrap_or(false);
        if let Some(command) = config.get("remote", Some(&remote_name), "uploadpack") {
            trace2_promisor_fetch_child_start(&remote_name, quiet);
            let _ = prefetch_via_configured_upload_pack(command, url)?;
            db.refresh_read_cache();
            if db.contains(oid).unwrap_or(false) {
                return Ok(true);
            }
            return Ok(false);
        }
    }
    Ok(false)
}

fn trace2_promisor_fetch_child_start(remote_name: &str, quiet: bool) {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let mut argv = vec![
        "git",
        "-c",
        "fetch.negotiationAlgorithm=noop",
        "fetch",
        remote_name,
        "--no-tags",
        "--no-write-fetch-head",
        "--recurse-submodules=no",
        "--stdin",
    ];
    if quiet {
        argv.push("--quiet");
    }
    let argv = argv
        .iter()
        .map(|arg| format!("\"{}\"", trace2_json_escape(arg)))
        .collect::<Vec<_>>()
        .join(",");
    let line = format!(
        "{{\"event\":\"child_start\",\"sid\":\"sley\",\"child_id\":0,\"argv\":[{argv}]}}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn trace2_json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn prefetch_via_configured_upload_pack(command: &str, repository: &str) -> Result<bool> {
    let command = format!("{command} {}", sley_config::sq_quote(repository));
    let output = ProcessCommand::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .output()?;
    io::stderr().write_all(&output.stderr)?;
    Ok(output.status.success())
}

pub(crate) fn read_blob(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<Vec<u8>> {
    match read_blob_content(db, oid, lazy_fetch)? {
        DiffBlobContent::Owned(bytes) => Ok(bytes),
        DiffBlobContent::Object(object) => match Arc::try_unwrap(object) {
            Ok(object) => Ok(object.body),
            Err(object) => Ok(object.body.clone()),
        },
    }
}

pub(crate) fn repo_path_to_path(path: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(path).into_owned())
}

pub(crate) fn diff_line_stats(old: Option<&[u8]>, new: Option<&[u8]>) -> DiffLineStats {
    if old.is_some_and(is_binary_content) || new.is_some_and(is_binary_content) {
        return DiffLineStats::Binary {
            old_size: old.map_or(0, <[u8]>::len),
            new_size: new.map_or(0, <[u8]>::len),
            unchanged: old == new,
        };
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

pub(crate) fn is_binary_content(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn is_binary_or_large_content(bytes: &[u8], big_file_threshold: u64) -> bool {
    bytes.len() as u64 >= big_file_threshold || is_binary_content(bytes)
}

/// `--stat` insertion/deletion line counts, computed by the shared diff-merge
/// Myers engine rather than a CLI-local LCS.
///
/// Myers produces a shortest edit script, so the count of `Insert` lines is
/// `new_len - lcs` and the count of `Delete` lines is `old_len - lcs` — exactly
/// the values the removed local LCS counter returned.
pub(crate) fn count_line_diff(old: &[u8], new: &[u8]) -> (usize, usize) {
    let old_lines = sley_diff_merge::split_lines(old);
    let new_lines = sley_diff_merge::split_lines(new);
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > prefix && new_end > prefix && old_lines[old_end - 1] == new_lines[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    let old_middle = &old_lines[prefix..old_end];
    let new_middle = &new_lines[prefix..new_end];
    if let Some(common) = trivial_lcs_len(old_middle, new_middle) {
        return (new_middle.len() - common, old_middle.len() - common);
    }
    const NO_COMMON_SCAN_MIN_PRODUCT: usize = 1_000_000;
    if old_middle.len().saturating_mul(new_middle.len()) >= NO_COMMON_SCAN_MIN_PRODUCT
        && !diff_lines_have_any_common(old_middle, new_middle)
    {
        return (new_middle.len(), old_middle.len());
    }

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

fn trivial_lcs_len(
    old: &[sley_diff_merge::DiffLine<'_>],
    new: &[sley_diff_merge::DiffLine<'_>],
) -> Option<usize> {
    if old.is_empty() || new.is_empty() {
        return Some(0);
    }
    if old.len() == 1 {
        return Some(usize::from(new.iter().any(|line| *line == old[0])));
    }
    if new.len() == 1 {
        return Some(usize::from(old.iter().any(|line| *line == new[0])));
    }
    None
}

fn diff_lines_have_any_common(
    old: &[sley_diff_merge::DiffLine<'_>],
    new: &[sley_diff_merge::DiffLine<'_>],
) -> bool {
    let (small, large) = if old.len() <= new.len() {
        (old, new)
    } else {
        (new, old)
    };
    let mut seen = HashSet::with_capacity(small.len());
    for line in small {
        seen.insert((line.content, line.has_newline));
    }
    large
        .iter()
        .any(|line| seen.contains(&(line.content, line.has_newline)))
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

pub(crate) fn apply_diff_pathspec(
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

pub(crate) fn apply_diff_max_depth(
    entries: Vec<sley_diff_merge::NameStatusEntry>,
    pathspec: &DiffPathspec,
    max_depth: Option<i64>,
) -> Vec<sley_diff_merge::NameStatusEntry> {
    let Some(max_depth) = max_depth else {
        return entries;
    };
    if max_depth < 0 {
        return entries;
    }
    entries
        .into_iter()
        .filter(|entry| pathspec.within_max_depth(&entry.path, max_depth))
        .collect()
}

pub(crate) fn parse_diff_max_depth(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("error: option `max-depth' expects a numerical value");
        GitError::Exit(129)
    })
}

pub(crate) fn reverse_diff_entries(
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

pub(crate) fn reverse_diff_entry(
    entry: sley_diff_merge::NameStatusEntry,
) -> sley_diff_merge::NameStatusEntry {
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
        // A reversed typechange is still a typechange (the `S_IFMT` bits still
        // differ once the two sides are swapped), so it keeps its status and just
        // flips the mode/oid pair like a modify.
        sley_diff_merge::NameStatus::Modified | sley_diff_merge::NameStatus::TypeChanged => {
            sley_diff_merge::NameStatusEntry {
                old_mode: entry.new_mode,
                new_mode: entry.old_mode,
                old_oid: entry.new_oid,
                new_oid: entry.old_oid,
                ..entry
            }
        }
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
        // An unmerged marker has no directional content to flip.
        sley_diff_merge::NameStatus::Unmerged => entry,
    }
}

#[derive(Default)]
pub(crate) struct DiffPathspec {
    filters: Vec<LsFilesPathFilter>,
}

impl DiffPathspec {
    pub(crate) fn new(
        cwd: &Path,
        worktree_root: &Path,
        path_args: &[String],
        magic: sley_worktree::PathspecMatchMagic,
    ) -> Result<Self> {
        let root = fs::canonicalize(worktree_root)?;
        let cwd = fs::canonicalize(cwd)?;
        let relative = cwd.strip_prefix(&root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", cwd.display()))
        })?;
        let prefix = relative.to_string_lossy().replace('\\', "/").into_bytes();
        let mut filters = Vec::new();
        for arg in path_args {
            let parse_arg = normalize_absolute_cli_pathspec(&root, &cwd, arg)?;
            let element = parse_normalized_pathspec_element(&prefix, &parse_arg, magic)?;
            let arg_path = Path::new(arg);
            let absolute = if arg_path.is_absolute() {
                arg_path.to_path_buf()
            } else {
                cwd.join(arg_path)
            };
            let recursive = arg == "." || arg.ends_with('/') || absolute.is_dir();
            filters.push(LsFilesPathFilter {
                original: arg.clone(),
                recursive,
                is_glob: !element.magic().literal
                    && sley_worktree::pathspec_is_glob(element.pattern()),
                element,
                matched: Cell::new(false),
            });
        }
        Ok(Self { filters })
    }

    pub(crate) fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return true;
        }
        pathspec_filters_match(&self.filters, path)
    }

    pub(crate) fn within_max_depth(&self, path: &[u8], max_depth: i64) -> bool {
        self.relative_depth(path)
            .is_some_and(|depth| depth <= max_depth)
    }

    fn relative_depth(&self, path: &[u8]) -> Option<i64> {
        if self.filters.is_empty() {
            return Some(diff_path_slash_depth(path));
        }
        let mut saw_include = false;
        let mut best: Option<i64> = None;
        for filter in &self.filters {
            if filter.is_exclude() {
                continue;
            }
            saw_include = true;
            if let Some(depth) = diff_relative_depth_from_spec(filter.element.pattern(), path) {
                best = Some(best.map_or(depth, |current| current.min(depth)));
            }
        }
        if saw_include {
            best
        } else {
            Some(diff_path_slash_depth(path))
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }
}

fn diff_relative_depth_from_spec(spec: &[u8], path: &[u8]) -> Option<i64> {
    let spec = spec.strip_suffix(b"/").unwrap_or(spec);
    if spec.is_empty() || spec == b"." {
        return Some(diff_path_slash_depth(path));
    }
    if path == spec {
        return Some(0);
    }
    if path.len() > spec.len() && path.starts_with(spec) && path.get(spec.len()) == Some(&b'/') {
        return Some(diff_path_component_count(&path[spec.len() + 1..]));
    }
    None
}

fn diff_path_slash_depth(path: &[u8]) -> i64 {
    path.iter().filter(|byte| **byte == b'/').count() as i64
}

fn diff_path_component_count(path: &[u8]) -> i64 {
    if path.is_empty() {
        0
    } else {
        diff_path_slash_depth(path) + 1
    }
}
