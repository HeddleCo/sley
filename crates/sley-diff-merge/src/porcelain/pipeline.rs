//! The entry render pipeline: raw → numstat/stat/shortstat → summary → patch,
//! plus the dirstat family and diffstat width math.

use super::content::{diff_entry_new_content, diff_entry_old_content, diff_line_stats};
use super::options::{
    DiffEntryRenderContext, DiffEntryRenderModes, DiffEntryStatSource, DiffStatWidths,
    DiffWorktreeCleanContext, DirstatMode, DirstatOptions, LazyObjectFetch,
};
use super::{
    LineStats, NameStatusEntry, RawOptions, RenderError, RenderServices, StatEntry, StatOptions,
    decimal_width,
};
use sley_config::GitConfig;
use sley_core::{ObjectFormat, Result};
use sley_odb::FileObjectDatabase;
use std::io::Write;
use std::path::Path;

fn map_porcelain_render(
    result: std::result::Result<super::RenderOutcome, RenderError>,
) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(RenderError::Output(error)) => Err(error.into()),
    }
}

pub fn write_diff_summary_entry(stdout: &mut dyn Write, entry: &NameStatusEntry) -> Result<()> {
    map_porcelain_render(super::render_summary_entry(stdout, entry))
}

pub fn write_diff_raw_entry(
    stdout: &mut dyn Write,
    entry: &NameStatusEntry,
    z: bool,
    zero_worktree_oids: bool,
    abbrev: Option<usize>,
    format: ObjectFormat,
) -> Result<()> {
    map_porcelain_render(super::render_raw_entry(
        stdout,
        entry,
        RawOptions {
            nul_terminated: z,
            zero_new_oid: zero_worktree_oids,
            abbrev,
            object_format: format,
            print_hash_ellipsis: std::env::var("GIT_PRINT_SHA1_ELLIPSIS")
                .is_ok_and(|value| value == "yes"),
        },
    ))
}

#[allow(clippy::too_many_arguments, clippy::expect_used)]
pub fn render_diff_entries<RawZero, PatchEntry>(
    stdout: &mut dyn Write,
    entries: &[NameStatusEntry],
    modes: DiffEntryRenderModes,
    mut context: DiffEntryRenderContext<'_>,
    mut raw_zero_worktree_oids: RawZero,
    mut write_patch_entry: PatchEntry,
) -> Result<()>
where
    RawZero: FnMut(&NameStatusEntry) -> bool,
    PatchEntry: FnMut(&mut dyn Write, &NameStatusEntry) -> Result<()>,
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
                    context.services,
                )?,
                None => write_diff_stat_materialized(
                    stdout,
                    stat_entries,
                    context.stat.options,
                    context.stat.config,
                    context.services,
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

pub fn write_diff_numstat_materialized_entry(
    stdout: &mut dyn Write,
    entry: &NameStatusEntry,
    stats: LineStats,
    z: bool,
) -> Result<()> {
    map_porcelain_render(super::render_numstat_entry(stdout, entry, stats, z))
}

pub fn write_diff_shortstat_materialized(
    stdout: &mut dyn Write,
    entries: &[StatEntry<'_>],
) -> Result<()> {
    map_porcelain_render(super::render_shortstat(stdout, entries))
}

/// git `decimal_width()`: columns needed to print `number` in decimal.
pub fn diff_stat_decimal_width(number: usize) -> i64 {
    decimal_width(number)
}

pub fn write_diff_stat_materialized_with_widths(
    stdout: &mut dyn Write,
    entries: &[StatEntry<'_>],
    options: StatOptions,
    widths: DiffStatWidths,
    services: &dyn RenderServices,
) -> Result<()> {
    let stat_width = if widths.stat_width == -1 {
        services.terminal_columns() - widths.line_prefix_width
    } else {
        widths.stat_width
    };
    map_porcelain_render(super::render_stat(
        stdout,
        entries,
        options,
        super::StatLayout {
            stat_width,
            name_width: widths.name_width,
            graph_width: widths.graph_width,
        },
        services,
    ))
}

pub fn write_diff_stat_materialized(
    stdout: &mut dyn Write,
    entries: &[StatEntry<'_>],
    options: StatOptions,
    config: Option<&GitConfig>,
    services: &dyn RenderServices,
) -> Result<()> {
    let mut widths = DiffStatWidths::terminal();
    if let Some(config) = config {
        widths.resolve_config(config);
    } else {
        widths.resolve_config_defaults();
    }
    write_diff_stat_materialized_with_widths(stdout, entries, options, widths, services)
}

/// git `parse_dirstat_params()`: comma-separated `changes|lines|files|
/// cumulative|noncumulative|<limit>` parameters. Unknown parameters append to
/// `errors` (one line each) and are counted in the returned error total.
pub fn parse_dirstat_params(
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
#[allow(clippy::too_many_arguments)]
pub fn write_diff_dirstat(
    stdout: &mut dyn Write,
    entries: &[NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    options: DirstatOptions,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
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
                        LineStats::Binary { .. } => {
                            let bytes = old_content.as_ref().map_or(0, Vec::len)
                                + new_content.as_ref().map_or(0, Vec::len);
                            (bytes as u64).div_ceil(64)
                        }
                        LineStats::Text { inserted, deleted } => (inserted + deleted) as u64,
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
                            let (copied, added) = crate::count_changes(old, new);
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
pub fn write_diff_stat_summary_line(
    stdout: &mut dyn Write,
    files: usize,
    inserted: usize,
    deleted: usize,
) -> Result<()> {
    map_porcelain_render(super::render_stat_summary(stdout, files, inserted, deleted))
}

pub fn diff_stat_totals(entries: &[StatEntry<'_>]) -> (usize, usize) {
    super::stat_totals(entries)
}
