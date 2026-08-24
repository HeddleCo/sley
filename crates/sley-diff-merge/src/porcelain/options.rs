//! Option, context, and seam types for the entry-level diff renderers.
//!
//! Hosts inject repository-bound capabilities (userdiff drivers, promisor
//! lazy fetch, submodule patch rendering, worktree clean filters) through the
//! callback traits in this module so the engine stays dependency-free.

use crate::format::{CompiledFuncname, DiffColors, WordDiffMode};
use crate::render::{ColorMoved, LineIndicators, LineRange, WsErrorHighlight};
use crate::{DiffAlgorithm, NameStatusEntry, WsIgnore};
use sley_config::GitConfig;
use sley_core::{ObjectFormat, ObjectId, Result};
use sley_odb::FileObjectDatabase;
use std::collections::HashMap;
use std::path::Path;

/// Whether a submodule's changes are omitted from diff output
/// (`submodule.<name>.ignore` / `--ignore-submodules`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleIgnoreMode {
    None,
    Untracked,
    Dirty,
    All,
}

pub fn parse_submodule_ignore_mode(value: &str) -> Option<SubmoduleIgnoreMode> {
    match value {
        "none" => Some(SubmoduleIgnoreMode::None),
        "untracked" => Some(SubmoduleIgnoreMode::Untracked),
        "dirty" => Some(SubmoduleIgnoreMode::Dirty),
        "all" => Some(SubmoduleIgnoreMode::All),
        _ => None,
    }
}

/// Already-resolved width policy for a diffstat table (`--stat=<w>` and the
/// `diff.statGraphWidth` / `diff.statNameWidth` config family).
#[derive(Debug, Clone, Copy)]
pub struct DiffStatWidths {
    pub stat_width: i64,
    pub name_width: i64,
    pub graph_width: i64,
    pub line_prefix_width: i64,
}

impl DiffStatWidths {
    pub fn terminal() -> Self {
        Self {
            stat_width: -1,
            name_width: -1,
            graph_width: -1,
            line_prefix_width: 0,
        }
    }

    pub fn plumbing() -> Self {
        Self {
            stat_width: 0,
            name_width: 0,
            graph_width: 0,
            line_prefix_width: 0,
        }
    }

    pub fn resolve_config(&mut self, config: &GitConfig) {
        if self.name_width == -1 {
            self.name_width = config
                .get("diff", None, "statnamewidth")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
        }
        if self.graph_width == -1 {
            self.graph_width = config
                .get("diff", None, "statgraphwidth")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
        }
    }

    pub fn resolve_config_defaults(&mut self) {
        if self.name_width == -1 {
            self.name_width = 0;
        }
        if self.graph_width == -1 {
            self.graph_width = 0;
        }
    }
}

/// Aggregation mode for `--dirstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirstatMode {
    Changes,
    Lines,
    Files,
}

#[derive(Debug, Clone, Copy)]
pub struct DirstatOptions {
    pub mode: DirstatMode,
    pub cumulative: bool,
    pub permille: i64,
}

impl Default for DirstatOptions {
    fn default() -> Self {
        Self {
            mode: DirstatMode::Changes,
            cumulative: false,
            permille: 30,
        }
    }
}

/// How a gitlink (submodule) difference is rendered in patch output
/// (`diff.submodule`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleDiffFormat {
    Short,
    Log,
    Diff,
}

impl SubmoduleDiffFormat {
    pub fn parse(value: &str) -> Self {
        match value {
            "short" => Self::Short,
            "diff" => Self::Diff,
            _ => Self::Log,
        }
    }
}

/// The userdiff capability a patch renderer needs from its host: per-path
/// driver resolution plus the process-spawning textconv helper.
pub trait PatchUserdiff {
    /// Resolve the driver for `path`, or `None` when the `diff` attribute is
    /// unspecified.
    ///
    /// # Errors
    /// Propagates fatal attribute/driver pattern errors.
    fn patch_driver_for_path(&self, path: &[u8]) -> Result<Option<PatchDriver>>;

    /// The `diff.wordRegex` config fallback.
    fn patch_config_word_regex(&self) -> Option<Vec<u8>>;

    /// Run a `diff.<driver>.textconv` helper; `None` when the helper fails or
    /// produces nothing.
    ///
    /// # Errors
    /// Propagates spawn failures.
    fn patch_run_textconv(&self, command: &str, content: &[u8]) -> Result<Option<Vec<u8>>>;
}

/// The subset of a resolved userdiff driver consumed by patch rendering.
#[derive(Clone, Default)]
pub struct PatchDriver {
    pub funcname: Option<CompiledFuncname>,
    pub word_regex: Option<Vec<u8>>,
    /// `Some(true)` = `-diff`; `Some(false)` = binary forced off; `None` =
    /// auto-detect.
    pub binary: Option<bool>,
    pub textconv: Option<String>,
}

/// Renders gitlink (submodule) entries whose format is not `Short`. The host
/// supplies this because `log`/`diff` submodule rendering needs history and
/// worktree access the engine does not own.
pub trait SubmodulePatchRender {
    ///
    /// # Errors
    /// Propagates write/subprocess errors.
    fn write_submodule_patch(
        &self,
        out: &mut dyn std::io::Write,
        entry: &NameStatusEntry,
        options: &DiffRenderOptions<'_>,
    ) -> Result<()>;
}

/// Lazy-object (promisor) fetch seam. A host with partial-clone support
/// implements both batch prefetch and single-object read; hosts without one
/// pass `None` wherever an `Option<&dyn LazyObjectFetch>` is expected, which
/// reproduces `lazy_fetch = false`.
pub trait LazyObjectFetch {
    ///
    /// # Errors
    /// Propagates fetch/read errors.
    fn read_object_maybe_prefetch(
        &self,
        db: &FileObjectDatabase,
        oid: &ObjectId,
    ) -> Result<std::sync::Arc<sley_object::EncodedObject>>;

    ///
    /// # Errors
    /// Propagates fetch errors.
    fn prefetch_entry_blobs(
        &self,
        db: &FileObjectDatabase,
        entries: &[NameStatusEntry],
        new_side_is_worktree: bool,
    ) -> Result<()>;
}

/// Applies the worktree clean/CRLF filter to freshly-read worktree bytes.
///
/// Arguments: `(path, content, index-blob oid)`; the index oid drives
/// `has_crlf_in_index` so `text=auto` does not strip CRLF when the recorded
/// blob already has CRLF. Returns the cleaned bytes.
pub type CleanFilterApply<'a> =
    &'a (dyn Fn(&[u8], &[u8], Option<&ObjectId>) -> Result<Vec<u8>> + 'a);

/// Worktree-side clean-filter context for diffs that read the worktree.
///
/// The host closes over its config/attributes/database handles; the engine
/// only ever invokes [`DiffWorktreeCleanContext::apply`].
#[derive(Clone, Copy)]
pub struct DiffWorktreeCleanContext<'a> {
    pub apply_clean: CleanFilterApply<'a>,
}

impl DiffWorktreeCleanContext<'_> {
    /// Apply the clean filter to `content` read from `path`.
    ///
    /// # Errors
    /// Propagates host filter errors.
    pub fn apply(
        &self,
        path: &[u8],
        content: &[u8],
        index_blob: Option<&ObjectId>,
    ) -> Result<Vec<u8>> {
        (self.apply_clean)(path, content, index_blob)
    }
}

/// A `--word-diff` request before per-file word-regex resolution.
pub struct WordDiffRequest<'a> {
    pub mode: WordDiffMode,
    /// `--word-diff-regex` / `--color-words=<re>` override.
    pub cli_regex: Option<&'a str>,
}

/// Preloaded contents for a `diff --no-index` entry (old side, new side).
pub type NoIndexContents<'a> = Option<(Option<&'a [u8]>, Option<&'a [u8]>)>;

/// Full option set for rendering one entry as a unified patch.
#[derive(Clone, Copy)]
pub struct DiffRenderOptions<'a> {
    pub db: &'a FileObjectDatabase,
    /// Lazy-object fetch hook; `None` disables promisor prefetching.
    pub lazy_fetch: Option<&'a dyn LazyObjectFetch>,
    pub worktree_root: Option<&'a Path>,
    pub use_worktree_new: bool,
    pub format: ObjectFormat,
    pub abbrev: usize,
    pub src_prefix: &'a str,
    pub dst_prefix: &'a str,
    /// Lines of hunk context (`-U<n>`); the porcelain default is 3.
    pub context: usize,
    /// Userdiff driver resolution (`diff=<driver>` attributes + config);
    /// `None` keeps the default funcname heuristic.
    pub userdiff: Option<&'a dyn PatchUserdiff>,
    /// Explicit function-name heading pattern for `@@ @@` section headers.
    /// `None` falls back to `userdiff` resolution or the built-in default
    /// funcname resolver.
    pub funcname: Option<&'a CompiledFuncname>,
    /// ANSI palette when color output is enabled.
    pub colors: Option<&'a DiffColors>,
    /// Word-diff rendering request (mode + the command-line regex override).
    pub word_diff: Option<&'a WordDiffRequest<'a>>,
    /// Hunk body line indicators (` `, `-`, `+` by default).
    pub line_indicators: LineIndicators,
    /// Omit the leading context marker on an otherwise-empty context line.
    pub suppress_blank_empty: bool,
    /// Preloaded file contents for `diff --no-index` (old, new), bypassing
    /// the object database / worktree reads.
    pub no_index_contents: NoIndexContents<'a>,
    /// Requested gitlink renderer. `Short` is the synthetic one-line patch;
    /// `Log` and `Diff` use submodule-native history and tree diff rendering
    /// through [`DiffRenderOptions::submodule_render`].
    pub submodule_format: SubmoduleDiffFormat,
    /// Gitlink paths whose worktree-side submodule dirt is visible after
    /// `--ignore-submodules` filtering. The bitmask uses the
    /// `DIRTY_SUBMODULE_*` bits.
    pub submodule_dirt: Option<&'a HashMap<Vec<u8>, u8>>,
    /// Whitespace-error highlighting (`--ws-error-highlight` /
    /// `diff.wsErrorHighlight`) when color is enabled. `None` disables it.
    pub ws_error: Option<WsErrorHighlight>,
    /// Moved-code coloring (`--color-moved`) when color is enabled and
    /// word-diff is disabled. `None` disables it.
    pub color_moved: Option<ColorMoved>,
    /// Extra inter-hunk merge distance (`--inter-hunk-context`).
    pub interhunk: usize,
    /// Whitespace-ignore flags (`-w`, `-b`, `--ignore-space-at-eol`,
    /// `--ignore-cr-at-eol`) applied to the line comparison.
    pub ws_ignore: WsIgnore,
    /// The line-diff algorithm (`--patience` / `--histogram` / default Myers).
    pub diff_algorithm: DiffAlgorithm,
    /// `--ignore-blank-lines`: drop change groups whose lines are all blank.
    pub ignore_blank_lines: bool,
    /// `-I<regex>` / `--ignore-matching-lines`: drop change groups all of whose
    /// lines match one of these (compiled ERE) regexes.
    pub ignore_regexes: &'a [sley_grep::Regex],
    /// `log -L`: restrict the emitted hunks to these post-image line ranges.
    /// `None` (every non-line-log caller) renders the full patch.
    pub line_ranges: Option<&'a [LineRange]>,
    /// `--indent-heuristic` / `diff.indentHeuristic`: shift slidable change
    /// groups to the most readable boundary. Enabled by default, matching git.
    pub indent_heuristic: bool,
    /// `--binary`: emit an applicable `GIT binary patch` block (literal-encoded,
    /// full index) for binary files instead of `Binary files … differ`.
    pub binary: bool,
    /// `--anchored=<text>` prefixes (git's patience anchors). Only consulted when
    /// `diff_algorithm` is patience; empty (the default) is plain patience.
    pub anchors: &'a [Vec<u8>],
    /// git's `DIFF_OPT_ALLOW_TEXTCONV`: when set, a regular-file side whose diff
    /// driver defines `diff.<d>.textconv` is converted to its text representation
    /// before binary detection and diffing. Enabled for porcelain patch output
    /// (`git diff`/`show`/`log -p`/`status -v`); off for plumbing (`diff-tree`,
    /// `diff-index`, `diff-files`) and patch generation (`format-patch`).
    pub allow_textconv: bool,
    /// Resolved `core.bigfilethreshold`: content at or above this size is
    /// treated as binary regardless of NUL detection.
    pub big_file_threshold: u64,
    /// Gitlink renderer for non-`Short` `submodule_format`s. Required whenever
    /// `submodule_format` may be `Log`/`Diff`; when absent those entries fall
    /// back to the synthetic `Subproject commit` body.
    pub submodule_render: Option<&'a dyn SubmodulePatchRender>,
}

/// Which output families `render_diff_entries` should emit.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiffEntryRenderModes {
    pub raw: bool,
    pub numstat: bool,
    pub stat: bool,
    pub shortstat: bool,
    pub summary: bool,
    pub patch: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DiffEntryRawRenderOptions {
    pub z: bool,
    pub abbrev: Option<usize>,
    pub format: ObjectFormat,
}

/// Pre-materialized line statistics feeding numstat/stat/shortstat output.
pub enum DiffEntryStatSource<'a> {
    Materialized(&'a [crate::porcelain::StatEntry<'a>]),
}

pub struct DiffEntryStatRenderOptions<'a> {
    pub source: Option<DiffEntryStatSource<'a>>,
    pub z: bool,
    pub options: crate::porcelain::StatOptions,
    pub widths: Option<DiffStatWidths>,
    pub config: Option<&'a GitConfig>,
}

/// Post-stat writer hook (e.g. a dirstat section emitted between the stat
/// family and the summary/patch families).
pub type AfterStatCallback<'a> = &'a mut (dyn FnMut(&mut dyn std::io::Write) -> Result<()> + 'a);

pub struct DiffEntryRenderContext<'a> {
    pub raw: DiffEntryRawRenderOptions,
    pub stat: DiffEntryStatRenderOptions<'a>,
    /// Host display-width/terminal services used by the diffstat writer.
    pub services: &'a dyn crate::porcelain::RenderServices,
    pub after_stat: Option<AfterStatCallback<'a>>,
    pub prefix_already_written: bool,
}
