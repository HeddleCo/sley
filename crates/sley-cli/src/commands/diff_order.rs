//! Diff path ordering: `-O<orderfile>` / `diff.orderfile` (diffcore-order.c) and
//! `--rotate-to` / `--skip-to` (diffcore-rotate.c).
//!
//! Both operate on the final, path-sorted list of [`NameStatusEntry`] just
//! before the diff is formatted, mirroring git's `diffcore_std`, which runs
//! `diffcore_order()` (orderfile) and then `diffcore_rotate()`.

use std::cmp::Ordering;
use std::path::Path;

use sley_core::GitError;
use sley_core::Result;
use sley_diff_merge::NameStatusEntry;
use sley_pathspec::wildmatch;

/// Parse an orderfile into its list of glob patterns, mirroring
/// `diffcore-order.c:prepare_order`. Each non-empty, non-`#` line is one
/// pattern; blank and comment lines are skipped. Reading the file failing
/// (missing / unreadable / `-O/dev/null` is fine — it just yields no patterns)
/// is reported as git's `die_errno("failed to read orderfile '%s'")`.
pub(crate) fn read_orderfile(path: &str) -> Result<Vec<Vec<u8>>> {
    let data = std::fs::read(Path::new(path)).map_err(|_| {
        eprintln!("fatal: failed to read orderfile '{path}'");
        GitError::Exit(128)
    })?;
    Ok(parse_orderfile_bytes(&data))
}

/// Split orderfile bytes into patterns. Empty lines and lines beginning with
/// `#` are comments and are dropped (git checks `*cp == '\n' || *cp == '#'`).
fn parse_orderfile_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    let mut patterns = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        patterns.push(line.to_vec());
    }
    patterns
}

/// Compute a path's order index against the parsed patterns, mirroring
/// `diffcore-order.c:match_order`. The first pattern that matches the path —
/// or any of its leading directory prefixes — gives the order; a path matching
/// no pattern sorts last (`patterns.len()`). Patterns are matched with git's
/// `wildmatch(..., 0)` (no `WM_PATHNAME`, so `*` spans `/`).
fn match_order(path: &[u8], patterns: &[Vec<u8>]) -> usize {
    for (i, pat) in patterns.iter().enumerate() {
        let mut p: &[u8] = path;
        while !p.is_empty() {
            if wildmatch(pat, p, 0) {
                return i;
            }
            match p.iter().rposition(|&b| b == b'/') {
                Some(idx) => p = &p[..idx],
                None => break,
            }
        }
    }
    patterns.len()
}

/// Reorder `items` by the orderfile patterns applied to each item's path
/// (`path_of`), stable so items with the same order index keep their original
/// relative order — git's `orig_order` tiebreak.
pub(crate) fn order_by_path<T>(
    items: &mut [T],
    patterns: &[Vec<u8>],
    path_of: impl Fn(&T) -> &[u8],
) {
    if items.is_empty() {
        return;
    }
    // `sort_by_key` is a stable sort, matching git's `(order, orig_order)` key.
    items.sort_by_key(|item| match_order(path_of(item), patterns));
}

/// Reorder name-status `entries` by the orderfile patterns (keyed on the new
/// path, like git's `pair->two->path`).
pub(crate) fn order_entries(entries: &mut [NameStatusEntry], patterns: &[Vec<u8>]) {
    order_by_path(entries, patterns, |entry| entry.path.as_bytes());
}

/// Rotate (or skip) the path-sorted `entries` so the list begins at `rotate_to`,
/// mirroring `diffcore-rotate.c:diffcore_rotate`.
///
/// `skip` drops the entries before the pivot instead of moving them to the end.
/// `strict` (set by plumbing `git diff`/`diff-index`/`diff-tree`/`diff-files`)
/// makes a `rotate_to` that names no diffed path a fatal error; non-strict
/// callers (`git log`/`git show`) pivot at the first path lexically `>=` the
/// target and silently no-op when every path precedes it.
pub(crate) fn rotate_entries(
    entries: &mut Vec<NameStatusEntry>,
    rotate_to: &[u8],
    skip: bool,
    strict: bool,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut pivot = entries.len();
    for (i, entry) in entries.iter().enumerate() {
        match rotate_to.cmp(entry.path.as_bytes()) {
            Ordering::Equal => {
                pivot = i;
                break;
            }
            // `queue[i]` is now lexically past the target: pivot here (unless
            // strict, which requires an exact match).
            Ordering::Less if !strict => {
                pivot = i;
                break;
            }
            _ => {}
        }
    }
    if pivot >= entries.len() {
        if strict {
            eprintln!(
                "fatal: No such path '{}' in the diff",
                String::from_utf8_lossy(rotate_to)
            );
            return Err(GitError::Exit(128));
        }
        return Ok(());
    }
    let mut rotated = entries.split_off(pivot);
    if !skip {
        rotated.append(entries);
    }
    *entries = rotated;
    Ok(())
}
