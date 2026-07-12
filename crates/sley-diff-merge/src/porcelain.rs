//! Repository-independent rendering for Git's raw, summary, numstat, and
//! diffstat output families.
//!
//! The diff engine owns the byte layout and row-planning semantics. Callers
//! inject terminal display-width measurement while the engine retains Git's
//! byte-level path quoting rules.

use crate::render::{
    CombinedRenderOptions as CombinedHunkOptions, HunkRenderOptions, render_combined_with,
    render_hunks,
};
use crate::{DiffAlgorithm, NameStatus, NameStatusEntry, WsIgnore};
use sley_core::{ObjectFormat, ObjectId};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

/// Default output family when the caller did not request an explicit format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultDiffOutput {
    /// Porcelain `diff` defaults to a patch.
    Patch,
    /// Plumbing `diff-index` defaults to raw records.
    Raw,
}

/// Requested output families before Git's precedence rules are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderSelectionOptions {
    /// Command-specific format used when no explicit family is requested.
    pub default_output: DefaultDiffOutput,
    /// Request raw records.
    pub raw: bool,
    /// Request patch output.
    pub patch: bool,
    /// Request status letters and paths.
    pub name_status: bool,
    /// Request paths only.
    pub name_only: bool,
    /// Request a diffstat table.
    pub stat: bool,
    /// Request numeric line statistics.
    pub numstat: bool,
    /// Request the aggregate short-stat line.
    pub shortstat: bool,
    /// Request create/delete/rename summary records.
    pub summary: bool,
    /// Another explicit format, such as dirstat, suppresses the default family.
    pub auxiliary_format: bool,
    /// `--no-patch` / an explicit no-output selection.
    pub suppress_output: bool,
}

/// Resolved output-family plan shared by porcelain and plumbing commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderSelection {
    /// Emit raw records.
    pub raw: bool,
    /// Emit patch output.
    pub patch: bool,
    /// Emit status letters and paths.
    pub name_status: bool,
    /// Emit paths only.
    pub name_only: bool,
    /// Emit a diffstat table.
    pub stat: bool,
    /// Emit numeric line statistics.
    pub numstat: bool,
    /// Emit the aggregate short-stat line.
    pub shortstat: bool,
    /// Emit create/delete/rename summary records.
    pub summary: bool,
}

impl RenderSelection {
    /// Whether line statistics must be materialized before rendering.
    #[must_use]
    pub const fn needs_line_stats(self) -> bool {
        self.stat || self.numstat || self.shortstat
    }

    /// Whether Git inserts a blank line before a following patch body.
    #[must_use]
    pub const fn separates_patch_from_prefix(self) -> bool {
        self.patch && (self.raw || self.stat || self.numstat || self.shortstat || self.summary)
    }
}

/// Resolve requested diff formats using Git's output-family precedence.
///
/// Name-only and name-status are exclusive presentation families and suppress
/// raw/stat/summary/patch output. Otherwise the command-specific default is
/// selected only when no explicit format (including an auxiliary family) was
/// requested. An explicit no-output selection suppresses the default and patch.
#[must_use]
pub fn select_render_formats(options: RenderSelectionOptions) -> RenderSelection {
    if options.name_only || options.name_status {
        return RenderSelection {
            name_status: options.name_status,
            name_only: options.name_only,
            ..RenderSelection::default()
        };
    }
    let has_explicit_format = options.raw
        || options.patch
        || options.stat
        || options.numstat
        || options.shortstat
        || options.summary
        || options.auxiliary_format;
    let use_default = !has_explicit_format && !options.suppress_output;
    RenderSelection {
        raw: options.raw || (use_default && options.default_output == DefaultDiffOutput::Raw),
        patch: !options.suppress_output
            && (options.patch
                || (use_default && options.default_output == DefaultDiffOutput::Patch)),
        name_status: false,
        name_only: false,
        stat: options.stat,
        numstat: options.numstat,
        shortstat: options.shortstat,
        summary: options.summary,
    }
}

/// Runtime services used while producing porcelain diff output.
pub trait RenderServices {
    /// Return the terminal display width of a rendered path.
    fn display_width(&self, rendered: &str) -> i64;
}

/// A rendering failure.
#[derive(Debug)]
pub enum RenderError {
    /// Writing the rendered bytes failed.
    Output(io::Error),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output(error) => write!(formatter, "could not write diff output: {error}"),
        }
    }
}

impl Error for RenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Output(error) => Some(error),
        }
    }
}

impl From<io::Error> for RenderError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

/// Observable result of one rendering operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderOutcome {
    /// Number of logical output records written.
    pub records_written: usize,
}

/// One already-resolved side of a direct blob comparison.
#[derive(Debug, Clone, Copy)]
pub struct BlobDiffSide<'a> {
    pub oid: ObjectId,
    pub mode: u32,
    pub path: &'a [u8],
    pub content: &'a [u8],
}

/// Repository-independent rendering controls for a direct blob patch.
#[derive(Debug, Clone, Copy)]
pub struct BlobPatchOptions<'a> {
    pub object_format: ObjectFormat,
    pub full_index: bool,
    pub abbrev: usize,
    pub src_prefix: &'a [u8],
    pub dst_prefix: &'a [u8],
    pub context: usize,
    pub interhunk: usize,
    pub algorithm: DiffAlgorithm,
    pub indent_heuristic: bool,
}

/// Render the complete patch for two resolved blobs, including Git's per-file
/// metadata header and the shared unified-diff hunk body.
pub fn render_blob_patch(
    out: &mut dyn Write,
    old: BlobDiffSide<'_>,
    new: BlobDiffSide<'_>,
    options: BlobPatchOptions<'_>,
) -> Result<RenderOutcome, RenderError> {
    if old.oid == new.oid && old.mode == new.mode {
        return Ok(RenderOutcome::default());
    }

    let old_name = prefixed_quoted_path(options.src_prefix, old.path);
    let new_name = prefixed_quoted_path(options.dst_prefix, new.path);
    writeln!(out, "diff --git {old_name} {new_name}")?;
    if old.mode != new.mode {
        writeln!(out, "old mode {:06o}", old.mode)?;
        writeln!(out, "new mode {:06o}", new.mode)?;
    }
    if old.oid == new.oid {
        return Ok(RenderOutcome { records_written: 1 });
    }
    let width = if options.full_index {
        options.object_format.hex_len()
    } else {
        options.abbrev.min(options.object_format.hex_len())
    };
    let old_hex = old.oid.to_hex();
    let new_hex = new.oid.to_hex();
    write!(out, "index {}..{}", &old_hex[..width], &new_hex[..width])?;
    if old.mode == new.mode {
        write!(out, " {:06o}", old.mode)?;
    }
    writeln!(out)?;
    if blob_is_binary(old.content) || blob_is_binary(new.content) {
        writeln!(out, "Binary files {old_name} and {new_name} differ")?;
        return Ok(RenderOutcome { records_written: 1 });
    }
    writeln!(out, "--- {old_name}")?;
    writeln!(out, "+++ {new_name}")?;

    let mut body = Vec::new();
    render_hunks(
        &mut body,
        Some(old.content),
        Some(new.content),
        &mut HunkRenderOptions {
            context: options.context,
            interhunk: options.interhunk,
            algorithm: options.algorithm,
            indent_heuristic: options.indent_heuristic,
            ..HunkRenderOptions::default()
        },
    );
    out.write_all(&body)?;
    Ok(RenderOutcome { records_written: 1 })
}

fn blob_is_binary(content: &[u8]) -> bool {
    content.iter().take(8000).any(|byte| *byte == 0)
}

fn prefixed_quoted_path(prefix: &[u8], path: &[u8]) -> String {
    let mut full = Vec::with_capacity(prefix.len() + path.len());
    full.extend_from_slice(prefix);
    full.extend_from_slice(path);
    quote_path(&full, true)
}

/// One parent side of an already-resolved combined diff entry.
#[derive(Debug, Clone, Copy)]
pub struct CombinedDiffParent<'a> {
    pub path: &'a [u8],
    pub mode: u32,
    pub oid: Option<ObjectId>,
    pub status: char,
}

/// Repository-independent metadata for one combined diff path.
#[derive(Debug, Clone, Copy)]
pub struct CombinedDiffEntry<'a> {
    pub result_path: &'a [u8],
    pub result_mode: u32,
    pub result_oid: Option<ObjectId>,
    pub parents: &'a [CombinedDiffParent<'a>],
}

/// Byte-rendering controls shared by combined raw and patch output.
#[derive(Debug, Clone, Copy)]
pub struct CombinedFormatOptions<'a> {
    pub object_format: ObjectFormat,
    pub dense: bool,
    pub all_paths: bool,
    pub context: usize,
    pub ws_ignore: WsIgnore,
    pub algorithm: DiffAlgorithm,
    pub src_prefix: &'a [u8],
    pub dst_prefix: &'a [u8],
    pub patch_abbrev: usize,
    pub raw_abbrev: Option<usize>,
    pub print_hash_ellipsis: bool,
}

/// Render one combined `--raw` record, including per-parent paths.
pub fn render_combined_raw_entry(
    out: &mut dyn Write,
    entry: CombinedDiffEntry<'_>,
    options: CombinedFormatOptions<'_>,
    nul_terminated: bool,
) -> Result<RenderOutcome, RenderError> {
    for _ in entry.parents {
        write!(out, ":")?;
    }
    for parent in entry.parents {
        write!(out, "{:06o} ", parent.mode)?;
    }
    write!(out, "{:06o}", entry.result_mode)?;
    for parent in entry.parents {
        write!(
            out,
            " {}",
            raw_oid(
                parent.oid.as_ref(),
                false,
                options.raw_abbrev,
                options.object_format,
                options.print_hash_ellipsis,
            )
        )?;
    }
    write!(
        out,
        " {} ",
        raw_oid(
            entry.result_oid.as_ref(),
            false,
            options.raw_abbrev,
            options.object_format,
            options.print_hash_ellipsis,
        )
    )?;
    for parent in entry.parents {
        write!(out, "{}", parent.status)?;
    }
    if nul_terminated {
        out.write_all(b"\0")?;
        if options.all_paths {
            for parent in entry.parents {
                out.write_all(parent.path)?;
                out.write_all(b"\0")?;
            }
        }
        out.write_all(entry.result_path)?;
        out.write_all(b"\0")?;
    } else {
        write!(out, "\t")?;
        if options.all_paths {
            for parent in entry.parents {
                write!(out, "{}\t", quote_path(parent.path, true))?;
            }
        }
        writeln!(out, "{}", quote_path(entry.result_path, true))?;
    }
    Ok(RenderOutcome { records_written: 1 })
}

/// Render one combined name-status record.
pub fn render_combined_name_status_entry(
    out: &mut dyn Write,
    entry: CombinedDiffEntry<'_>,
    all_paths: bool,
    nul_terminated: bool,
) -> Result<RenderOutcome, RenderError> {
    for parent in entry.parents {
        write!(out, "{}", parent.status)?;
    }
    if nul_terminated {
        out.write_all(b"\0")?;
        if all_paths {
            for parent in entry.parents {
                out.write_all(parent.path)?;
                out.write_all(b"\0")?;
            }
        }
        out.write_all(entry.result_path)?;
        out.write_all(b"\0")?;
    } else {
        write!(out, "\t")?;
        if all_paths {
            for parent in entry.parents {
                write!(out, "{}\t", quote_path(parent.path, true))?;
            }
        }
        writeln!(out, "{}", quote_path(entry.result_path, true))?;
    }
    Ok(RenderOutcome { records_written: 1 })
}

/// Render one complete combined patch from already-resolved side contents.
pub fn render_combined_patch(
    out: &mut dyn Write,
    entry: CombinedDiffEntry<'_>,
    result_content: &[u8],
    parent_contents: &[&[u8]],
    options: CombinedFormatOptions<'_>,
) -> Result<RenderOutcome, RenderError> {
    let mode_differs = entry
        .parents
        .iter()
        .any(|parent| parent.mode != entry.result_mode);
    let mut body = Vec::new();
    let show_hunks = render_combined_with(
        &mut body,
        result_content,
        parent_contents,
        &CombinedHunkOptions {
            dense: options.dense,
            context: options.context,
            algorithm: options.algorithm,
            ws_ignore: options.ws_ignore,
        },
    );
    if !show_hunks && !mode_differs {
        return Ok(RenderOutcome::default());
    }

    let head = if options.dense {
        "diff --cc "
    } else {
        "diff --combined "
    };
    writeln!(out, "{head}{}", quote_path(entry.result_path, true))?;
    write!(out, "index ")?;
    for (index, parent) in entry.parents.iter().enumerate() {
        if index > 0 {
            write!(out, ",")?;
        }
        write!(out, "{}", combined_patch_oid(parent.oid.as_ref(), options))?;
    }
    writeln!(
        out,
        "..{}",
        combined_patch_oid(entry.result_oid.as_ref(), options)
    )?;

    let deleted = entry.result_mode == 0;
    let added = !deleted && entry.parents.iter().all(|parent| parent.status == 'A');
    if mode_differs {
        if added {
            writeln!(out, "new file mode {:06o}", entry.result_mode)?;
        } else {
            if deleted {
                write!(out, "deleted file ")?;
            }
            write!(out, "mode ")?;
            for (index, parent) in entry.parents.iter().enumerate() {
                if index > 0 {
                    write!(out, ",")?;
                }
                write!(out, "{:06o}", parent.mode)?;
            }
            if !deleted {
                write!(out, "..{:06o}", entry.result_mode)?;
            }
            writeln!(out)?;
        }
    }

    if options.all_paths {
        for parent in entry.parents {
            if parent.status == 'A' {
                writeln!(out, "--- /dev/null")?;
            } else {
                writeln!(
                    out,
                    "--- {}",
                    prefixed_quoted_path(options.src_prefix, parent.path)
                )?;
            }
        }
    } else if added {
        writeln!(out, "--- /dev/null")?;
    } else {
        writeln!(
            out,
            "--- {}",
            prefixed_quoted_path(options.src_prefix, entry.result_path)
        )?;
    }
    if deleted {
        writeln!(out, "+++ /dev/null")?;
    } else {
        writeln!(
            out,
            "+++ {}",
            prefixed_quoted_path(options.dst_prefix, entry.result_path)
        )?;
    }
    out.write_all(&body)?;
    Ok(RenderOutcome { records_written: 1 })
}

fn combined_patch_oid(oid: Option<&ObjectId>, options: CombinedFormatOptions<'_>) -> String {
    match oid {
        Some(oid) => {
            let hex = oid.to_hex();
            hex[..options.patch_abbrev.min(hex.len())].to_string()
        }
        None => "0".repeat(options.patch_abbrev.min(options.object_format.hex_len())),
    }
}

/// Options for one `--raw` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawOptions {
    /// Use NUL terminators and unquoted paths.
    pub nul_terminated: bool,
    /// Render the post-image object id as all zeroes (worktree side).
    pub zero_new_oid: bool,
    /// Explicit object-id abbreviation width.
    pub abbrev: Option<usize>,
    /// Repository object format.
    pub object_format: ObjectFormat,
    /// Append the legacy `...` suffix to abbreviated ids.
    pub print_hash_ellipsis: bool,
}

/// Render one Git `--raw` record.
pub fn render_raw_entry(
    out: &mut dyn Write,
    entry: &NameStatusEntry,
    options: RawOptions,
) -> Result<RenderOutcome, RenderError> {
    let old_mode = entry.old_mode.unwrap_or(0);
    let new_mode = entry.new_mode.unwrap_or(0);
    let old_oid = raw_oid(
        entry.old_oid.as_ref(),
        false,
        options.abbrev,
        options.object_format,
        options.print_hash_ellipsis,
    );
    let new_oid = raw_oid(
        entry.new_oid.as_ref(),
        options.zero_new_oid,
        options.abbrev,
        options.object_format,
        options.print_hash_ellipsis,
    );
    write!(
        out,
        ":{old_mode:06o} {new_mode:06o} {old_oid} {new_oid} {}",
        entry.status.label()
    )?;
    if options.nul_terminated {
        out.write_all(b"\0")?;
        if let Some(old_path) = &entry.old_path {
            out.write_all(old_path)?;
            out.write_all(b"\0")?;
        }
        out.write_all(&entry.path)?;
        out.write_all(b"\0")?;
    } else {
        if let Some(old_path) = &entry.old_path {
            write!(out, "\t{}", quote_path(old_path, true))?;
        }
        writeln!(out, "\t{}", quote_path(&entry.path, true))?;
    }
    Ok(RenderOutcome { records_written: 1 })
}

fn raw_oid(
    oid: Option<&ObjectId>,
    zero: bool,
    abbrev: Option<usize>,
    format: ObjectFormat,
    print_hash_ellipsis: bool,
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
    if print_hash_ellipsis && hex.len() < format.hex_len() {
        hex.push_str("...");
    }
    hex
}

/// Render one `--summary` record when the entry has summary output.
pub fn render_summary_entry(
    out: &mut dyn Write,
    entry: &NameStatusEntry,
) -> Result<RenderOutcome, RenderError> {
    let wrote = match entry.status {
        NameStatus::Added => {
            writeln!(
                out,
                " create mode {:06o} {}",
                entry.new_mode.unwrap_or(0),
                quote_path(&entry.path, true)
            )?;
            true
        }
        NameStatus::Deleted => {
            writeln!(
                out,
                " delete mode {:06o} {}",
                entry.old_mode.unwrap_or(0),
                quote_path(&entry.path, true)
            )?;
            true
        }
        NameStatus::Renamed(score) | NameStatus::Copied(score) => {
            if let Some(old_path) = &entry.old_path {
                let operation = if matches!(entry.status, NameStatus::Renamed(_)) {
                    "rename"
                } else {
                    "copy"
                };
                let path = pprint_rename(old_path, &entry.path, true);
                writeln!(out, " {operation} {path} ({score}%)")?;
                true
            } else {
                false
            }
        }
        NameStatus::Modified | NameStatus::TypeChanged => {
            if entry.old_mode != entry.new_mode {
                if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode) {
                    writeln!(
                        out,
                        " mode change {old_mode:06o} => {new_mode:06o} {}",
                        quote_path(&entry.path, true)
                    )?;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        }
        NameStatus::Unmerged => false,
    };
    Ok(RenderOutcome {
        records_written: usize::from(wrote),
    })
}

/// Per-file line statistics shared by numstat, shortstat, and diffstat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStats {
    /// A binary file pair, including pre- and post-image byte sizes.
    Binary {
        old_size: usize,
        new_size: usize,
        unchanged: bool,
    },
    /// A textual file pair.
    Text { inserted: usize, deleted: usize },
}

/// Materialized statistics for one diff entry.
#[derive(Debug, Clone, Copy)]
pub struct StatEntry<'a> {
    pub entry: &'a NameStatusEntry,
    pub stats: LineStats,
}

/// Render one `--numstat` record.
pub fn render_numstat_entry(
    out: &mut dyn Write,
    entry: &NameStatusEntry,
    stats: LineStats,
    nul_terminated: bool,
) -> Result<RenderOutcome, RenderError> {
    let stats = if matches!(entry.status, NameStatus::Unmerged) {
        LineStats::Text {
            inserted: 0,
            deleted: 0,
        }
    } else {
        stats
    };
    match stats {
        LineStats::Binary { .. } => write!(out, "-\t-\t")?,
        LineStats::Text { inserted, deleted } => write!(out, "{inserted}\t{deleted}\t")?,
    }
    if nul_terminated {
        if let Some(old_path) = &entry.old_path {
            out.write_all(b"\0")?;
            out.write_all(old_path)?;
            out.write_all(b"\0")?;
            out.write_all(&entry.path)?;
            out.write_all(b"\0")?;
        } else {
            out.write_all(&entry.path)?;
            out.write_all(b"\0")?;
        }
    } else if let Some(old_path) = &entry.old_path {
        writeln!(out, "{}", pprint_rename(old_path, &entry.path, true))?;
    } else {
        writeln!(out, "{}", quote_path(&entry.path, true))?;
    }
    Ok(RenderOutcome { records_written: 1 })
}

/// Rendering options for a complete `--stat` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatOptions {
    pub compact_summary: bool,
    pub stat_count: Option<usize>,
    pub color: bool,
    pub quote_path_fully: bool,
}

/// Already-resolved width policy for a diffstat table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatLayout {
    /// Total table width after terminal/config resolution.
    pub stat_width: i64,
    /// Maximum path-name width, or zero for automatic sizing.
    pub name_width: i64,
    /// Maximum graph width, or zero for automatic sizing.
    pub graph_width: i64,
}

/// Return the aggregate textual insertions and deletions.
#[must_use]
pub fn stat_totals(entries: &[StatEntry<'_>]) -> (usize, usize) {
    let mut inserted = 0;
    let mut deleted = 0;
    for data in entries {
        if matches!(data.entry.status, NameStatus::Unmerged) {
            continue;
        }
        if let LineStats::Text {
            inserted: entry_inserted,
            deleted: entry_deleted,
        } = data.stats
        {
            inserted += entry_inserted;
            deleted += entry_deleted;
        }
    }
    (inserted, deleted)
}

fn stat_changed_file_count(entries: &[StatEntry<'_>]) -> usize {
    entries
        .iter()
        .filter(|data| !matches!(data.entry.status, NameStatus::Unmerged))
        .count()
}

/// Render `--shortstat` output.
pub fn render_shortstat(
    out: &mut dyn Write,
    entries: &[StatEntry<'_>],
) -> Result<RenderOutcome, RenderError> {
    if entries.is_empty() {
        return Ok(RenderOutcome::default());
    }
    let (inserted, deleted) = stat_totals(entries);
    render_stat_summary(out, stat_changed_file_count(entries), inserted, deleted)
}

/// Render a complete Git diffstat table and its summary trailer.
pub fn render_stat<S: RenderServices + ?Sized>(
    out: &mut dyn Write,
    entries: &[StatEntry<'_>],
    options: StatOptions,
    layout: StatLayout,
    services: &S,
) -> Result<RenderOutcome, RenderError> {
    if entries.is_empty() {
        return Ok(RenderOutcome::default());
    }
    let rows = stat_rows(entries, options);
    let count = options.stat_count.unwrap_or(rows.len()).min(rows.len());

    let mut max_len = 0i64;
    let mut max_change = 0i64;
    let mut number_width = 0i64;
    let mut bin_width = 0i64;
    for row in rows.iter().take(count) {
        max_len = max_len.max(services.display_width(&row.path));
        match row.stats {
            StatValue::Unmerged => bin_width = bin_width.max("Unmerged".len() as i64),
            StatValue::Binary {
                old_size,
                new_size,
                unchanged,
            } => {
                let (added, deleted) = if unchanged {
                    (0, 0)
                } else {
                    (new_size, old_size)
                };
                bin_width = bin_width.max(14 + decimal_width(added) + decimal_width(deleted));
                number_width = number_width.max(3);
            }
            StatValue::Text { inserted, deleted } => {
                max_change = max_change.max((inserted + deleted) as i64);
            }
        }
    }

    let mut width = if layout.stat_width != 0 {
        layout.stat_width
    } else {
        80
    };
    number_width = decimal_width(max_change as usize).max(number_width);
    if width < 16 + 6 + number_width {
        width = 16 + 6 + number_width;
    }
    let mut graph_width = if max_change + 4 > bin_width {
        max_change
    } else {
        bin_width - 4
    };
    if layout.graph_width > 0 && layout.graph_width < graph_width {
        graph_width = layout.graph_width;
    }
    let mut name_width = if layout.name_width > 0 && layout.name_width < max_len {
        layout.name_width
    } else {
        max_len
    };
    if name_width + number_width + 6 + graph_width > width {
        if graph_width > width * 3 / 8 - number_width - 6 {
            graph_width = (width * 3 / 8 - number_width - 6).max(6);
        }
        if layout.graph_width > 0 && graph_width > layout.graph_width {
            graph_width = layout.graph_width;
        }
        if name_width > width - number_width - 6 - graph_width {
            name_width = width - number_width - 6 - graph_width;
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }

    let number_width = number_width.max(0) as usize;
    for row in rows.iter().take(count) {
        let mut len = name_width;
        let full_name = row.path.as_str();
        let name_len = services.display_width(full_name);
        let mut name = full_name;
        let mut marker = "";
        if name_width < name_len {
            marker = "...";
            len = (len - 3).max(0);
            while services.display_width(name) > len {
                let mut chars = name.chars();
                chars.next();
                name = chars.as_str();
            }
            if let Some(position) = name.find('/') {
                name = &name[position..];
            }
        }
        let padding = (len - services.display_width(name)).max(0) as usize;
        match row.stats {
            StatValue::Unmerged => {
                writeln!(out, " {marker}{name}{:padding$} | Unmerged", "")?;
            }
            StatValue::Binary {
                old_size,
                new_size,
                unchanged,
            } => {
                write!(
                    out,
                    " {marker}{name}{:padding$} | {:>number_width$}",
                    "", "Bin"
                )?;
                if unchanged {
                    writeln!(out)?;
                    continue;
                }
                writeln!(
                    out,
                    " {} -> {} bytes",
                    color_deleted(&old_size.to_string(), options.color),
                    color_inserted(&new_size.to_string(), options.color)
                )?;
            }
            StatValue::Text { inserted, deleted } => {
                let total_changed = inserted + deleted;
                let mut add = inserted as i64;
                let mut del = deleted as i64;
                if graph_width <= max_change && max_change > 0 {
                    let mut total = scale_linear(add + del, graph_width, max_change);
                    if total < 2 && add > 0 && del > 0 {
                        total = 2;
                    }
                    if add < del {
                        add = scale_linear(add, graph_width, max_change);
                        del = total - add;
                    } else {
                        del = scale_linear(del, graph_width, max_change);
                        add = total - del;
                    }
                }
                write!(
                    out,
                    " {marker}{name}{:padding$} | {total_changed:>number_width$}{}",
                    "",
                    if total_changed > 0 { " " } else { "" }
                )?;
                if add > 0 {
                    let pluses = std::iter::repeat_n('+', add as usize).collect::<String>();
                    write!(out, "{}", color_inserted(&pluses, options.color))?;
                }
                if del > 0 {
                    let minuses = std::iter::repeat_n('-', del as usize).collect::<String>();
                    write!(out, "{}", color_deleted(&minuses, options.color))?;
                }
                writeln!(out)?;
            }
        }
    }
    if count < rows.len() {
        writeln!(out, " ...")?;
    }
    let (inserted, deleted) = stat_totals(entries);
    render_stat_summary(out, stat_changed_file_count(entries), inserted, deleted)?;
    Ok(RenderOutcome {
        records_written: count + usize::from(count < rows.len()) + 1,
    })
}

/// Render Git's `N files changed` summary line.
pub fn render_stat_summary(
    out: &mut dyn Write,
    files: usize,
    inserted: usize,
    deleted: usize,
) -> Result<RenderOutcome, RenderError> {
    write!(out, " {files} {} changed", plural(files, "file", "files"))?;
    if inserted > 0 || deleted == 0 {
        write!(
            out,
            ", {inserted} {}(+)",
            plural(inserted, "insertion", "insertions")
        )?;
    }
    if deleted > 0 || inserted == 0 {
        write!(
            out,
            ", {deleted} {}(-)",
            plural(deleted, "deletion", "deletions")
        )?;
    }
    writeln!(out)?;
    Ok(RenderOutcome { records_written: 1 })
}

/// Git's `decimal_width()`.
#[must_use]
pub fn decimal_width(number: usize) -> i64 {
    let mut width = 1i64;
    let mut number = number / 10;
    while number > 0 {
        width += 1;
        number /= 10;
    }
    width
}

fn scale_linear(value: i64, width: i64, max_change: i64) -> i64 {
    if value == 0 {
        0
    } else {
        1 + value * (width - 1) / max_change
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatRow {
    path: String,
    stats: StatValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatValue {
    Unmerged,
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

fn stat_rows(entries: &[StatEntry<'_>], options: StatOptions) -> Vec<StatRow> {
    entries
        .iter()
        .map(|data| {
            let stats = if matches!(data.entry.status, NameStatus::Unmerged) {
                StatValue::Unmerged
            } else {
                match data.stats {
                    LineStats::Binary {
                        old_size,
                        new_size,
                        unchanged,
                    } => StatValue::Binary {
                        old_size,
                        new_size,
                        unchanged,
                    },
                    LineStats::Text { inserted, deleted } => StatValue::Text { inserted, deleted },
                }
            };
            let mut path = if let Some(old_path) = &data.entry.old_path {
                pprint_rename(old_path, &data.entry.path, options.quote_path_fully)
            } else {
                quote_path(&data.entry.path, options.quote_path_fully)
            };
            if options.compact_summary
                && let Some(summary) = compact_summary(data.entry)
            {
                path.push(' ');
                path.push_str(summary);
            }
            StatRow { path, stats }
        })
        .collect()
}

/// Collapse the common rename prefix/suffix using Git's `pprint_rename()`
/// representation.
#[must_use]
pub fn pprint_rename(old: &[u8], new: &[u8], quote_path_fully: bool) -> String {
    let quoted_old = quote_path(old, quote_path_fully);
    let quoted_new = quote_path(new, quote_path_fully);
    if quoted_old.starts_with('"') || quoted_new.starts_with('"') {
        return format!("{quoted_old} => {quoted_new}");
    }
    let mut prefix_len = 0usize;
    let mut index = 0usize;
    while index < old.len() && index < new.len() && old[index] == new[index] {
        if old[index] == b'/' {
            prefix_len = index + 1;
        }
        index += 1;
    }
    let mut suffix_len = 0usize;
    let lower = prefix_len as isize - isize::from(prefix_len > 0);
    let mut old_index = old.len() as isize;
    let mut new_index = new.len() as isize;
    while old_index >= lower && new_index >= lower {
        let old_byte = if old_index == old.len() as isize {
            0
        } else {
            old[old_index as usize]
        };
        let new_byte = if new_index == new.len() as isize {
            0
        } else {
            new[new_index as usize]
        };
        if old_byte != new_byte {
            break;
        }
        if old_byte == b'/' {
            suffix_len = old.len() - old_index as usize;
        }
        old_index -= 1;
        new_index -= 1;
    }
    let old_middle_len = old.len().saturating_sub(prefix_len + suffix_len);
    let new_middle_len = new.len().saturating_sub(prefix_len + suffix_len);
    let mut rendered = String::new();
    if prefix_len + suffix_len > 0 {
        rendered.push_str(&String::from_utf8_lossy(&old[..prefix_len]));
        rendered.push('{');
    }
    rendered.push_str(&String::from_utf8_lossy(
        &old[prefix_len..prefix_len + old_middle_len],
    ));
    rendered.push_str(" => ");
    rendered.push_str(&String::from_utf8_lossy(
        &new[prefix_len..prefix_len + new_middle_len],
    ));
    if prefix_len + suffix_len > 0 {
        rendered.push('}');
        rendered.push_str(&String::from_utf8_lossy(&old[old.len() - suffix_len..]));
    }
    rendered
}

fn compact_summary(entry: &NameStatusEntry) -> Option<&'static str> {
    match (entry.old_mode, entry.new_mode) {
        (None, Some(_)) => Some("(new)"),
        (Some(_), None) => Some("(gone)"),
        (Some(old), Some(new)) if old != new => match (old & 0o111 != 0, new & 0o111 != 0) {
            (false, true) => Some("(mode +x)"),
            (true, false) => Some("(mode -x)"),
            _ => Some("(mode)"),
        },
        _ => None,
    }
}

fn quote_path(path: &[u8], quote_path_fully: bool) -> String {
    let needs_quotes = path.iter().any(|&byte| {
        byte == b'"'
            || byte == b'\\'
            || byte == b'\n'
            || byte == b'\t'
            || byte < 0x20
            || byte == 0x7f
            || (quote_path_fully && byte >= 0x80)
    });
    if !needs_quotes {
        return String::from_utf8_lossy(path).into_owned();
    }
    let mut output = Vec::with_capacity(path.len() + 2);
    output.push(b'"');
    for &byte in path {
        match byte {
            b'"' => output.extend_from_slice(b"\\\""),
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0x20..=0x7e => output.push(byte),
            0x80..=0xff if !quote_path_fully => output.push(byte),
            _ => output.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    output.push(b'"');
    String::from_utf8_lossy(&output).into_owned()
}

fn color_inserted(value: &str, color: bool) -> String {
    if color {
        format!("\x1b[32m{value}\x1b[m")
    } else {
        value.to_string()
    }
}

fn color_deleted(value: &str, color: bool) -> String {
    if color {
        format!("\x1b[31m{value}\x1b[m")
    } else {
        value.to_string()
    }
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::{BString, ObjectId};

    struct Services;

    impl RenderServices for Services {
        fn display_width(&self, rendered: &str) -> i64 {
            rendered.chars().count() as i64
        }
    }

    fn modified(format: ObjectFormat) -> NameStatusEntry {
        NameStatusEntry {
            status: NameStatus::Modified,
            path: BString::from("src/lib.rs"),
            old_path: None,
            old_mode: Some(0o100644),
            new_mode: Some(0o100644),
            old_oid: Some(
                ObjectId::from_raw(format, &vec![0x11; format.raw_len()])
                    .expect("test object id should be valid"),
            ),
            new_oid: Some(
                ObjectId::from_raw(format, &vec![0x22; format.raw_len()])
                    .expect("test object id should be valid"),
            ),
        }
    }

    #[test]
    fn raw_output_respects_hash_format_and_legacy_ellipsis() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let entry = modified(format);
            let mut output = Vec::new();
            render_raw_entry(
                &mut output,
                &entry,
                RawOptions {
                    nul_terminated: false,
                    zero_new_oid: false,
                    abbrev: Some(7),
                    object_format: format,
                    print_hash_ellipsis: true,
                },
            )
            .expect("raw rendering should succeed");
            assert_eq!(
                String::from_utf8(output).expect("raw output should be UTF-8"),
                ":100644 100644 1111111... 2222222... M\tsrc/lib.rs\n"
            );
        }
    }

    #[test]
    fn direct_blob_patch_owns_metadata_and_hunk_rendering() {
        let format = ObjectFormat::Sha1;
        let old_oid =
            sley_core::object_id_for_bytes(format, "blob", b"one\n").expect("old blob id");
        let new_oid =
            sley_core::object_id_for_bytes(format, "blob", b"two\n").expect("new blob id");
        let mut output = Vec::new();
        render_blob_patch(
            &mut output,
            BlobDiffSide {
                oid: old_oid,
                mode: 0o100644,
                path: b"one",
                content: b"one\n",
            },
            BlobDiffSide {
                oid: new_oid,
                mode: 0o100755,
                path: b"two",
                content: b"two\n",
            },
            BlobPatchOptions {
                object_format: format,
                full_index: true,
                abbrev: 7,
                src_prefix: b"a/",
                dst_prefix: b"b/",
                context: 3,
                interhunk: 0,
                algorithm: DiffAlgorithm::Myers,
                indent_heuristic: true,
            },
        )
        .expect("blob patch rendering");
        assert_eq!(
            String::from_utf8(output).expect("utf8 patch"),
            format!(
                "diff --git a/one b/two\nold mode 100644\nnew mode 100755\nindex {old_oid}..{new_oid}\n--- a/one\n+++ b/two\n@@ -1 +1 @@\n-one\n+two\n"
            )
        );

        let mut mode_only = Vec::new();
        render_blob_patch(
            &mut mode_only,
            BlobDiffSide {
                oid: old_oid,
                mode: 0o100644,
                path: b"script",
                content: b"one\n",
            },
            BlobDiffSide {
                oid: old_oid,
                mode: 0o100755,
                path: b"script",
                content: b"one\n",
            },
            BlobPatchOptions {
                object_format: format,
                full_index: true,
                abbrev: 7,
                src_prefix: b"a/",
                dst_prefix: b"b/",
                context: 3,
                interhunk: 0,
                algorithm: DiffAlgorithm::Myers,
                indent_heuristic: true,
            },
        )
        .expect("mode-only blob patch rendering");
        assert_eq!(
            mode_only,
            b"diff --git a/script b/script\nold mode 100644\nnew mode 100755\n"
        );
    }

    #[test]
    fn combined_renderers_own_parent_paths_metadata_and_hunks() {
        let format = ObjectFormat::Sha1;
        let first_oid =
            sley_core::object_id_for_bytes(format, "blob", b"one\n").expect("first parent oid");
        let second_oid =
            sley_core::object_id_for_bytes(format, "blob", b"two\n").expect("second parent oid");
        let result_oid =
            sley_core::object_id_for_bytes(format, "blob", b"result\n").expect("result oid");
        let parents = [
            CombinedDiffParent {
                path: b"old\tone",
                mode: 0o100644,
                oid: Some(first_oid),
                status: 'R',
            },
            CombinedDiffParent {
                path: b"old\ttwo",
                mode: 0o100644,
                oid: Some(second_oid),
                status: 'R',
            },
        ];
        let entry = CombinedDiffEntry {
            result_path: b"new\tname",
            result_mode: 0o100644,
            result_oid: Some(result_oid),
            parents: &parents,
        };
        let options = CombinedFormatOptions {
            object_format: format,
            dense: false,
            all_paths: true,
            context: 3,
            ws_ignore: WsIgnore::default(),
            algorithm: DiffAlgorithm::Myers,
            src_prefix: b"a/",
            dst_prefix: b"b/",
            patch_abbrev: 7,
            raw_abbrev: Some(7),
            print_hash_ellipsis: false,
        };

        let mut raw = Vec::new();
        render_combined_raw_entry(&mut raw, entry, options, false).expect("combined raw");
        assert_eq!(
            String::from_utf8(raw).expect("utf8 raw"),
            format!(
                "::100644 100644 100644 {} {} {} RR\t\"old\\tone\"\t\"old\\ttwo\"\t\"new\\tname\"\n",
                &first_oid.to_hex()[..7],
                &second_oid.to_hex()[..7],
                &result_oid.to_hex()[..7],
            )
        );

        let mut patch = Vec::new();
        render_combined_patch(
            &mut patch,
            entry,
            b"result\n",
            &[b"one\n", b"two\n"],
            options,
        )
        .expect("combined patch");
        let patch = String::from_utf8(patch).expect("utf8 patch");
        assert!(patch.starts_with("diff --combined \"new\\tname\"\nindex "));
        assert!(patch.contains("--- \"a/old\\tone\"\n--- \"a/old\\ttwo\"\n"));
        assert!(patch.contains("+++ \"b/new\\tname\"\n@@@"));
    }

    #[test]
    fn rename_paths_share_one_engine_representation() {
        assert_eq!(
            pprint_rename(b"src/old/file.rs", b"src/new/file.rs", true),
            "src/{old => new}/file.rs"
        );
        assert_eq!(quote_path(b"line\nbreak", true), "\"line\\nbreak\"");
        assert_eq!(quote_path("café".as_bytes(), true), "\"caf\\303\\251\"");
        assert_eq!(quote_path("café".as_bytes(), false), "café");
    }

    #[test]
    fn summary_renderer_owns_rename_and_mode_change_records() {
        let mut rename = modified(ObjectFormat::Sha1);
        rename.status = NameStatus::Renamed(87);
        rename.old_path = Some(BString::from("src/old/file.rs"));
        rename.path = BString::from("src/new/file.rs");
        let mut output = Vec::new();
        let outcome = render_summary_entry(&mut output, &rename)
            .expect("rename summary rendering should succeed");
        assert_eq!(outcome.records_written, 1);
        assert_eq!(
            String::from_utf8(output).expect("summary output should be UTF-8"),
            " rename src/{old => new}/file.rs (87%)\n"
        );

        let mut mode_change = modified(ObjectFormat::Sha1);
        mode_change.new_mode = Some(0o100755);
        let mut output = Vec::new();
        render_summary_entry(&mut output, &mode_change)
            .expect("mode summary rendering should succeed");
        assert_eq!(
            String::from_utf8(output).expect("summary output should be UTF-8"),
            " mode change 100644 => 100755 src/lib.rs\n"
        );
    }

    #[test]
    fn stat_and_numstat_render_from_the_same_materialized_stats() {
        let entry = modified(ObjectFormat::Sha1);
        let data = [StatEntry {
            entry: &entry,
            stats: LineStats::Text {
                inserted: 2,
                deleted: 1,
            },
        }];
        let mut stat = Vec::new();
        render_stat(
            &mut stat,
            &data,
            StatOptions {
                compact_summary: false,
                stat_count: None,
                color: false,
                quote_path_fully: true,
            },
            StatLayout {
                stat_width: 80,
                name_width: 0,
                graph_width: 0,
            },
            &Services,
        )
        .expect("diffstat rendering should succeed");
        assert_eq!(
            String::from_utf8(stat).expect("diffstat output should be UTF-8"),
            " src/lib.rs | 3 ++-\n 1 file changed, 2 insertions(+), 1 deletion(-)\n"
        );

        let mut numstat = Vec::new();
        render_numstat_entry(&mut numstat, &entry, data[0].stats, false)
            .expect("numstat rendering should succeed");
        assert_eq!(
            String::from_utf8(numstat).expect("numstat output should be UTF-8"),
            "2\t1\tsrc/lib.rs\n"
        );
    }

    fn selection(default_output: DefaultDiffOutput) -> RenderSelectionOptions {
        RenderSelectionOptions {
            default_output,
            raw: false,
            patch: false,
            name_status: false,
            name_only: false,
            stat: false,
            numstat: false,
            shortstat: false,
            summary: false,
            auxiliary_format: false,
            suppress_output: false,
        }
    }

    #[test]
    fn render_selection_distinguishes_porcelain_and_plumbing_defaults() {
        assert_eq!(
            select_render_formats(selection(DefaultDiffOutput::Patch)),
            RenderSelection {
                patch: true,
                ..RenderSelection::default()
            }
        );
        assert_eq!(
            select_render_formats(selection(DefaultDiffOutput::Raw)),
            RenderSelection {
                raw: true,
                ..RenderSelection::default()
            }
        );
    }

    #[test]
    fn name_formats_suppress_other_render_families() {
        let mut options = selection(DefaultDiffOutput::Patch);
        options.name_status = true;
        options.raw = true;
        options.patch = true;
        options.stat = true;
        assert_eq!(
            select_render_formats(options),
            RenderSelection {
                name_status: true,
                ..RenderSelection::default()
            }
        );
    }

    #[test]
    fn render_selection_reports_stats_and_patch_separator_policy() {
        let mut options = selection(DefaultDiffOutput::Patch);
        options.patch = true;
        options.numstat = true;
        let selected = select_render_formats(options);
        assert!(selected.needs_line_stats());
        assert!(selected.separates_patch_from_prefix());

        let mut suppressed = selection(DefaultDiffOutput::Patch);
        suppressed.suppress_output = true;
        assert_eq!(
            select_render_formats(suppressed),
            RenderSelection::default()
        );
    }
}
