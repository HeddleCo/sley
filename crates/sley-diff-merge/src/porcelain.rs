//! Repository-independent rendering for Git's raw, summary, numstat, and
//! diffstat output families.
//!
//! The diff engine owns the byte layout and row-planning semantics. Callers
//! inject terminal display-width measurement while the engine retains Git's
//! byte-level path quoting rules.

use crate::{NameStatus, NameStatusEntry};
use sley_core::{ObjectFormat, ObjectId};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

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
}
