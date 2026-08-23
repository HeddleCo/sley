//! Entry-level unified-patch rendering (`write_diff_patch_entry`), ported
//! byte-for-byte from the CLI's former `diff_render.rs` patch tier.

use super::binary_patch::write_diff_binary_patch_entry;
use super::options::{DiffRenderOptions, PatchUserdiff, SubmoduleDiffFormat};
use super::content::{is_binary_or_large_content, is_gitlink_pair};
use super::{diff_entry_new_content, diff_entry_old_content};
use crate::format::{DiffColors, WordDiffAdapter, WordDiffConfig, heading_classifier, render_colors};
use crate::render::{HunkRenderOptions, render_hunks};
use crate::ws;
use crate::{NameStatus, NameStatusEntry, is_type_change};
use sley_core::{GitError, ObjectFormat, ObjectId, Result, object_id_for_bytes};
use sley_formats::quoted_path;
use sley_odb::{FileObjectDatabase, ObjectPrefixResolution};
use std::io::Write;

/// Write one metainfo header line, wrapped in the meta color when enabled.
fn write_diff_meta_line(
    stdout: &mut dyn Write,
    colors: Option<&DiffColors>,
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

/// Whether a tree/index mode is a regular file (`S_ISREG`). Textconv acts only
/// on regular-file blobs, never on symlinks (`120000`) or gitlinks (`160000`).
fn diff_mode_is_regular_file(mode: Option<u32>) -> bool {
    matches!(mode, Some(m) if (m & 0o170000) == 0o100000)
}

pub fn write_diff_patch_entry(
    stdout: &mut dyn Write,
    entry: &NameStatusEntry,
    mut options: DiffRenderOptions<'_>,
) -> Result<()> {
    // A symlink target that is an incomplete line is not a whitespace error:
    // git clears `WS_INCOMPLETE_LINE` when the new side is a symlink (diff.c
    // "symlink being an incomplete line is not a news"), so the `\ No newline at
    // end of file` marker is rendered in the context color rather than
    // highlighted. Applies before the typechange split below so the split's
    // symlink-creation half inherits the cleared rule.
    if entry.new_mode == Some(0o120000)
        && let Some(ws_error) = options.ws_error.as_mut()
    {
        ws_error.rule &= !ws::WS_INCOMPLETE_LINE;
    }
    // A filepair whose two sides have different file types (regular↔symlink,
    // regular↔gitlink, symlink↔gitlink) cannot be rendered as one textual diff.
    // git's `run_diff` (diff.c) splits it into a deletion of the old side
    // followed by a creation of the new side, each shown through the normal
    // add/delete patch path. The single `T` status survives in raw/name-status/
    // summary; only the patch body is split.
    if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
        && is_type_change(old_mode, new_mode)
    {
        let deletion_path = entry.old_path.clone().unwrap_or_else(|| entry.path.clone());
        let deletion = NameStatusEntry {
            status: NameStatus::Deleted,
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
        let creation = NameStatusEntry {
            status: NameStatus::Added,
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
        && options.submodule_format != SubmoduleDiffFormat::Short
        && options.no_index_contents.is_none()
    {
        // Non-short gitlink rendering is host-provided (needs submodule history
        // and worktree access); without a host renderer fall back to the
        // synthetic `Subproject commit` body below.
        if let Some(renderer) = options.submodule_render {
            return renderer.write_submodule_patch(stdout, entry, &options);
        }
    }
    let lazy_fetch = options.lazy_fetch;
    let (mut old_content, mut new_content) = match options.no_index_contents {
        Some((old, new)) => (old.map(<[u8]>::to_vec), new.map(<[u8]>::to_vec)),
        None => (
            diff_entry_old_content(entry, options.db, lazy_fetch)?,
            diff_entry_new_content(
                entry,
                options.db,
                options.worktree_root,
                options.use_worktree_new,
                None,
                lazy_fetch,
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
    let quote = |path: &[u8]| quoted_path(path, false, true);
    let old_similarity_path = quote(old_path);
    let similarity_path = quote(&entry.path);
    let colors = options.colors;
    let (old_driver, new_driver) = match options.userdiff {
        Some(resolver) => (
            resolver.patch_driver_for_path(old_path)?,
            resolver.patch_driver_for_path(&entry.path)?,
        ),
        None => (None, None),
    };
    // Textconv (git's `fill_textconv`): for porcelain `-p` output, replace a
    // regular-file side's bytes with `diff.<driver>.textconv`'s output before
    // binary detection and diffing. The recorded blob oids (and thus the `index`
    // line) are unaffected; symlinks/gitlinks are never converted (not regular
    // files), and a textconv helper that fails leaves the side unconverted.
    if options.allow_textconv
        && let Some(resolver) = options.userdiff
    {
        if let Some(driver) = old_driver.as_ref()
            && let Some(command) = driver.textconv.as_deref()
            && diff_mode_is_regular_file(entry.old_mode)
            && let Some(content) = old_content.as_deref()
            && let Some(converted) = resolver.patch_run_textconv(command, content)?
        {
            old_content = Some(converted);
        }
        if let Some(driver) = new_driver.as_ref()
            && let Some(command) = driver.textconv.as_deref()
            && diff_mode_is_regular_file(entry.new_mode)
            && let Some(content) = new_content.as_deref()
            && let Some(converted) = resolver.patch_run_textconv(command, content)?
        {
            new_content = Some(converted);
        }
    }
    let binary_override = old_driver
        .as_ref()
        .and_then(|driver| driver.binary)
        .or_else(|| new_driver.as_ref().and_then(|driver| driver.binary));
    let treat_as_binary = match binary_override {
        Some(binary) => binary,
        None => {
            old_content
                .as_deref()
                .is_some_and(|content| is_binary_or_large_content(content, options.big_file_threshold))
                || new_content
                    .as_deref()
                    .is_some_and(|content| is_binary_or_large_content(content, options.big_file_threshold))
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
    if matches!(entry.status, NameStatus::Modified) && !content_changed && mode_unchanged {
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
        && matches!(entry.status, NameStatus::Modified)
        && old_content.is_some()
        && new_content.is_some()
    {
        let regex_match = (!options.ignore_regexes.is_empty()).then_some(move |line: &[u8]| {
            options
                .ignore_regexes
                .iter()
                .any(|re| re.is_match_with_case(line, false))
        });
        let change_ignore =
            (options.ignore_blank_lines || !options.ignore_regexes.is_empty()).then(|| {
                crate::render::ChangeIgnore {
                    ignore_blank_lines: options.ignore_blank_lines,
                    regex_match: regex_match.as_ref().map(|f| f as &dyn Fn(&[u8]) -> bool),
                }
            });
        let mut probe_options = HunkRenderOptions {
            context: options.context,
            interhunk: options.interhunk,
            ws_ignore: options.ws_ignore,
            algorithm: options.diff_algorithm,
            change_ignore: change_ignore.as_ref(),
            ..Default::default()
        };
        let mut probe = Vec::new();
        render_hunks(
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
        NameStatus::Added => {
            if let Some(mode) = entry.new_mode {
                write_diff_meta_line(stdout, colors, &format!("new file mode {mode:06o}"))?;
            }
        }
        NameStatus::Deleted => {
            if let Some(mode) = entry.old_mode {
                write_diff_meta_line(stdout, colors, &format!("deleted file mode {mode:06o}"))?;
            }
        }
        NameStatus::Modified
        | NameStatus::TypeChanged
        | NameStatus::Renamed(_)
        | NameStatus::Copied(_) => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                write_diff_meta_line(stdout, colors, &format!("old mode {old_mode:06o}"))?;
                write_diff_meta_line(stdout, colors, &format!("new mode {new_mode:06o}"))?;
            }
        }
        // Unmerged paths are surfaced via the raw/name-status `U` line, not a
        // patch hunk, so they carry no meta header here.
        NameStatus::Unmerged => {}
    }
    write_diff_similarity_headers(&mut *stdout, entry, &old_similarity_path, &similarity_path)?;
    if !content_changed {
        return Ok(());
    }
    let no_index_stdin_add = options.no_index_contents.is_some()
        && matches!(entry.status, NameStatus::Added)
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
    let empty_add_or_delete = matches!(entry.status, NameStatus::Added | NameStatus::Deleted)
        && old_content.as_deref().unwrap_or_default().is_empty()
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
        .or_else(|| old_driver.as_ref().and_then(|driver| driver.funcname.as_ref()))
        .or_else(|| new_driver.as_ref().and_then(|driver| driver.funcname.as_ref()));
    let default_colors;
    let word_regex;
    let word_diff = match options.word_diff {
        Some(request) => {
            let spec: Option<Vec<u8>> = request
                .cli_regex
                .map(|regex| regex.as_bytes().to_vec())
                .or_else(|| old_driver.as_ref().and_then(|driver| driver.word_regex.clone()))
                .or_else(|| new_driver.as_ref().and_then(|driver| driver.word_regex.clone()))
                .or_else(|| options.userdiff.and_then(PatchUserdiff::patch_config_word_regex));
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
            default_colors = DiffColors::default();
            Some(WordDiffConfig {
                mode: request.mode,
                regex: word_regex.as_ref(),
                colors: colors.unwrap_or(&default_colors),
            })
        }
        None => None,
    };
    let mut heading = heading_classifier(funcname);
    let mut word_diff_adapter = word_diff
        .as_ref()
        .map(WordDiffAdapter::new);
    let ws_error = colors.and(options.ws_error);
    let ignore_regexes = options.ignore_regexes;
    let regex_match = (!ignore_regexes.is_empty()).then_some(move |line: &[u8]| {
        ignore_regexes
            .iter()
            .any(|re| re.is_match_with_case(line, false))
    });
    let change_ignore = (options.ignore_blank_lines || !ignore_regexes.is_empty()).then(|| {
        crate::render::ChangeIgnore {
            ignore_blank_lines: options.ignore_blank_lines,
            regex_match: regex_match.as_ref().map(|f| f as &dyn Fn(&[u8]) -> bool),
        }
    });
    let mut render_options = HunkRenderOptions {
        context: options.context,
        interhunk: options.interhunk,
        heading: Some(&mut heading),
        colors: colors.map(render_colors),
        word_diff: word_diff_adapter
            .as_mut()
            .map(|adapter| adapter as &mut dyn crate::render::HunkWordDiff),
        line_indicators: options.line_indicators,
        suppress_blank_empty: options.suppress_blank_empty,
        ws_error,
        color_moved: colors.and(options.color_moved).filter(|_| word_diff.is_none()),
        ws_ignore: options.ws_ignore,
        algorithm: options.diff_algorithm,
        indent_heuristic: options.indent_heuristic,
        change_ignore: change_ignore.as_ref(),
        line_ranges: options.line_ranges,
        anchors: options.anchors,
    };
    let mut hunks = Vec::new();
    render_hunks(
        &mut hunks,
        old_content.as_deref(),
        new_content.as_deref(),
        &mut render_options,
    );
    if hunks.is_empty() {
        return Ok(());
    }
    match entry.status {
        NameStatus::Added => {
            write_diff_meta_line(stdout, colors, "--- /dev/null")?;
        }
        _ => {
            write_diff_meta_line(stdout, colors, &format!("--- {old_header_path}"))?;
        }
    }
    match entry.status {
        NameStatus::Deleted => {
            write_diff_meta_line(stdout, colors, "+++ /dev/null")?;
        }
        _ => {
            write_diff_meta_line(stdout, colors, &format!("+++ {header_path}"))?;
        }
    }
    stdout.write_all(&hunks)?;
    Ok(())
}

pub(super) fn write_diff_similarity_headers(
    stdout: &mut dyn Write,
    entry: &NameStatusEntry,
    old_path: &str,
    path: &str,
) -> Result<()> {
    match entry.status {
        NameStatus::Renamed(score) => {
            writeln!(stdout, "similarity index {score}%")?;
            writeln!(stdout, "rename from {old_path}")?;
            writeln!(stdout, "rename to {path}")?;
        }
        NameStatus::Copied(score) => {
            writeln!(stdout, "similarity index {score}%")?;
            writeln!(stdout, "copy from {old_path}")?;
            writeln!(stdout, "copy to {path}")?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn diff_patch_prefixed_path(prefix: &str, path: &[u8]) -> String {
    quoted_path(&diff_patch_prefixed_path_bytes(prefix, path), false, true)
}

pub(super) fn diff_patch_file_header_path(prefix: &str, path: &[u8]) -> String {
    let raw = diff_patch_prefixed_path_bytes(prefix, path);
    let mut quoted = quoted_path(&raw, false, true);
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

pub(super) fn diff_patch_oid(
    db: &FileObjectDatabase,
    oid: Option<&ObjectId>,
    content: Option<&[u8]>,
    format: ObjectFormat,
    abbrev: usize,
) -> String {
    let hex = oid
        .cloned()
        .or_else(|| {
            content.and_then(|content| object_id_for_bytes(format, "blob", content).ok())
        })
        .map(|oid| oid.to_hex())
        .unwrap_or_else(|| "0".repeat(format.hex_len()));
    let mut width = abbrev.min(hex.len());
    // Patch index lines use `find_unique_abbrev`, not a blind prefix slice.
    // Only repository-backed OIDs participate: a no-index/worktree content
    // hash may not exist in the ODB and therefore has no repository collision
    // set to extend against.
    if let Some(_oid) = oid.filter(|oid| !oid.is_null()) {
        while width < hex.len()
            && matches!(
                db.resolve_prefix(&hex[..width]),
                Ok(ObjectPrefixResolution::Ambiguous(_))
            )
        {
            width += 1;
        }
    }
    hex[..width].to_string()
}

pub(super) fn diff_patch_mode_suffix(entry: &NameStatusEntry) -> String {
    match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) if old_mode == new_mode => format!(" {old_mode:06o}"),
        _ => String::new(),
    }
}
