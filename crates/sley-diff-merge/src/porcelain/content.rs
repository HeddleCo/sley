//! Blob content resolution for diff rendering: object/worktree/gitlink reads,
//! binary detection, line statistics, and stat-entry materialization.

use super::options::{DiffWorktreeCleanContext, LazyObjectFetch};
use super::{LineStats, NameStatus, NameStatusEntry, StatEntry};
use sley_core::{GitError, ObjectId, Result};
use sley_object::{EncodedObject, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

pub(crate) enum DiffBlobContent {
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

fn read_blob_content(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
) -> Result<DiffBlobContent> {
    let object = match lazy_fetch {
        Some(fetch) => fetch.read_object_maybe_prefetch(db, oid)?,
        None => db.read_object(oid)?,
    };
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "diff expected blob object {oid}"
        )));
    }
    Ok(DiffBlobContent::Object(object))
}

pub fn read_blob(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
) -> Result<Vec<u8>> {
    match read_blob_content(db, oid, lazy_fetch)? {
        DiffBlobContent::Owned(bytes) => Ok(bytes),
        DiffBlobContent::Object(object) => match Arc::try_unwrap(object) {
            Ok(object) => Ok(object.body),
            Err(object) => Ok(object.body.clone()),
        },
    }
}

/// The synthetic blob content git diffs a gitlink as: `Subproject commit
/// <oid>\n` (diff.c diff_populate_filespec), with an optional `-dirty` suffix
/// for a worktree-side submodule whose own tree has changes.
pub fn gitlink_diff_content(oid: &ObjectId, dirty: bool) -> Vec<u8> {
    let suffix = if dirty { "-dirty" } else { "" };
    format!("Subproject commit {oid}{suffix}\n").into_bytes()
}

pub fn is_gitlink_pair(entry: &NameStatusEntry) -> bool {
    entry.old_mode == Some(0o160000) || entry.new_mode == Some(0o160000)
}

pub(crate) fn is_binary_or_large_content(bytes: &[u8], big_file_threshold: u64) -> bool {
    bytes.len() as u64 >= big_file_threshold || is_binary_content(bytes)
}

pub fn is_binary_content(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// git's `repo_path_to_path`-style conversion of a slash-separated repository
/// path into a filesystem path.
pub fn repo_path_to_path(path: &[u8]) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(path).into_owned())
}

pub fn diff_line_stats(old: Option<&[u8]>, new: Option<&[u8]>) -> LineStats {
    if old.is_some_and(is_binary_content) || new.is_some_and(is_binary_content) {
        return LineStats::Binary {
            old_size: old.map_or(0, <[u8]>::len),
            new_size: new.map_or(0, <[u8]>::len),
            unchanged: old == new,
        };
    }
    match (old, new) {
        (None, None) => LineStats::Text {
            inserted: 0,
            deleted: 0,
        },
        (None, Some(new)) => LineStats::Text {
            inserted: count_diff_lines(new),
            deleted: 0,
        },
        (Some(old), None) => LineStats::Text {
            inserted: 0,
            deleted: count_diff_lines(old),
        },
        (Some(old), Some(new)) => {
            let (inserted, deleted) = count_line_diff(old, new);
            LineStats::Text { inserted, deleted }
        }
    }
}

/// `--stat` insertion/deletion line counts, computed by the shared diff-merge
/// Myers engine rather than a CLI-local LCS.
///
/// Myers produces a shortest edit script, so the count of `Insert` lines is
/// `new_len - lcs` and the count of `Delete` lines is `old_len - lcs` — exactly
/// the values the removed local LCS counter returned.
// The two independently shrinking suffix cursors intentionally index different
// line arrays; grouping both subtractions would change the comparison.
#[allow(clippy::suspicious_operation_groupings)]
pub fn count_line_diff(old: &[u8], new: &[u8]) -> (usize, usize) {
    let old_lines = crate::split_lines(old);
    let new_lines = crate::split_lines(new);
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > prefix
        && new_end > prefix
        && old_lines.get(old_end - 1) == new_lines.get(new_end - 1)
    {
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
    for op in crate::myers_diff_lines(&old_lines, &new_lines) {
        match op {
            crate::DiffOp::Insert(n) => inserted += n,
            crate::DiffOp::Delete(n) => deleted += n,
            crate::DiffOp::Equal(_) => {}
        }
    }
    (inserted, deleted)
}

fn trivial_lcs_len(old: &[crate::DiffLine<'_>], new: &[crate::DiffLine<'_>]) -> Option<usize> {
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

fn diff_lines_have_any_common(old: &[crate::DiffLine<'_>], new: &[crate::DiffLine<'_>]) -> bool {
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

fn diff_entry_old_stat_content(
    entry: &NameStatusEntry,
    db: &FileObjectDatabase,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
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
    entry: &NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
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
                    crate::gitlink_head_oid(&sub_root, db.object_format()).or(entry.old_oid)
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

pub fn diff_entry_old_content(
    entry: &NameStatusEntry,
    db: &FileObjectDatabase,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
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

pub fn diff_entry_new_content(
    entry: &NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
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
                    crate::gitlink_head_oid(&sub_root, db.object_format()).or(entry.old_oid)
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
                return Ok(Some(crate::symlink_target_bytes(&path)?));
            }
            Ok(_) => {
                let content = fs::read(path)?;
                return match worktree_clean {
                    Some(clean) => clean
                        .apply(&entry.path, &content, entry.old_oid.as_ref())
                        .map(Some),
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

/// Non-gitlink blob OIDs referenced by `entries` (both sides). Unchanged paths
/// never appear in the queue, so same-OID skips (t4067 #3) fall out naturally.
pub fn collect_diff_entry_blob_oids(entries: &[NameStatusEntry]) -> Vec<ObjectId> {
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

pub fn collect_diff_stat_entries<'a>(
    entries: &'a [NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
) -> Result<Vec<StatEntry<'a>>> {
    collect_diff_stat_entries_with_worktree_clean(
        entries,
        db,
        worktree_root,
        use_worktree_new,
        None,
        lazy_fetch,
    )
}

pub fn collect_diff_stat_entries_with_worktree_clean<'a>(
    entries: &'a [NameStatusEntry],
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    lazy_fetch: Option<&dyn LazyObjectFetch>,
) -> Result<Vec<StatEntry<'a>>> {
    // Batch-hydrate every blob the stat pass will open so a partial clone does
    // one promisor negotiation rather than one per path (t4067).
    if let Some(fetch) = lazy_fetch {
        fetch.prefetch_entry_blobs(db, entries, use_worktree_new)?;
    }
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
        stat_entries.push(StatEntry { entry, stats });
    }
    Ok(stat_entries)
}

/// Whether a name-status entry produces any visible diff output once the
/// whitespace-ignore (`-w`/`-b`/eol) and change-group-ignore
/// (`--ignore-blank-lines` / `-I<regex>`) flags are applied — git's
/// `DIFF_OPT_HAS_CHANGES`, which `--exit-code`/`--quiet` reflect. A
/// non-content change (add/delete/rename/copy/mode change) always counts; a
/// same-mode pure content modification counts only if a hunk survives the
/// ignore filters.
#[allow(clippy::too_many_arguments)]
pub fn diff_entry_produces_output(
    entry: &NameStatusEntry,
    db: &FileObjectDatabase,
    worktree_root: Option<&Path>,
    use_worktree_new: bool,
    worktree_clean: Option<&DiffWorktreeCleanContext<'_>>,
    interhunk: usize,
    context: usize,
    ws_ignore: crate::WsIgnore,
    ignore_blank_lines: bool,
    ignore_regexes: &[sley_grep::Regex],
    lazy_fetch: Option<&dyn LazyObjectFetch>,
) -> Result<bool> {
    // Non-modification statuses, mode changes, and renames/copies always show.
    let mode_unchanged = match (entry.old_mode, entry.new_mode) {
        (Some(old_mode), Some(new_mode)) => old_mode == new_mode,
        _ => true,
    };
    if !matches!(entry.status, NameStatus::Modified) || !mode_unchanged {
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
    let change_ignore =
        (ignore_blank_lines || !ignore_regexes.is_empty()).then(|| crate::render::ChangeIgnore {
            ignore_blank_lines,
            regex_match: regex_match.as_ref().map(|f| f as &dyn Fn(&[u8]) -> bool),
        });
    let mut probe_options = crate::render::HunkRenderOptions {
        context,
        interhunk,
        ws_ignore,
        algorithm: crate::DiffAlgorithm::Myers,
        change_ignore: change_ignore.as_ref(),
        ..Default::default()
    };
    let mut probe = Vec::new();
    crate::render::render_hunks(
        &mut probe,
        old_content.as_deref(),
        new_content.as_deref(),
        &mut probe_options,
    );
    Ok(!probe.is_empty())
}
