use sley_core::{GitError, ObjectFormat, ObjectId, RepoPath, Result, object_id_for_bytes};

mod name;
pub mod range;
pub mod render;
pub mod ws;

pub use sley_core::BString;
use sley_index::{BorrowedIndex, Index, IndexStatCache};
use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntries, TreeEntry};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

// ===========================================================================
// Gitlink (submodule) resolution helpers.
//
// A gitlink is a mode-160000 tree/index entry whose oid names the commit an
// embedded repository has checked out. These helpers resolve, for a directory
// in the working tree, (a) the embedded repository's git directory — either a
// `.git` directory or a `.git` *file* carrying a `gitdir: <path>` pointer (the
// layout `git submodule add`/`update` creates, pointing into the
// superproject's `.git/modules/<name>`) — and (b) the commit its HEAD names.
// They are the native equivalent of upstream's `resolve_gitlink_ref()`.
// ===========================================================================

/// Resolve the git directory of an embedded repository whose working tree is
/// at `sub_root`. A `.git` directory is returned as-is; a `.git` file is
/// followed through its `gitdir: <path>` pointer (a relative pointer resolves
/// against `sub_root`). Returns `None` when there is no `.git` entry or the
/// pointer does not name an existing directory.
pub fn gitlink_git_dir(sub_root: &Path) -> Option<PathBuf> {
    let dot_git = sub_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    if !metadata.is_file() {
        return None;
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let target = contents.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = PathBuf::from(target);
    let git_dir = if target.is_absolute() {
        target
    } else {
        sub_root.join(target)
    };
    if git_dir.is_dir() {
        Some(git_dir)
    } else {
        None
    }
}

/// When `sub_root` holds a *broken* gitlink — a `.git` file whose `gitdir:`
/// pointer names a directory that no longer exists (e.g. the submodule's git
/// directory was moved out of `.git/modules/`) — return that unresolved gitdir
/// path. git's status / diff-index fail fatally ("not a git repository: …")
/// here. Returns `None` for a valid gitlink (a `.git` directory, or a `.git`
/// file with a live gitdir) and for an *unpopulated* gitlink (no `.git` entry at
/// all), both of which git treats as non-fatal (the latter as unchanged).
pub fn gitlink_broken_gitdir(sub_root: &Path) -> Option<PathBuf> {
    let dot_git = sub_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if !metadata.is_file() {
        // No `.git` (unpopulated) or a real `.git` directory — not broken.
        return None;
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let target = contents.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target_path = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        sub_root.join(target)
    };
    if target_path.is_dir() {
        None
    } else {
        Some(target_path)
    }
}

/// Resolve the commit checked out in the embedded repository at `sub_root`
/// (the value a gitlink entry for that path records): its git directory's
/// HEAD, followed through symbolic refs. `None` when `sub_root` is not a
/// repository or its HEAD does not resolve to a commit (e.g. an unborn
/// branch) — upstream's `resolve_gitlink_ref() < 0` case.
pub fn gitlink_head_oid(sub_root: &Path, format: ObjectFormat) -> Option<ObjectId> {
    let git_dir = gitlink_git_dir(sub_root)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut target = store.read_ref("HEAD").ok()??;
    // Follow symbolic-ref chains defensively (git caps the depth too).
    for _ in 0..10 {
        match target {
            RefTarget::Direct(oid) => return Some(oid),
            RefTarget::Symbolic(name) => target = store.read_ref(&name).ok()??,
        }
    }
    None
}

// ===========================================================================
// Line-level diff (Myers O(ND)) and 3-way blob merge (diff3).
//
// These operate purely on in-memory blobs and never touch the ODB or the
// filesystem. They are the engine the CLI layers `git merge`, `cherry-pick`,
// and `revert` on top of.
// ===========================================================================

/// A single line of a blob, slicing into the original buffer.
///
/// `content` includes the line's own trailing newline byte when present;
/// `has_newline` records whether this line ended with `\n` in the source. Only
/// the final line of a blob can have `has_newline == false` (a file with "no
/// newline at end of file"). Comparing two `DiffLine`s for equality compares
/// both the bytes and the trailing-newline flag, so a line that gained or lost
/// its terminating newline is treated as a real change, matching git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffLine<'a> {
    /// The raw bytes of the line, including the trailing `\n` if it had one.
    pub content: &'a [u8],
    /// Whether the line was terminated by a newline in the source blob.
    pub has_newline: bool,
}

impl<'a> DiffLine<'a> {
    /// The line bytes without any trailing newline.
    pub fn bytes_without_newline(&self) -> &'a [u8] {
        if self.has_newline {
            self.content.strip_suffix(b"\n").unwrap_or(self.content)
        } else {
            self.content
        }
    }
}

/// Split a blob into lines, preserving the exact bytes of each line.
///
/// Each returned [`DiffLine`] borrows from `blob`; its `content` includes the
/// terminating `\n`. The returned vector is empty for an empty blob. A blob
/// whose final byte is not `\n` yields a final line with `has_newline ==
/// false` — git's "\ No newline at end of file" case.
pub fn split_lines(blob: &[u8]) -> Vec<DiffLine<'_>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let len = blob.len();
    let mut idx = 0usize;
    while idx < len {
        if blob[idx] == b'\n' {
            lines.push(DiffLine {
                content: &blob[start..=idx],
                has_newline: true,
            });
            idx += 1;
            start = idx;
        } else {
            idx += 1;
        }
    }
    if start < len {
        lines.push(DiffLine {
            content: &blob[start..len],
            has_newline: false,
        });
    }
    lines
}

/// A run-length entry in a Myers edit script.
///
/// Each variant carries the number of consecutive lines it applies to:
/// - [`DiffOp::Equal`] — `n` lines common to both `old` and `new`.
/// - [`DiffOp::Delete`] — `n` lines present in `old` but not `new`.
/// - [`DiffOp::Insert`] — `n` lines present in `new` but not `old`.
///
/// Walking the script in order and consuming `old`/`new` lines accordingly
/// reconstructs `new` from `old`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    /// `n` lines are identical in both sequences.
    Equal(usize),
    /// `n` lines are removed from the old sequence.
    Delete(usize),
    /// `n` lines are added in the new sequence.
    Insert(usize),
}

/// Compute a minimal line-level edit script transforming `old` into `new`
/// using Myers' O(ND) difference algorithm.
///
/// Lines are compared for equality by their full bytes (see [`DiffLine`]). The
/// result is a coalesced sequence of [`DiffOp`] runs; consecutive ops of the
/// same kind are merged so the script is compact. The script is a standard
/// (shortest-edit-script) diff: the number of `Delete` + `Insert` lines is
/// minimal.
pub fn myers_diff_lines(old: &[DiffLine<'_>], new: &[DiffLine<'_>]) -> Vec<DiffOp> {
    // Trim a common prefix and suffix first. This keeps the O(ND) search small
    // for the typical case of a localized edit and does not affect minimality.
    let n_total = old.len();
    let m_total = new.len();
    let mut prefix = 0usize;
    while prefix < n_total && prefix < m_total && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < n_total - prefix
        && suffix < m_total - prefix
        && old[n_total - 1 - suffix] == new[m_total - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid = &old[prefix..n_total - suffix];
    let new_mid = &new[prefix..m_total - suffix];

    let mut ops: Vec<DiffOp> = Vec::new();
    if prefix > 0 {
        ops.push(DiffOp::Equal(prefix));
    }
    myers_core(old_mid, new_mid, &mut ops);
    if suffix > 0 {
        ops.push(DiffOp::Equal(suffix));
    }
    coalesce_ops(ops)
}

/// Classic forward Myers O(ND) shortest-edit-script search over the trimmed
/// sub-problem, followed by a backtrack through the stored traces.
///
/// `old`/`new` are the trimmed (no common prefix/suffix) line slices. Per-line
/// ops are appended to `out` in order; they are coalesced by the caller. This
/// is the algorithm from Myers' 1986 paper, which yields a shortest edit script
/// (minimal number of insertions + deletions).
fn myers_core(old: &[DiffLine<'_>], new: &[DiffLine<'_>], out: &mut Vec<DiffOp>) {
    let n = old.len() as isize;
    let m = new.len() as isize;
    if n == 0 {
        if m > 0 {
            out.push(DiffOp::Insert(m as usize));
        }
        return;
    }
    if m == 0 {
        out.push(DiffOp::Delete(n as usize));
        return;
    }

    let max = (n + m) as usize;
    let offset = max as isize; // shift so diagonal k maps to index (k + offset)
    let width = 2 * max + 1;
    // v[k + offset] holds the furthest-reaching x on diagonal k for the current d.
    let mut v = vec![0isize; width];
    // Save a snapshot of v after each d so we can backtrack the chosen path.
    let mut trace: Vec<Vec<isize>> = Vec::new();

    let mut found_d: Option<usize> = None;
    'search: for d in 0..=(max as isize) {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let kidx = (k + offset) as usize;
            // Decide whether we arrived here by moving down (insert, from k+1)
            // or right (delete, from k-1). Prefer the move that reaches further.
            let mut x = if k == -d
                || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize])
            {
                // Move down: x stays, y increases (insertion from new).
                v[(k + 1 + offset) as usize]
            } else {
                // Move right: x increases (deletion from old).
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            // Follow the diagonal (matching lines) as far as possible.
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[kidx] = x;
            if x >= n && y >= m {
                found_d = Some(d as usize);
                break 'search;
            }
            k += 2;
        }
    }

    // A shortest edit path always exists, so found_d is set; if somehow not,
    // fall back to a delete-all/insert-all script (still correct, not minimal).
    let Some(d_end) = found_d else {
        out.push(DiffOp::Delete(n as usize));
        out.push(DiffOp::Insert(m as usize));
        return;
    };

    backtrack(n, m, &trace, d_end, offset, out);
}

/// Reconstruct the edit script from the saved Myers traces.
///
/// Walks backward from `(n, m)` to `(0, 0)`, emitting per-line `Delete`,
/// `Insert`, and `Equal` ops, then reverses them into forward order before
/// appending to `out`. `n`/`m` are the lengths of the (trimmed) old/new slices.
fn backtrack(
    n: isize,
    m: isize,
    trace: &[Vec<isize>],
    d_end: usize,
    offset: isize,
    out: &mut Vec<DiffOp>,
) {
    let mut x = n;
    let mut y = m;
    let mut rev: Vec<DiffOp> = Vec::new();

    for d in (0..=d_end).rev() {
        let v = &trace[d];
        let k = x - y;
        // Determine the predecessor diagonal, mirroring the forward step rule.
        let prev_k = if k == -(d as isize)
            || (k != d as isize && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize])
        {
            k + 1 // came from a down move (insert)
        } else {
            k - 1 // came from a right move (delete)
        };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;

        // Emit the diagonal (equal) moves taken after reaching the predecessor.
        while x > prev_x && y > prev_y {
            rev.push(DiffOp::Equal(1));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                // Down move: an insertion of new[prev_y].
                rev.push(DiffOp::Insert(1));
            } else {
                // Right move: a deletion of old[prev_x].
                rev.push(DiffOp::Delete(1));
            }
            x = prev_x;
            y = prev_y;
        }
    }

    rev.reverse();
    out.extend(rev);
}

/// Merge adjacent ops of the same kind so the script is compact.
fn coalesce_ops(ops: Vec<DiffOp>) -> Vec<DiffOp> {
    let mut out: Vec<DiffOp> = Vec::with_capacity(ops.len());
    for op in ops {
        match (out.last_mut(), op) {
            (Some(DiffOp::Equal(prev)), DiffOp::Equal(n)) => *prev += n,
            (Some(DiffOp::Delete(prev)), DiffOp::Delete(n)) => *prev += n,
            (Some(DiffOp::Insert(prev)), DiffOp::Insert(n)) => *prev += n,
            _ => out.push(op),
        }
    }
    out
}

// ===========================================================================
// Whitespace-ignoring line comparison (git xdiff's XDF_WHITESPACE_FLAGS).
//
// git's xdiff compares two records (lines, including the trailing `\n`) for
// equality under whitespace-ignore flags via `xdl_recmatch`. Rather than
// re-implement the Myers core to take a custom equality predicate, we map each
// flavour to a *canonicalization* of the line bytes that produces identical
// output iff `xdl_recmatch` would return 1, then diff on the canonicalized
// lines while emitting the original bytes. This is exact: it is a behavioural
// port of `xdiff/xutils.c:xdl_recmatch` and `xdl_blankline`.
// ===========================================================================

/// Whitespace-ignore flags for line comparison, mirroring git's
/// `XDF_WHITESPACE_FLAGS` (`-w`, `-b`, `--ignore-space-at-eol`,
/// `--ignore-cr-at-eol`). Only one of the whitespace flavours is honoured per
/// git's precedence (`-w` ⊃ `-b` ⊃ `--ignore-space-at-eol` ⊃
/// `--ignore-cr-at-eol`); when several are set, the strongest wins, matching
/// the cascade in `xdl_recmatch`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WsIgnore {
    /// `-w` / `--ignore-all-space`: ignore all whitespace when comparing lines.
    pub all_space: bool,
    /// `-b` / `--ignore-space-change`: ignore changes in amount of whitespace.
    pub space_change: bool,
    /// `--ignore-space-at-eol`: ignore whitespace at end of line.
    pub space_at_eol: bool,
    /// `--ignore-cr-at-eol`: ignore a carriage-return at end of line.
    pub cr_at_eol: bool,
}

impl WsIgnore {
    /// True when no whitespace-ignore flavour is active.
    pub fn is_empty(&self) -> bool {
        !(self.all_space || self.space_change || self.space_at_eol || self.cr_at_eol)
    }
}

/// `XDL_ISSPACE` — git uses C `isspace` over the unsigned byte (space, `\t`,
/// `\n`, `\r`, `\x0b` vertical tab, `\x0c` form feed).
#[inline]
fn xdl_isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Canonicalize a line's bytes (including any trailing `\n`) for whitespace-
/// insensitive comparison, exactly mirroring `xdl_recmatch`'s acceptance set:
/// two original lines are equal under `ignore` iff their canonical forms are
/// byte-identical.
///
/// * `all_space` (`-w`): drop every whitespace byte.
/// * `space_change` (`-b`): collapse each run of whitespace to a single `' '`
///   and strip trailing whitespace (a run on one side matches a run on the
///   other regardless of length; leading/internal whitespace must still align,
///   trailing whitespace is dropped entirely).
/// * `space_at_eol`: strip trailing whitespace only.
/// * `cr_at_eol`: drop a single `\r` immediately before a terminating `\n`.
///
/// Exposed crate-internally so the change-compaction pass in [`crate::render`]
/// can compare lines for sliding under the exact same equality the line-level
/// diff uses (git's `recs_match` on the whitespace-canonicalized record).
pub(crate) fn canonicalize_line_for_match(line: &[u8], ignore: WsIgnore) -> Vec<u8> {
    canonicalize_line(line, ignore)
}

fn canonicalize_line(line: &[u8], ignore: WsIgnore) -> Vec<u8> {
    if ignore.all_space {
        return line.iter().copied().filter(|&c| !xdl_isspace(c)).collect();
    }
    if ignore.space_change {
        let mut out = Vec::with_capacity(line.len());
        let mut i = 0usize;
        while i < line.len() {
            if xdl_isspace(line[i]) {
                // Collapse the whole whitespace run to a single space.
                while i < line.len() && xdl_isspace(line[i]) {
                    i += 1;
                }
                out.push(b' ');
            } else {
                out.push(line[i]);
                i += 1;
            }
        }
        // Strip a trailing collapsed-space (trailing whitespace is ignored).
        if out.last() == Some(&b' ') {
            out.pop();
        }
        return out;
    }
    if ignore.space_at_eol {
        let mut end = line.len();
        while end > 0 && xdl_isspace(line[end - 1]) {
            end -= 1;
        }
        return line[..end].to_vec();
    }
    if ignore.cr_at_eol {
        // Drop a `\r` directly before a terminating `\n`.
        if let Some(stripped) = line.strip_suffix(b"\n") {
            if let Some(without_cr) = stripped.strip_suffix(b"\r") {
                let mut out = without_cr.to_vec();
                out.push(b'\n');
                return out;
            }
        } else if let Some(without_cr) = line.strip_suffix(b"\r") {
            // Incomplete final line: a bare trailing `\r` is also ignored.
            return without_cr.to_vec();
        }
        return line.to_vec();
    }
    line.to_vec()
}

/// `xdl_blankline`: a line is "blank" when, after applying the active
/// whitespace flags, it has no content. With no whitespace flags, git treats a
/// record of size ≤ 1 (empty, or a lone `\n`) as blank; with flags, a line all
/// of whose bytes are whitespace is blank.
fn line_is_blank(line: &[u8], ignore: WsIgnore) -> bool {
    if ignore.is_empty() {
        line.len() <= 1
    } else {
        line.iter().all(|&c| xdl_isspace(c))
    }
}

/// Compute a line-level edit script transforming `old` into `new`, comparing
/// lines under the whitespace-ignore flags `ignore` while the returned ops
/// still index the *original* lines position-for-position.
///
/// When `ignore.is_empty()`, this is identical to [`myers_diff_lines`]. With
/// flags, lines are canonicalized (see [`canonicalize_line`]) for the equality
/// test only; the ops consume the same number of old/new lines as the originals
/// so the caller can render the original bytes.
pub fn myers_diff_lines_ws(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    ignore: WsIgnore,
    algorithm: DiffAlgorithm,
) -> Vec<DiffOp> {
    if ignore.is_empty() {
        return diff_lines_with_algorithm(old, new, algorithm);
    }
    let old_canon: Vec<Vec<u8>> = old
        .iter()
        .map(|l| canonicalize_line(l.content, ignore))
        .collect();
    let new_canon: Vec<Vec<u8>> = new
        .iter()
        .map(|l| canonicalize_line(l.content, ignore))
        .collect();
    let old_lines: Vec<DiffLine<'_>> = old_canon
        .iter()
        .map(|c| DiffLine {
            content: c.as_slice(),
            has_newline: true,
        })
        .collect();
    let new_lines: Vec<DiffLine<'_>> = new_canon
        .iter()
        .map(|c| DiffLine {
            content: c.as_slice(),
            has_newline: true,
        })
        .collect();
    diff_lines_with_algorithm(&old_lines, &new_lines, algorithm)
}

// ===========================================================================
// Alternative diff algorithms: patience and histogram.
//
// Both share the recursive "anchor and recurse" shape used by git's xdiff
// implementations of `--patience` and `--histogram`:
//
//   1. trim the common prefix and suffix of the current line range,
//   2. pick one or more common lines that are confidently aligned (the
//      "anchors") according to the algorithm's rule,
//   3. recurse on the gaps to the left of, between, and to the right of the
//      anchors,
//   4. when no anchor can be found, fall back to the Myers shortest-edit-script
//      search for that range so the result is still a valid LCS-correct diff.
//
// They operate purely on slices of [`DiffLine`]s and emit the same coalesced
// [`DiffOp`] run sequence as [`myers_diff_lines`], so any caller can swap
// algorithms freely. The two functions differ only in the anchor-selection
// rule in steps 2/3.
// ===========================================================================

/// A hashable key for a line, used to bucket equal lines when finding anchors.
///
/// Mirrors [`DiffLine`]'s `PartialEq`: two lines are the same iff their bytes
/// and their trailing-newline flag match. Keying on this tuple lets us hash
/// lines without changing the public [`DiffLine`] type.
type LineKey<'a> = (&'a [u8], bool);

#[inline]
fn line_key<'a>(line: &DiffLine<'a>) -> LineKey<'a> {
    (line.content, line.has_newline)
}

/// Compute a line-level edit script transforming `old` into `new` using the
/// patience diff algorithm (Bram Cohen's algorithm, as in `git diff
/// --patience`).
///
/// Patience diff anchors on lines that occur *exactly once* in both `old` and
/// `new`; it aligns those unique lines via a longest-increasing-subsequence
/// ("patience sorting") pass and recurses into the gaps, falling back to Myers
/// when a gap has no unique common line. The result is a valid LCS-correct edit
/// script with the same shape as [`myers_diff_lines`]: walking it reconstructs
/// `new` from `old`, and every [`DiffOp::Equal`] run covers genuinely equal
/// lines. Patience tends to produce more human-readable hunks than Myers when
/// blocks of lines are moved or repeated, though it is not guaranteed to be a
/// shortest edit script.
pub fn patience_diff_lines(old: &[DiffLine<'_>], new: &[DiffLine<'_>]) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();
    patience_recurse(old, new, 0, old.len(), 0, new.len(), &mut ops);
    coalesce_ops(ops)
}

/// Compute a line-level edit script transforming `old` into `new` using the
/// histogram diff algorithm (as in `git diff --histogram`, derived from JGit).
///
/// Histogram diff is a patience-style unique-anchor algorithm with a fallback:
/// it builds an occurrence histogram of `old` and, scanning `new`, picks the
/// longest run of matching lines whose `old` line has the *fewest* occurrences
/// (preferring truly unique lines, like patience, but still able to anchor on
/// low-frequency lines when no globally-unique line exists). It then recurses
/// on the regions on either side of that run, falling back to Myers only when
/// no common line exists in a region. The result is a valid LCS-correct edit
/// script with the same shape as [`myers_diff_lines`].
pub fn histogram_diff_lines(old: &[DiffLine<'_>], new: &[DiffLine<'_>]) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();
    histogram_recurse(old, new, 0, old.len(), 0, new.len(), &mut ops);
    coalesce_ops(ops)
}

/// Dispatch to the line-diff implementation selected by `algorithm`.
///
/// All variants return the same coalesced [`DiffOp`] run sequence as
/// [`myers_diff_lines`], so callers can switch algorithms without changing how
/// they consume the result.
///
/// - [`DiffAlgorithm::Myers`] and [`DiffAlgorithm::Minimal`] use the Myers
///   O(ND) shortest-edit-script search ([`myers_diff_lines`]); that search is
///   already minimal in deletions + insertions, so `Minimal` is an alias for
///   it here rather than a distinct slower mode.
/// - [`DiffAlgorithm::Patience`] uses [`patience_diff_lines`].
/// - [`DiffAlgorithm::Histogram`] uses [`histogram_diff_lines`].
pub fn diff_lines_with_algorithm(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    algorithm: DiffAlgorithm,
) -> Vec<DiffOp> {
    match algorithm {
        DiffAlgorithm::Myers | DiffAlgorithm::Minimal => myers_diff_lines(old, new),
        DiffAlgorithm::Patience => patience_diff_lines(old, new),
        DiffAlgorithm::Histogram => histogram_diff_lines(old, new),
    }
}

/// Emit ops for an empty-on-one-side range; returns `true` if it handled it.
///
/// Covers the recursion base cases where one side of `old[a0..a1]` /
/// `new[b0..b1]` is empty: a pure deletion, a pure insertion, or nothing at
/// all. Used by both the patience and histogram recursions before they look
/// for an anchor.
fn emit_trivial_range(a0: usize, a1: usize, b0: usize, b1: usize, out: &mut Vec<DiffOp>) -> bool {
    let old_len = a1 - a0;
    let new_len = b1 - b0;
    if old_len == 0 && new_len == 0 {
        return true;
    }
    if old_len == 0 {
        out.push(DiffOp::Insert(new_len));
        return true;
    }
    if new_len == 0 {
        out.push(DiffOp::Delete(old_len));
        return true;
    }
    false
}

/// Trim the common prefix/suffix of `old[a0..a1]` vs `new[b0..b1]`.
///
/// Emits an `Equal` for the matched prefix immediately, returns the inner
/// (still-differing) range, and reports the matched-suffix length so the caller
/// can emit its `Equal` *after* it has processed the inner range. This keeps
/// the per-range work proportional to the actual edit, mirroring the prefix /
/// suffix trim in [`myers_diff_lines`].
fn trim_common(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    mut a0: usize,
    mut a1: usize,
    mut b0: usize,
    mut b1: usize,
    out: &mut Vec<DiffOp>,
) -> (usize, usize, usize, usize, usize) {
    let mut prefix = 0usize;
    while a0 < a1 && b0 < b1 && old[a0] == new[b0] {
        a0 += 1;
        b0 += 1;
        prefix += 1;
    }
    if prefix > 0 {
        out.push(DiffOp::Equal(prefix));
    }
    let mut suffix = 0usize;
    while a1 > a0 && b1 > b0 && old[a1 - 1] == new[b1 - 1] {
        a1 -= 1;
        b1 -= 1;
        suffix += 1;
    }
    (a0, a1, b0, b1, suffix)
}

/// Recursive patience-diff worker over `old[a0..a1]` vs `new[b0..b1]`.
fn patience_recurse(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
    out: &mut Vec<DiffOp>,
) {
    if emit_trivial_range(a0, a1, b0, b1, out) {
        return;
    }
    let (a0, a1, b0, b1, suffix) = trim_common(old, new, a0, a1, b0, b1, out);
    if !emit_trivial_range(a0, a1, b0, b1, out) {
        match patience_anchors(old, new, a0, a1, b0, b1) {
            Some(anchors) => {
                // Walk the aligned anchors in order, recursing into each gap
                // before emitting the anchor line as Equal.
                let mut cur_a = a0;
                let mut cur_b = b0;
                for (ai, bi) in anchors {
                    patience_recurse(old, new, cur_a, ai, cur_b, bi, out);
                    out.push(DiffOp::Equal(1));
                    cur_a = ai + 1;
                    cur_b = bi + 1;
                }
                // Tail after the last anchor.
                patience_recurse(old, new, cur_a, a1, cur_b, b1, out);
            }
            // No unique common line in this range: defer to Myers, which always
            // yields a valid (and minimal) script for the leftover block.
            None => myers_core(&old[a0..a1], &new[b0..b1], out),
        }
    }
    if suffix > 0 {
        out.push(DiffOp::Equal(suffix));
    }
}

/// Find the patience anchors for `old[a0..a1]` vs `new[b0..b1]`.
///
/// An anchor is a line that occurs exactly once in `old[a0..a1]` and exactly
/// once in `new[b0..b1]`. The matched (old_index, new_index) pairs are reduced
/// to their longest increasing subsequence by new-index (the patience-sort LCS)
/// so the returned anchors are strictly increasing in *both* indices and can be
/// used as split points. Returns `None` when there are no such unique common
/// lines (the caller then falls back to Myers).
fn patience_anchors(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
) -> Option<Vec<(usize, usize)>> {
    // Count occurrences and remember the (single) position of each line in each
    // side's range. `count > 1` poisons the position so we can ignore it.
    struct Occ {
        count: usize,
        pos: usize,
    }
    let mut in_old: HashMap<LineKey<'_>, Occ> = HashMap::new();
    for (i, line) in old.iter().enumerate().take(a1).skip(a0) {
        in_old
            .entry(line_key(line))
            .and_modify(|o| o.count += 1)
            .or_insert(Occ { count: 1, pos: i });
    }
    let mut in_new: HashMap<LineKey<'_>, Occ> = HashMap::new();
    for (j, line) in new.iter().enumerate().take(b1).skip(b0) {
        in_new
            .entry(line_key(line))
            .and_modify(|o| o.count += 1)
            .or_insert(Occ { count: 1, pos: j });
    }

    // Collect lines unique in both, ordered by their position in `old`.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (i, line) in old.iter().enumerate().take(a1).skip(a0) {
        let key = line_key(line);
        let Some(o) = in_old.get(&key) else { continue };
        if o.count != 1 || o.pos != i {
            continue;
        }
        // A line unique in both ranges is a candidate anchor.
        if let Some(n) = in_new.get(&key)
            && n.count == 1
        {
            pairs.push((i, n.pos));
        }
    }
    if pairs.is_empty() {
        return None;
    }

    // Patience sort: longest increasing subsequence of new-indices. `pairs` is
    // already sorted by old-index, so an LIS by new-index yields a set of
    // anchors increasing in both coordinates.
    let lis = longest_increasing_by_new(&pairs);
    if lis.is_empty() { None } else { Some(lis) }
}

/// Longest increasing subsequence of `pairs` (sorted by old-index) keyed on the
/// new-index, returned as the chosen (old_index, new_index) pairs in order.
///
/// This is the patience-sorting core: standard O(k log k) LIS with predecessor
/// links so the actual subsequence (not just its length) is recovered. Because
/// the input is pre-sorted by old-index and the new-indices are distinct, the
/// result is strictly increasing in both coordinates.
fn longest_increasing_by_new(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if pairs.is_empty() {
        return Vec::new();
    }
    // tails[len-1] = index into `pairs` of the smallest possible tail value of
    // an increasing subsequence of length `len`.
    let mut tails: Vec<usize> = Vec::new();
    // prev[i] = index into `pairs` of the predecessor of pairs[i] in its LIS.
    let mut prev: Vec<Option<usize>> = vec![None; pairs.len()];

    for i in 0..pairs.len() {
        let val = pairs[i].1;
        // Binary search for the first tail whose new-index is >= val.
        let mut lo = 0usize;
        let mut hi = tails.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pairs[tails[mid]].1 < val {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo > 0 {
            prev[i] = Some(tails[lo - 1]);
        }
        if lo == tails.len() {
            tails.push(i);
        } else {
            tails[lo] = i;
        }
    }

    // Reconstruct by following predecessor links from the last tail.
    let mut result: Vec<(usize, usize)> = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        result.push(pairs[i]);
        cur = prev[i];
    }
    result.reverse();
    result
}

/// Recursive histogram-diff worker over `old[a0..a1]` vs `new[b0..b1]`.
fn histogram_recurse(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
    out: &mut Vec<DiffOp>,
) {
    if emit_trivial_range(a0, a1, b0, b1, out) {
        return;
    }
    let (a0, a1, b0, b1, suffix) = trim_common(old, new, a0, a1, b0, b1, out);
    if !emit_trivial_range(a0, a1, b0, b1, out) {
        match histogram_region(old, new, a0, a1, b0, b1) {
            Some(region) => {
                // Recurse left of the matched run, emit the run as Equal, then
                // recurse right of it.
                histogram_recurse(old, new, a0, region.old_start, b0, region.new_start, out);
                out.push(DiffOp::Equal(region.len));
                histogram_recurse(
                    old,
                    new,
                    region.old_start + region.len,
                    a1,
                    region.new_start + region.len,
                    b1,
                    out,
                );
            }
            // No common line at all in this range: hand it to Myers.
            None => myers_core(&old[a0..a1], &new[b0..b1], out),
        }
    }
    if suffix > 0 {
        out.push(DiffOp::Equal(suffix));
    }
}

/// The longest common run chosen by the histogram heuristic for one range.
struct HistogramRegion {
    old_start: usize,
    new_start: usize,
    len: usize,
}

/// Choose the histogram anchor run for `old[a0..a1]` vs `new[b0..b1]`.
///
/// Builds an occurrence histogram of the `old` range, then scans the `new`
/// range. For each `new` line that also appears in `old`, it extends a matching
/// run backward and forward and scores candidate alignments, preferring the run
/// whose anchoring `old` line has the *fewest* occurrences (ties broken by run
/// length, then by earliest position). This is the JGit/`git --histogram`
/// heuristic: rare lines make the most reliable anchors. Returns `None` if no
/// `new` line appears in the `old` range.
fn histogram_region(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
) -> Option<HistogramRegion> {
    // Occurrence count and the list of positions of each line within old[a0..a1].
    let mut buckets: HashMap<LineKey<'_>, Vec<usize>> = HashMap::new();
    for (i, line) in old.iter().enumerate().take(a1).skip(a0) {
        buckets.entry(line_key(line)).or_default().push(i);
    }

    let mut best: Option<HistogramRegion> = None;
    // Lower occurrence count is better; among equal counts, longer run wins.
    let mut best_count = usize::MAX;
    let mut best_len = 0usize;

    let mut bj = b0;
    while bj < b1 {
        let key = line_key(&new[bj]);
        let Some(positions) = buckets.get(&key) else {
            bj += 1;
            continue;
        };
        let occ = positions.len();
        // For every place this line sits in `old`, measure the maximal matching
        // run that passes through (positions[*], bj).
        let mut next_bj = bj + 1;
        for &ai in positions {
            // Extend backward while lines keep matching and we stay in range.
            let mut start_a = ai;
            let mut start_b = bj;
            while start_a > a0 && start_b > b0 && old[start_a - 1] == new[start_b - 1] {
                start_a -= 1;
                start_b -= 1;
            }
            // Extend forward from the run start.
            let mut len = 0usize;
            while start_a + len < a1
                && start_b + len < b1
                && old[start_a + len] == new[start_b + len]
            {
                len += 1;
            }
            // Score this run by the rarest occurrence count along it; using the
            // anchor line's own count is the standard, cheaper approximation.
            let run_count = occ;
            let better = run_count < best_count || (run_count == best_count && len > best_len);
            if better && len > 0 {
                best_count = run_count;
                best_len = len;
                best = Some(HistogramRegion {
                    old_start: start_a,
                    new_start: start_b,
                    len,
                });
                // Skip past this matched run in `new` so we do not re-evaluate
                // every interior line of the same run from scratch.
                if start_b + len > next_bj {
                    next_bj = start_b + len;
                }
            }
        }
        bj = next_bj.max(bj + 1);
    }

    best
}

/// Which conflict-marker style [`merge_blobs`] emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictStyle {
    /// Standard two-section markers (`<<<<<<<` / `=======` / `>>>>>>>`).
    #[default]
    Merge,
    /// `diff3` style: also include the common-ancestor section between `ours`
    /// and the `=======` divider, delimited by `|||||||`.
    Diff3,
}

/// Labels and style controlling [`merge_blobs`] conflict markers.
#[derive(Debug, Clone, Copy)]
pub struct MergeBlobOptions<'a> {
    /// Label after the opening `<<<<<<<` marker (typically the local branch).
    pub ours_label: &'a str,
    /// Label after the closing `>>>>>>>` marker (typically the other branch).
    pub theirs_label: &'a str,
    /// Label after the `|||||||` marker (only used for [`ConflictStyle::Diff3`]).
    pub base_label: &'a str,
    /// Which marker style to emit.
    pub style: ConflictStyle,
    /// How to resolve a textual conflict. [`MergeFavor::Union`] keeps both sides'
    /// lines with no markers (and a non-conflicted result); other values leave
    /// markers (favouring ours/theirs is applied by the caller at the file level).
    pub favor: MergeFavor,
}

impl Default for MergeBlobOptions<'_> {
    fn default() -> Self {
        Self {
            ours_label: "ours",
            theirs_label: "theirs",
            base_label: "base",
            style: ConflictStyle::Merge,
            favor: MergeFavor::None,
        }
    }
}

/// The outcome of a 3-way blob merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeBlobResult {
    /// The merged blob bytes, including any conflict markers.
    pub content: Vec<u8>,
    /// True when at least one region conflicted and markers were written.
    pub conflicted: bool,
}

/// Perform a 3-way merge of three blobs using the diff3 algorithm.
///
/// `base` is the common ancestor; `ours` and `theirs` are the two sides. The
/// merge diffs base→ours and base→theirs (with [`myers_diff_lines`]) and walks
/// the base in lockstep:
/// - regions unchanged on both sides emit the base lines unchanged;
/// - regions changed on exactly one side take that side's lines;
/// - regions changed on both sides emit the side lines if they are
///   byte-identical, otherwise a conflict (and [`MergeBlobResult::conflicted`]
///   is set).
///
/// An empty `base` is supported: every line is then "added on both sides", so
/// the result is the shared content if `ours == theirs`, else a single
/// conflict (add/add).
pub fn merge_blobs(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    options: &MergeBlobOptions<'_>,
) -> MergeBlobResult {
    let base_lines = split_lines(base);
    let ours_lines = split_lines(ours);
    let theirs_lines = split_lines(theirs);

    // Per-side matched (equal) base regions, paired with the corresponding side
    // ranges, computed via Myers.
    let ours_matches = matching_regions(&base_lines, &ours_lines);
    let theirs_matches = matching_regions(&base_lines, &theirs_lines);

    // Intersect the two match lists to get segments of base that are unchanged
    // on BOTH sides, each carrying the exact aligned side indices. Between these
    // common-stable segments lie the (potentially conflicting) changed regions.
    let stable = common_stable_segments(&ours_matches, &theirs_matches);

    let mut writer = MergeWriter::new(options);
    // Cursors: next unconsumed line in base, ours, theirs.
    let mut base_idx = 0usize;
    let mut our_idx = 0usize;
    let mut their_idx = 0usize;

    for seg in &stable {
        // Unstable (changed) region preceding this stable segment.
        let base_region = &base_lines[base_idx..seg.base_start];
        let our_region = &ours_lines[our_idx..seg.ours_start];
        let their_region = &theirs_lines[their_idx..seg.theirs_start];
        emit_region(&mut writer, base_region, our_region, their_region);

        // The stable segment itself is identical on all three: emit base lines.
        writer.emit_lines(&base_lines[seg.base_start..seg.base_start + seg.len]);

        base_idx = seg.base_start + seg.len;
        our_idx = seg.ours_start + seg.len;
        their_idx = seg.theirs_start + seg.len;
    }

    // Trailing unstable region after the last stable segment (or the whole input
    // when there are no common-stable segments).
    emit_region(
        &mut writer,
        &base_lines[base_idx..],
        &ours_lines[our_idx..],
        &theirs_lines[their_idx..],
    );

    writer.finish()
}

/// Resolve and emit one changed region (the gap between two common-stable
/// segments) according to diff3 rules.
fn emit_region(
    writer: &mut MergeWriter<'_>,
    base_region: &[DiffLine<'_>],
    our_region: &[DiffLine<'_>],
    their_region: &[DiffLine<'_>],
) {
    if our_region.is_empty() && their_region.is_empty() {
        return;
    }
    let our_changed = our_region != base_region;
    let their_changed = their_region != base_region;
    match (our_changed, their_changed) {
        (false, false) => writer.emit_lines(base_region),
        (true, false) => writer.emit_lines(our_region),
        (false, true) => writer.emit_lines(their_region),
        (true, true) => {
            if our_region == their_region {
                // Both sides made the same change: no conflict.
                writer.emit_lines(our_region);
            } else {
                writer.emit_conflict_refined(our_region, base_region, their_region);
            }
        }
    }
}

/// One unit produced by zealous conflict refinement: either context lines shared
/// by both sides (emitted verbatim) or a minimal conflict spanning the named
/// ours/theirs line ranges.
enum RefineItem {
    Context(std::ops::Range<usize>),
    Conflict(std::ops::Range<usize>, std::ops::Range<usize>),
}

/// git's `xdl_refine_conflicts` + `xdl_simplify_non_conflicts` (level
/// `XDL_MERGE_ZEALOUS`): re-diff the two conflicting sides against each other,
/// factor the lines they share out of the conflict as context, and split the
/// remainder into the minimal set of conflicting hunks — then re-merge any two
/// conflicts separated by 3 or fewer context lines (the smaller-output rule).
///
/// Ranges index into `ours`/`theirs`; `Context` ranges are in ours coordinates
/// (the shared lines are identical on both sides).
fn refine_conflict_items(ours: &[DiffLine<'_>], theirs: &[DiffLine<'_>]) -> Vec<RefineItem> {
    // Coalesce the ours-vs-theirs diff into alternating context (equal) and
    // conflict (changed) runs.
    let ops = myers_diff_lines(ours, theirs);
    let mut raw: Vec<RefineItem> = Vec::new();
    let mut oi = 0usize;
    let mut ti = 0usize;
    let mut pending: Option<(usize, usize, usize, usize)> = None; // o0,o1,t0,t1
    for op in ops {
        match op {
            DiffOp::Equal(n) => {
                if let Some((o0, o1, t0, t1)) = pending.take() {
                    raw.push(RefineItem::Conflict(o0..o1, t0..t1));
                }
                raw.push(RefineItem::Context(oi..oi + n));
                oi += n;
                ti += n;
            }
            DiffOp::Delete(n) => {
                let entry = pending.get_or_insert((oi, oi, ti, ti));
                entry.1 = oi + n;
                oi += n;
            }
            DiffOp::Insert(n) => {
                let entry = pending.get_or_insert((oi, oi, ti, ti));
                entry.3 = ti + n;
                ti += n;
            }
        }
    }
    if let Some((o0, o1, t0, t1)) = pending.take() {
        raw.push(RefineItem::Conflict(o0..o1, t0..t1));
    }

    // Merge two conflicts when the context between them is <= 3 lines: the
    // absorbed context lines are identical on both sides, so they fold into the
    // combined conflict's ours and theirs ranges alike.
    let mut out: Vec<RefineItem> = Vec::new();
    let mut idx = 0usize;
    while idx < raw.len() {
        match &raw[idx] {
            RefineItem::Context(range) => {
                let small = range.len() <= 3;
                let prev_conflict = matches!(out.last(), Some(RefineItem::Conflict(..)));
                let next_conflict = matches!(raw.get(idx + 1), Some(RefineItem::Conflict(..)));
                if small && prev_conflict && next_conflict {
                    let Some(RefineItem::Conflict(po, pt)) = out.pop() else {
                        unreachable!()
                    };
                    let RefineItem::Conflict(no, nt) = &raw[idx + 1] else {
                        unreachable!()
                    };
                    out.push(RefineItem::Conflict(po.start..no.end, pt.start..nt.end));
                    idx += 2;
                } else {
                    out.push(RefineItem::Context(range.clone()));
                    idx += 1;
                }
            }
            RefineItem::Conflict(o, t) => {
                out.push(RefineItem::Conflict(o.clone(), t.clone()));
                idx += 1;
            }
        }
    }
    out
}

/// A matched (equal) region between `base` and one side: `base_start..+len`
/// lines of base equal `side_start..+len` lines of that side.
#[derive(Debug, Clone, Copy)]
struct MatchRegion {
    base_start: usize,
    side_start: usize,
    len: usize,
}

/// A run of base lines unchanged on *both* sides, with the aligned side starts.
#[derive(Debug, Clone, Copy)]
struct StableSegment {
    base_start: usize,
    ours_start: usize,
    theirs_start: usize,
    len: usize,
}

/// Compute the matched regions between base and a side using [`myers_diff_lines`].
///
/// Each `Equal(n)` run becomes a [`MatchRegion`]; the regions are returned in
/// increasing base order. (Equal runs are coalesced by the diff, so adjacent
/// regions are already maximal.)
fn matching_regions(base: &[DiffLine<'_>], side: &[DiffLine<'_>]) -> Vec<MatchRegion> {
    let ops = myers_diff_lines(base, side);
    let mut regions = Vec::new();
    let mut base_idx = 0usize;
    let mut side_idx = 0usize;
    for op in ops {
        match op {
            DiffOp::Equal(n) => {
                regions.push(MatchRegion {
                    base_start: base_idx,
                    side_start: side_idx,
                    len: n,
                });
                base_idx += n;
                side_idx += n;
            }
            DiffOp::Delete(n) => base_idx += n,
            DiffOp::Insert(n) => side_idx += n,
        }
    }
    regions
}

/// Intersect the ours/theirs match lists (both in base coordinates) to find the
/// base ranges unchanged on both sides, recording the aligned side indices.
///
/// For each overlapping pair of base ranges `[bs, be)` the ours-side index of
/// `bs` is `o.side_start + (bs - o.base_start)` and likewise for theirs; both
/// map contiguously across the overlap. The returned segments are in increasing
/// base order and never overlap.
fn common_stable_segments(ours: &[MatchRegion], theirs: &[MatchRegion]) -> Vec<StableSegment> {
    let mut segments = Vec::new();
    let mut oi = 0usize;
    let mut ti = 0usize;
    while oi < ours.len() && ti < theirs.len() {
        let o = ours[oi];
        let t = theirs[ti];
        let o_end = o.base_start + o.len;
        let t_end = t.base_start + t.len;
        let lo = o.base_start.max(t.base_start);
        let hi = o_end.min(t_end);
        if lo < hi {
            segments.push(StableSegment {
                base_start: lo,
                ours_start: o.side_start + (lo - o.base_start),
                theirs_start: t.side_start + (lo - t.base_start),
                len: hi - lo,
            });
        }
        // Advance whichever range ends first.
        if o_end <= t_end {
            oi += 1;
        } else {
            ti += 1;
        }
    }
    segments
}

/// Accumulates merged output and renders conflict markers byte-for-byte like
/// upstream git.
struct MergeWriter<'a> {
    out: Vec<u8>,
    conflicted: bool,
    options: &'a MergeBlobOptions<'a>,
}

impl<'a> MergeWriter<'a> {
    fn new(options: &'a MergeBlobOptions<'a>) -> Self {
        Self {
            out: Vec::new(),
            conflicted: false,
            options,
        }
    }

    /// Append raw line bytes (each line already carries its own newline, except
    /// possibly a final no-newline line).
    fn emit_lines(&mut self, lines: &[DiffLine<'_>]) {
        for line in lines {
            self.out.extend_from_slice(line.content);
        }
    }

    /// Emit a conflict hunk. Conflict markers always begin on their own line,
    /// so if the preceding emitted content did not end in a newline (a
    /// no-newline-at-end side), insert one first — matching git, which prints
    /// the "\ No newline at end of file" content followed by a newline before
    /// the next marker.
    fn emit_conflict(
        &mut self,
        ours: &[DiffLine<'_>],
        base: &[DiffLine<'_>],
        theirs: &[DiffLine<'_>],
    ) {
        // Union: keep both sides' lines (ours then theirs) with no markers, and do
        // NOT flag a conflict — git's `XDL_MERGE_FAVOR_UNION`.
        if self.options.favor == MergeFavor::Union {
            self.emit_section(ours);
            self.ensure_newline();
            self.emit_section(theirs);
            return;
        }
        self.conflicted = true;
        self.write_marker(b'<', self.options.ours_label);
        self.emit_section(ours);
        if self.options.style == ConflictStyle::Diff3 {
            self.ensure_newline();
            self.write_marker(b'|', self.options.base_label);
            self.emit_section(base);
        }
        self.ensure_newline();
        self.write_divider();
        self.emit_section(theirs);
        self.ensure_newline();
        self.write_marker(b'>', self.options.theirs_label);
    }

    /// Emit a conflict with git's zealous refinement applied. The default
    /// (non-diff3) merge re-diffs the two sides to shrink the conflict to the
    /// lines that genuinely differ (`xdl_refine_conflicts`); diff3-style output
    /// keeps the conflict whole (the base section straddles it), a favored merge
    /// resolves at a coarser granularity, and an empty side cannot be refined —
    /// all three fall back to a single unrefined conflict hunk.
    fn emit_conflict_refined(
        &mut self,
        ours: &[DiffLine<'_>],
        base: &[DiffLine<'_>],
        theirs: &[DiffLine<'_>],
    ) {
        if self.options.style == ConflictStyle::Diff3
            || self.options.favor != MergeFavor::None
            || ours.is_empty()
            || theirs.is_empty()
        {
            self.emit_conflict(ours, base, theirs);
            return;
        }
        for item in refine_conflict_items(ours, theirs) {
            match item {
                RefineItem::Context(range) => self.emit_lines(&ours[range]),
                RefineItem::Conflict(o, t) => self.emit_conflict(&ours[o], &[], &theirs[t]),
            }
        }
    }

    /// Emit one side's lines inside a conflict, preserving their exact bytes.
    fn emit_section(&mut self, lines: &[DiffLine<'_>]) {
        for line in lines {
            self.out.extend_from_slice(line.content);
        }
    }

    /// Ensure the buffer ends with a newline before writing the next marker, so
    /// markers always start a fresh line even after a no-newline final line.
    fn ensure_newline(&mut self) {
        if !self.out.is_empty() && self.out.last() != Some(&b'\n') {
            self.out.push(b'\n');
        }
    }

    /// Write a marker line: 7 copies of `ch`, then (if the label is non-empty)
    /// a space and the label, then a newline. No trailing space for an empty
    /// label — byte-for-byte with upstream git.
    fn write_marker(&mut self, ch: u8, label: &str) {
        for _ in 0..7 {
            self.out.push(ch);
        }
        if !label.is_empty() {
            self.out.push(b' ');
            self.out.extend_from_slice(label.as_bytes());
        }
        self.out.push(b'\n');
    }

    /// Write the `=======` divider line (never labelled).
    fn write_divider(&mut self) {
        for _ in 0..7 {
            self.out.push(b'=');
        }
        self.out.push(b'\n');
    }

    fn finish(self) -> MergeBlobResult {
        MergeBlobResult {
            content: self.out,
            conflicted: self.conflicted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAlgorithm {
    Myers,
    Minimal,
    Patience,
    Histogram,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Add { path: RepoPath },
    Delete { path: RepoPath },
    Modify { path: RepoPath },
    Rename { old: RepoPath, new: RepoPath },
    Copy { source: RepoPath, dest: RepoPath },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: RepoPath,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameStatus {
    Added,
    Deleted,
    Modified,
    /// A path whose file type (`S_IFMT` bits of the mode) changed between the two
    /// sides — regular↔symlink, regular↔gitlink, symlink↔gitlink. git renders this
    /// as `T` (`DIFF_STATUS_TYPE_CHANGED`, set by `diffcore.h`'s
    /// `DIFF_PAIR_TYPE_CHANGED` before rename/modify resolution). An exec-bit-only
    /// change (100644↔100755) is NOT a typechange — same `S_IFMT`.
    TypeChanged,
    Renamed(u8),
    Copied(u8),
    /// An unmerged (conflicted) path: the index holds higher-stage entries.
    /// git emits a standalone `U <path>` pair (`diff_unmerge`) for it in
    /// addition to the regular worktree-vs-stage-2 modify.
    Unmerged,
}

impl NameStatus {
    pub const fn code(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::TypeChanged => 'T',
            Self::Renamed(_) => 'R',
            Self::Copied(_) => 'C',
            Self::Unmerged => 'U',
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Renamed(score) => format!("R{score:03}"),
            Self::Copied(score) => format!("C{score:03}"),
            _ => self.code().to_string(),
        }
    }
}

/// The bit mask isolating the file-type bits of a git mode (`S_IFMT`). Regular
/// files are `0o100000`, symlinks `0o120000`, gitlinks `0o160000`, trees
/// `0o040000`.
pub const S_IFMT: u32 = 0o170000;

/// Whether a pair of (non-zero) modes constitutes a git "typechange": the file
/// type bits (`S_IFMT`) differ. Mirrors `diffcore.h`'s `DIFF_PAIR_TYPE_CHANGED`
/// (`(S_IFMT & one->mode) != (S_IFMT & two->mode)`). An exec-bit-only change
/// (`0o100644` ↔ `0o100755`) is NOT a typechange — same `S_IFMT`.
#[must_use]
pub const fn is_type_change(old_mode: u32, new_mode: u32) -> bool {
    (old_mode & S_IFMT) != (new_mode & S_IFMT)
}

/// Classify a both-sides-present change whose entries already differ: a
/// [`NameStatus::TypeChanged`] when the modes' `S_IFMT` bits differ, otherwise a
/// plain [`NameStatus::Modified`]. git sets `DIFF_STATUS_TYPE_CHANGED` before any
/// rename/modify resolution (`diff.c` ~6650).
#[must_use]
pub const fn modify_or_type_change(old_mode: u32, new_mode: u32) -> NameStatus {
    if is_type_change(old_mode, new_mode) {
        NameStatus::TypeChanged
    } else {
        NameStatus::Modified
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusEntry {
    pub status: NameStatus,
    pub path: BString,
    pub old_path: Option<BString>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub old_oid: Option<ObjectId>,
    pub new_oid: Option<ObjectId>,
}

impl NameStatusEntry {
    pub fn line(&self) -> String {
        if let Some(old_path) = &self.old_path {
            format!(
                "{}\t{}\t{}",
                self.status.label(),
                String::from_utf8_lossy(old_path.as_bytes()),
                String::from_utf8_lossy(self.path.as_bytes())
            )
        } else {
            format!(
                "{}\t{}",
                self.status.label(),
                String::from_utf8_lossy(self.path.as_bytes())
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexGitlinkEntry {
    pub path: BString,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexWorktreeDiff {
    pub entries: Vec<NameStatusEntry>,
    pub staged_gitlinks: Vec<IndexGitlinkEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffNameStatusOptions {
    pub detect_renames: bool,
    pub detect_copies: bool,
    pub find_copies_harder: bool,
    pub rename_empty: bool,
}

impl Default for DiffNameStatusOptions {
    fn default() -> Self {
        Self {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
        }
    }
}

/// git's default minimum similarity (as a percentage) for a pair of files to be
/// reported as a rename or copy. Matches `git`'s built-in `-M`/`-C` threshold
/// of 50% (`DEFAULT_RENAME_SCORE` is `MAX_SCORE / 2`).
pub const DEFAULT_RENAME_THRESHOLD: u8 = 50;

/// Options controlling inexact (similarity-based) rename and copy detection,
/// layered additively on top of [`DiffNameStatusOptions`].
///
/// This is a separate struct rather than new fields on [`DiffNameStatusOptions`]
/// so that existing callers — which build `DiffNameStatusOptions` with a struct
/// literal — keep compiling unchanged. Code that wants inexact detection uses
/// the `*_with_rename_options` entry points and this type instead.
///
/// [`Default`] preserves the existing behaviour exactly: `detect_inexact` is
/// `false`, so unless a caller opts in, only exact-OID rename/copy detection
/// runs (identical to the plain `*_with_options` functions). When
/// `detect_inexact` is enabled, files added on one side are paired with the most
/// similar deleted/modified file on the other side whose similarity meets the
/// relevant threshold; exact-OID matches still take priority and are always
/// scored 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenameDetectionOptions {
    /// The base name-status options (rename/copy enable flags, find-copies-harder,
    /// rename-empty). Exact detection honours these exactly as before.
    pub base: DiffNameStatusOptions,
    /// Enable inexact (content-similarity) detection. When `false`, only exact
    /// OID matches are detected, matching the legacy `*_with_options` behaviour.
    pub detect_inexact: bool,
    /// Minimum similarity percentage (`0..=100`) for an inexact *rename*. Pairs
    /// scoring below this are not reported as renames. Defaults to
    /// [`DEFAULT_RENAME_THRESHOLD`].
    pub rename_threshold: u8,
    /// Minimum similarity percentage (`0..=100`) for an inexact *copy*. Defaults
    /// to [`DEFAULT_RENAME_THRESHOLD`]; git uses the same default for `-C` as for
    /// `-M` unless `-C<n>` overrides it.
    pub copy_threshold: u8,
    /// Cap on the inexact rename matrix (git's `diff.renameLimit` /
    /// `merge.renameLimit`): when the number of candidate sources times the
    /// number of candidate destinations exceeds `rename_limit²`, inexact
    /// detection is skipped entirely (only exact-OID renames survive). `0` means
    /// unlimited — git's `too_many_rename_candidates` treats a non-positive limit
    /// the same way.
    pub rename_limit: usize,
}

impl Default for RenameDetectionOptions {
    fn default() -> Self {
        Self {
            base: DiffNameStatusOptions::default(),
            detect_inexact: false,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
        }
    }
}

impl RenameDetectionOptions {
    /// Build inexact-enabled options from a base [`DiffNameStatusOptions`], using
    /// the default thresholds for both renames and copies.
    pub fn inexact(base: DiffNameStatusOptions) -> Self {
        Self {
            base,
            detect_inexact: true,
            ..Self::default()
        }
    }
}

pub fn diff_name_status_head_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_head_worktree_with_options(
        worktree_root,
        git_dir,
        format,
        DiffNameStatusOptions::default(),
    )
}

pub fn diff_name_status_head_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let IndexSnapshot {
        entries: index,
        stat_cache,
    } = read_index_snapshot(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let candidate_paths = candidate_path_set(head.keys().chain(index.keys()));
    let worktree = worktree_entries_for_path_set(
        worktree_root,
        format,
        &candidate_paths,
        &index_gitlinks,
        Some(&stat_cache),
    )?;
    let changes = diff_name_status_maps_for_path_set(&head, &worktree, &candidate_paths, options)?;
    Ok(mark_unstaged_worktree_oids_unresolved(
        changes, &index, &worktree,
    ))
}

/// HEAD-vs-worktree name-status with full rename/copy options, including inexact
/// (similarity) detection when enabled. Worktree blob content is read directly
/// from the working tree; HEAD-side blobs come from the object database.
pub fn diff_name_status_head_worktree_with_rename_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let IndexSnapshot {
        entries: index,
        stat_cache,
    } = read_index_snapshot(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let candidate_paths = candidate_path_set(head.keys().chain(index.keys()));
    let worktree = worktree_entries_for_path_set(
        worktree_root,
        format,
        &candidate_paths,
        &index_gitlinks,
        Some(&stat_cache),
    )?;
    let cache = worktree_blob_cache_for_path_set(
        worktree_root,
        &head,
        &worktree,
        &candidate_paths,
        options,
    )?;
    let changes = diff_name_status_maps_with_renames_for_path_set(
        &head,
        &worktree,
        &candidate_paths,
        options,
        |oid| cache_or_odb_blob(&cache, &db, oid),
    )?;
    Ok(mark_unstaged_worktree_oids_unresolved(
        changes, &index, &worktree,
    ))
}

pub fn diff_name_status_head_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_head_index_with_options(git_dir, format, DiffNameStatusOptions::default())
}

pub fn diff_name_status_head_index_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps(&head, &index, head.keys().chain(index.keys()), options)
}

/// HEAD-vs-index name-status with full rename/copy options, including inexact
/// (similarity) detection when enabled. All blob content (both sides) comes from
/// the object database.
pub fn diff_name_status_head_index_with_rename_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps_with_renames(
        &head,
        &index,
        head.keys().chain(index.keys()),
        options,
        |oid| read_blob_bytes(&db, oid),
    )
}

/// Read an arbitrary tree object's flattened blob entries (recursively) keyed by
/// repository-relative path. This is the tree-side counterpart used by
/// `git diff-index <tree-ish>`: unlike [`head_tree_entries`] it does not consult
/// `HEAD`, so any commit/tag (peeled to a tree) or tree oid can be compared.
///
/// The canonical empty tree (`git hash-object -t tree /dev/null`) is treated as
/// always present and yields no entries, even when the object was never written
/// to the database. git makes the same guarantee, which keeps the common idiom
/// `git diff-index --cached <empty-tree-sha>` working in a fresh repository.
fn tree_entries(
    tree_oid: &ObjectId,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    if *tree_oid == empty_tree_oid(format)? {
        return Ok(entries);
    }
    collect_tree_entries(db, format, tree_oid, Vec::new(), &mut entries)?;
    Ok(entries)
}

/// The well-known oid of the empty tree for `format` (the hash of a zero-length
/// tree object). git hard-codes this value and treats it as always existing.
fn empty_tree_oid(format: ObjectFormat) -> Result<ObjectId> {
    object_id_for_bytes(format, "tree", b"")
}

/// Name-status diff of an arbitrary tree against the index, the engine behind
/// `git diff-index --cached <tree-ish>`. Exact rename/copy detection follows
/// `options`; all blob content comes from the object database.
pub fn diff_name_status_tree_index_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let tree = tree_entries(tree_oid, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps(&tree, &index, tree.keys().chain(index.keys()), options)
}

/// Tree-vs-index name-status with full rename/copy options, including inexact
/// (similarity) detection when enabled. Both sides read blob content from the
/// object database. Counterpart of
/// [`diff_name_status_head_index_with_rename_options`] for an arbitrary tree.
pub fn diff_name_status_tree_index_with_rename_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let tree = tree_entries(tree_oid, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps_with_renames(
        &tree,
        &index,
        tree.keys().chain(index.keys()),
        options,
        |oid| read_blob_bytes(&db, oid),
    )
}

/// Name-status diff of an arbitrary tree against the working tree, the engine
/// behind plain `git diff-index <tree-ish>` (no `--cached`). New-side oids for
/// paths whose worktree contents differ from the index are cleared (rendered as
/// zeros), matching git, which only reports the worktree blob oid when it is
/// known-clean against the index.
pub fn diff_name_status_tree_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let tree = tree_entries(tree_oid, format, &db)?;
    let IndexSnapshot {
        entries: index,
        stat_cache,
    } = read_index_snapshot(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let candidate_paths = candidate_path_set(tree.keys().chain(index.keys()));
    let worktree = worktree_entries_for_path_set(
        worktree_root,
        format,
        &candidate_paths,
        &index_gitlinks,
        Some(&stat_cache),
    )?;
    let changes = diff_name_status_maps_for_path_set(&tree, &worktree, &candidate_paths, options)?;
    Ok(mark_unstaged_worktree_oids_unresolved(
        changes, &index, &worktree,
    ))
}

/// Tree-vs-worktree name-status with full rename/copy options, including inexact
/// (similarity) detection when enabled. Worktree blob content is read directly
/// from the working tree (via an oid-keyed cache); tree-side blobs come from the
/// object database. As with [`diff_name_status_tree_worktree_with_options`],
/// new-side oids for paths that differ from the index are cleared.
pub fn diff_name_status_tree_worktree_with_rename_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let tree = tree_entries(tree_oid, format, &db)?;
    let IndexSnapshot {
        entries: index,
        stat_cache,
    } = read_index_snapshot(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let candidate_paths = candidate_path_set(tree.keys().chain(index.keys()));
    let worktree = worktree_entries_for_path_set(
        worktree_root,
        format,
        &candidate_paths,
        &index_gitlinks,
        Some(&stat_cache),
    )?;
    let cache = worktree_blob_cache_for_path_set(
        worktree_root,
        &tree,
        &worktree,
        &candidate_paths,
        options,
    )?;
    let changes = diff_name_status_maps_with_renames_for_path_set(
        &tree,
        &worktree,
        &candidate_paths,
        options,
        |oid| cache_or_odb_blob(&cache, &db, oid),
    )?;
    Ok(mark_unstaged_worktree_oids_unresolved(
        changes, &index, &worktree,
    ))
}

pub fn diff_name_status_index_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_index_worktree_with_options(
        worktree_root,
        git_dir,
        format,
        DiffNameStatusOptions::default(),
    )
}

pub fn diff_name_status_index_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    Ok(diff_name_status_index_worktree_with_options_and_gitlinks(
        worktree_root,
        git_dir,
        format,
        options,
    )?
    .entries)
}

pub fn diff_name_status_index_worktree_with_options_and_gitlinks(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<IndexWorktreeDiff> {
    let IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    } = diff_name_status_index_worktree_changes(worktree_root.as_ref(), git_dir.as_ref(), format)?;
    let entries = apply_name_status_options_to_index_worktree_changes(entries, options)?;
    Ok(IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    })
}

/// Index-vs-worktree name-status with full rename/copy options, including inexact
/// (similarity) detection when enabled. Worktree blob content is read directly
/// from the working tree; index-side blobs come from the object database.
pub fn diff_name_status_index_worktree_with_rename_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    Ok(
        diff_name_status_index_worktree_with_rename_options_and_gitlinks(
            worktree_root,
            git_dir,
            format,
            options,
        )?
        .entries,
    )
}

pub fn diff_name_status_index_worktree_with_rename_options_and_gitlinks(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: RenameDetectionOptions,
) -> Result<IndexWorktreeDiff> {
    let IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    } = diff_name_status_index_worktree_changes(worktree_root.as_ref(), git_dir.as_ref(), format)?;
    // Index-vs-worktree diffs only consider tracked index paths; untracked
    // worktree files are not additions, so rename/copy detection has no add
    // destinations to pair. Apply the base options for completeness.
    let entries = apply_name_status_options_to_index_worktree_changes(entries, options.base)?;
    Ok(IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    })
}

fn diff_name_status_index_worktree_changes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<IndexWorktreeDiff> {
    let index_path = sley_index::repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IndexWorktreeDiff {
                entries: Vec::new(),
                staged_gitlinks: Vec::new(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    let index_bytes = fs::read(&index_path)?;
    if let Ok(index) = BorrowedIndex::parse(&index_bytes, format)
        && index.extension(&sley_index::INDEX_EXT_LINK)?.is_none()
        && !index.entries.iter().any(borrowed_entry_is_sparse_dir)
    {
        let (has_non_normal_stage, staged_gitlinks) =
            index_worktree_metadata_for_entries(&index.entries);
        if has_non_normal_stage {
            return diff_name_status_index_worktree_changes_from_snapshot(
                worktree_root,
                git_dir,
                format,
            );
        }
        let stat_cache =
            IndexStatCache::from_index_mtime_only(sley_index::file_mtime_parts(&index_metadata));
        let entries = diff_name_status_index_worktree_changes_for_borrowed_entries(
            worktree_root,
            format,
            &index.entries,
            &stat_cache,
        )?;
        return Ok(IndexWorktreeDiff {
            entries,
            staged_gitlinks,
        });
    }
    let index = expand_sparse_index_for_worktree_diff(
        sley_index::read_repository_index(git_dir, format)?,
        git_dir,
        format,
    )?;
    let (has_non_normal_stage, staged_gitlinks) =
        index_worktree_metadata_for_entries(&index.entries);
    if has_non_normal_stage {
        return diff_name_status_index_worktree_changes_from_snapshot(
            worktree_root,
            git_dir,
            format,
        );
    }
    let stat_cache =
        IndexStatCache::from_index_mtime_only(sley_index::file_mtime_parts(&index_metadata));
    let entries = diff_name_status_index_worktree_changes_for_entries(
        worktree_root,
        format,
        &index.entries,
        &stat_cache,
    )?;
    Ok(IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    })
}

fn borrowed_entry_is_sparse_dir(entry: &sley_index::IndexEntryRef<'_>) -> bool {
    entry.mode == sley_index::SPARSE_DIR_MODE && entry.is_skip_worktree()
}

fn expand_sparse_index_for_worktree_diff(
    mut index: Index,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Index> {
    if !index
        .entries
        .iter()
        .any(sley_index::IndexEntry::is_sparse_dir)
    {
        return Ok(index);
    }

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut expanded = Vec::with_capacity(index.entries.len());
    for entry in std::mem::take(&mut index.entries) {
        if !entry.is_sparse_dir() {
            expanded.push(entry);
            continue;
        }

        let dir_prefix = entry.path.as_bytes();
        for (rel_path, (mode, oid)) in flatten_tree(&db, format, &entry.oid)? {
            let mut path = dir_prefix.to_vec();
            path.extend_from_slice(&rel_path);
            let mut expanded_entry = sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: 0,
                flags_extended: 0,
                path: BString::from(path),
            };
            expanded_entry.set_skip_worktree(true);
            expanded_entry.refresh_name_length();
            expanded.push(expanded_entry);
        }
    }

    expanded.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    index.entries = expanded;
    index.clear_sparse_extension()?;
    Ok(index)
}

fn diff_name_status_index_worktree_changes_for_borrowed_entries(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntryRef<'_>],
    stat_cache: &IndexStatCache,
) -> Result<Vec<NameStatusEntry>> {
    const PARALLEL_SCAN_MIN_ENTRIES: usize = 2048;
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(8);
    if workers <= 1 || entries.len() < PARALLEL_SCAN_MIN_ENTRIES {
        return diff_name_status_index_worktree_changes_for_borrowed_entry_chunk(
            worktree_root,
            format,
            entries,
            stat_cache,
        );
    }
    let chunk_size = entries.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in entries.chunks(chunk_size) {
            handles.push(scope.spawn(move || {
                diff_name_status_index_worktree_changes_for_borrowed_entry_chunk(
                    worktree_root,
                    format,
                    chunk,
                    stat_cache,
                )
            }));
        }
        let mut changes = Vec::new();
        for handle in handles {
            let chunk_changes = handle
                .join()
                .map_err(|_| GitError::Command("diff worker panicked".into()))??;
            changes.extend(chunk_changes);
        }
        Ok(changes)
    })
}

fn diff_name_status_index_worktree_changes_for_entries(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntry],
    stat_cache: &IndexStatCache,
) -> Result<Vec<NameStatusEntry>> {
    const PARALLEL_SCAN_MIN_ENTRIES: usize = 2048;
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(8);
    if workers <= 1 || entries.len() < PARALLEL_SCAN_MIN_ENTRIES {
        return diff_name_status_index_worktree_changes_for_entry_chunk(
            worktree_root,
            format,
            entries,
            stat_cache,
        );
    }
    let chunk_size = entries.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in entries.chunks(chunk_size) {
            handles.push(scope.spawn(move || {
                diff_name_status_index_worktree_changes_for_entry_chunk(
                    worktree_root,
                    format,
                    chunk,
                    stat_cache,
                )
            }));
        }
        let mut changes = Vec::new();
        for handle in handles {
            let chunk_changes = handle
                .join()
                .map_err(|_| GitError::Command("diff worker panicked".into()))??;
            changes.extend(chunk_changes);
        }
        Ok(changes)
    })
}

fn diff_name_status_index_worktree_changes_for_entry_chunk(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntry],
    stat_cache: &IndexStatCache,
) -> Result<Vec<NameStatusEntry>> {
    let mut changes = Vec::new();
    let mut path = PathBuf::from(worktree_root);
    for entry in entries {
        worktree_path_for_repo_path_into(&mut path, worktree_root, entry.path.as_bytes());
        if let Some(change) = index_worktree_change_for_entry(&path, format, entry, stat_cache)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn diff_name_status_index_worktree_changes_for_borrowed_entry_chunk(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntryRef<'_>],
    stat_cache: &IndexStatCache,
) -> Result<Vec<NameStatusEntry>> {
    let mut changes = Vec::new();
    let mut path = PathBuf::from(worktree_root);
    for entry in entries {
        worktree_path_for_repo_path_into(&mut path, worktree_root, entry.path);
        if let Some(change) = index_worktree_change_for_entry(&path, format, entry, stat_cache)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn index_worktree_metadata_for_entries(
    entries: &[impl WorktreeIndexEntry],
) -> (bool, Vec<IndexGitlinkEntry>) {
    let mut needs_snapshot = false;
    let mut staged_gitlinks = Vec::new();
    for entry in entries {
        if entry.stage() != sley_index::Stage::Normal {
            needs_snapshot = true;
        }
        // Intent-to-add entries (`git add -N`) must take the snapshot path, which
        // diffs them as new files rather than loading their empty-blob id.
        if entry.is_intent_to_add() {
            needs_snapshot = true;
        }
        if sley_index::is_gitlink(entry.mode()) {
            staged_gitlinks.push(IndexGitlinkEntry {
                path: BString::from_bytes(entry.git_path()),
                oid: entry.oid(),
            });
        }
    }
    (needs_snapshot, staged_gitlinks)
}

fn diff_name_status_index_worktree_changes_from_snapshot(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<IndexWorktreeDiff> {
    let IndexSnapshot {
        entries: index,
        stat_cache,
    } = read_index_snapshot(git_dir, format)?;
    // Intent-to-add (`git add -N`) paths are placeholders: git's `run_diff_files`
    // diffs them as a brand-new file (`/dev/null` → worktree), never loading the
    // recorded empty-blob id. `read_index_snapshot` drops the ITA flag, so read
    // the set of ITA stage-0 paths separately and override their verdict below.
    let intent_to_add_paths = read_intent_to_add_paths(git_dir, format)?;
    // `read_index_snapshot` collapses each path to a single entry; for an
    // unmerged path it keeps the last-written stage. To match git's
    // `run_diff_files` we need the conflict stages, so read them separately:
    // git diffs the worktree against the "ours" stage (stage 2, the default
    // `diff_unmerged_stage`) and additionally emits a standalone `U <path>`
    // pair via `diff_unmerge` (diff-lib.c).
    let unmerged = read_unmerged_stages(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let staged_gitlinks = index_gitlinks
        .iter()
        .map(|(path, oid)| IndexGitlinkEntry {
            path: BString::from_bytes(path),
            oid: *oid,
        })
        .collect();
    let mut changes = Vec::new();
    for (git_path, left) in &index {
        // For a conflicted path git first queues the `U` pair, then compares the
        // worktree against stage 2 (ours). The snapshot's collapsed `left` may
        // be the wrong stage, so override it with the stage-2 entry when present.
        let conflict_stages = unmerged.get(git_path);
        let right = worktree_entry_for_path(
            worktree_root,
            format,
            git_path,
            &index_gitlinks,
            Some(&stat_cache),
        )?;
        if conflict_stages.is_some() {
            // git's `diff_unmerge` makes a pair with a null old side and the
            // worktree mode on the new side (diff-lib.c `wt_mode`); the oids stay
            // zero. The raw line is `:000000 <wt_mode> 0..0 0..0 U <path>`.
            changes.push(NameStatusEntry {
                status: NameStatus::Unmerged,
                path: git_path.clone().into(),
                old_path: None,
                old_mode: None,
                new_mode: right.as_ref().map(|entry| entry.mode),
                old_oid: None,
                new_oid: None,
            });
        }
        // The index side for the modify comparison: stage 2 (ours) for a
        // conflict, otherwise the normal stage-0 entry. If the conflict has no
        // stage-2 (deleted on our side / added by them), git has no entry to
        // diff the worktree against, so it emits only the `U` line.
        let left = match conflict_stages {
            Some(stages) => match stages.ours.as_ref() {
                Some(ours) => ours,
                None => continue,
            },
            None => left,
        };
        // Intent-to-add placeholder: git's `run_diff_files` diffs it as a new
        // file. With the worktree file present, queue an `Added` pair whose old
        // side is null (`/dev/null` → worktree blob); with the file gone, an ITA
        // entry yields no diff-files entry (there is nothing to add).
        if intent_to_add_paths.contains(git_path.as_slice()) {
            if let Some(right) = right {
                changes.push(NameStatusEntry {
                    status: NameStatus::Added,
                    path: git_path.clone().into(),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(right.mode),
                    old_oid: None,
                    new_oid: Some(right.oid),
                });
            }
            continue;
        }
        let Some(right) = right else {
            changes.push(NameStatusEntry {
                status: NameStatus::Deleted,
                path: git_path.clone().into(),
                old_path: None,
                old_mode: Some(left.mode),
                new_mode: None,
                old_oid: Some(left.oid),
                new_oid: None,
            });
            continue;
        };
        if right != *left {
            changes.push(NameStatusEntry {
                status: modify_or_type_change(left.mode, right.mode),
                path: git_path.clone().into(),
                old_path: None,
                old_mode: Some(left.mode),
                new_mode: Some(right.mode),
                old_oid: Some(left.oid),
                new_oid: Some(right.oid),
            });
        }
    }
    Ok(IndexWorktreeDiff {
        entries: changes,
        staged_gitlinks,
    })
}

/// The conflict stages recorded for one unmerged index path.
struct ConflictStages {
    ours: Option<TrackedEntry>,
}

/// Read the higher-stage (conflict) index entries, keyed by path, recording the
/// "ours" (stage 2) entry git diffs the worktree against. Paths with only a
/// stage-0 entry are absent from the result.
fn read_unmerged_stages(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, ConflictStages>> {
    let index_path = sley_index::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = sley_index::read_repository_index(git_dir, format)?;
    let mut out: BTreeMap<Vec<u8>, ConflictStages> = BTreeMap::new();
    for entry in &index.entries {
        let stage = entry.stage();
        if stage == sley_index::Stage::Normal {
            continue;
        }
        let path = entry.path.clone().into_bytes();
        let slot = out.entry(path).or_insert(ConflictStages { ours: None });
        if stage == sley_index::Stage::Ours {
            slot.ours = Some(TrackedEntry {
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }
    Ok(out)
}

fn apply_name_status_options_to_index_worktree_changes(
    mut changes: Vec<NameStatusEntry>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if options.detect_renames {
        changes = detect_exact_renames_from_changes(changes, options.rename_empty);
    } else if options.detect_copies {
        changes.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    }
    Ok(changes)
}

fn detect_exact_renames_from_changes(
    changes: Vec<NameStatusEntry>,
    rename_empty: bool,
) -> Vec<NameStatusEntry> {
    let added = changes
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.status == NameStatus::Added)
        .collect::<Vec<_>>();
    let deleted = changes
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.status == NameStatus::Deleted)
        .collect::<Vec<_>>();
    let mut consumed_added = BTreeSet::new();
    let mut consumed_deleted = BTreeSet::new();
    let mut result = Vec::new();

    for (deleted_index, deleted_entry) in deleted {
        let Some(old_oid) = deleted_entry.old_oid else {
            continue;
        };
        if !rename_empty && is_empty_blob_oid(&old_oid) {
            continue;
        }
        if let Some((added_index, added_entry)) = added.iter().find(|(added_index, added_entry)| {
            !consumed_added.contains(added_index) && added_entry.new_oid == Some(old_oid)
        }) {
            consumed_deleted.insert(deleted_index);
            consumed_added.insert(*added_index);
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(100),
                path: added_entry.path.clone(),
                old_path: Some(deleted_entry.path.clone()),
                old_mode: deleted_entry.old_mode,
                new_mode: added_entry.new_mode,
                old_oid: deleted_entry.old_oid,
                new_oid: added_entry.new_oid,
            });
        }
    }

    for (index, entry) in changes.into_iter().enumerate() {
        if consumed_added.contains(&index) || consumed_deleted.contains(&index) {
            continue;
        }
        result.push(entry);
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

/// Index-vs-worktree name-status for **`git diff-files`** (plumbing), which
/// selects changed paths by the cached *stat* rather than by content.
///
/// This is the crucial difference from [`diff_name_status_index_worktree_with_options`]
/// (the engine behind porcelain `git diff`): porcelain `git diff` refreshes the
/// index first, so a stat-dirty-but-content-identical entry (a `touch`ed file, or
/// a freshly `rm --cached`-then-`reset --no-refresh` entry with a zeroed cached
/// stat) is re-stamped clean and suppressed. `git diff-files` does **not** refresh
/// — it reports every entry whose cached stat fails to prove it clean as `M`,
/// without re-hashing the content to "rescue" it (`builtin/diff.c` →
/// `run_diff_files` → `ie_match_stat`). The raw / name-only / name-status output
/// and the `--quiet`/`--exit-code` status therefore list such entries even when
/// the content is byte-identical; patch/stat output, which diffs actual content,
/// renders them as an empty hunk.
///
/// We layer that stat-based selection on top of the content-based diff: the
/// content diff already catches adds/deletes/genuine-content modifies (with
/// rename detection), and we then append a `Modified` entry for any stage-0 path
/// whose worktree file is present and whose cached stat is dirty per
/// [`IndexStatCache::index_entry_worktree_stat_dirty`] but which the content diff
/// did not already report. Content-identical stat-dirty entries cannot be rename
/// sources/targets (their content is unchanged), so they never interact with the
/// rename machinery — they are plain `M`.
pub fn diff_name_status_index_worktree_for_diff_files_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let changes =
        diff_name_status_index_worktree_with_options(worktree_root, git_dir, format, options)?;
    augment_with_stat_dirty_entries(worktree_root, git_dir, format, changes)
}

/// As [`diff_name_status_index_worktree_for_diff_files_with_options`], but with
/// full rename/copy options (the `git diff-files -M/-C` path). The stat-dirty
/// augmentation is identical; only the underlying content diff differs.
pub fn diff_name_status_index_worktree_for_diff_files_with_rename_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let changes = diff_name_status_index_worktree_with_rename_options(
        worktree_root,
        git_dir,
        format,
        options,
    )?;
    augment_with_stat_dirty_entries(worktree_root, git_dir, format, changes)
}

/// Append a `Modified` entry for every stage-0 index path whose worktree file is
/// present and whose cached stat is dirty (`ce_match_stat` "changed") but which
/// `content_changes` did not already report. The result is re-sorted by path so
/// the merged set keeps git's diff-queue ordering. New-side oids on the added
/// entries are left `None` (rendered as zeros in raw output), matching git, which
/// reports the worktree blob oid only for entries it has hashed.
fn augment_with_stat_dirty_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    mut content_changes: Vec<NameStatusEntry>,
) -> Result<Vec<NameStatusEntry>> {
    let IndexSnapshot {
        entries: index,
        stat_cache,
    } = read_index_snapshot(git_dir, format)?;
    // Paths the content diff already accounts for (by new-side path, the position
    // git queues a pair at — a rename's destination, a modify/add/delete's path).
    let already_reported: BTreeSet<&[u8]> = content_changes
        .iter()
        .map(|entry| entry.path.as_bytes())
        .collect();
    let mut extras = Vec::new();
    for (git_path, tracked) in &index {
        if already_reported.contains(git_path.as_slice()) {
            continue;
        }
        let Some(cached) = stat_cache.entry_for_git_path(git_path) else {
            continue;
        };
        // Gitlinks (submodules) have their own dirtiness model and are not stat-
        // compared here; the content diff already handles changed gitlink oids.
        if sley_index::is_gitlink(tracked.mode) {
            continue;
        }
        let path = worktree_path_for_repo_path(worktree_root, git_path);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            // A missing worktree file is a deletion, which the content diff
            // already reports; nothing to add here.
            continue;
        };
        if !(metadata.is_file() || metadata.file_type().is_symlink()) {
            continue;
        }
        match stat_cache.index_entry_worktree_stat_verdict(cached, &metadata) {
            sley_index::StatVerdict::Clean => continue,
            sley_index::StatVerdict::Dirty => {}
            // A racily-clean entry must be resolved by content: git re-hashes it
            // (`ce_compare_data`) and only reports `M` when the worktree bytes
            // actually differ from the cached oid — so a `touch`ed-then-re-`add`ed
            // file (same-second mtime as the index) stays clean.
            sley_index::StatVerdict::RacyNeedsContentCheck => {
                if worktree_oid_matches_index(worktree_root, git_path, &metadata, tracked, format)?
                {
                    continue;
                }
            }
        }
        extras.push(NameStatusEntry {
            status: NameStatus::Modified,
            path: git_path.clone().into(),
            old_path: None,
            old_mode: Some(tracked.mode),
            new_mode: Some(tracked.mode),
            old_oid: Some(tracked.oid),
            new_oid: None,
        });
    }
    if !extras.is_empty() {
        content_changes.extend(extras);
        content_changes
            .sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    }
    Ok(content_changes)
}

/// Hash the worktree file or symlink at `path` into the `(mode, oid)` blob entry
/// git would record for it. `metadata` must already describe a regular file or a
/// symlink — gitlink and directory classification is the caller's concern, since
/// those need the index/HEAD context this leaf does not have. This is the single
/// owner of the symlink-vs-regular body-source and mode split that `diff-files`
/// ([`worktree_oid_matches_index`]), the candidate-path collector
/// ([`worktree_entry_for_path`]), and the index↔worktree walk
/// ([`index_worktree_change_for_entry`]) all share; before consolidation each
/// open-coded it with a bare `0o120000`.
fn classify_worktree_entry(
    path: &Path,
    metadata: &fs::Metadata,
    format: ObjectFormat,
) -> Result<TrackedEntry> {
    let is_symlink = metadata.file_type().is_symlink();
    let body = if is_symlink {
        symlink_target_bytes(path)?
    } else {
        fs::read(path)?
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    let mode = if is_symlink {
        sley_index::SYMLINK_MODE
    } else {
        file_mode(metadata)
    };
    Ok(TrackedEntry { mode, oid })
}

/// Whether the worktree file at `git_path` hashes to the index entry's oid (mode
/// included). Used to resolve a racily-clean `diff-files` entry: git re-hashes the
/// content and only reports it changed when the bytes truly differ. Shares the
/// worktree-oid computation with [`worktree_entry_for_path`] via
/// [`classify_worktree_entry`].
fn worktree_oid_matches_index(
    worktree_root: &Path,
    git_path: &[u8],
    metadata: &fs::Metadata,
    index_entry: &TrackedEntry,
    format: ObjectFormat,
) -> Result<bool> {
    let path = worktree_path_for_repo_path(worktree_root, git_path);
    let entry = classify_worktree_entry(&path, metadata, format)?;
    Ok(entry.oid == index_entry.oid && entry.mode == index_entry.mode)
}

pub fn diff_name_status_trees_with_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    // `--find-copies-harder` may pair an *unchanged* left-side file as a copy
    // source, so it needs the complete left map; every other mode only consults
    // changed paths, so the pruned simultaneous walk (which skips identical
    // subtrees) suffices and produces byte-identical output.
    let needs_full_maps = options.detect_copies && options.find_copies_harder;
    let (left_entries, right_entries) = if needs_full_maps {
        collect_full_tree_pair(db, format, left_tree, right_tree)?
    } else {
        changed_tree_entries(db, format, left_tree, right_tree)?
    };
    diff_name_status_maps(
        &left_entries,
        &right_entries,
        left_entries.keys().chain(right_entries.keys()),
        options,
    )
}

pub fn diff_name_status_empty_tree_with_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    right_tree: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let left_entries = BTreeMap::new();
    let mut right_entries = BTreeMap::new();
    collect_tree_entries(db, format, right_tree, Vec::new(), &mut right_entries)?;
    diff_name_status_maps(&left_entries, &right_entries, right_entries.keys(), options)
}

/// Diff two trees with full rename/copy options, including inexact (similarity)
/// detection when [`RenameDetectionOptions::detect_inexact`] is set.
///
/// Blob bytes for similarity scoring are read from `db`. This is the inexact-
/// aware counterpart of [`diff_name_status_trees_with_options`]; passing
/// `RenameDetectionOptions::default()` (or `RenameDetectionOptions { base, ..
/// default }` with `detect_inexact: false`) reproduces the exact-only behaviour.
pub fn diff_name_status_trees_with_rename_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    // See `diff_name_status_trees_with_options`: only `--find-copies-harder`
    // needs unchanged left entries as copy sources; otherwise the pruned walk
    // (skipping identical subtrees) yields identical output far more cheaply.
    let needs_full_maps = options.base.detect_copies && options.base.find_copies_harder;
    let (left_entries, right_entries) = if needs_full_maps {
        collect_full_tree_pair(db, format, left_tree, right_tree)?
    } else {
        changed_tree_entries(db, format, left_tree, right_tree)?
    };
    diff_name_status_maps_with_renames(
        &left_entries,
        &right_entries,
        left_entries.keys().chain(right_entries.keys()),
        options,
        |oid| read_blob_bytes(db, oid),
    )
}

/// Diff the empty tree against `right_tree` with full rename/copy options.
///
/// As with [`diff_name_status_trees_with_rename_options`], inexact detection is
/// gated on [`RenameDetectionOptions::detect_inexact`]; the left (empty) side
/// has no sources, so only copies among the right-side additions can match when
/// `find_copies_harder` is set.
pub fn diff_name_status_empty_tree_with_rename_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    right_tree: &ObjectId,
    options: RenameDetectionOptions,
) -> Result<Vec<NameStatusEntry>> {
    let left_entries = BTreeMap::new();
    let mut right_entries = BTreeMap::new();
    collect_tree_entries(db, format, right_tree, Vec::new(), &mut right_entries)?;
    diff_name_status_maps_with_renames(
        &left_entries,
        &right_entries,
        right_entries.keys(),
        options,
        |oid| read_blob_bytes(db, oid),
    )
}

/// Read a blob's raw bytes from the ODB, returning `None` if the object cannot
/// be read or is not a blob. Used as the similarity-scoring blob fetcher; a
/// missing object simply makes a candidate pair non-similar rather than failing
/// the whole diff.
fn read_blob_bytes(db: &FileObjectDatabase, oid: &ObjectId) -> Option<Vec<u8>> {
    match db.read_object(oid) {
        Ok(object) if object.object_type == ObjectType::Blob => Some(object.body.clone()),
        _ => None,
    }
}

/// Build the raw per-path add/delete/modify change list (before any rename or
/// copy detection) from the two entry maps and the candidate path set.
fn raw_name_status_changes_for_unique_paths<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: impl Iterator<Item = &'a Vec<u8>>,
) -> Vec<NameStatusEntry> {
    let mut changes = Vec::new();
    for path in paths {
        let left = left_entries.get(path);
        let right = right_entries.get(path);
        let status = match (left, right) {
            (None, Some(_)) => Some(NameStatus::Added),
            (Some(_), None) => Some(NameStatus::Deleted),
            (Some(left), Some(right)) if left != right => {
                Some(modify_or_type_change(left.mode, right.mode))
            }
            _ => None,
        };
        if let Some(status) = status {
            changes.push(NameStatusEntry {
                status,
                path: path.clone().into(),
                old_path: None,
                old_mode: left.map(|entry| entry.mode),
                new_mode: right.map(|entry| entry.mode),
                old_oid: left.map(|entry| entry.oid),
                new_oid: right.map(|entry| entry.oid),
            });
        }
    }
    changes
}

fn diff_name_status_maps<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let paths = candidate_path_set(candidate_paths);
    diff_name_status_maps_for_path_set(left_entries, right_entries, &paths, options)
}

fn diff_name_status_maps_for_path_set(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: &BTreeSet<Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_maps_for_unique_paths(
        left_entries,
        right_entries,
        candidate_paths.iter(),
        options,
    )
}

fn diff_name_status_maps_for_unique_paths<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let mut changes =
        raw_name_status_changes_for_unique_paths(left_entries, right_entries, candidate_paths);
    if options.detect_renames {
        changes = detect_exact_renames(changes, left_entries, right_entries, options.rename_empty);
    }
    if options.detect_copies {
        changes = detect_exact_copies(
            changes,
            left_entries,
            right_entries,
            options.find_copies_harder,
            options.rename_empty,
        );
    }
    Ok(changes)
}

/// Like [`diff_name_status_maps`], but additionally runs inexact (similarity)
/// rename/copy detection when `options.detect_inexact` is set.
///
/// `fetch_blob` resolves an [`ObjectId`] to that blob's raw bytes; it is only
/// consulted for the candidate pairs considered during inexact detection, and
/// only when inexact detection is enabled. A pair whose blob bytes cannot be
/// fetched is simply skipped (treated as not similar), so a missing object never
/// fails the whole diff.
fn diff_name_status_maps_with_renames<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: RenameDetectionOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Vec<u8>>,
) -> Result<Vec<NameStatusEntry>> {
    let paths = candidate_path_set(candidate_paths);
    diff_name_status_maps_with_renames_for_path_set(
        left_entries,
        right_entries,
        &paths,
        options,
        fetch_blob,
    )
}

fn diff_name_status_maps_with_renames_for_path_set(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: &BTreeSet<Vec<u8>>,
    options: RenameDetectionOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Vec<u8>>,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_maps_with_renames_for_unique_paths(
        left_entries,
        right_entries,
        candidate_paths.iter(),
        options,
        fetch_blob,
    )
}

fn diff_name_status_maps_with_renames_for_unique_paths<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: RenameDetectionOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Vec<u8>>,
) -> Result<Vec<NameStatusEntry>> {
    let base = options.base;
    let mut changes =
        raw_name_status_changes_for_unique_paths(left_entries, right_entries, candidate_paths);
    if base.detect_renames {
        changes = detect_exact_renames(changes, left_entries, right_entries, base.rename_empty);
    }
    // Inexact rename detection runs after exact renames so exact matches keep
    // priority (and their score of 100). It only fires when rename detection is
    // enabled at all, mirroring git's `-M`.
    if base.detect_renames && options.detect_inexact {
        changes = detect_inexact_renames(changes, &options, &fetch_blob);
    }
    if base.detect_copies {
        changes = detect_exact_copies(
            changes,
            left_entries,
            right_entries,
            base.find_copies_harder,
            base.rename_empty,
        );
    }
    if base.detect_copies && options.detect_inexact {
        changes = detect_inexact_copies(changes, left_entries, &options, &fetch_blob);
    }
    Ok(changes)
}

fn detect_exact_renames(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    rename_empty: bool,
) -> Vec<NameStatusEntry> {
    let added = changes
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.status == NameStatus::Added)
        .map(|(idx, entry)| (idx, entry.path.clone()))
        .collect::<Vec<_>>();
    // Candidate sources in path order (git's `rename_src` ordering), so the
    // best-source search tie-breaks deterministically.
    let mut sources = changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Deleted)
        .filter_map(|entry| {
            left_entries
                .get(entry.path.as_bytes())
                .map(|left| (entry.path.clone(), left.oid))
        })
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    let mut src_used = vec![false; sources.len()];
    let mut consumed = BTreeSet::new();
    let mut renamed_old_paths = BTreeSet::new();
    let mut result = Vec::new();

    // git's `find_identical_files`: for each destination, among the still-unused
    // sources with the identical OID, prefer one that shares the destination's
    // basename (score 2 short-circuits; otherwise the first such source wins).
    // Iterating destinations (not sources) is what lets a same-basename source
    // win over an alphabetically-earlier different-basename one.
    for (idx, new_path) in &added {
        let Some(right) = right_entries.get(new_path.as_bytes()) else {
            continue;
        };
        if !rename_empty && is_empty_blob_oid(&right.oid) {
            continue;
        }
        let mut best: Option<usize> = None;
        let mut best_score = -1i32;
        for (si, (src_path, src_oid)) in sources.iter().enumerate() {
            if src_used[si] || *src_oid != right.oid {
                continue;
            }
            let score = 1 + i32::from(path_basename(src_path) == path_basename(new_path));
            if score > best_score {
                best = Some(si);
                best_score = score;
                if score == 2 {
                    break;
                }
            }
        }
        if let Some(si) = best {
            src_used[si] = true;
            consumed.insert(*idx);
            let old_path = sources[si].0.clone();
            let left = &left_entries[old_path.as_bytes()];
            renamed_old_paths.insert(old_path.clone());
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(100),
                path: new_path.clone(),
                old_path: Some(old_path),
                old_mode: Some(left.mode),
                new_mode: Some(right.mode),
                old_oid: Some(left.oid),
                new_oid: Some(right.oid),
            });
        }
    }

    for (idx, entry) in changes.into_iter().enumerate() {
        if entry.status == NameStatus::Added && consumed.contains(&idx) {
            continue;
        }
        if entry.status == NameStatus::Deleted && renamed_old_paths.contains(&entry.path) {
            continue;
        }
        result.push(entry);
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

fn detect_exact_copies(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    find_copies_harder: bool,
    rename_empty: bool,
) -> Vec<NameStatusEntry> {
    let changed_sources = changes
        .iter()
        .filter(|entry| matches!(entry.status, NameStatus::Deleted | NameStatus::Modified))
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let source_paths = left_entries
        .keys()
        .filter(|path| find_copies_harder || changed_sources.contains(path.as_slice()))
        .cloned()
        .collect::<Vec<_>>();

    let mut result = Vec::new();
    for entry in changes {
        if entry.status != NameStatus::Added {
            result.push(entry);
            continue;
        }
        let Some(right) = right_entries.get(entry.path.as_bytes()) else {
            result.push(entry);
            continue;
        };
        if let Some(old_path) = source_paths.iter().find(|old_path| {
            old_path.as_slice() != entry.path.as_bytes()
                && left_entries.get(*old_path).is_some_and(|left| {
                    left.oid == right.oid && (rename_empty || !is_empty_blob_oid(&left.oid))
                })
        }) {
            result.push(NameStatusEntry {
                status: NameStatus::Copied(100),
                path: entry.path,
                old_path: Some(old_path.clone().into()),
                old_mode: left_entries
                    .get(old_path.as_slice())
                    .map(|entry| entry.mode),
                new_mode: entry.new_mode,
                old_oid: left_entries.get(old_path.as_slice()).map(|entry| entry.oid),
                new_oid: entry.new_oid,
            });
        } else {
            result.push(entry);
        }
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

/// Old-side metadata of a rename source, snapshotted before the source delete
/// entry is consumed so it can be attached to the renamed destination.
#[derive(Debug, Clone)]
struct RenameSourceMeta {
    path: BString,
    mode: Option<u32>,
    oid: Option<ObjectId>,
}

/// A scored candidate pairing of a deleted source with an added destination,
/// used to order inexact-rename assignment best-match-first.
struct ScoredPair {
    /// Index into the `deleted` candidate list.
    src: usize,
    /// Index into the `added` candidate list.
    dst: usize,
    /// Similarity percentage in `0..=100`.
    score: u8,
}

/// Inexact rename detection: pair still-unmatched deleted files with still-
/// unmatched added files by content similarity, replacing the best matches
/// (similarity >= `rename_threshold`) with [`NameStatus::Renamed`].
///
/// Exact renames have already run, so the only `Deleted`/`Added` entries left
/// here are ones with no identical-OID partner. Assignment is greedy by
/// descending score (then by source/destination order for determinism), and
/// each source and destination is used at most once — matching git's
/// `diffcore-rename` behaviour. Empty blobs are never used as a rename source
/// when `rename_empty` is false, mirroring exact detection.
fn detect_inexact_renames(
    changes: Vec<NameStatusEntry>,
    options: &RenameDetectionOptions,
    fetch_blob: &impl Fn(&ObjectId) -> Option<Vec<u8>>,
) -> Vec<NameStatusEntry> {
    let threshold = options.rename_threshold;
    // A threshold above 100 can never be met; nothing to do.
    if threshold > 100 {
        return changes;
    }

    // Collect the candidate sources (Deletes) and destinations (Adds) with their
    // positions in `changes`, fetching blob bytes once each.
    let mut deleted: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut added: Vec<(usize, Vec<u8>)> = Vec::new();
    for (idx, entry) in changes.iter().enumerate() {
        match entry.status {
            NameStatus::Deleted => {
                let Some(oid) = entry.old_oid.as_ref() else {
                    continue;
                };
                if !options.base.rename_empty && is_empty_blob_oid(oid) {
                    continue;
                }
                if let Some(bytes) = fetch_blob(oid) {
                    deleted.push((idx, bytes));
                }
            }
            NameStatus::Added => {
                let Some(oid) = entry.new_oid.as_ref() else {
                    continue;
                };
                if !options.base.rename_empty && is_empty_blob_oid(oid) {
                    continue;
                }
                if let Some(bytes) = fetch_blob(oid) {
                    added.push((idx, bytes));
                }
            }
            _ => {}
        }
    }

    if deleted.is_empty() || added.is_empty() {
        return changes;
    }

    // git's `too_many_rename_candidates`: if the rename matrix would exceed a
    // `rename_limit` square, skip inexact detection wholesale (exact-OID renames
    // were already resolved upstream). A non-positive limit is unlimited.
    if options.rename_limit > 0
        && deleted
            .len()
            .saturating_mul(added.len())
            .gt(&options.rename_limit.saturating_mul(options.rename_limit))
    {
        return changes;
    }

    let mut src_used = vec![false; deleted.len()];
    let mut dst_used = vec![false; added.len()];
    // destination changes-index -> (source changes-index, score).
    let mut rename_of: BTreeMap<usize, (usize, u8)> = BTreeMap::new();

    // Basename pre-pass (git's `find_basename_matches`): before the global
    // matrix, pair unique-basename src/dst at the stricter basename score, so a
    // same-basename rename wins over a globally-more-similar different basename.
    // git only does this for pure rename detection (`!want_copies`); when copies
    // are also wanted it culls differently and skips the basename heuristic.
    if !options.base.detect_copies {
        let src_paths: Vec<&[u8]> = deleted.iter().map(|(idx, _)| &changes[*idx].path[..]).collect();
        let dst_paths: Vec<&[u8]> = added.iter().map(|(idx, _)| &changes[*idx].path[..]).collect();
        let basename_pairs = basename_rename_matches(
            &src_paths,
            &dst_paths,
            &src_used,
            &dst_used,
            threshold,
            |si, di| Some(blob_similarity(&deleted[si].1, &added[di].1)),
        );
        for (si, di, score) in basename_pairs {
            src_used[si] = true;
            dst_used[di] = true;
            rename_of.insert(added[di].0, (deleted[si].0, score));
        }
    }

    // Score every remaining (delete, add) pair; keep only those meeting the
    // threshold.
    let mut pairs: Vec<ScoredPair> = Vec::new();
    for (si, (_, src_bytes)) in deleted.iter().enumerate() {
        if src_used[si] {
            continue;
        }
        for (di, (_, dst_bytes)) in added.iter().enumerate() {
            if dst_used[di] {
                continue;
            }
            let score = blob_similarity(src_bytes, dst_bytes);
            if score >= threshold {
                pairs.push(ScoredPair {
                    src: si,
                    dst: di,
                    score,
                });
            }
        }
    }
    // Best score first; ties broken by source then destination order so the
    // result is deterministic regardless of input ordering.
    pairs.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.dst.cmp(&b.dst))
    });

    for pair in pairs {
        if src_used[pair.src] || dst_used[pair.dst] {
            continue;
        }
        src_used[pair.src] = true;
        dst_used[pair.dst] = true;
        let src_change_idx = deleted[pair.src].0;
        let dst_change_idx = added[pair.dst].0;
        rename_of.insert(dst_change_idx, (src_change_idx, pair.score));
    }

    if rename_of.is_empty() {
        return changes;
    }

    // Snapshot the source (delete) entries' metadata before we consume them, so
    // each renamed destination can carry the correct old path/mode/oid.
    let consumed_sources: BTreeSet<usize> =
        rename_of.values().map(|(src_idx, _)| *src_idx).collect();
    let source_meta: BTreeMap<usize, RenameSourceMeta> = consumed_sources
        .iter()
        .map(|&src_idx| {
            let src = &changes[src_idx];
            (
                src_idx,
                RenameSourceMeta {
                    path: src.path.clone(),
                    mode: src.old_mode,
                    oid: src.old_oid,
                },
            )
        })
        .collect();

    let mut result = Vec::with_capacity(changes.len());
    for (idx, entry) in changes.into_iter().enumerate() {
        if consumed_sources.contains(&idx) {
            // This delete became the source of a rename; drop it.
            continue;
        }
        if let Some((src_idx, score)) = rename_of.get(&idx) {
            // The destination becomes a rename from the matched source. Pull the
            // old-side metadata from the snapshot; the new-side metadata stays as
            // the destination's.
            let meta = source_meta
                .get(src_idx)
                .cloned()
                .unwrap_or(RenameSourceMeta {
                    path: BString::default(),
                    mode: None,
                    oid: None,
                });
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(*score),
                path: entry.path,
                old_path: Some(meta.path),
                old_mode: meta.mode,
                new_mode: entry.new_mode,
                old_oid: meta.oid,
                new_oid: entry.new_oid,
            });
            continue;
        }
        result.push(entry);
    }

    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

/// Inexact copy detection: for each still-`Added` file, find the most similar
/// candidate *source* on the left side (similarity >= `copy_threshold`) and, if
/// found, report it as a [`NameStatus::Copied`]. The source is not removed
/// (copies leave the original in place).
///
/// Candidate sources follow the same rule as exact copy detection: with
/// `find_copies_harder` every left-side path is eligible; otherwise only paths
/// that were themselves changed (deleted or modified) on this diff. Exact copies
/// have already run, so any remaining `Added` here had no identical-OID source.
fn detect_inexact_copies(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    options: &RenameDetectionOptions,
    fetch_blob: &impl Fn(&ObjectId) -> Option<Vec<u8>>,
) -> Vec<NameStatusEntry> {
    let threshold = options.copy_threshold;
    if threshold > 100 {
        return changes;
    }

    let changed_sources = changes
        .iter()
        .filter(|entry| matches!(entry.status, NameStatus::Deleted | NameStatus::Modified))
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    // Eligible source paths, paired with their bytes (fetched lazily/once).
    let mut sources: Vec<(Vec<u8>, &TrackedEntry, Vec<u8>)> = Vec::new();
    for (path, tracked) in left_entries {
        if !(options.base.find_copies_harder || changed_sources.contains(path.as_slice())) {
            continue;
        }
        if !options.base.rename_empty && is_empty_blob_oid(&tracked.oid) {
            continue;
        }
        if let Some(bytes) = fetch_blob(&tracked.oid) {
            sources.push((path.clone(), tracked, bytes));
        }
    }
    if sources.is_empty() {
        return changes;
    }

    // git's `too_many_rename_candidates`: when the copy matrix would exceed a
    // `rename_limit` square, skip inexact copy detection wholesale. Under
    // `--find-copies-harder` the source set is every left-side path (not just the
    // changed ones), so this O(sources × dests) matrix is the one most likely to
    // blow up — yet the rename gate above guards only `detect_inexact_renames`.
    // A non-positive limit is unlimited. Mirrors the identical gate there.
    let dest_count = changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Added)
        .count();
    if options.rename_limit > 0
        && sources
            .len()
            .saturating_mul(dest_count)
            .gt(&options.rename_limit.saturating_mul(options.rename_limit))
    {
        return changes;
    }

    let mut result = Vec::with_capacity(changes.len());
    for entry in changes {
        if entry.status != NameStatus::Added {
            result.push(entry);
            continue;
        }
        let Some(new_oid) = entry.new_oid.as_ref() else {
            result.push(entry);
            continue;
        };
        let Some(dst_bytes) = fetch_blob(new_oid) else {
            result.push(entry);
            continue;
        };

        // Pick the best-scoring source path that meets the threshold. Ties are
        // broken by path order (BTreeMap iteration is sorted) so the choice is
        // deterministic.
        let mut best: Option<(usize, u8)> = None;
        for (i, (src_path, _, src_bytes)) in sources.iter().enumerate() {
            if src_path.as_slice() == entry.path.as_bytes() {
                continue;
            }
            let score = blob_similarity(src_bytes, &dst_bytes);
            if score < threshold {
                continue;
            }
            match best {
                Some((_, best_score)) if best_score >= score => {}
                _ => best = Some((i, score)),
            }
        }

        if let Some((src_idx, score)) = best {
            let (src_path, src_tracked, _) = &sources[src_idx];
            result.push(NameStatusEntry {
                status: NameStatus::Copied(score),
                path: entry.path,
                old_path: Some(src_path.clone().into()),
                old_mode: Some(src_tracked.mode),
                new_mode: entry.new_mode,
                old_oid: Some(src_tracked.oid),
                new_oid: entry.new_oid,
            });
        } else {
            result.push(entry);
        }
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

fn is_empty_blob_oid(oid: &ObjectId) -> bool {
    object_id_for_bytes(oid.format(), "blob", b"").is_ok_and(|empty| empty == *oid)
}

// ===========================================================================
// Content similarity (the engine for inexact `-M`/`-C` rename/copy detection).
//
// This mirrors upstream git's similarity estimate from `diffcore-delta.c`
// (the span-hash counting) and `diffcore-rename.c` (the score formula), so the
// `R<score>`/`C<score>` we emit match git's percentages.
//
// The metric, precisely:
//
//   1. Each blob is broken into *spans*. Starting at a byte, we accumulate a
//      rolling hash of the bytes and end the span at the first `\n` (inclusive)
//      or once the span reaches `MAX_SPAN_BYTES` (64) bytes, whichever comes
//      first. (The 64-byte cap keeps a file with no/few newlines — e.g. a
//      binary blob or one very long line — from collapsing into a single span,
//      so similarity still tracks shared substrings.) Each span yields a
//      `(hash, byte_count)` pair, where `byte_count` is the span's length in
//      bytes. This is the exact loop git uses in `hash_chars()`.
//
//   2. The two blobs' spans are reduced to multisets keyed by hash: for each
//      hash we keep the total number of bytes spanned by entries with that
//      hash, on each side. `common_bytes` is then the sum over all hashes of
//      `min(bytes_on_src, bytes_on_dst)` — the bytes that exist on both sides.
//      This is git's `src_copied`.
//
//   3. The score is `common_bytes / max(size_src, size_dst)`, scaled to a
//      percentage and rounded to the nearest integer:
//
//          score% = round(common_bytes * 100 / max(size_src, size_dst))
//
//      git computes an internal score `src_copied * MAX_SCORE / max_size` with
//      `MAX_SCORE == 60000` and reports `round(score * 100 / MAX_SCORE)`; that
//      is algebraically the same rounded percentage, which we compute directly
//      to avoid intermediate precision loss.
//
// Edge cases match git: two empty blobs are 100% similar (identical content);
// an empty blob vs a non-empty one is 0%. Equal byte buffers are always 100%.

/// Maximum number of bytes in a single similarity span before it is force-cut.
///
/// git uses 64 (`hash_chars()` breaks a span once `++chunks >= 64`).
const MAX_SPAN_BYTES: usize = 64;

/// Compute the content similarity of two blobs as an integer percentage in
/// `0..=100`, using git's span-hash counting metric (see the module comment
/// above for the exact definition).
///
/// The result is symmetric (`blob_similarity(a, b) == blob_similarity(b, a)`)
/// because the score divides the common-byte count by the larger of the two
/// sizes. Byte-identical blobs return `100`; a non-empty blob compared against
/// an empty one returns `0`; two empty blobs return `100`.
///
/// This is the same number git prints as `similarity index N%` and uses to
/// decide `-M`/`-C` rename and copy detection.
pub fn blob_similarity(a: &[u8], b: &[u8]) -> u8 {
    // Fast paths that also pin down the empty-blob conventions.
    if a == b {
        return 100;
    }
    let max_size = a.len().max(b.len());
    if max_size == 0 {
        // Both empty (and not caught by `a == b` only if both are empty, which
        // they are here) -> identical.
        return 100;
    }

    let src = span_hash_counts(a, blob_is_text(a));
    let dst = span_hash_counts(b, blob_is_text(b));
    let common = common_span_bytes(&src, &dst);

    // Match git's diffcore-rename integer math exactly. git computes an internal
    // score `src_copied * MAX_SCORE / max_size` (MAX_SCORE == 60000) with integer
    // truncation, then reports the similarity index as `score * 100 / MAX_SCORE`,
    // truncated again. This two-step truncation -- *not* a single rounded
    // `common * 100 / max_size` -- is what yields git's exact percentages: e.g.
    // common=4, max_size=6 gives 4*60000/6=40000 then 40000*100/60000=66 (git's
    // `R066`), whereas a rounded single step would give 67.
    const MAX_SCORE: u64 = 60000;
    let internal = (common as u64 * MAX_SCORE) / max_size as u64;
    let score = internal * 100 / MAX_SCORE;
    score.min(100) as u8
}

/// The basename of a slash-separated path: the portion after the last `/`
/// (git's `get_basename`).
pub fn path_basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => &path[slash + 1..],
        None => path,
    }
}

/// The stricter score a basename match must reach: git's `min_basename_score`
/// with the default `GIT_BASENAME_FACTOR` of 0.5, i.e. halfway between the
/// rename threshold and 100%. (For the default 50% threshold this is 75%.)
pub fn basename_min_score(threshold: u8) -> u8 {
    let threshold = threshold.min(100);
    threshold + (100 - threshold) / 2
}

/// git's `find_basename_matches`: among the still-unmatched rename sources and
/// destinations, pair those whose basename is UNIQUE on *both* sides and whose
/// similarity meets [`basename_min_score`]. Returns the `(src_local, dst_local,
/// score)` pairings to apply *before* the full O(n·m) similarity matrix, so a
/// same-basename rename wins over a globally-more-similar different-basename
/// candidate (diffcore-rename.c).
///
/// `src_paths`/`dst_paths` are the candidate paths, indexed in parallel with the
/// `src_used`/`dst_used` flags (entries already consumed by exact-OID matching).
/// `similarity(src_local, dst_local)` returns the blob similarity for a pair, or
/// `None` when a blob is unreadable / ineligible. Only unique basenames are
/// considered: git's plain-diff path has no directory-rename fallback, so an
/// ambiguous basename is skipped entirely.
pub fn basename_rename_matches(
    src_paths: &[&[u8]],
    dst_paths: &[&[u8]],
    src_used: &[bool],
    dst_used: &[bool],
    threshold: u8,
    mut similarity: impl FnMut(usize, usize) -> Option<u8>,
) -> Vec<(usize, usize, u8)> {
    let min_score = basename_min_score(threshold);
    // basename -> Some(unique local index), or None once a second candidate with
    // the same basename appears (ambiguous).
    let mut src_by_base: HashMap<&[u8], Option<usize>> = HashMap::new();
    for (si, path) in src_paths.iter().enumerate() {
        if src_used.get(si).copied().unwrap_or(false) {
            continue;
        }
        src_by_base
            .entry(path_basename(path))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(si));
    }
    let mut dst_by_base: HashMap<&[u8], Option<usize>> = HashMap::new();
    for (di, path) in dst_paths.iter().enumerate() {
        if dst_used.get(di).copied().unwrap_or(false) {
            continue;
        }
        dst_by_base
            .entry(path_basename(path))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(di));
    }
    let mut matches = Vec::new();
    let mut dst_taken = vec![false; dst_paths.len()];
    for (si, path) in src_paths.iter().enumerate() {
        if src_used.get(si).copied().unwrap_or(false) {
            continue;
        }
        let base = path_basename(path);
        // Both basenames must be unique among the remaining candidates.
        let Some(Some(src_idx)) = src_by_base.get(base).copied() else {
            continue;
        };
        if src_idx != si {
            continue;
        }
        let Some(Some(dst_idx)) = dst_by_base.get(base).copied() else {
            continue;
        };
        if dst_used.get(dst_idx).copied().unwrap_or(false) || dst_taken[dst_idx] {
            continue;
        }
        let Some(score) = similarity(si, dst_idx) else {
            continue;
        };
        if score < min_score {
            continue;
        }
        dst_taken[dst_idx] = true;
        matches.push((si, dst_idx, score));
    }
    matches
}

/// Break `data` into spans and return, per span hash, the total number of bytes
/// covered by spans with that hash. Spans end at a newline (inclusive) or once
/// they reach [`MAX_SPAN_BYTES`] bytes — exactly git's `hash_chars()` loop.
///
/// The returned map is `hash -> total_span_bytes`. Summing all values yields
/// `data.len()`, so the byte accounting is exact.
fn span_hash_counts(data: &[u8], is_text: bool) -> BTreeMap<u64, usize> {
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    let mut idx = 0usize;
    let len = data.len();
    while idx < len {
        // Roll a hash over the bytes of this span. The mixing mirrors git's
        // two-accumulator scheme from `diffcore-delta.c`; the exact constants do
        // not matter for correctness (any good per-span hash works), only that
        // identical spans collide and distinct spans rarely do.
        let mut accum1: u32 = 0;
        let mut accum2: u32 = 0;
        let mut span_len = 0usize;
        loop {
            let c = data[idx] as u32;
            idx += 1;
            // Ignore CR in a CRLF sequence for text blobs, so a file that only
            // differs by LF<->CRLF is still scored as (near-)identical — git's
            // `hash_chars()` does the same, which is what makes a CRLF-only
            // rename detectable.
            if is_text && c == u32::from(b'\r') && idx < len && data[idx] == b'\n' {
                continue;
            }
            span_len += 1;
            accum1 = (accum1 << 7) ^ (accum2 >> 25);
            accum2 = (accum2 << 7) ^ (accum1 >> 25);
            accum1 = accum1.wrapping_add(c);
            let newline = c == u32::from(b'\n');
            if span_len >= MAX_SPAN_BYTES || newline || idx >= len {
                break;
            }
        }
        // Fold the two accumulators (and the span length) into one 64-bit key.
        // Including the length keeps spans of different lengths from colliding
        // when their rolling-hash states happen to coincide.
        let hash = ((accum1 as u64) << 32) ^ (accum2 as u64) ^ ((span_len as u64) << 1);
        *counts.entry(hash).or_insert(0) += span_len;
    }
    counts
}

/// Sum, over every hash present in both maps, the smaller of the two byte
/// counts. This is git's `src_copied`: the number of bytes that appear on both
/// sides (counting multiplicity via the per-hash byte totals).
/// git `diffcore_count_changes()`: span-hash byte accounting between two
/// blobs. Returns `(src_copied, literal_added)` — the bytes of `src` that
/// survive into `dst`, and the bytes of `dst` not accounted for by `src`.
/// `--dirstat`'s default "changes" damage is
/// `(src.len() - src_copied) + literal_added`.
pub fn count_changes(src: &[u8], dst: &[u8]) -> (usize, usize) {
    let src_counts = span_hash_counts(src, blob_is_text(src));
    let dst_counts = span_hash_counts(dst, blob_is_text(dst));
    let copied = common_span_bytes(&src_counts, &dst_counts);
    (copied, dst.len() - copied)
}

/// Whether a blob is treated as text for span hashing (git's
/// `diff_filespec_is_binary` / `buffer_is_binary`): a NUL byte within the first
/// 8000 bytes marks it binary, in which case CRs are hashed literally.
fn blob_is_text(data: &[u8]) -> bool {
    const FIRST_FEW_BYTES: usize = 8000;
    !data.iter().take(FIRST_FEW_BYTES).any(|&byte| byte == 0)
}

fn common_span_bytes(src: &BTreeMap<u64, usize>, dst: &BTreeMap<u64, usize>) -> usize {
    let mut common = 0usize;
    // Iterate the smaller map for a few less lookups.
    let (small, large) = if src.len() <= dst.len() {
        (src, dst)
    } else {
        (dst, src)
    };
    for (hash, small_bytes) in small {
        if let Some(large_bytes) = large.get(hash) {
            common += (*small_bytes).min(*large_bytes);
        }
    }
    common
}

fn diff_entry_sort_path(entry: &NameStatusEntry) -> &[u8] {
    // git's diffcore re-inserts rename/copy pairs at their *destination*'s
    // position, so the queue (raw, numstat, stat, ...) sorts by the new path.
    entry.path.as_bytes()
}

fn mark_unstaged_worktree_oids_unresolved(
    changes: Vec<NameStatusEntry>,
    index_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    worktree_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
) -> Vec<NameStatusEntry> {
    changes
        .into_iter()
        .map(|mut entry| {
            let worktree_entry = worktree_entries.get(entry.path.as_bytes());
            if worktree_entry != index_entries.get(entry.path.as_bytes()) {
                entry.new_oid = None;
            }
            entry
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedEntry {
    mode: u32,
    oid: ObjectId,
}

/// A path-keyed map of tracked entries: one flattened side of a tree (or index/
/// worktree) snapshot.
type TrackedEntryMap = BTreeMap<Vec<u8>, TrackedEntry>;

/// The `(left, right)` sides produced by a tree-vs-tree comparison.
type TrackedEntryPair = (TrackedEntryMap, TrackedEntryMap);

struct IndexSnapshot {
    entries: BTreeMap<Vec<u8>, TrackedEntry>,
    stat_cache: IndexStatCache,
}

fn read_index_entries(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let index_path = sley_index::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = expand_sparse_index_for_worktree_diff(
        sley_index::read_repository_index(git_dir, format)?,
        git_dir,
        format,
    )?;
    Ok(index
        .entries
        .into_iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal && !entry.is_intent_to_add())
        .map(|entry| {
            (
                entry.path.into_bytes(),
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            )
        })
        .collect())
}

/// Collect the set of stage-0 paths flagged intent-to-add (`git add -N`) in the
/// index. These diff as new files rather than as modifications of their recorded
/// empty-blob id.
fn read_intent_to_add_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<std::collections::HashSet<Vec<u8>>> {
    let index_path = sley_index::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(std::collections::HashSet::new());
    }
    let index = expand_sparse_index_for_worktree_diff(
        sley_index::read_repository_index(git_dir, format)?,
        git_dir,
        format,
    )?;
    Ok(index
        .entries
        .iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal && entry.is_intent_to_add())
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect())
}

fn read_index_snapshot(git_dir: &Path, format: ObjectFormat) -> Result<IndexSnapshot> {
    let index_path = sley_index::repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IndexSnapshot {
                entries: BTreeMap::new(),
                stat_cache: IndexStatCache::default(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    let index = expand_sparse_index_for_worktree_diff(
        sley_index::read_repository_index(git_dir, format)?,
        git_dir,
        format,
    )?;
    let stat_cache =
        IndexStatCache::from_index_mtime(&index, sley_index::file_mtime_parts(&index_metadata));
    let entries = index
        .entries
        .into_iter()
        .map(|entry| {
            (
                entry.path.into_bytes(),
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            )
        })
        .collect();
    Ok(IndexSnapshot {
        entries,
        stat_cache,
    })
}

trait WorktreeIndexEntry {
    fn git_path(&self) -> &[u8];
    fn stage(&self) -> sley_index::Stage;
    fn mode(&self) -> u32;
    fn oid(&self) -> ObjectId;
    fn is_intent_to_add(&self) -> bool;
    fn is_skip_worktree(&self) -> bool;
    fn reusable_with(&self, stat_cache: &IndexStatCache, metadata: &fs::Metadata) -> bool;
}

impl WorktreeIndexEntry for sley_index::IndexEntry {
    fn git_path(&self) -> &[u8] {
        self.path.as_bytes()
    }

    fn stage(&self) -> sley_index::Stage {
        sley_index::IndexEntry::stage(self)
    }

    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> ObjectId {
        self.oid
    }

    fn is_intent_to_add(&self) -> bool {
        sley_index::IndexEntry::is_intent_to_add(self)
    }

    fn is_skip_worktree(&self) -> bool {
        sley_index::IndexEntry::is_skip_worktree(self)
    }

    fn reusable_with(&self, stat_cache: &IndexStatCache, metadata: &fs::Metadata) -> bool {
        stat_cache.reusable_index_entry(self, metadata).is_some()
    }
}

impl WorktreeIndexEntry for sley_index::IndexEntryRef<'_> {
    fn git_path(&self) -> &[u8] {
        self.path
    }

    fn stage(&self) -> sley_index::Stage {
        sley_index::IndexEntryRef::stage(self)
    }

    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> ObjectId {
        self.oid
    }

    fn is_intent_to_add(&self) -> bool {
        sley_index::IndexEntryRef::is_intent_to_add(self)
    }

    fn is_skip_worktree(&self) -> bool {
        sley_index::IndexEntryRef::is_skip_worktree(self)
    }

    fn reusable_with(&self, stat_cache: &IndexStatCache, metadata: &fs::Metadata) -> bool {
        stat_cache.reusable_index_entry_ref(self, metadata)
    }
}

fn tracked_entry_from_index(entry: &impl WorktreeIndexEntry) -> TrackedEntry {
    TrackedEntry {
        mode: entry.mode(),
        oid: entry.oid(),
    }
}

fn head_tree_entries(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let refs = FileRefStore::new(git_dir, format);
    let Some(head) = refs.read_ref("HEAD")? else {
        return Ok(BTreeMap::new());
    };
    let commit_oid = match head {
        RefTarget::Direct(oid) => Some(oid),
        RefTarget::Symbolic(name) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
    };
    let Some(commit_oid) = commit_oid else {
        return Ok(BTreeMap::new());
    };
    let object = db.read_object(&commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "HEAD {commit_oid} is not a commit"
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, &commit.tree, Vec::new(), &mut entries)?;
    Ok(entries)
}

/// Flatten `tree_oid` into `entries` (keyed by `prefix`-rooted full paths),
/// adapting the canonical [`flatten_tree`] tuples into [`TrackedEntry`].
///
/// `flatten_tree` flattens from an empty prefix; each of its paths is rejoined
/// under `prefix` with [`join_tree_path`], reproducing the recursive
/// prefix-building this helper previously did inline. Used by the full
/// (non-pruned) flatten paths: `--find-copies-harder` and the changed-subtree
/// add/delete sides of the simultaneous diff walk.
fn collect_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    for (rel_path, (mode, oid)) in flatten_tree(db, format, tree_oid)? {
        let path = join_tree_path(&prefix, &rel_path);
        entries.insert(path, TrackedEntry { mode, oid });
    }
    Ok(())
}

/// Git's mode value for a subtree (directory) entry inside a tree object.
const TREE_ENTRY_MODE: u32 = 0o040000;

/// Read `tree_oid` and parse it as a tree, erroring if the object is some other
/// type. Shared by the simultaneous tree-diff walk so both sides validate the
/// object type identically to [`collect_tree_entries`].
fn read_tree_object(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<Tree> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Tree::parse(format, &object.body)
}

/// Append `name` to `prefix` with a `/` separator (mirroring the path
/// construction in [`collect_tree_entries`]), returning the joined path.
fn join_tree_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// Fully flatten both trees into independent `left`/`right` maps (every blob on
/// each side, no pruning). Used only on the `--find-copies-harder` path, where
/// copy detection may reach into otherwise-unchanged subtrees for a source.
fn collect_full_tree_pair(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
) -> Result<TrackedEntryPair> {
    let mut left = BTreeMap::new();
    collect_tree_entries(db, format, left_tree, Vec::new(), &mut left)?;
    let mut right = BTreeMap::new();
    collect_tree_entries(db, format, right_tree, Vec::new(), &mut right)?;
    Ok((left, right))
}

/// Walk two trees *simultaneously*, collecting into `left` and `right` only the
/// blob entries that differ between the two sides — every entry that is present
/// and byte-identical (same mode + same OID) on both sides is omitted, and any
/// subtree whose OID is identical on both sides is skipped wholesale without
/// being read or recursed into. This is the core optimization git relies on to
/// make tree diffs cheap: equal subtrees are pruned in O(1).
///
/// The resulting `left`/`right` maps are exactly the subset of the fully
/// flattened maps (as produced by [`collect_tree_entries`]) restricted to the
/// paths that participate in an Added/Deleted/Modified change. Because
/// [`raw_name_status_changes`] emits nothing for a path that is identical on both
/// sides, diffing these pruned maps yields byte-identical name-status output to
/// diffing the full maps. (Callers that need the *complete* left map — i.e.
/// `--find-copies-harder`, where an unchanged file may be a copy source — must
/// still use [`collect_tree_entries`]; see the tree-diff entry points.)
fn changed_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
) -> Result<TrackedEntryPair> {
    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    // Identical root trees produce no changes at all and need not be read.
    if left_tree != right_tree {
        diff_tree_pair(
            db,
            format,
            left_tree,
            right_tree,
            &[],
            &mut left,
            &mut right,
        )?;
    }
    Ok((left, right))
}

/// Recursively diff two subtrees rooted at `prefix`, appending differing blob
/// entries to `left` / `right`. Invariant: the two OIDs are already known to
/// differ (identical subtrees are pruned by the caller before recursing).
fn diff_tree_pair(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    prefix: &[u8],
    left: &mut BTreeMap<Vec<u8>, TrackedEntry>,
    right: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let left_entries = read_tree_object(db, format, left_tree)?.entries;
    let right_entries = read_tree_object(db, format, right_tree)?.entries;

    // Index the right side by name so the union of names can be walked without
    // relying on git's directory-aware entry ordering. (Iterating the union of
    // names, rather than a positional merge, keeps correctness independent of
    // entry order.)
    let mut right_by_name: HashMap<&[u8], &TreeEntry> = HashMap::with_capacity(right_entries.len());
    for entry in &right_entries {
        right_by_name.insert(entry.name.as_bytes(), entry);
    }

    for left_entry in &left_entries {
        match right_by_name.remove(left_entry.name.as_bytes()) {
            Some(right_entry) => {
                merge_tree_entry(
                    db,
                    format,
                    prefix,
                    Some(left_entry),
                    Some(right_entry),
                    left,
                    right,
                )?;
            }
            None => {
                merge_tree_entry(db, format, prefix, Some(left_entry), None, left, right)?;
            }
        }
    }
    // Names only present on the right are pure additions.
    for right_entry in &right_entries {
        if right_by_name.contains_key(right_entry.name.as_bytes()) {
            merge_tree_entry(db, format, prefix, None, Some(right_entry), left, right)?;
        }
    }
    Ok(())
}

/// Reconcile a single name that may appear on the left side, the right side, or
/// both, recording any resulting blob change(s) into `left` / `right`. This
/// reproduces exactly the union-of-flattened-maps semantics:
///
/// * tree vs tree with equal OID -> pruned (no read, no recursion);
/// * tree vs tree with differing OID -> recurse;
/// * blob vs blob, equal mode+OID -> unchanged, emitted nowhere;
/// * blob vs blob, differing mode or OID -> both sides recorded (a Modify);
/// * a tree on one side and a non-tree on the other (or a name present on only
///   one side) -> the flattened paths differ (`name/...` vs `name`), so the two
///   are unrelated: the tree side is flattened wholesale and the blob side is
///   recorded independently (an Add and/or a Delete).
fn merge_tree_entry(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    prefix: &[u8],
    left_entry: Option<&TreeEntry>,
    right_entry: Option<&TreeEntry>,
    left: &mut BTreeMap<Vec<u8>, TrackedEntry>,
    right: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let left_is_tree = left_entry.is_some_and(|entry| entry.mode == TREE_ENTRY_MODE);
    let right_is_tree = right_entry.is_some_and(|entry| entry.mode == TREE_ENTRY_MODE);

    if let (Some(left_entry), Some(right_entry)) = (left_entry, right_entry) {
        if left_is_tree && right_is_tree {
            // Two subtrees under the same name: prune if identical, else recurse.
            if left_entry.oid == right_entry.oid {
                return Ok(());
            }
            let path = join_tree_path(prefix, left_entry.name.as_bytes());
            return diff_tree_pair(
                db,
                format,
                &left_entry.oid,
                &right_entry.oid,
                &path,
                left,
                right,
            );
        }
        if !left_is_tree && !right_is_tree {
            // Two blobs under the same name. Identical mode+OID means unchanged
            // (nothing emitted); otherwise both sides are recorded so the diff
            // sees a Modify, matching the full-map `left != right` comparison.
            if left_entry.mode == right_entry.mode && left_entry.oid == right_entry.oid {
                return Ok(());
            }
            let path = join_tree_path(prefix, left_entry.name.as_bytes());
            left.insert(
                path.clone(),
                TrackedEntry {
                    mode: left_entry.mode,
                    oid: left_entry.oid,
                },
            );
            right.insert(
                path,
                TrackedEntry {
                    mode: right_entry.mode,
                    oid: right_entry.oid,
                },
            );
            return Ok(());
        }
        // Mixed: tree on one side, blob on the other. Their flattened paths
        // never collide, so handle each side as if the name existed only there.
    }

    // Left side (if any): record as deletions.
    if let Some(left_entry) = left_entry {
        let path = join_tree_path(prefix, left_entry.name.as_bytes());
        if left_is_tree {
            collect_tree_entries(db, format, &left_entry.oid, path, left)?;
        } else {
            left.insert(
                path,
                TrackedEntry {
                    mode: left_entry.mode,
                    oid: left_entry.oid,
                },
            );
        }
    }
    // Right side (if any): record as additions.
    if let Some(right_entry) = right_entry {
        let path = join_tree_path(prefix, right_entry.name.as_bytes());
        if right_is_tree {
            collect_tree_entries(db, format, &right_entry.oid, path, right)?;
        } else {
            right.insert(
                path,
                TrackedEntry {
                    mode: right_entry.mode,
                    oid: right_entry.oid,
                },
            );
        }
    }
    Ok(())
}

fn index_gitlinks(index: &BTreeMap<Vec<u8>, TrackedEntry>) -> BTreeMap<Vec<u8>, ObjectId> {
    index
        .iter()
        .filter(|(_, entry)| sley_index::is_gitlink(entry.mode))
        .map(|(path, entry)| (path.clone(), entry.oid))
        .collect()
}

fn candidate_path_set<'a>(candidate_paths: impl Iterator<Item = &'a Vec<u8>>) -> BTreeSet<Vec<u8>> {
    candidate_paths.cloned().collect()
}

fn worktree_entries_for_path_set(
    worktree_root: &Path,
    format: ObjectFormat,
    candidates: &BTreeSet<Vec<u8>>,
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
    stat_cache: Option<&IndexStatCache>,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    worktree_entries_for_unique_paths(
        worktree_root,
        format,
        candidates.iter(),
        index_gitlinks,
        stat_cache,
    )
}

fn worktree_entries_for_unique_paths<'a>(
    worktree_root: &Path,
    format: ObjectFormat,
    candidates: impl Iterator<Item = &'a Vec<u8>>,
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
    stat_cache: Option<&IndexStatCache>,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    for git_path in candidates {
        if let Some(entry) =
            worktree_entry_for_path(worktree_root, format, git_path, index_gitlinks, stat_cache)?
        {
            entries.insert(git_path.clone(), entry);
        }
    }
    Ok(entries)
}

fn worktree_entry_for_path(
    worktree_root: &Path,
    format: ObjectFormat,
    git_path: &[u8],
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
    stat_cache: Option<&IndexStatCache>,
) -> Result<Option<TrackedEntry>> {
    let path = worktree_path_for_repo_path(worktree_root, git_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let file_type = metadata.file_type();
    if let Some(staged_oid) = index_gitlinks.get(git_path)
        && metadata.is_dir()
    {
        let oid = gitlink_head_oid(&path, format).unwrap_or(*staged_oid);
        return Ok(Some(TrackedEntry {
            mode: sley_index::GITLINK_MODE,
            oid,
        }));
    }
    if metadata.is_dir() {
        if let Some(oid) = gitlink_head_oid(&path, format) {
            return Ok(Some(TrackedEntry {
                mode: sley_index::GITLINK_MODE,
                oid,
            }));
        }
        return Ok(None);
    }
    if !(metadata.is_file() || file_type.is_symlink()) {
        return Ok(None);
    }
    if let Some(entry) = stat_cache.and_then(|cache| cache.reusable_entry(git_path, &metadata)) {
        return Ok(Some(tracked_entry_from_index(entry)));
    }
    Ok(Some(classify_worktree_entry(&path, &metadata, format)?))
}

fn index_worktree_change_for_entry(
    path: &Path,
    format: ObjectFormat,
    index_entry: &impl WorktreeIndexEntry,
    stat_cache: &IndexStatCache,
) -> Result<Option<NameStatusEntry>> {
    let git_path = index_entry.git_path();
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound && index_entry.is_skip_worktree() =>
        {
            return Ok(None);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(index_worktree_deleted_entry(index_entry)));
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let file_type = metadata.file_type();
    let right = if metadata.is_dir() {
        if sley_index::is_gitlink(index_entry.mode()) {
            let oid = gitlink_head_oid(path, format).unwrap_or(index_entry.oid());
            Some(TrackedEntry {
                mode: sley_index::GITLINK_MODE,
                oid,
            })
        } else {
            gitlink_head_oid(path, format).map(|oid| TrackedEntry {
                mode: sley_index::GITLINK_MODE,
                oid,
            })
        }
    } else if metadata.is_file() || file_type.is_symlink() {
        if index_entry.reusable_with(stat_cache, &metadata) {
            return Ok(None);
        }
        Some(classify_worktree_entry(path, &metadata, format)?)
    } else {
        None
    };
    let Some(right) = right else {
        return Ok(Some(index_worktree_deleted_entry(index_entry)));
    };
    let left = tracked_entry_from_index(index_entry);
    if right == left {
        return Ok(None);
    }
    Ok(Some(NameStatusEntry {
        status: modify_or_type_change(left.mode, right.mode),
        path: git_path.to_vec().into(),
        old_path: None,
        old_mode: Some(left.mode),
        new_mode: Some(right.mode),
        old_oid: Some(left.oid),
        new_oid: Some(right.oid),
    }))
}

fn index_worktree_deleted_entry(index_entry: &impl WorktreeIndexEntry) -> NameStatusEntry {
    NameStatusEntry {
        status: NameStatus::Deleted,
        path: index_entry.git_path().to_vec().into(),
        old_path: None,
        old_mode: Some(index_entry.mode()),
        new_mode: None,
        old_oid: Some(index_entry.oid()),
        new_oid: None,
    }
}

fn worktree_blob_cache_for_path_set(
    worktree_root: &Path,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: &BTreeSet<Vec<u8>>,
    options: RenameDetectionOptions,
) -> Result<HashMap<ObjectId, Vec<u8>>> {
    worktree_blob_cache_for_unique_paths(
        worktree_root,
        left_entries,
        right_entries,
        candidate_paths.iter(),
        options,
    )
}

fn worktree_blob_cache_for_unique_paths<'a>(
    worktree_root: &Path,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: RenameDetectionOptions,
) -> Result<HashMap<ObjectId, Vec<u8>>> {
    if !options.detect_inexact || !(options.base.detect_renames || options.base.detect_copies) {
        return Ok(HashMap::new());
    }
    let base = options.base;
    let mut changes =
        raw_name_status_changes_for_unique_paths(left_entries, right_entries, candidate_paths);
    if base.detect_renames {
        changes = detect_exact_renames(changes, left_entries, right_entries, base.rename_empty);
    }
    if base.detect_copies {
        changes = detect_exact_copies(
            changes,
            left_entries,
            right_entries,
            base.find_copies_harder,
            base.rename_empty,
        );
    }
    let has_rename_source = base.detect_renames
        && changes.iter().any(|entry| {
            entry.status == NameStatus::Deleted
                && entry
                    .old_oid
                    .as_ref()
                    .is_some_and(|oid| base.rename_empty || !is_empty_blob_oid(oid))
        });
    let has_copy_source = base.detect_copies
        && (base.find_copies_harder
            || changes
                .iter()
                .any(|entry| matches!(entry.status, NameStatus::Deleted | NameStatus::Modified)));
    if !has_rename_source && !has_copy_source {
        return Ok(HashMap::new());
    }
    let candidate_oids = changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Added)
        .filter_map(|entry| entry.new_oid)
        .filter(|oid| base.rename_empty || !is_empty_blob_oid(oid))
        .collect::<BTreeSet<_>>();
    if candidate_oids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut cache = HashMap::new();
    for (git_path, entry) in right_entries {
        if sley_index::is_gitlink(entry.mode) || !candidate_oids.contains(&entry.oid) {
            continue;
        }
        let path = worktree_path_for_repo_path(worktree_root, git_path);
        let body = if sley_index::is_symlink_mode(entry.mode) {
            symlink_target_bytes(&path)?
        } else {
            fs::read(&path)?
        };
        cache.entry(entry.oid).or_insert(body);
    }
    Ok(cache)
}

/// A blob fetcher that consults an in-memory `oid -> bytes` cache first (e.g.
/// freshly-read worktree files) and falls back to the object database.
fn cache_or_odb_blob(
    cache: &HashMap<ObjectId, Vec<u8>>,
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> Option<Vec<u8>> {
    if let Some(bytes) = cache.get(oid) {
        return Some(bytes.clone());
    }
    read_blob_bytes(db, oid)
}

#[cfg(unix)]
fn worktree_path_for_repo_path(worktree_root: &Path, path: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut out = PathBuf::from(worktree_root);
    out.push(OsStr::from_bytes(path));
    out
}

#[cfg(unix)]
fn worktree_path_for_repo_path_into(out: &mut PathBuf, worktree_root: &Path, path: &[u8]) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    out.clear();
    out.push(worktree_root);
    out.push(OsStr::from_bytes(path));
}

#[cfg(not(unix))]
fn worktree_path_for_repo_path(worktree_root: &Path, path: &[u8]) -> PathBuf {
    worktree_root.join(repo_path_to_path(path))
}

#[cfg(not(unix))]
fn worktree_path_for_repo_path_into(out: &mut PathBuf, worktree_root: &Path, path: &[u8]) {
    out.clear();
    out.push(worktree_root);
    out.push(repo_path_to_path(path));
}

#[cfg(not(unix))]
fn repo_path_to_path(path: &[u8]) -> PathBuf {
    let mut out = PathBuf::new();
    for component in String::from_utf8_lossy(path).split('/') {
        if !component.is_empty() {
            out.push(component);
        }
    }
    out
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

/// Read a symbolic link's target as git stores it: the raw target path bytes,
/// with no trailing newline. This is the "content" of a symlink blob (mode
/// `120000`) — git's `diff_populate_filespec` uses `strbuf_readlink` for a
/// worktree symlink rather than dereferencing it.
#[cfg(unix)]
pub fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let target = fs::read_link(path)?;
    Ok(target.as_os_str().as_bytes().to_vec())
}

/// See the unix variant: the raw symlink target bytes git stores as the blob.
#[cfg(not(unix))]
pub fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    Ok(target.to_string_lossy().replace('\\', "/").into_bytes())
}

// ---------------------------------------------------------------------------
// Unified / git diff patch parsing and application (engine for `git apply`/`git am`).
//
// Operates purely on in-memory byte buffers; the caller is responsible for
// reading/writing blobs from the working tree or the object database. The
// parser understands the textual format git produces (`diff --git`, `---`/`+++`
// file headers, `@@` hunk headers, context/`+`/`-` body lines, the
// `\ No newline at end of file` marker, `/dev/null` for added/deleted files,
// file mode headers, and `rename from`/`rename to` headers).
// ---------------------------------------------------------------------------

/// A single line inside a hunk. The stored bytes never include the trailing
/// line terminator; whether the line is terminated by `\n` is tracked
/// separately on the [`Hunk`] (see [`Hunk::old_no_newline`] /
/// [`Hunk::new_no_newline`]) so the no-final-newline case can be reproduced
/// byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    /// A line present in both the old and new versions.
    Context(Vec<u8>),
    /// A line added by the patch (present only in the new version).
    Insert(Vec<u8>),
    /// A line removed by the patch (present only in the old version).
    Delete(Vec<u8>),
}

impl HunkLine {
    /// The line content, without any trailing newline.
    pub fn content(&self) -> &[u8] {
        match self {
            Self::Context(bytes) | Self::Insert(bytes) | Self::Delete(bytes) => bytes,
        }
    }
}

/// A single `@@ -old_start,old_len +new_start,new_len @@` hunk.
///
/// `old_start` / `new_start` are 1-based line numbers as they appear in the
/// patch header. The `*_no_newline` flags record that the final line on that
/// side of the hunk is *not* terminated by a newline (the `\ No newline at end
/// of file` marker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<HunkLine>,
    /// The last context/deleted line of the old file lacks a trailing newline.
    pub old_no_newline: bool,
    /// The last context/inserted line of the new file lacks a trailing newline.
    pub new_no_newline: bool,
    /// The 1-based line number (in the patch input) of each entry in `lines`,
    /// used by `git apply`'s whitespace-error reporting (git's `state->linenr`).
    /// Empty when the patch was not parsed from input (e.g. synthesised hunks).
    pub line_input_lines: Vec<usize>,
}

/// A patch targeting a single file. Produced by [`parse_unified_patch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// Path on the `a/` (old) side, or `None` for a newly created file.
    pub old_path: Option<Vec<u8>>,
    /// Path on the `b/` (new) side, or `None` for a deleted file.
    pub new_path: Option<Vec<u8>>,
    /// Mode of the old file, when a mode header was present.
    pub old_mode: Option<u32>,
    /// Mode of the new file, when a mode header was present.
    pub new_mode: Option<u32>,
    pub hunks: Vec<Hunk>,
    /// The patch creates a new file (`--- /dev/null` / `new file mode`).
    pub is_new: bool,
    /// The patch deletes the file (`+++ /dev/null` / `deleted file mode`).
    pub is_delete: bool,
    /// The patch renames the file (`rename from`/`rename to`).
    pub is_rename: bool,
    /// The patch copies the file (`copy from`/`copy to`).
    pub is_copy: bool,
    /// Similarity score from `similarity index N%`, used for rename/copy summaries.
    pub similarity: Option<u8>,
    /// Dissimilarity score from `dissimilarity index N%`, used for rewrite summaries.
    pub dissimilarity: Option<u8>,
    /// Hex object id prefixes from the `index <old>..<new>[ mode]` line, if any.
    /// Carried verbatim (abbreviated or full); the binary apply and the `-3`
    /// fallback need these to resolve the pre-/post-image blobs.
    pub old_oid_hex: Option<Vec<u8>>,
    pub new_oid_hex: Option<Vec<u8>>,
    /// True when the patch is binary: either a `GIT binary patch` block (with
    /// `binary` payload) or a metadata-only `Binary files ... differ` line
    /// (no payload — the postimage must be reconstructed from the object store).
    pub is_binary: bool,
    /// The `GIT binary patch` payload, when this is a binary file patch. The
    /// fragment bytes are still zlib-deflated (the caller inflates them with
    /// the recorded original length), matching git's two-hunk forward/reverse
    /// layout.
    pub binary: Option<BinaryPatch>,
    /// True for git (`diff --git`) patches, whose names are relative to the
    /// repository top-level; false for traditional diffs, whose names are
    /// relative to the current directory (git's `is_toplevel_relative`). The
    /// `apply` cwd-prefix is only prepended to non-toplevel-relative patches.
    pub is_toplevel_relative: bool,
}

/// A `GIT binary patch` payload: a mandatory forward hunk (preimage → postimage)
/// and an optional reverse hunk (postimage → preimage), mirroring git's
/// `parse_binary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryPatch {
    pub forward: BinaryHunk,
    pub reverse: Option<BinaryHunk>,
}

/// One binary hunk: the encoding method and the still-deflated data, plus the
/// declared original (inflated) length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryHunk {
    pub method: BinaryMethod,
    /// Length of the data *after* inflation (the `literal <N>` / `delta <N>`
    /// number). The caller inflates `deflated` to exactly this many bytes.
    pub origlen: usize,
    /// base85-decoded, still zlib-deflated bytes.
    pub deflated: Vec<u8>,
}

/// How a binary hunk encodes the postimage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryMethod {
    /// The inflated bytes ARE the postimage (`literal <N>`).
    Literal,
    /// The inflated bytes are a git delta to apply to the preimage (`delta <N>`).
    Delta,
}

/// Outcome of applying a [`FilePatch`] to a base buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The patch applied cleanly; carries the resulting file bytes.
    Applied(Vec<u8>),
    /// At least one hunk's context/deleted lines did not match the base.
    Rejected,
}

/// The minimum number of context lines git's `apply` insists on keeping when
/// it tries to fuzz a hunk into place — git's `apply_state.p_context`, which is
/// initialised to `UINT_MAX` (the `-C<n>` option lowers it). The fuzz loop in
/// `apply_one_fragment` stops the moment both leading and trailing context have
/// been reduced to this floor; with the default `UINT_MAX` floor that test is
/// already satisfied on the first failure, so **the default `git apply` / `git
/// am` path does no context fuzz and no begin/end relaxation at all** — a hunk
/// whose full preimage does not match at a valid position is simply rejected.
/// We keep the floor configurable so the structure mirrors git's, but the
/// shared apply engine only ever runs with the default.
const MIN_FUZZ_CONTEXT: usize = usize::MAX;

/// Parse a unified/git diff into one [`FilePatch`] per file it touches.
///
/// The parser is intentionally lenient about leading commentary (commit
/// messages, `index <oid>..<oid>` lines, etc.): anything that is not part of a
/// recognised header or hunk body is skipped. It errors only on structurally
/// invalid hunks (bad `@@` headers, body lines that overflow the declared hunk
/// counts, or hunk bodies that appear with no preceding file header).
pub fn parse_unified_patch(input: &[u8]) -> Result<Vec<FilePatch>> {
    parse_unified_patch_with_recount(input, false)
}

/// Parse a unified/git diff, optionally ignoring hunk header line counts and
/// recounting them from the hunk body. This mirrors `git apply --recount`.
pub fn parse_unified_patch_with_recount(input: &[u8], recount: bool) -> Result<Vec<FilePatch>> {
    parse_unified_patch_with_options(input, recount, &PatchPathOptions::default())
}

/// Path-resolution options for [`parse_unified_patch_with_options`], mirroring
/// `git apply`'s `-p<n>` strip (`p_value`) and `--directory=<root>` prefix.
#[derive(Clone)]
pub struct PatchPathOptions {
    /// Number of leading path components to strip (`-p<n>`); default 1.
    pub p_value: usize,
    /// Whether `p_value` was given explicitly. When false, traditional (non-git)
    /// diffs guess it from the `---`/`+++` lines.
    pub p_value_known: bool,
    /// `--directory=<dir>` root, normalised with a trailing slash (or empty).
    pub root: Vec<u8>,
    /// The cwd prefix (`state->prefix`): the current directory relative to the
    /// work tree, with a trailing slash (empty at the top level). Used only to
    /// guess `-p<n>` for traditional patches run from a subdirectory; the prefix
    /// itself is prepended to names by the caller, not here.
    pub prefix: Vec<u8>,
}

impl Default for PatchPathOptions {
    fn default() -> Self {
        PatchPathOptions {
            p_value: 1,
            p_value_known: false,
            root: Vec::new(),
            prefix: Vec::new(),
        }
    }
}

/// Parse a unified/git diff, applying `-p<n>` strip and `--directory` prefix to
/// every resolved pathname exactly as `git apply` does.
pub fn parse_unified_patch_with_options(
    input: &[u8],
    recount: bool,
    options: &PatchPathOptions,
) -> Result<Vec<FilePatch>> {
    let lines = split_patch_lines(input);
    let mut parser = PatchParser {
        lines: &lines,
        index: 0,
        recount,
        p_value: options.p_value,
        p_value_known: options.p_value_known,
        root: options.root.clone(),
        prefix: options.prefix.clone(),
    };
    parser.parse()
}

/// Apply a single-file patch to `base`, returning the patched bytes.
///
/// This mirrors git's `apply.c` (`apply_one_fragment` / `find_pos` /
/// `match_fragment`) for the default, no-whitespace-fuzz settings `git am`
/// and `git apply` use:
///
/// * Each hunk builds a *preimage* (context + deleted lines) and *postimage*
///   (context + inserted lines).
/// * A hunk anchored at the file start (`old_start <= 1`) must match the
///   beginning of the file (`match_beginning`); a hunk with no trailing context
///   must match the end of the file (`match_end`).
/// * The full preimage is matched byte-for-byte; the search starts at the
///   recorded position and ping-pongs outward across the whole image.
/// * Fuzz is applied *only* by dropping leading/trailing context lines (never
///   by jumping to a spurious context-only match); if no position matches even
///   after dropping all context, the hunk — and thus the whole patch — is
///   [`ApplyOutcome::Rejected`].
///
/// Rejecting (rather than spuriously applying at a wrong offset) is what lets
/// `git am -3` correctly fall back to its 3-way merge path.
///
/// New-file patches (empty/ignored base) and the no-final-newline case are
/// handled byte-accurately. Clean exact-position applies are byte-identical to
/// the previous behaviour.
pub fn apply_file_patch(base: &[u8], patch: &FilePatch) -> ApplyOutcome {
    apply_file_patch_with_options(base, patch, &ApplyFileOptions::default())
}

/// Options for [`apply_file_patch_with_options`], mirroring the `git apply`
/// flags that change fragment placement.
#[derive(Clone, Default)]
pub struct ApplyFileOptions {
    /// `--unidiff-zero`: trust the line numbers of context-free hunks instead of
    /// forcing them to anchor at the file's beginning/end.
    pub unidiff_zero: bool,
}

/// Reverse a file patch (`git apply -R`): swap the old/new names, modes, hunk
/// ranges, and no-newline flags, exchange add↔delete status, and flip every
/// `Insert`/`Delete` line. Applying the result undoes the original patch.
pub fn reverse_file_patch(patch: &FilePatch) -> FilePatch {
    let hunks = patch
        .hunks
        .iter()
        .map(|hunk| {
            let lines = hunk
                .lines
                .iter()
                .map(|line| match line {
                    HunkLine::Context(b) => HunkLine::Context(b.clone()),
                    HunkLine::Insert(b) => HunkLine::Delete(b.clone()),
                    HunkLine::Delete(b) => HunkLine::Insert(b.clone()),
                })
                .collect();
            Hunk {
                old_start: hunk.new_start,
                old_len: hunk.new_len,
                new_start: hunk.old_start,
                new_len: hunk.old_len,
                lines,
                old_no_newline: hunk.new_no_newline,
                new_no_newline: hunk.old_no_newline,
                // Reversal keeps the line order (only the +/- sense flips), so the
                // per-line patch-input line numbers carry over unchanged.
                line_input_lines: hunk.line_input_lines.clone(),
            }
        })
        .collect();
    // git's `reverse_patches` only swaps the modes when the patch actually
    // carries a new mode (a mode change) or is a deletion; a content-only patch
    // keeps its (old) mode so the type-mismatch check still compares against it.
    let (old_mode, new_mode) = if patch.new_mode.is_some() || patch.is_delete {
        (patch.new_mode, patch.old_mode)
    } else {
        (patch.old_mode, patch.new_mode)
    };
    FilePatch {
        old_path: patch.new_path.clone(),
        new_path: patch.old_path.clone(),
        old_mode,
        new_mode,
        hunks,
        is_new: patch.is_delete,
        is_delete: patch.is_new,
        is_rename: patch.is_rename,
        is_copy: patch.is_copy,
        similarity: patch.similarity,
        dissimilarity: patch.dissimilarity,
        // Swap the index OIDs so a reverse-applied binary patch resolves the
        // (formerly new) preimage and (formerly old) postimage correctly.
        old_oid_hex: patch.new_oid_hex.clone(),
        new_oid_hex: patch.old_oid_hex.clone(),
        is_binary: patch.is_binary,
        binary: patch.binary.as_ref().map(|binary| BinaryPatch {
            // `-R` swaps forward and reverse hunks (git's apply_in_reverse).
            forward: binary.reverse.clone().unwrap_or_else(|| binary.forward.clone()),
            reverse: Some(binary.forward.clone()),
        }),
        is_toplevel_relative: patch.is_toplevel_relative,
    }
}

/// Apply a single-file patch with explicit fragment-placement options.
pub fn apply_file_patch_with_options(
    base: &[u8],
    patch: &FilePatch,
    options: &ApplyFileOptions,
) -> ApplyOutcome {
    // A pure deletion with no hunks yields an empty file.
    if patch.is_delete && patch.hunks.is_empty() {
        return ApplyOutcome::Applied(Vec::new());
    }
    // A new file: the only sensible base is empty; ignore whatever was passed
    // and build the result from the inserted lines.
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };

    // The "image" git mutates as each hunk applies. We splice in place so later
    // hunks see the effect of earlier ones (git carries the running offset for
    // the same reason).
    let mut image = split_blob_lines(base_for_match);

    // git seeds the search for hunk N at `newpos-1` *plus* the offset earlier
    // hunks drifted by, so a uniform shift only costs the search once.
    let mut running_offset: isize = 0;

    for hunk in &patch.hunks {
        match apply_one_hunk(&mut image, hunk, running_offset, options.unidiff_zero) {
            Some(drift) => running_offset += drift,
            None => return ApplyOutcome::Rejected,
        }
    }

    ApplyOutcome::Applied(join_lines(&image))
}

/// The outcome of a hunk-by-hunk apply (`git apply --reject`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectApply {
    /// The bytes after every hunk that applied (rejected hunks are skipped).
    pub content: Vec<u8>,
    /// Indices into `patch.hunks` of the hunks that did not apply.
    pub rejected: Vec<usize>,
}

/// Apply a single-file patch hunk-by-hunk, collecting the hunks that do not
/// apply rather than rejecting the whole patch — `git apply --reject`.
///
/// Each hunk is tried independently against the running image; an applied hunk
/// contributes its offset to later hunks (git's `apply_fragments` carries the
/// running line shift), a rejected hunk is recorded and left out. The returned
/// `content` is the image after all applicable hunks; `rejected` lists the
/// 0-based indices of the hunks the caller must write to `<file>.rej`.
pub fn apply_file_patch_rejecting(
    base: &[u8],
    patch: &FilePatch,
    options: &ApplyFileOptions,
) -> RejectApply {
    if patch.is_delete && patch.hunks.is_empty() {
        return RejectApply {
            content: Vec::new(),
            rejected: Vec::new(),
        };
    }
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };
    let mut image = split_blob_lines(base_for_match);
    let mut running_offset: isize = 0;
    let mut rejected = Vec::new();
    for (index, hunk) in patch.hunks.iter().enumerate() {
        match apply_one_hunk(&mut image, hunk, running_offset, options.unidiff_zero) {
            Some(drift) => running_offset += drift,
            None => rejected.push(index),
        }
    }
    RejectApply {
        content: join_lines(&image),
        rejected,
    }
}

/// Reconstruct the unified-diff text of one hunk for a `.rej` file. Mirrors the
/// raw fragment text git copies into `<file>.rej`: the `@@ -os[,oc] +ns[,nc] @@`
/// header (the `,1` count is omitted, matching git) followed by each line with
/// its ` `/`+`/`-` prefix, plus the `\ No newline at end of file` note where the
/// old/new side's final line is unterminated.
pub fn render_reject_hunk(hunk: &Hunk) -> Vec<u8> {
    fn range(start: usize, count: usize) -> String {
        if count == 1 {
            start.to_string()
        } else {
            format!("{start},{count}")
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"@@ -");
    out.extend_from_slice(range(hunk.old_start, hunk.old_len).as_bytes());
    out.extend_from_slice(b" +");
    out.extend_from_slice(range(hunk.new_start, hunk.new_len).as_bytes());
    out.extend_from_slice(b" @@\n");
    // The last old-side line is the last Context/Delete; the last new-side line
    // is the last Context/Insert. Their no-newline state drives the markers.
    let last_old = hunk
        .lines
        .iter()
        .rposition(|line| matches!(line, HunkLine::Context(_) | HunkLine::Delete(_)));
    let last_new = hunk
        .lines
        .iter()
        .rposition(|line| matches!(line, HunkLine::Context(_) | HunkLine::Insert(_)));
    for (index, line) in hunk.lines.iter().enumerate() {
        let (prefix, content) = match line {
            HunkLine::Context(bytes) => (b' ', bytes),
            HunkLine::Insert(bytes) => (b'+', bytes),
            HunkLine::Delete(bytes) => (b'-', bytes),
        };
        out.push(prefix);
        out.extend_from_slice(content);
        out.push(b'\n');
        let old_incomplete = hunk.old_no_newline && Some(index) == last_old;
        let new_incomplete = hunk.new_no_newline && Some(index) == last_new;
        if old_incomplete || new_incomplete {
            out.extend_from_slice(b"\\ No newline at end of file\n");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Whitespace-aware apply (`git apply --whitespace=fix` / `--ignore-space-change`)
//
// A faithful port of git's `apply_one_fragment` matching path (`match_fragment`,
// `find_pos`, `line_by_line_fuzzy_match`, `update_pre_post_images`,
// `update_image`). It adds the matching concerns that need the whitespace rule —
// blank-at-EOF tolerance, whitespace-corrected / whitespace-ignoring context
// matching, and removal of newly-added blank lines at EOF — on top of the plain
// exact-match engine above. The patch's `+` lines are expected to already carry
// their whitespace fixes (the apply command's whitespace pass applies them);
// this routine fixes the *context* lines as part of matching.
// ---------------------------------------------------------------------------

/// Options for [`apply_file_patch_ws`].
#[derive(Clone, Copy)]
pub struct WsApplyOptions {
    /// `--unidiff-zero`.
    pub unidiff_zero: bool,
    /// The per-path whitespace rule.
    pub ws_rule: ws::WsRule,
    /// `--whitespace=fix` (git's `correct_ws_error`).
    pub ws_fix: bool,
    /// `--ignore-space-change` / `--ignore-whitespace` (git's `ignore_ws_change`).
    pub ws_ignore_change: bool,
}

/// Outcome of [`apply_file_patch_ws`].
pub enum WsApplyOutcome {
    /// Applied; carries the bytes and the count of blank lines removed at EOF.
    Applied {
        content: Vec<u8>,
        blank_at_eof_removed: usize,
    },
    /// At least one hunk could not be placed.
    Rejected,
}

/// A pre/postimage line carrying git's `LINE_COMMON` flag (set for context lines,
/// clear for added/deleted lines).
#[derive(Clone)]
struct WsImageLine {
    content: Vec<u8>,
    no_newline: bool,
    common: bool,
}

impl WsImageLine {
    fn bytes(&self) -> Vec<u8> {
        let mut out = self.content.clone();
        if !self.no_newline {
            out.push(b'\n');
        }
        out
    }
}

fn line_bytes(line: &Line) -> Vec<u8> {
    let mut out = line.content.clone();
    if !line.no_newline {
        out.push(b'\n');
    }
    out
}

/// Split ws-fixed line bytes back into a [`WsImageLine`] (content sans trailing
/// newline, plus the no-newline flag).
fn ws_line_from_bytes(bytes: Vec<u8>, common: bool) -> WsImageLine {
    if bytes.last() == Some(&b'\n') {
        WsImageLine {
            content: bytes[..bytes.len() - 1].to_vec(),
            no_newline: false,
            common,
        }
    } else {
        WsImageLine {
            content: bytes,
            no_newline: true,
            common,
        }
    }
}

/// Whitespace-aware single-file apply — git's `apply_one_fragment` matching path.
pub fn apply_file_patch_ws(base: &[u8], patch: &FilePatch, opts: &WsApplyOptions) -> WsApplyOutcome {
    if patch.is_delete && patch.hunks.is_empty() {
        return WsApplyOutcome::Applied {
            content: Vec::new(),
            blank_at_eof_removed: 0,
        };
    }
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };
    let mut image = split_blob_lines(base_for_match);
    let mut running_offset: isize = 0;
    let mut blank_removed = 0usize;
    for hunk in &patch.hunks {
        match apply_one_fragment_ws(&mut image, hunk, running_offset, opts, &mut blank_removed) {
            Some(drift) => running_offset += drift,
            None => return WsApplyOutcome::Rejected,
        }
    }
    WsApplyOutcome::Applied {
        content: join_lines(&image),
        blank_at_eof_removed: blank_removed,
    }
}

fn apply_one_fragment_ws(
    image: &mut Vec<Line>,
    hunk: &Hunk,
    running_offset: isize,
    opts: &WsApplyOptions,
    blank_removed: &mut usize,
) -> Option<isize> {
    let blank_eof = opts.ws_rule & ws::WS_BLANK_AT_EOF != 0;
    let mut preimage: Vec<WsImageLine> = Vec::new();
    let mut postimage: Vec<WsImageLine> = Vec::new();
    let mut leading = 0usize;
    let mut trailing = 0usize;
    let mut seen_change = false;
    // git's `new_blank_lines_at_end`: blank lines added at the end, where a blank
    // *context* line does not reset the run but a non-blank line does.
    let mut new_blank_lines_at_end = 0usize;
    for hl in &hunk.lines {
        let mut added_blank_line = false;
        let mut is_blank_context = false;
        match hl {
            HunkLine::Context(bytes) => {
                if blank_eof && ws::ws_blank_line(bytes) {
                    is_blank_context = true;
                }
                preimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: true,
                });
                postimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: true,
                });
                if !seen_change {
                    leading += 1;
                }
                trailing += 1;
            }
            HunkLine::Delete(bytes) => {
                preimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: false,
                });
                seen_change = true;
                trailing = 0;
            }
            HunkLine::Insert(bytes) => {
                postimage.push(WsImageLine {
                    content: bytes.clone(),
                    no_newline: false,
                    common: false,
                });
                if blank_eof && ws::ws_blank_line(bytes) {
                    added_blank_line = true;
                }
                seen_change = true;
                trailing = 0;
            }
        }
        if added_blank_line {
            new_blank_lines_at_end += 1;
        } else if is_blank_context {
            // leave the running count alone
        } else {
            new_blank_lines_at_end = 0;
        }
    }
    if hunk.old_no_newline
        && let Some(last) = preimage.last_mut()
    {
        last.no_newline = true;
    }
    if hunk.new_no_newline
        && let Some(last) = postimage.last_mut()
    {
        last.no_newline = true;
    }

    let mut match_beginning = hunk.old_start == 0 || (hunk.old_start == 1 && !opts.unidiff_zero);
    let mut match_end = !opts.unidiff_zero && trailing == 0;

    let mut expected = if preimage.is_empty() {
        new_side_position(hunk, running_offset)
    } else {
        expected_position(hunk, running_offset)
    };
    let hunk_expected = expected;
    let mut leading_v = leading;
    let mut trailing_v = trailing;

    let applied_pos = loop {
        if let Some(pos) = find_pos_ws(
            image,
            &mut preimage,
            &mut postimage,
            expected,
            opts,
            match_beginning,
            match_end,
        ) {
            break pos;
        }
        #[allow(clippy::absurd_extreme_comparisons)]
        if leading_v <= MIN_FUZZ_CONTEXT && trailing_v <= MIN_FUZZ_CONTEXT {
            return None;
        }
        if match_beginning || match_end {
            match_beginning = false;
            match_end = false;
            continue;
        }
        if leading_v >= trailing_v {
            preimage.remove(0);
            postimage.remove(0);
            expected -= 1;
            leading_v -= 1;
        }
        if trailing_v > leading_v {
            preimage.pop();
            postimage.pop();
            trailing_v -= 1;
        }
    };

    // Remove the blank lines added at EOF when the hunk lands at (or beyond) the
    // end of the image — git's `--whitespace=fix` blank-at-EOF correction.
    if new_blank_lines_at_end > 0
        && preimage.len() + applied_pos >= image.len()
        && blank_eof
        && opts.ws_fix
    {
        for _ in 0..new_blank_lines_at_end {
            postimage.pop();
        }
        *blank_removed += new_blank_lines_at_end;
    }

    // git's `update_image`: the preimage may extend beyond EOF, so only the part
    // that falls within the image is removed.
    let preimage_limit = preimage.len().min(image.len() - applied_pos);
    let replacement: Vec<Line> = postimage
        .iter()
        .map(|line| Line {
            content: line.content.clone(),
            no_newline: line.no_newline,
        })
        .collect();
    image.splice(applied_pos..applied_pos + preimage_limit, replacement);
    Some(applied_pos as isize - hunk_expected)
}

/// Port of git's `find_pos`: ping-pong outward from `expected` calling
/// [`match_fragment_ws`] at each candidate line. On a match the preimage and the
/// common lines of the postimage may be rewritten in place (whitespace fix).
fn find_pos_ws(
    image: &[Line],
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    expected: isize,
    opts: &WsApplyOptions,
    match_beginning: bool,
    match_end: bool,
) -> Option<usize> {
    let line_nr = image.len();
    let pre_nr = preimage.len();
    let mut line: isize = if match_beginning {
        0
    } else if match_end {
        line_nr as isize - pre_nr as isize
    } else {
        expected
    };
    if line < 0 {
        line = 0;
    }
    if line as usize > line_nr {
        line = line_nr as isize;
    }
    let start = line as usize;
    let mut backwards = start;
    let mut forwards = start;
    let mut current = start;
    let mut i: u64 = 0;
    loop {
        if match_fragment_ws(
            image,
            preimage,
            postimage,
            current,
            opts,
            match_beginning,
            match_end,
        ) {
            return Some(current);
        }
        loop {
            if backwards == 0 && forwards == line_nr {
                return None;
            }
            if i & 1 == 1 {
                if backwards == 0 {
                    i += 1;
                    continue;
                }
                backwards -= 1;
                current = backwards;
            } else {
                if forwards == line_nr {
                    i += 1;
                    continue;
                }
                forwards += 1;
                current = forwards;
            }
            break;
        }
        i += 1;
    }
}

/// Port of git's `match_fragment`. Returns whether `preimage` matches `image` at
/// `current_lno`, trying (in order) an exact match, then — when `--whitespace=fix`
/// or `--ignore-space-change` is in effect — a whitespace-corrected or
/// whitespace-ignoring match, rewriting the preimage and the postimage's common
/// lines to the matched whitespace on success.
fn match_fragment_ws(
    image: &[Line],
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    current_lno: usize,
    opts: &WsApplyOptions,
    match_beginning: bool,
    match_end: bool,
) -> bool {
    let blank_eof = opts.ws_rule & ws::WS_BLANK_AT_EOF != 0;
    let preimage_limit: usize;
    if preimage.len() + current_lno <= image.len() {
        preimage_limit = preimage.len();
        if match_end && (preimage.len() + current_lno != image.len()) {
            return false;
        }
    } else if opts.ws_fix && blank_eof {
        // The hunk extends beyond EOF and we are removing blank lines there; only
        // the in-image prefix must match, the rest of the preimage must be blank.
        preimage_limit = image.len() - current_lno;
    } else {
        return false;
    }

    if match_beginning && current_lno != 0 {
        return false;
    }

    if preimage_limit == preimage.len() {
        // Try an exact byte match of the whole preimage.
        let mut exact = true;
        if match_end && current_lno + preimage_limit != image.len() {
            exact = false;
        }
        if exact {
            for i in 0..preimage_limit {
                let img = &image[current_lno + i];
                let pre = &preimage[i];
                if img.content != pre.content || img.no_newline != pre.no_newline {
                    exact = false;
                    break;
                }
            }
        }
        if exact {
            return true;
        }
    } else {
        // The preimage extends beyond EOF: there must be at least one non-blank
        // context line within the in-image prefix.
        let mut all_blank = true;
        for line in preimage.iter().take(preimage_limit) {
            if !line.content.iter().all(|&b| ws::is_space(b)) {
                all_blank = false;
                break;
            }
        }
        if all_blank {
            return false;
        }
    }

    // No exact match. Try fuzzy / whitespace-corrected matching.
    if opts.ws_ignore_change {
        return fuzzy_match_ws(image, preimage, postimage, current_lno, preimage_limit);
    }
    if !opts.ws_fix {
        return false;
    }

    // Whitespace-corrected match: fix the in-image preimage lines and the target
    // lines, requiring equality; the beyond-EOF preimage lines must become blank.
    let mut fixed: Vec<WsImageLine> = Vec::with_capacity(preimage.len());
    for i in 0..preimage_limit {
        let fixed_pre = ws::ws_fix_bytes(&preimage[i].bytes(), opts.ws_rule);
        let fixed_tgt = ws::ws_fix_bytes(&line_bytes(&image[current_lno + i]), opts.ws_rule);
        if fixed_pre != fixed_tgt {
            return false;
        }
        fixed.push(ws_line_from_bytes(fixed_pre, preimage[i].common));
    }
    for line in preimage.iter().skip(preimage_limit) {
        let fixed_pre = ws::ws_fix_bytes(&line.bytes(), opts.ws_rule);
        if !fixed_pre.iter().all(|&b| ws::is_space(b)) {
            return false;
        }
        fixed.push(ws_line_from_bytes(fixed_pre, line.common));
    }
    update_pre_post_images_ws(preimage, postimage, fixed);
    true
}

/// Port of git's `line_by_line_fuzzy_match` (the `--ignore-space-change` path):
/// compare each line ignoring whitespace runs; on success the matched lines use
/// the *target's* whitespace in-image and the *preimage's* whitespace beyond EOF.
fn fuzzy_match_ws(
    image: &[Line],
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    current_lno: usize,
    preimage_limit: usize,
) -> bool {
    for i in 0..preimage_limit {
        if !fuzzy_matchlines(&line_bytes(&image[current_lno + i]), &preimage[i].bytes()) {
            return false;
        }
    }
    // The beyond-EOF preimage lines must be all whitespace.
    for line in preimage.iter().skip(preimage_limit) {
        if !line.bytes().iter().all(|&b| ws::is_space(b)) {
            return false;
        }
    }
    // Build the fixed preimage: in-image lines take the target's whitespace, the
    // beyond-EOF lines keep the preimage's whitespace.
    let mut fixed: Vec<WsImageLine> = Vec::with_capacity(preimage.len());
    for i in 0..preimage_limit {
        let img = &image[current_lno + i];
        fixed.push(WsImageLine {
            content: img.content.clone(),
            no_newline: img.no_newline,
            common: preimage[i].common,
        });
    }
    for line in preimage.iter().skip(preimage_limit) {
        fixed.push(line.clone());
    }
    update_pre_post_images_ws(preimage, postimage, fixed);
    true
}

/// Port of git's `fuzzy_matchlines`: compare two lines ignoring whitespace
/// differences (any whitespace run matches any other; line endings are ignored).
fn fuzzy_matchlines(s1: &[u8], s2: &[u8]) -> bool {
    let trim = |s: &[u8]| {
        let mut end = s.len();
        while end > 0 && (s[end - 1] == b'\r' || s[end - 1] == b'\n') {
            end -= 1;
        }
        end
    };
    let end1 = trim(s1);
    let end2 = trim(s2);
    let (mut i, mut j) = (0usize, 0usize);
    while i < end1 && j < end2 {
        if ws::is_space(s1[i]) {
            if !ws::is_space(s2[j]) {
                return false;
            }
            while i < end1 && ws::is_space(s1[i]) {
                i += 1;
            }
            while j < end2 && ws::is_space(s2[j]) {
                j += 1;
            }
        } else if s1[i] != s2[j] {
            return false;
        } else {
            i += 1;
            j += 1;
        }
    }
    i == end1 && j == end2
}

/// Port of git's `update_pre_post_images`: replace the preimage with the fixed
/// lines (carrying the original common flags), then rewrite each *common* line of
/// the postimage to use the fixed preimage's content. A common postimage line
/// whose fixed-preimage counterpart ran out (a trailing blank trimmed at EOF) is
/// dropped (git's `reduced`).
fn update_pre_post_images_ws(
    preimage: &mut Vec<WsImageLine>,
    postimage: &mut Vec<WsImageLine>,
    fixed: Vec<WsImageLine>,
) {
    *preimage = fixed;
    let mut new_post: Vec<WsImageLine> = Vec::with_capacity(postimage.len());
    let mut ctx = 0usize;
    for line in postimage.iter() {
        if !line.common {
            new_post.push(line.clone());
            continue;
        }
        while ctx < preimage.len() && !preimage[ctx].common {
            ctx += 1;
        }
        if ctx >= preimage.len() {
            // preimage ran out (a fixed-away trailing blank): drop this line.
            continue;
        }
        new_post.push(WsImageLine {
            content: preimage[ctx].content.clone(),
            no_newline: preimage[ctx].no_newline,
            common: true,
        });
        ctx += 1;
    }
    *postimage = new_post;
}

/// Splice a single hunk into `image`, returning the offset (applied position −
/// expected position) so later hunks can carry it forward, or `None` if the
/// hunk cannot be located (which rejects the whole patch).
///
/// Faithful to git's `apply_one_fragment`: build preimage/postimage, try the
/// full preimage at progressively-reduced context, and on a match replace the
/// matched preimage region with the postimage.
fn apply_one_hunk(
    image: &mut Vec<Line>,
    hunk: &Hunk,
    running_offset: isize,
    unidiff_zero: bool,
) -> Option<isize> {
    // preimage = context + deletes (the old side we must find in the image).
    // postimage = context + inserts (what replaces it). They share their
    // leading/trailing *context* runs, which fuzz peels off symmetrically.
    let mut preimage: Vec<Line> = Vec::new();
    let mut postimage: Vec<Line> = Vec::new();
    let mut leading = 0usize; // context lines before the first +/-
    let mut trailing = 0usize; // context lines after the last +/-
    let mut seen_change = false;
    for hl in &hunk.lines {
        match hl {
            HunkLine::Context(bytes) => {
                preimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                postimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                if !seen_change {
                    leading += 1;
                }
                trailing += 1;
            }
            HunkLine::Delete(bytes) => {
                preimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                seen_change = true;
                trailing = 0;
            }
            HunkLine::Insert(bytes) => {
                postimage.push(Line {
                    content: bytes.clone(),
                    no_newline: false,
                });
                seen_change = true;
                trailing = 0;
            }
        }
    }

    // Mark the no-final-newline state on the last preimage/postimage line so the
    // exact-match check and the spliced result reproduce a missing terminal
    // newline byte-for-byte.
    if hunk.old_no_newline
        && let Some(last) = preimage.last_mut()
    {
        last.no_newline = true;
    }
    if hunk.new_no_newline
        && let Some(last) = postimage.last_mut()
    {
        last.no_newline = true;
    }

    // A hunk that is `@@ -1,L ... @@` (or `@@ -0,0 ... @@` for an add-to-empty)
    // must match the beginning, and a hunk with no trailing context must match
    // the end — UNLESS `--unidiff-zero` was given, which tells apply to trust the
    // line numbers of a context-free hunk (`match_beginning = !oldpos ||
    // (oldpos == 1 && !unidiff_zero)`, `match_end = !unidiff_zero && !trailing`).
    let mut match_beginning = hunk.old_start == 0 || (hunk.old_start == 1 && !unidiff_zero);
    let mut match_end = !unidiff_zero && trailing == 0;

    // git anchors the search at `newpos-1` (0-based), carried by the running
    // offset from earlier hunks. The anchor (`pos` in git) shifts up whenever a
    // *leading* context line is peeled, because the preimage then begins one
    // line later in its own content. For a context-free pure insertion the
    // preimage is empty and matches anywhere, so the anchor alone decides the
    // result — there we must use the new-side line number exactly as git's
    // `newpos - 1` does (the old-side `oldpos` differs for `@@ -1,0 +2,1 @@`
    // inserts).
    let mut expected = if preimage.is_empty() {
        new_side_position(hunk, running_offset)
    } else {
        expected_position(hunk, running_offset)
    };
    // The full hunk's expected position never moves, so the returned drift is
    // measured against it (not the context-reduced anchor).
    let hunk_expected = expected;

    loop {
        if let Some(pos) = find_hunk_pos(image, &preimage, expected, match_beginning, match_end) {
            // Splice: drop the matched preimage lines, insert the postimage.
            let take = preimage.len();
            let replacement: Vec<Line> = postimage.clone();
            image.splice(pos..pos + take, replacement);
            return Some(pos as isize - hunk_expected);
        }

        // No position matched. Mirror git's guard *order* exactly: it first
        // checks whether context is already at the floor (`p_context`) and, if
        // so, gives up BEFORE relaxing match_beginning/match_end or peeling
        // context. With the default `UINT_MAX` floor this fires on the very
        // first failure, so the default path never fuzzes and never relaxes the
        // begin/end anchors — it rejects. (The comparison is intentionally
        // against the floor so the structure stays faithful to git even though
        // the default floor makes it unconditionally true.)
        #[allow(clippy::absurd_extreme_comparisons)]
        if leading <= MIN_FUZZ_CONTEXT && trailing <= MIN_FUZZ_CONTEXT {
            return None;
        }

        // git relaxes the begin/end anchors before peeling context: a hunk that
        // "must match the start/end" but didn't is retried free-floating first.
        if match_beginning || match_end {
            match_beginning = false;
            match_end = false;
            continue;
        }

        // Reduce context: peel the larger side (both if equal), exactly as git.
        if leading >= trailing {
            // Drop the first context line from pre+post; the anchor slides up.
            preimage.remove(0);
            postimage.remove(0);
            expected -= 1;
            leading -= 1;
        }
        if trailing > leading {
            preimage.pop();
            postimage.pop();
            trailing -= 1;
        }
    }
}

/// A line with its content (sans terminator) and whether it is newline-terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    content: Vec<u8>,
    no_newline: bool,
}

/// Split a blob into [`Line`]s. A trailing `\n` does not produce an empty final
/// line; instead the last real line is marked `no_newline = false`. A file that
/// does not end in `\n` marks its final line `no_newline = true`. An empty blob
/// produces no lines.
fn split_blob_lines(data: &[u8]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < data.len() {
        match data[start..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let end = start + rel;
                lines.push(Line {
                    content: data[start..end].to_vec(),
                    no_newline: false,
                });
                start = end + 1;
            }
            None => {
                lines.push(Line {
                    content: data[start..].to_vec(),
                    no_newline: true,
                });
                start = data.len();
            }
        }
    }
    lines
}

/// Reassemble lines into a byte buffer, honouring per-line newline state.
fn join_lines(lines: &[Line]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&line.content);
        if !line.no_newline {
            out.push(b'\n');
        }
    }
    out
}

/// The naive 0-based position where a hunk expects to apply, given the running
/// offset accumulated from earlier hunks.
fn expected_position(hunk: &Hunk, running_offset: isize) -> isize {
    // `old_start` is 1-based; an empty old side (new-file hunk) uses 0.
    let base = if hunk.old_start == 0 {
        0
    } else {
        hunk.old_start as isize - 1
    };
    base + running_offset
}

/// git's `pos = frag->newpos ? newpos - 1 : 0` anchor, used for a context-free
/// pure insertion whose empty preimage matches anywhere.
fn new_side_position(hunk: &Hunk, running_offset: isize) -> isize {
    let base = if hunk.new_start == 0 {
        0
    } else {
        hunk.new_start as isize - 1
    };
    base + running_offset
}

/// Find the 0-based line index in `image` where `preimage` (the hunk's context
/// + deleted lines, possibly already context-reduced by fuzz) matches.
///
/// Port of git's `find_pos`: start the search at `expected` (clamped, or forced
/// to 0/end when `match_beginning`/`match_end`), then ping-pong outward across
/// the *whole* image — backward and forward alternately — until both ends are
/// exhausted. Returns the first matching line index, or `None`.
fn find_hunk_pos(
    image: &[Line],
    preimage: &[Line],
    expected: isize,
    match_beginning: bool,
    match_end: bool,
) -> Option<usize> {
    let line_nr = image.len();
    let pre_nr = preimage.len();

    // git: if we must match the beginning, start at 0; if we must match the
    // end, start where the preimage would end exactly at EOF.
    let mut line: isize = if match_beginning {
        0
    } else if match_end {
        line_nr as isize - pre_nr as isize
    } else {
        expected
    };
    if line < 0 {
        line = 0;
    }
    if line as usize > line_nr {
        line = line_nr as isize;
    }

    let start = line as usize;
    let mut backwards = start;
    let mut forwards = start;
    let mut current = start;

    let mut i: u64 = 0;
    loop {
        if preimage_matches_at(image, preimage, current, match_beginning, match_end) {
            return Some(current);
        }

        loop {
            // Both ends exhausted: no match anywhere.
            if backwards == 0 && forwards == line_nr {
                return None;
            }
            if i & 1 == 1 {
                // Step backward.
                if backwards == 0 {
                    i += 1;
                    continue;
                }
                backwards -= 1;
                current = backwards;
            } else {
                // Step forward.
                if forwards == line_nr {
                    i += 1;
                    continue;
                }
                forwards += 1;
                current = forwards;
            }
            break;
        }
        i += 1;
    }
}

/// Whether `preimage` matches `image` starting at line `pos`.
///
/// Port of git's `match_fragment` for the default (no whitespace-fuzz) path:
/// a byte-exact full-preimage match. Honours `match_beginning` (pos must be 0)
/// and `match_end` (the preimage must reach *exactly* the end of the image),
/// and reproduces git's terminal-newline semantics — a preimage line marked
/// "no newline" only matches when it is the image's final line and that line is
/// itself newline-free.
fn preimage_matches_at(
    image: &[Line],
    preimage: &[Line],
    pos: usize,
    match_beginning: bool,
    match_end: bool,
) -> bool {
    if match_beginning && pos != 0 {
        return false;
    }
    // The whole preimage must fall within the image.
    if pos + preimage.len() > image.len() {
        return false;
    }
    if match_end && pos + preimage.len() != image.len() {
        return false;
    }
    for (i, pre) in preimage.iter().enumerate() {
        let img = &image[pos + i];
        if img.content != pre.content {
            return false;
        }
        // git compares the raw byte buffers, so a missing terminal newline on
        // either side only matches the other when both agree. A preimage line
        // that lacks a newline can only sit on the image's final line (which
        // must itself lack one); a preimage line that *has* a newline cannot
        // match a newline-free image line.
        if pre.no_newline != img.no_newline {
            return false;
        }
    }
    true
}

/// Split raw patch bytes into lines, preserving the *content* without the
/// trailing `\n` (a final unterminated line is kept). Carriage returns are kept
/// as-is so CRLF patch bodies round-trip.
fn split_patch_lines(input: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < input.len() {
        match input[start..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let end = start + rel;
                lines.push(&input[start..end]);
                start = end + 1;
            }
            None => {
                lines.push(&input[start..]);
                start = input.len();
            }
        }
    }
    lines
}

struct PatchParser<'a> {
    lines: &'a [&'a [u8]],
    index: usize,
    recount: bool,
    /// `-p<n>` strip count (git's `state->p_value`); shared across the input so
    /// a guessed value sticks for subsequent traditional patches.
    p_value: usize,
    p_value_known: bool,
    /// `--directory` root (normalised, trailing slash) prepended to every name.
    root: Vec<u8>,
    /// The cwd prefix (`state->prefix`), used only to guess `-p<n>` for
    /// traditional patches run from a subdirectory.
    prefix: Vec<u8>,
}

impl<'a> PatchParser<'a> {
    fn parse(&mut self) -> Result<Vec<FilePatch>> {
        let mut patches = Vec::new();
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            if line.starts_with(b"diff --git ") {
                patches.push(self.parse_file(Some(line))?);
            } else if line.starts_with(b"--- ") {
                // A bare unified diff with no `diff --git` header.
                patches.push(self.parse_file(None)?);
            } else if line.starts_with(b"@@ ") {
                return Err(GitError::InvalidFormat(
                    "hunk header encountered before any file header".to_string(),
                ));
            } else {
                // Skip commentary / unrelated lines.
                self.index += 1;
            }
        }
        Ok(patches)
    }

    /// Parse one file's headers and hunks. When `diff_line` is `Some`, the
    /// current line is the `diff --git` header (already inspected by the
    /// caller); otherwise parsing starts at a `--- ` line of a traditional diff.
    fn parse_file(&mut self, diff_line: Option<&[u8]>) -> Result<FilePatch> {
        match diff_line {
            Some(diff_line) => self.parse_git_file(diff_line),
            None => self.parse_traditional_file(),
        }
    }

    /// p_value with one component removed — git uses `p_value - 1` for the
    /// `rename`/`copy from`/`to` extended headers, whose names lack the `a/`/`b/`
    /// prefix the `---`/`+++` lines carry.
    fn p_minus_one(&self) -> usize {
        self.p_value.saturating_sub(1)
    }

    /// Parse a git (`diff --git`) file section, resolving every pathname through
    /// git's `git_header_name` / `find_name` with the active `-p<n>`/`--directory`.
    fn parse_git_file(&mut self, diff_line: &[u8]) -> Result<FilePatch> {
        let mut patch = empty_file_patch();
        // `def_name`: the common name from the `diff --git` line, used when the
        // section carries no explicit `---`/`+++`/rename names.
        let rest = &diff_line[b"diff --git ".len()..];
        let mut def_name = name::git_header_name(self.p_value, rest);
        if let (Some(d), false) = (def_name.as_mut(), self.root.is_empty()) {
            let mut s = self.root.clone();
            s.extend_from_slice(d);
            *d = s;
        }
        self.index += 1;

        // Git patches name files relative to the repository top-level, so the
        // `apply` cwd-prefix is never prepended to them (git's is_toplevel_relative).
        patch.is_toplevel_relative = true;

        // Set once a `GIT binary patch` / `Binary files … differ` body is seen,
        // so the file is not run through the textual hunk parser afterwards.
        let mut binary_seen = false;

        // Extended headers until the first `---`/`@@`/next `diff --git`.
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            if line.starts_with(b"--- ") {
                self.parse_git_old_header(&line[b"--- ".len()..], &mut patch);
                self.index += 1;
                break;
            } else if line.starts_with(b"@@ ") {
                // No `---`/`+++` (e.g. pure rename or mode change with no body).
                break;
            } else if line.starts_with(b"diff --git ") {
                // Next file began with no body for this one.
                break;
            } else if let Some(rest) = strip_prefix(line, b"old mode ") {
                patch.old_mode = Some(self.parse_mode_line(rest)?);
            } else if let Some(rest) = strip_prefix(line, b"new mode ") {
                patch.new_mode = Some(self.parse_mode_line(rest)?);
            } else if let Some(rest) = strip_prefix(line, b"new file mode ") {
                patch.is_new = true;
                patch.new_mode = Some(self.parse_mode_line(rest)?);
                patch.new_path = def_name.clone();
            } else if let Some(rest) = strip_prefix(line, b"deleted file mode ") {
                patch.is_delete = true;
                patch.old_mode = Some(self.parse_mode_line(rest)?);
                patch.old_path = def_name.clone();
            } else if let Some(rest) = strip_prefix(line, b"index ") {
                // `index <old>..<new>[ <mode>]`: capture the blob OIDs (needed by
                // the binary apply and the `-3` fallback) and the unchanged-file
                // mode (git's gitdiff_index → gitdiff_oldmode).
                self.parse_index_line(rest, &mut patch)?;
            } else if let Some(rest) = strip_prefix(line, b"rename from ") {
                patch.is_rename = true;
                patch.old_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"rename to ") {
                patch.is_rename = true;
                patch.new_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"copy from ") {
                patch.is_copy = true;
                patch.old_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"copy to ") {
                patch.is_copy = true;
                patch.new_path = name::find_name(rest, None, self.p_minus_one(), 0, &self.root);
            } else if let Some(rest) = strip_prefix(line, b"similarity index ") {
                patch.similarity = parse_percent(rest);
            } else if let Some(rest) = strip_prefix(line, b"dissimilarity index ") {
                patch.dissimilarity = parse_percent(rest);
            } else if line == b"GIT binary patch" {
                // The binary payload follows (no `---`/`+++`, no `@@` hunks).
                let gitbin_line = self.index + 1;
                patch.is_binary = true;
                patch.binary = Some(self.parse_binary_block(gitbin_line)?);
                binary_seen = true;
                break;
            } else if apply_is_binary_files_differ(line) {
                // A `--binary`-less diff records only `Binary files … differ`;
                // the postimage has to come from the object store at apply time.
                patch.is_binary = true;
                binary_seen = true;
                self.index += 1;
                break;
            } else {
                // Unrecognised commentary line — ignore.
                self.index += 1;
                continue;
            }
            self.index += 1;
        }

        // `+++` header (the old-file branch above already advanced past `---`).
        if !binary_seen
            && self.index < self.lines.len()
            && self.lines[self.index].starts_with(b"+++ ")
        {
            let line = self.lines[self.index];
            self.parse_git_new_header(&line[b"+++ ".len()..], &mut patch);
            self.index += 1;
        }

        // No explicit names anywhere: fall back to `def_name`, or fail like git
        // when `-p<n>` stripped every component away.
        if patch.old_path.is_none() && patch.new_path.is_none() {
            match &def_name {
                Some(d) => {
                    patch.old_path = Some(d.clone());
                    patch.new_path = Some(d.clone());
                }
                None => {
                    return Err(GitError::InvalidFormat(format!(
                        "git diff header lacks filename information when removing {} \
                         leading pathname components",
                        self.p_value
                    )));
                }
            }
        }

        // Binary patches carry no `@@` hunks.
        if !binary_seen {
            self.parse_hunks(&mut patch)?;
        }
        Ok(patch)
    }

    /// Parse a `index <old>..<new>[ <mode>]` line, capturing the blob OIDs and,
    /// when present, the unchanged-file mode (git's `gitdiff_index`).
    fn parse_index_line(&self, rest: &[u8], patch: &mut FilePatch) -> Result<()> {
        let Some(dotdot) = find_subslice(rest, b"..") else {
            return Ok(());
        };
        let old = &rest[..dotdot];
        let after = &rest[dotdot + 2..];
        // `new` runs to the first space (mode) or end of line.
        let (new, mode_part) = match after.iter().position(|&b| b == b' ') {
            Some(space) => (&after[..space], Some(&after[space + 1..])),
            None => (after, None),
        };
        if !old.is_empty() {
            patch.old_oid_hex = Some(old.to_vec());
        }
        if !new.is_empty() {
            patch.new_oid_hex = Some(new.to_vec());
        }
        if let Some(mode) = mode_part
            && !mode.is_empty()
        {
            patch.old_mode = Some(self.parse_mode_line(mode)?);
        }
        Ok(())
    }

    /// Parse a `<octal mode>` field, mirroring git's `parse_mode_line`: leading
    /// octal digits terminated by whitespace or end of line. Errors otherwise.
    fn parse_mode_line(&self, rest: &[u8]) -> Result<u32> {
        let mut value: u32 = 0;
        let mut i = 0;
        while i < rest.len() && (b'0'..=b'7').contains(&rest[i]) {
            value = value
                .checked_mul(8)
                .and_then(|value| value.checked_add((rest[i] - b'0') as u32))
                .ok_or_else(|| self.invalid_mode_error(rest))?;
            i += 1;
        }
        if i == 0 || (i < rest.len() && !rest[i].is_ascii_whitespace()) {
            return Err(self.invalid_mode_error(rest));
        }
        Ok(value)
    }

    fn invalid_mode_error(&self, rest: &[u8]) -> GitError {
        GitError::InvalidFormat(format!(
            "invalid mode on line {}: {}",
            self.index + 1,
            lossy(rest)
        ))
    }

    /// Parse a `GIT binary patch` body: a mandatory forward hunk and an optional
    /// reverse hunk, each base85-encoded over zlib-deflated data. Mirrors git's
    /// `parse_binary`. `gitbin_line` is the 1-based line of the `GIT binary patch`
    /// marker (used in the "unrecognized binary patch" message).
    fn parse_binary_block(&mut self, gitbin_line: usize) -> Result<BinaryPatch> {
        // self.index points at "GIT binary patch"; advance past it.
        self.index += 1;
        let forward = match self.parse_binary_hunk()? {
            Some(hunk) => hunk,
            None => {
                return Err(GitError::InvalidFormat(format!(
                    "binary-unrecognized:{gitbin_line}"
                )));
            }
        };
        let reverse = self.parse_binary_hunk()?;
        Ok(BinaryPatch { forward, reverse })
    }

    /// Parse one binary hunk (method line + base85 data lines + blank terminator),
    /// or `Ok(None)` when the current line is not a `literal`/`delta` method line.
    fn parse_binary_hunk(&mut self) -> Result<Option<BinaryHunk>> {
        if self.index >= self.lines.len() {
            return Ok(None);
        }
        let line = self.lines[self.index];
        let (method, num) = if let Some(rest) = strip_prefix(line, b"delta ") {
            (BinaryMethod::Delta, rest)
        } else if let Some(rest) = strip_prefix(line, b"literal ") {
            (BinaryMethod::Literal, rest)
        } else {
            return Ok(None);
        };
        let origlen = parse_leading_usize(num);
        self.index += 1;

        let mut deflated = Vec::new();
        loop {
            if self.index >= self.lines.len() {
                // Ran out of input before the blank terminator (truncated patch).
                return Err(self.corrupt_binary_error());
            }
            let data = self.lines[self.index];
            if data.is_empty() {
                // Blank line terminates the hunk.
                self.index += 1;
                break;
            }
            // git counts the trailing newline in its line length; our split-off
            // lines do not carry it, so `git llen == data.len() + 1`.
            let len = data.len();
            if len < 6 || !(len - 1).is_multiple_of(5) {
                return Err(self.corrupt_binary_error());
            }
            let max_byte_length = (len - 1) / 5 * 4;
            let byte_length = match data[0] {
                b'A'..=b'Z' => (data[0] - b'A') as usize + 1,
                b'a'..=b'z' => (data[0] - b'a') as usize + 27,
                _ => return Err(self.corrupt_binary_error()),
            };
            if max_byte_length < byte_length || byte_length <= max_byte_length.saturating_sub(4) {
                return Err(self.corrupt_binary_error());
            }
            let decoded =
                decode_base85(&data[1..], byte_length).ok_or_else(|| self.corrupt_binary_error())?;
            deflated.extend_from_slice(&decoded);
            self.index += 1;
        }
        Ok(Some(BinaryHunk {
            method,
            origlen,
            deflated,
        }))
    }

    fn corrupt_binary_error(&self) -> GitError {
        GitError::InvalidFormat(format!("binary-corrupt:{}", self.index + 1))
    }

    fn parse_git_old_header(&self, rest: &[u8], patch: &mut FilePatch) {
        if name::is_dev_null(rest) {
            patch.is_new = true;
            patch.old_path = None;
        } else if patch.old_path.is_none() {
            patch.old_path = name::find_name(rest, None, self.p_value, name::TERM_TAB, &self.root);
        }
    }

    fn parse_git_new_header(&self, rest: &[u8], patch: &mut FilePatch) {
        if name::is_dev_null(rest) {
            patch.is_delete = true;
            patch.new_path = None;
        } else if patch.new_path.is_none() {
            patch.new_path = name::find_name(rest, None, self.p_value, name::TERM_TAB, &self.root);
        }
    }

    /// Parse a traditional (non-git) diff section, mirroring git's
    /// `parse_traditional_patch`: guess the strip count, recognise epoch
    /// timestamps as creation/deletion, and prefer the shorter of the two names.
    fn parse_traditional_file(&mut self) -> Result<FilePatch> {
        let mut patch = empty_file_patch();
        let first_line = self.lines[self.index];
        let first = first_line[b"--- ".len()..].to_vec();
        self.index += 1;
        let second = if self.index < self.lines.len()
            && self.lines[self.index].starts_with(b"+++ ")
        {
            let s = self.lines[self.index][b"+++ ".len()..].to_vec();
            self.index += 1;
            Some(s)
        } else {
            None
        };

        if let Some(second) = &second {
            if !self.p_value_known {
                let p0 = name::guess_p_value(&first, &self.root, &self.prefix);
                let q0 = name::guess_p_value(second, &self.root, &self.prefix);
                let p = if p0.is_none() { q0 } else { p0 };
                if let Some(pv) = p
                    && Some(pv) == q0
                {
                    self.p_value = pv;
                    self.p_value_known = true;
                }
            }

            let name = if name::is_dev_null(&first) {
                patch.is_new = true;
                let name = name::find_name_traditional(second, None, self.p_value, &self.root);
                patch.new_path = name.clone();
                name
            } else if name::is_dev_null(second) {
                patch.is_delete = true;
                let name = name::find_name_traditional(&first, None, self.p_value, &self.root);
                patch.old_path = name.clone();
                name
            } else {
                let first_name =
                    name::find_name_traditional(&first, None, self.p_value, &self.root);
                let name = name::find_name_traditional(
                    second,
                    first_name.as_deref(),
                    self.p_value,
                    &self.root,
                );
                if name::has_epoch_timestamp(&first) {
                    patch.is_new = true;
                    patch.new_path = name.clone();
                } else if name::has_epoch_timestamp(second) {
                    patch.is_delete = true;
                    patch.old_path = name.clone();
                } else {
                    patch.old_path = name.clone();
                    patch.new_path = name.clone();
                }
                name
            };
            // git's `parse_traditional_patch`: a name that strips away every
            // component (e.g. `-p2` against a one-component `file_in_root`) is a
            // hard error — the whole apply fails rather than silently skipping the
            // unresolved file.
            if name.is_none() {
                return Err(GitError::InvalidFormat(format!(
                    "unable to find filename in patch at line {}",
                    self.index
                )));
            }
        }

        self.parse_hunks(&mut patch)?;
        Ok(patch)
    }

    /// Parse the hunk bodies that follow a file header, stopping at the next
    /// file header.
    fn parse_hunks(&mut self, patch: &mut FilePatch) -> Result<()> {
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            // git's `parse_single_patch` only treats a line as a fragment when it
            // begins with `@@ -` (old side first). A `@@ +…` line — e.g. the
            // malformed header a Subversion-generated diff emits — is not a hunk;
            // it (and the lines after it) are skipped as commentary, so a deletion
            // with no real hunk still applies from its metadata alone.
            if line.starts_with(b"@@ -") {
                let hunk = self.parse_hunk()?;
                patch.hunks.push(hunk);
            } else if line.starts_with(b"diff --git ") {
                break;
            } else if line.starts_with(b"--- ") {
                // Start of a subsequent bare diff.
                break;
            } else {
                // Trailing commentary between/after hunks.
                self.index += 1;
            }
        }
        Ok(())
    }

    fn parse_hunk(&mut self) -> Result<Hunk> {
        let header = self.lines[self.index];
        let (old_start, old_len, new_start, new_len) = parse_hunk_header(header)?;
        self.index += 1;

        let mut hunk = Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines: Vec::new(),
            old_no_newline: false,
            new_no_newline: false,
            line_input_lines: Vec::new(),
        };
        let mut old_seen = 0usize;
        let mut new_seen = 0usize;

        while self.index < self.lines.len() {
            // Stop when both sides are satisfied. In recount mode the header
            // counts are intentionally ignored; the next hunk/file header ends
            // the body.
            if !self.recount && old_seen >= old_len && new_seen >= new_len {
                break;
            }
            let line = self.lines[self.index];
            if self.recount
                && (line.starts_with(b"@@ ")
                    || line.starts_with(b"diff --git ")
                    || line.starts_with(b"diff a/")
                    || line.starts_with(b"--- "))
            {
                break;
            }
            if line.is_empty() {
                // A wholly empty line in a unified diff is a context line whose
                // content is the empty string (git emits a bare ` `, but some
                // tooling/email transport strips the trailing space).
                hunk.lines.push(HunkLine::Context(Vec::new()));
                hunk.line_input_lines.push(self.index + 1);
                old_seen += 1;
                new_seen += 1;
                self.index += 1;
                continue;
            }
            match line[0] {
                b' ' => {
                    hunk.lines.push(HunkLine::Context(line[1..].to_vec()));
                    hunk.line_input_lines.push(self.index + 1);
                    old_seen += 1;
                    new_seen += 1;
                }
                b'+' => {
                    hunk.lines.push(HunkLine::Insert(line[1..].to_vec()));
                    hunk.line_input_lines.push(self.index + 1);
                    new_seen += 1;
                }
                b'-' => {
                    hunk.lines.push(HunkLine::Delete(line[1..].to_vec()));
                    hunk.line_input_lines.push(self.index + 1);
                    old_seen += 1;
                }
                b'\\' => {
                    // `\ No newline at end of file` — applies to the line just
                    // emitted. Set the appropriate side flag(s).
                    self.mark_no_newline(&mut hunk);
                    self.index += 1;
                    continue;
                }
                _ => {
                    // Anything else terminates the hunk body.
                    break;
                }
            }
            self.index += 1;
        }

        // A trailing `\ No newline` may follow the final body line even after
        // the counts are satisfied; consume it.
        if self.index < self.lines.len() && self.lines[self.index].starts_with(b"\\") {
            self.mark_no_newline(&mut hunk);
            self.index += 1;
        }

        if self.recount {
            hunk.old_len = old_seen;
            hunk.new_len = new_seen;
        } else if old_seen != old_len || new_seen != new_len {
            return Err(GitError::InvalidFormat(format!(
                "hunk body line counts mismatch: header declared -{old_len},+{new_len} \
                 but body had -{old_seen},+{new_seen}"
            )));
        }

        Ok(hunk)
    }

    /// Set the no-newline flag based on the kind of the most recently pushed
    /// hunk line.
    fn mark_no_newline(&self, hunk: &mut Hunk) {
        match hunk.lines.last() {
            Some(HunkLine::Context(_)) => {
                hunk.old_no_newline = true;
                hunk.new_no_newline = true;
            }
            Some(HunkLine::Insert(_)) => hunk.new_no_newline = true,
            Some(HunkLine::Delete(_)) => hunk.old_no_newline = true,
            None => {}
        }
    }
}

/// An all-empty [`FilePatch`] for the parser to fill in.
fn empty_file_patch() -> FilePatch {
    FilePatch {
        old_path: None,
        new_path: None,
        old_mode: None,
        new_mode: None,
        hunks: Vec::new(),
        is_new: false,
        is_delete: false,
        is_rename: false,
        is_copy: false,
        similarity: None,
        dissimilarity: None,
        old_oid_hex: None,
        new_oid_hex: None,
        is_binary: false,
        binary: None,
        is_toplevel_relative: false,
    }
}

/// Parse an `@@ -l,s +l,s @@` header into `(old_start, old_len, new_start,
/// new_len)`. A missing `,s` means a length of 1.
fn parse_hunk_header(line: &[u8]) -> Result<(usize, usize, usize, usize)> {
    let err = || GitError::InvalidFormat(format!("malformed hunk header: {}", lossy(line)));
    let rest = strip_prefix(line, b"@@ ").ok_or_else(err)?;
    // Up to the closing ` @@`.
    let close = find_subslice(rest, b" @@").ok_or_else(err)?;
    let ranges = &rest[..close];
    let mut parts = ranges.split(|&b| b == b' ').filter(|p| !p.is_empty());
    let old = parts.next().ok_or_else(err)?;
    let new = parts.next().ok_or_else(err)?;
    let old = strip_prefix(old, b"-").ok_or_else(err)?;
    let new = strip_prefix(new, b"+").ok_or_else(err)?;
    let (old_start, old_len) = parse_range(old).ok_or_else(err)?;
    let (new_start, new_len) = parse_range(new).ok_or_else(err)?;
    Ok((old_start, old_len, new_start, new_len))
}

/// Parse `start[,len]` into `(start, len)`, defaulting `len` to 1.
fn parse_range(range: &[u8]) -> Option<(usize, usize)> {
    match range.iter().position(|&b| b == b',') {
        Some(comma) => {
            let start = parse_usize(&range[..comma])?;
            let len = parse_usize(&range[comma + 1..])?;
            Some((start, len))
        }
        None => Some((parse_usize(range)?, 1)),
    }
}

fn parse_usize(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: usize = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(value)
}

fn parse_percent(bytes: &[u8]) -> Option<u8> {
    let trimmed = trim_ascii_end(bytes)
        .strip_suffix(b"%")
        .unwrap_or(trim_ascii_end(bytes));
    let value = parse_usize(trimmed)?;
    u8::try_from(value).ok().filter(|value| *value <= 100)
}

fn strip_prefix<'b>(line: &'b [u8], prefix: &[u8]) -> Option<&'b [u8]> {
    if line.starts_with(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

/// Whether a diff body line is a metadata-only binary marker (`Binary files …
/// differ` / `Files … differ`), git's binhdr detection.
fn apply_is_binary_files_differ(line: &[u8]) -> bool {
    line.ends_with(b" differ")
        && (line.starts_with(b"Binary files ") || line.starts_with(b"Files "))
}

/// Parse leading decimal digits (git uses `strtoul`, which ignores trailing
/// junk). Returns 0 when there are no leading digits.
fn parse_leading_usize(bytes: &[u8]) -> usize {
    let mut value = 0usize;
    for &b in bytes {
        if !b.is_ascii_digit() {
            break;
        }
        value = value.saturating_mul(10).saturating_add((b - b'0') as usize);
    }
    value
}

/// git's base85 alphabet (`base85.c` `en85`).
const BASE85_ALPHABET: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

fn base85_value(ch: u8) -> Option<u32> {
    BASE85_ALPHABET
        .iter()
        .position(|&c| c == ch)
        .map(|index| index as u32)
}

/// Decode `len` bytes from a base85 buffer (5 chars → 4 bytes, big-endian), a
/// port of git's `decode_85`. Returns `None` on an invalid alphabet character or
/// an overflowing 5-char group. `buffer` must contain `ceil(len/4) * 5` chars.
fn decode_base85(buffer: &[u8], len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut pos = 0usize;
    let mut remaining = len;
    while remaining > 0 {
        let mut acc: u32 = 0;
        // First four characters never overflow a u32 (85^4 < 2^32).
        for _ in 0..4 {
            let de = base85_value(*buffer.get(pos)?)?;
            pos += 1;
            acc = acc * 85 + de;
        }
        let de = base85_value(*buffer.get(pos)?)?;
        pos += 1;
        // The fifth character can overflow; reject it as git does.
        if 0xffff_ffffu32 / 85 < acc {
            return None;
        }
        acc *= 85;
        if 0xffff_ffffu32 - de < acc {
            return None;
        }
        acc += de;

        let cnt = remaining.min(4);
        remaining -= cnt;
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[..cnt]);
    }
    Some(out)
}

/// Apply a git delta (`delta.c` `patch_delta`) to reconstruct the postimage from
/// `base`. The delta begins with the base size and result size as varints,
/// followed by copy (`0x80` bit set: offset/size from base) and insert (literal
/// bytes) opcodes. Returns `None` on any malformed/inconsistent delta.
pub fn git_patch_delta(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut data = 0usize;
    let read_hdr_size = |data: &mut usize| -> Option<usize> {
        let mut size = 0usize;
        let mut shift = 0u32;
        loop {
            let cmd = *delta.get(*data)?;
            *data += 1;
            size |= ((cmd & 0x7f) as usize).checked_shl(shift)?;
            shift += 7;
            if cmd & 0x80 == 0 {
                break;
            }
        }
        Some(size)
    };

    let base_size = read_hdr_size(&mut data)?;
    if base_size != base.len() {
        return None;
    }
    let result_size = read_hdr_size(&mut data)?;
    let mut out = Vec::with_capacity(result_size);

    while data < delta.len() {
        let cmd = delta[data];
        data += 1;
        if cmd & 0x80 != 0 {
            // Copy from base.
            let mut cp_off = 0usize;
            let mut cp_size = 0usize;
            if cmd & 0x01 != 0 {
                cp_off = *delta.get(data)? as usize;
                data += 1;
            }
            if cmd & 0x02 != 0 {
                cp_off |= (*delta.get(data)? as usize) << 8;
                data += 1;
            }
            if cmd & 0x04 != 0 {
                cp_off |= (*delta.get(data)? as usize) << 16;
                data += 1;
            }
            if cmd & 0x08 != 0 {
                cp_off |= (*delta.get(data)? as usize) << 24;
                data += 1;
            }
            if cmd & 0x10 != 0 {
                cp_size = *delta.get(data)? as usize;
                data += 1;
            }
            if cmd & 0x20 != 0 {
                cp_size |= (*delta.get(data)? as usize) << 8;
                data += 1;
            }
            if cmd & 0x40 != 0 {
                cp_size |= (*delta.get(data)? as usize) << 16;
                data += 1;
            }
            if cp_size == 0 {
                cp_size = 0x10000;
            }
            let end = cp_off.checked_add(cp_size)?;
            if end > base.len() || cp_size > result_size {
                return None;
            }
            out.extend_from_slice(&base[cp_off..end]);
        } else if cmd != 0 {
            // Insert literal bytes from the delta.
            let len = cmd as usize;
            let end = data.checked_add(len)?;
            if end > delta.len() {
                return None;
            }
            out.extend_from_slice(&delta[data..end]);
            data = end;
        } else {
            // Opcode 0 is reserved.
            return None;
        }
    }

    if data != delta.len() || out.len() != result_size {
        return None;
    }
    Some(out)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn trim_ascii_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ===========================================================================
// Library tree-merge seam (`merge_trees`).
//
// This is the single 3-way tree-merge engine that every merge porcelain calls.
// Before it existed the logic was duplicated across the CLI: `merge-tree
// --write-tree` had its own copy and `git merge` / `cherry-pick` / `revert`
// had a second copy. Both copies implemented the identical per-path diff3
// resolution; the only differences were *rendering* (write-tree emits a tree +
// stage list + messages; the porcelains stage an index + materialize a
// worktree). This seam computes the merge once and returns a per-path result
// rich enough for both renderings, so the resolution lives in exactly one
// place.
//
// The result is byte-identical to the old per-command copies on every cell
// they already handled (clean merges, content / add-add / modify-delete
// conflicts, mode merges). On top of that it adds rename-aware resolution: a
// file renamed on one side and modified on the other follows the rename,
// gated by [`MergeTreesOptions::detect_renames`] (the classic merge-ort
// non-recursive rename case).
// ===========================================================================

/// Flattened tree: repository-relative path -> (mode, blob/symlink/gitlink oid).
pub type MergeEntryMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;

/// Whether to favour one side wholesale for textual conflicts (`-Xours` /
/// `-Xtheirs`), or to leave conflict markers in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeFavor {
    /// Leave conflict markers in place (the default).
    None,
    /// On a textual conflict, take ours' content wholesale.
    Ours,
    /// On a textual conflict, take theirs' content wholesale.
    Theirs,
    /// On a textual conflict, keep BOTH sides' lines (ours then theirs) with no
    /// markers — git's `merge=union` attribute / `--union` (`XDL_MERGE_FAVOR_UNION`).
    Union,
}

/// Options controlling a [`merge_trees`] run.
pub struct MergeTreesOptions<'a> {
    /// Conflict-marker label for ours (e.g. a branch name or `HEAD`).
    pub ours_label: &'a str,
    /// Conflict-marker label for theirs.
    pub theirs_label: &'a str,
    /// Diff3 ancestor label (the `|||||||` side); merge porcelains use
    /// `"merged common ancestors"`.
    pub ancestor_label: &'a str,
    /// `-Xours` / `-Xtheirs` favouring for textual conflicts.
    pub favor: MergeFavor,
    /// Enable rename-aware merging: a file renamed on one side and modified on
    /// the other follows the rename. When `false`, the merge is purely
    /// path-keyed (the historical behaviour).
    pub detect_renames: bool,
    /// Minimum similarity (`0..=100`) for inexact rename detection.
    pub rename_threshold: u8,
    /// Cap on the inexact rename matrix (`merge.renameLimit`/`diff.renameLimit`).
    /// `0` means unlimited; otherwise inexact detection is skipped when the
    /// candidate source × destination count exceeds `rename_limit²`.
    pub rename_limit: usize,
    /// Directory-rename detection mode. When [`DirectoryRenames::False`], a file
    /// added on one side under a directory that the *other* side renamed stays
    /// put. When enabled, such files are re-homed into the renamed directory,
    /// matching `merge.directoryRenames`. Requires `detect_renames` to have any
    /// effect (directory renames are inferred from the file renames it finds).
    pub directory_renames: DirectoryRenames,
    /// Conflict-marker style for textual conflicts (`merge.conflictStyle`).
    pub style: ConflictStyle,
}

/// How directory-rename detection behaves, mirroring git's
/// `merge.directoryRenames` configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DirectoryRenames {
    /// Disable directory-rename detection (`merge.directoryRenames=false`).
    #[default]
    False,
    /// Apply directory renames silently (`merge.directoryRenames=true`).
    True,
    /// Detect directory renames but treat each re-homed path as a conflict
    /// requiring confirmation (`merge.directoryRenames=conflict`). git's default.
    Conflict,
}

impl Default for MergeTreesOptions<'_> {
    fn default() -> Self {
        Self {
            ours_label: "ours",
            theirs_label: "theirs",
            ancestor_label: "merged common ancestors",
            favor: MergeFavor::None,
            detect_renames: false,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            directory_renames: DirectoryRenames::False,
            style: ConflictStyle::Merge,
        }
    }
}

/// The kind of conflict recorded for a path, used to render the stable
/// conflict-type token and human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeConflictKind {
    /// Both sides changed the file content differently (or both added it with
    /// differing content — an add/add).
    Content { add_add: bool },
    /// The file was deleted on one side and modified on the other.
    ModifyDelete {
        /// The side label that deleted the path.
        deleted_in: String,
        /// The side label that modified (and thus kept) the path.
        modified_in: String,
    },
    /// A file renamed on one side, with a content conflict against the other
    /// side's change at the destination.
    RenameContent {
        /// The original (pre-rename) path.
        old_path: Vec<u8>,
    },
    /// Two paths were renamed to the same destination, producing a
    /// rename/rename(2to1) conflict.
    RenameRenameTwoToOne {
        /// Ours' pre-destination path.
        ours_path: Vec<u8>,
        /// Theirs' pre-destination path.
        theirs_path: Vec<u8>,
    },
    /// One source path was renamed to different destinations on each side,
    /// producing a rename/rename(1to2) conflict.
    RenameRenameOneToTwo {
        /// The pre-rename source path.
        old_path: Vec<u8>,
        /// Ours' destination path.
        ours_path: Vec<u8>,
        /// Theirs' destination path.
        theirs_path: Vec<u8>,
        /// The label for our side.
        ours_label: String,
        /// The label for their side.
        theirs_label: String,
    },
    /// An auxiliary higher-stage entry for a rename/rename(1to2) conflict. The
    /// user-facing message is emitted by [`RenameRenameOneToTwo`].
    RenameRenameOneToTwoStage,
    /// A directory was split evenly across multiple destinations, so no
    /// directory rename could be applied for paths the other side left there.
    DirRenameSplit {
        /// The original directory with no unique destination.
        source_dir: Vec<u8>,
    },
    /// A file renamed on one side whose source was deleted on the other side.
    RenameDelete {
        /// The pre-rename source path.
        old_path: Vec<u8>,
        /// The side label that performed the rename.
        renamed_in: String,
        /// The side label that deleted the source.
        deleted_in: String,
    },
    /// A file collides with a directory at the same path in the merged result:
    /// the directory wins at the original path and the file is moved aside to
    /// `path~<branch>` (merge-ort's D/F conflict, `unique_path`). git emits
    /// `CONFLICT (file/directory): directory in the way of <old> from <branch>;
    /// moving it to <new> instead.`
    FileDirectory {
        /// The original (pre-move) path now occupied by the directory.
        original_path: Vec<u8>,
        /// The side label whose file was moved aside.
        moved_from: String,
    },
    /// A path was added/renamed under a directory the other side renamed, so the
    /// merge silently moved it into the renamed directory but, in
    /// `merge.directoryRenames=conflict` mode, flags it for the user to confirm.
    /// git emits `CONFLICT (file location): ... suggesting it should perhaps be
    /// moved to <new_path>.` The tree still contains the re-homed content.
    DirRenameLocation {
        /// The pre-re-home path (`old_path` in git's message): where the side
        /// placed the file before directory-rename detection moved it.
        old_path: Vec<u8>,
        /// `Some(source)` when the file was *renamed* into `old_path` by this
        /// side (git's "renamed to" wording, naming the original `source`);
        /// `None` when it was a fresh add (git's "added in" wording).
        renamed_from: Option<Vec<u8>>,
        /// The side label that added/renamed the file (`branch_with_new_path`).
        added_in: String,
        /// The side label that renamed the directory (`branch_with_dir_rename`).
        dir_renamed_in: String,
        /// True when the directory rename moved the file back onto its own base
        /// source path (rename-to-self) and the other side modified that path. The
        /// `CONFLICT (file location)` message is the same, but git records the
        /// path UNMERGED (stages 1/2/3) instead of staging the re-homed content
        /// cleanly: the index writers stage these 1/2/3, not at stage 0.
        back_to_self: bool,
    },
    /// A directory rename would have moved one or more paths onto this path, but
    /// it is already occupied (a file/dir in the way) or several sources map
    /// here. git emits `CONFLICT (implicit dir rename): Existing file/dir at
    /// <path> in the way of implicit directory rename(s) putting the following
    /// path(s) there: <sources>.` The path keeps its original content; the
    /// re-homed sources are left where they were.
    DirRenameImplicitCollision {
        /// The source path(s) the directory rename tried to move onto this path.
        sources: Vec<Vec<u8>>,
    },
    /// The two sides hold different object types at one path (regular↔symlink,
    /// regular↔gitlink, symlink↔gitlink). git's `process_entry` (merge-ort.c
    /// ~4220) renames each *regular-file* side to `path~<branch>` so each type
    /// can be recorded somewhere, ignoring `-Xours`/`-Xtheirs`, and emits a
    /// single `CONFLICT (distinct types)` line. (gitlink↔gitlink and
    /// symlink↔symlink share an `S_IFMT` and never reach this arm.) This kind is
    /// attached to the leaf that carries the message — the side left at
    /// `original_path` when only one side moved, else ours; the other renamed
    /// leaf carries [`DistinctTypesStage`].
    DistinctTypes {
        /// The original colliding path (git's message subject and sort key).
        original_path: Vec<u8>,
        /// `Some(p)` when ours was renamed aside to `p`; `None` when ours stayed
        /// at `original_path`.
        ours_renamed: Option<Vec<u8>>,
        /// `Some(p)` when theirs was renamed aside to `p`; `None` when theirs
        /// stayed at `original_path`.
        theirs_renamed: Option<Vec<u8>>,
    },
    /// The non-message-carrying leaf of a [`DistinctTypes`] conflict. The
    /// user-facing line is emitted once by the [`DistinctTypes`] leaf.
    DistinctTypesStage,
}

/// One resolved/conflicted path in the merged tree.
#[derive(Debug, Clone)]
pub struct MergedPath {
    /// Destination path in the merged tree.
    pub path: Vec<u8>,
    /// The per-stage (1=base, 2=ours, 3=theirs) entries when conflicted; all
    /// `None` for a clean resolution.
    pub stages: MergeStages,
    /// `Some((mode, oid))` is the final leaf written to the merged tree; `None`
    /// means the path is absent in the result (a clean delete).
    pub result: Option<(u32, ObjectId)>,
    /// When conflicted, the worktree bytes + mode to materialize (content with
    /// conflict markers, or the surviving side's bytes). `None` for a clean
    /// path.
    pub worktree: Option<(u32, Vec<u8>)>,
    /// `Some(..)` exactly when this path conflicted.
    pub conflict: Option<MergeConflictKind>,
    /// True when this path went through a textual 3-way content merge (both
    /// sides diverged and both were mergeable files). Drives the "Auto-merging
    /// <path>" informational message, which `git merge-tree` emits for every
    /// such path — clean or conflicted.
    pub auto_merged: bool,
}

impl MergedPath {
    /// True when this path resolved cleanly (no conflict recorded).
    pub fn is_clean(&self) -> bool {
        self.conflict.is_none()
    }
}

/// Per-stage higher-order index entries for a conflicted path.
#[derive(Debug, Clone, Default)]
pub struct MergeStages {
    pub base: Option<(u32, ObjectId)>,
    pub ours: Option<(u32, ObjectId)>,
    pub theirs: Option<(u32, ObjectId)>,
}

/// The outcome of a 3-way tree merge: the merged top-level tree plus per-path
/// detail and a clean/conflicted flag.
#[derive(Debug, Clone)]
pub struct MergeTreesResult {
    /// Object id of the merged top-level tree (always written, even on
    /// conflict — conflicted blobs go in with their marker content).
    pub tree: ObjectId,
    /// Per-path results, sorted by path.
    pub paths: Vec<MergedPath>,
    /// False if any path conflicted.
    pub clean: bool,
    /// Original paths removed by rename or directory-rename rewrites. These are
    /// cleanup-only paths for porcelains materializing a conflicted merge; they
    /// are absent from the merged tree.
    pub cleanup_paths: Vec<Vec<u8>>,
    /// Non-conflict informational messages produced while detecting renames.
    pub info_messages: Vec<MergeInfoMessage>,
}

impl MergeTreesResult {
    /// Iterate over the paths that conflicted, in path order.
    pub fn conflicts(&self) -> impl Iterator<Item = &MergedPath> {
        self.paths.iter().filter(|entry| entry.conflict.is_some())
    }
}

/// Non-conflict merge information that porcelain commands may print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeInfoMessage {
    /// A directory rename was skipped because the suggested target directory was
    /// itself renamed away on this side.
    DirRenameSkippedDueToRerename {
        old_dir: Vec<u8>,
        path: Vec<u8>,
        new_dir: Vec<u8>,
    },
    /// A path was updated due to a directory rename in
    /// `merge.directoryRenames=true` mode.
    DirRenameApplied {
        old_path: Vec<u8>,
        new_path: Vec<u8>,
        renamed_from: Option<Vec<u8>>,
        added_in: String,
        dir_renamed_in: String,
    },
    /// A directory-rename location conflict that overlaps another conflict at
    /// the same final path, such as a content conflict. The path's primary
    /// conflict kind remains attached to the path; this carries git's extra
    /// `CONFLICT (file location)` line.
    DirRenameLocationConflict {
        old_path: Vec<u8>,
        new_path: Vec<u8>,
        renamed_from: Option<Vec<u8>>,
        added_in: String,
        dir_renamed_in: String,
    },
    /// A rename/delete conflict whose conflicted destination was later moved
    /// aside by directory/file conflict handling. The primary per-path conflict
    /// remains `FileDirectory`; this preserves git's extra rename/delete line.
    RenameDeleteConflict {
        old_path: Vec<u8>,
        new_path: Vec<u8>,
        renamed_in: String,
        deleted_in: String,
    },
}

/// Read a tree object (by oid) into a flattened path -> (mode, oid) map,
/// descending into subtrees. The canonical empty tree yields an empty map.
pub fn flatten_tree(
    reader: &impl ObjectReader,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<MergeEntryMap> {
    let mut entries = BTreeMap::new();
    if *tree_oid == empty_tree_oid(format)? {
        return Ok(entries);
    }
    collect_flat_tree(reader, format, tree_oid, Vec::new(), &mut entries)?;
    Ok(entries)
}

fn collect_flat_tree(
    reader: &impl ObjectReader,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut MergeEntryMap,
) -> Result<()> {
    let object = reader.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {}, found {}",
            tree_oid,
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            collect_flat_tree(reader, format, &entry.oid, path, entries)?;
        } else {
            entries.insert(path, (entry.mode, entry.oid));
        }
    }
    Ok(())
}

/// True for a plain file blob (regular or executable) — i.e. a mode whose
/// content can be textually 3-way merged. Symlinks and gitlinks are excluded.
pub fn is_mergeable_file_mode(mode: u32) -> bool {
    mode == 0o100644 || mode == 0o100755
}

/// 3-way merge of three trees into a single merged tree.
///
/// `base` is the common-ancestor tree (`None` for unrelated histories — every
/// path is then treated as added on both sides). `ours`/`theirs` are the two
/// sides. Cleanly-merged blob content and the resulting (sub)trees are written
/// to `db`; the returned [`MergeTreesResult`] carries the merged top-level tree
/// oid plus per-path detail.
///
/// This is the shared engine behind `git merge-tree --write-tree`, `git merge`,
/// `git cherry-pick`, and `git revert`. It is behaviour-preserving relative to
/// the per-command copies it replaced, and additionally resolves renames when
/// [`MergeTreesOptions::detect_renames`] is set.
pub fn merge_trees(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: Option<&ObjectId>,
    ours: &ObjectId,
    theirs: &ObjectId,
    options: &MergeTreesOptions<'_>,
) -> Result<MergeTreesResult> {
    let base_map = match base {
        Some(tree) => flatten_tree(db, format, tree)?,
        None => MergeEntryMap::new(),
    };
    let ours_map = flatten_tree(db, format, ours)?;
    let theirs_map = flatten_tree(db, format, theirs)?;
    merge_entry_maps(db, format, &base_map, &ours_map, &theirs_map, options)
}

/// [`merge_trees`] operating on already-flattened entry maps. The merge
/// porcelains often hold the flattened maps already (e.g. cherry-pick builds
/// `theirs` from a picked commit's tree), so this avoids re-reading them.
pub fn merge_entry_maps(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<MergeTreesResult> {
    // Rename-aware step: detect files renamed on exactly one side relative to
    // base, so a modification on the other side follows the rename. This is the
    // non-recursive merge-ort rename case. We compute a rewrite map that, for a
    // one-sided rename old->new, presents the *other* side's `old` content at
    // `new` (and drops `old`), letting the path-keyed core below do the 3-way
    // content merge at the destination.
    let (mut renames, side_renames) = if options.detect_renames {
        let (renames, ours_side, theirs_side) =
            detect_merge_renames(db, format, base_map, ours_map, theirs_map, options)?;
        (renames, Some((ours_side, theirs_side)))
    } else {
        (MergeRenames::default(), None)
    };

    // Build the effective per-side maps with file renames applied.
    let (mut eff_base, mut eff_ours, mut eff_theirs) =
        apply_merge_renames(base_map, ours_map, theirs_map, &renames);

    // Directory-rename detection: when one side renamed a whole directory and
    // the other side added a file under (or renamed a file into) the old
    // directory, re-home that path into the renamed directory — including
    // transitive renames (a file the other side renamed into a directory this
    // side renamed follows on into the final directory). This is the
    // merge.directoryRenames behaviour, applied as a rewrite of the rename/add
    // destination paths so every merged path consults directory renames.
    let mut dir_rename_dirty = false;
    let mut rehomed_paths: BTreeMap<Vec<u8>, RehomeSides> = BTreeMap::new();
    let mut dir_rename_two_to_one: Vec<DirRenameTwoToOne> = Vec::new();
    let mut dir_rename_collisions: Vec<DirRenameCollision> = Vec::new();
    let mut dir_rename_splits: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut dir_rename_back_to_self: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut info_messages = Vec::new();
    let mut cleanup_paths: BTreeSet<Vec<u8>> = renames
        .dest_to_source
        .values()
        .map(|rename| rename.source.clone())
        .collect();
    if options.directory_renames != DirectoryRenames::False
        && let Some((ours_side, theirs_side)) = &side_renames
    {
        let dir_renames = compute_directory_renames(ours_map, theirs_map, ours_side, theirs_side);
        let outcome = apply_directory_renames(
            base_map,
            &eff_base,
            &eff_ours,
            &eff_theirs,
            ours_side,
            theirs_side,
            &dir_renames,
            &renames.dest_to_source,
        );
        eff_base = outcome.base;
        eff_ours = outcome.ours;
        eff_theirs = outcome.theirs;
        rehomed_paths = outcome.rehomed;
        dir_rename_collisions = outcome.collisions;
        dir_rename_splits = outcome.splits;
        dir_rename_back_to_self = outcome.back_to_self;
        info_messages = outcome.info_messages;
        dir_rename_dirty = outcome.dirty;
        remap_rename_destinations(&mut renames, &rehomed_paths);
        drop_collapsed_rename_rename_conflicts(&mut renames);
        dir_rename_two_to_one = collect_dir_rename_two_to_one(&renames, &rehomed_paths);
    }
    for info in rehomed_paths
        .values()
        .flat_map(|sides| [&sides.ours, &sides.theirs])
        .flatten()
    {
        cleanup_paths.insert(info.old_path.clone());
    }
    if options.directory_renames == DirectoryRenames::True {
        for (dest, sides) in &rehomed_paths {
            for info in [&sides.ours, &sides.theirs].into_iter().flatten() {
                let (added_in, dir_renamed_in) = if info.added_on_ours {
                    (
                        options.ours_label.to_string(),
                        options.theirs_label.to_string(),
                    )
                } else {
                    (
                        options.theirs_label.to_string(),
                        options.ours_label.to_string(),
                    )
                };
                info_messages.push(MergeInfoMessage::DirRenameApplied {
                    old_path: info.old_path.clone(),
                    new_path: dest.clone(),
                    renamed_from: info.renamed_from.clone(),
                    added_in,
                    dir_renamed_in,
                });
            }
        }
    }
    // In =conflict mode, every re-homed path is reported as a location conflict
    // (the tree still gets the re-homed content, but the merge is marked dirty).
    let dir_rename_conflict_paths: BTreeMap<Vec<u8>, RehomeSides> =
        if options.directory_renames == DirectoryRenames::Conflict {
            rehomed_paths.clone()
        } else {
            BTreeMap::new()
        };

    let mut all_paths = BTreeSet::new();
    all_paths.extend(eff_base.keys().cloned());
    all_paths.extend(eff_ours.keys().cloned());
    all_paths.extend(eff_theirs.keys().cloned());

    let mut paths: Vec<MergedPath> = Vec::new();
    let mut leaves: MergeEntryMap = BTreeMap::new();
    let mut clean = true;

    for path in all_paths {
        let base = eff_base.get(&path).cloned();
        let ours = eff_ours.get(&path).cloned();
        let theirs = eff_theirs.get(&path).cloned();
        let rename = renames.dest_to_source.get(&path);
        let old_path = rename.map(|r| r.source.clone());

        // Trivial resolutions (identical to the historical per-command logic).
        if ours == theirs {
            if let Some(entry) = ours {
                leaves.insert(path.clone(), entry);
            }
            paths.push(clean_path(path, ours));
            continue;
        }
        if ours == base {
            if let Some(entry) = &theirs {
                leaves.insert(path.clone(), *entry);
            }
            paths.push(clean_path(path, theirs));
            continue;
        }
        if theirs == base {
            if let Some(entry) = &ours {
                leaves.insert(path.clone(), *entry);
            }
            paths.push(clean_path(path, ours));
            continue;
        }

        // Both sides diverged. Decide how to combine.
        let content_mergeable = matches!(&ours, Some((mode, _)) if is_mergeable_file_mode(*mode))
            && matches!(&theirs, Some((mode, _)) if is_mergeable_file_mode(*mode))
            && match &base {
                Some((mode, _)) => is_mergeable_file_mode(*mode),
                None => true,
            };

        if let (true, Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) =
            (content_mergeable, &ours, &theirs)
        {
            let add_add = base.is_none();
            let base_bytes = match &base {
                Some((_, oid)) => merge_blob_bytes(db, oid)?,
                None => Vec::new(),
            };
            let ours_bytes = merge_blob_bytes(db, ours_oid)?;
            let theirs_bytes = merge_blob_bytes(db, theirs_oid)?;
            // When this destination came from a one-sided rename, git qualifies
            // the conflict-marker labels with the per-side path (the renaming
            // side shows the new path, the other side the old path), e.g.
            // `<<<<<<< HEAD:old.txt` / `>>>>>>> feature:new.txt`.
            let rehome = rehomed_paths.get(&path);
            let (ours_label, theirs_label) = match rename {
                Some(MergeRename { source, side }) => {
                    let (ours_path, theirs_path) = match side {
                        // theirs renamed -> ours kept the source path.
                        RenameSide::Theirs => (source.as_slice(), path.as_slice()),
                        // ours renamed -> theirs kept the source path.
                        RenameSide::Ours => (path.as_slice(), source.as_slice()),
                    };
                    (
                        qualify_label(options.ours_label, ours_path),
                        qualify_label(options.theirs_label, theirs_path),
                    )
                }
                None => {
                    let ours_path = rehome
                        .and_then(|info| info.ours.as_ref())
                        .map_or(path.as_slice(), |info| info.old_path.as_slice());
                    let theirs_path = rehome
                        .and_then(|info| info.theirs.as_ref())
                        .map_or(path.as_slice(), |info| info.old_path.as_slice());
                    if ours_path != path.as_slice() || theirs_path != path.as_slice() {
                        (
                            qualify_label(options.ours_label, ours_path),
                            qualify_label(options.theirs_label, theirs_path),
                        )
                    } else {
                        (
                            options.ours_label.to_string(),
                            options.theirs_label.to_string(),
                        )
                    }
                }
            };
            let result = merge_blobs(
                &base_bytes,
                &ours_bytes,
                &theirs_bytes,
                &MergeBlobOptions {
                    ours_label: &ours_label,
                    theirs_label: &theirs_label,
                    base_label: options.ancestor_label,
                    style: options.style,
                    favor: options.favor,
                },
            );

            let base_mode = base.as_ref().map(|(mode, _)| *mode);
            let (resolved_mode, mode_conflict) =
                merge_file_modes(base_mode, *ours_mode, *theirs_mode);

            if !result.conflicted && !mode_conflict {
                let oid = db.write_object(EncodedObject::new(ObjectType::Blob, result.content))?;
                leaves.insert(path.clone(), (resolved_mode, oid));
                paths.push(clean_path_auto(path, Some((resolved_mode, oid)), true));
            } else if options.favor != MergeFavor::None && !mode_conflict {
                let chosen = if options.favor == MergeFavor::Ours {
                    ours
                } else {
                    theirs
                };
                if let Some(entry) = chosen {
                    leaves.insert(path.clone(), entry);
                }
                paths.push(clean_path_auto(path, chosen, true));
            } else {
                clean = false;
                let oid =
                    db.write_object(EncodedObject::new(ObjectType::Blob, result.content.clone()))?;
                leaves.insert(path.clone(), (resolved_mode, oid));
                let worktree_mode = if *ours_mode == *theirs_mode {
                    *ours_mode
                } else {
                    0o100644
                };
                let conflict = if let Some(old) = &old_path {
                    MergeConflictKind::RenameContent {
                        old_path: old.clone(),
                    }
                } else if add_add {
                    match rehome.and_then(|info| Some((info.ours.as_ref()?, info.theirs.as_ref()?)))
                    {
                        Some((ours_info, theirs_info)) => MergeConflictKind::RenameRenameTwoToOne {
                            ours_path: ours_info.old_path.clone(),
                            theirs_path: theirs_info.old_path.clone(),
                        },
                        None => MergeConflictKind::Content { add_add },
                    }
                } else {
                    MergeConflictKind::Content { add_add }
                };
                paths.push(MergedPath {
                    path: path.clone(),
                    stages: stages_for(&base, &ours, &theirs),
                    result: Some((resolved_mode, oid)),
                    worktree: Some((worktree_mode, result.content)),
                    conflict: Some(conflict),
                    auto_merged: true,
                });
            }
        } else if base.is_some() && (ours.is_none() || theirs.is_none()) {
            // modify/delete.
            clean = false;
            let (deleted_in, modified_in, surviving) = if ours.is_none() {
                (
                    options.ours_label.to_string(),
                    options.theirs_label.to_string(),
                    theirs,
                )
            } else {
                (
                    options.theirs_label.to_string(),
                    options.ours_label.to_string(),
                    ours,
                )
            };
            let worktree = match &surviving {
                Some((mode, oid)) => Some((*mode, merge_worktree_bytes(db, *mode, oid)?)),
                None => None,
            };
            if let Some(entry) = surviving {
                leaves.insert(path.clone(), entry);
            }
            paths.push(MergedPath {
                path: path.clone(),
                stages: stages_for(&base, &ours, &theirs),
                result: surviving,
                worktree,
                conflict: Some(MergeConflictKind::ModifyDelete {
                    deleted_in,
                    modified_in,
                }),
                auto_merged: false,
            });
        } else if let (Some(&(ours_mode, ours_oid)), Some(&(theirs_mode, theirs_oid))) =
            (ours.as_ref(), theirs.as_ref())
            && sley_index::is_symlink_mode(ours_mode)
            && sley_index::is_symlink_mode(theirs_mode)
        {
            // Both sides are symlinks that diverged from the base and from each
            // other (the trivial oid resolutions above already took the agreeing
            // cases). A symlink is never textually merged; git's
            // `handle_content_merge` symlink arm (merge-ort.c) resolves CLEAN to
            // a side under `-Xours`/`-Xtheirs`, and otherwise records a CONFLICT
            // carrying ours' target.
            match options.favor {
                MergeFavor::Ours => {
                    leaves.insert(path.clone(), (ours_mode, ours_oid));
                    paths.push(clean_path_auto(path.clone(), Some((ours_mode, ours_oid)), false));
                }
                MergeFavor::Theirs => {
                    leaves.insert(path.clone(), (theirs_mode, theirs_oid));
                    paths.push(clean_path_auto(
                        path.clone(),
                        Some((theirs_mode, theirs_oid)),
                        false,
                    ));
                }
                MergeFavor::None | MergeFavor::Union => {
                    clean = false;
                    leaves.insert(path.clone(), (ours_mode, ours_oid));
                    let worktree = Some((ours_mode, merge_worktree_bytes(db, ours_mode, &ours_oid)?));
                    paths.push(MergedPath {
                        path: path.clone(),
                        stages: stages_for(&base, &ours, &theirs),
                        result: Some((ours_mode, ours_oid)),
                        worktree,
                        conflict: Some(MergeConflictKind::Content {
                            add_add: base.is_none(),
                        }),
                        auto_merged: false,
                    });
                }
            }
        } else if let (Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) =
            (ours, theirs)
            && is_type_change(ours_mode, theirs_mode)
        {
            // Distinct types at one path: both sides present with different
            // `S_IFMT` (regular↔symlink, regular↔gitlink, symlink↔gitlink).
            // Mirror merge-ort's `process_entry`: rename each regular-file side
            // to `path~<branch>` so each type is recorded somewhere; ignore
            // `-Xours`/`-Xtheirs`. gitlink↔gitlink and symlink↔symlink share an
            // `S_IFMT` and are handled by the arms above.
            clean = false;
            // git renames the regular-file side(s): only the regular side when
            // exactly one is regular, both when neither is (symlink↔gitlink).
            let (rename_ours, rename_theirs) = if is_mergeable_file_mode(ours_mode) {
                (true, false)
            } else if is_mergeable_file_mode(theirs_mode) {
                (false, true)
            } else {
                (true, true)
            };
            // git keeps the base stage (index stage 1) for a side only when that
            // side shares the base's file type.
            let ours_base = base.filter(|(mode, _)| !is_type_change(*mode, ours_mode));
            let theirs_base = base.filter(|(mode, _)| !is_type_change(*mode, theirs_mode));
            // Name and reserve ours' aside-path first so the two renamed paths
            // can never collide (`unique_df_path` consults `leaves`/`paths`).
            let ours_path = if rename_ours {
                unique_df_path(&path, options.ours_label, &leaves, &paths)
            } else {
                path.clone()
            };
            leaves.insert(ours_path.clone(), (ours_mode, ours_oid));
            let theirs_path = if rename_theirs {
                unique_df_path(&path, options.theirs_label, &leaves, &paths)
            } else {
                path.clone()
            };
            leaves.insert(theirs_path.clone(), (theirs_mode, theirs_oid));

            // The message is emitted once, by the leaf left at `original_path`
            // when only one side moved (matching git's keying), else by ours.
            let ours_carries_message = !rename_ours || rename_theirs;
            let distinct = MergeConflictKind::DistinctTypes {
                original_path: path.clone(),
                ours_renamed: rename_ours.then(|| ours_path.clone()),
                theirs_renamed: rename_theirs.then(|| theirs_path.clone()),
            };
            let ours_worktree =
                Some((ours_mode, merge_worktree_bytes(db, ours_mode, &ours_oid)?));
            paths.push(MergedPath {
                path: ours_path,
                stages: MergeStages {
                    base: ours_base,
                    ours: Some((ours_mode, ours_oid)),
                    theirs: None,
                },
                result: Some((ours_mode, ours_oid)),
                worktree: ours_worktree,
                conflict: Some(if ours_carries_message {
                    distinct.clone()
                } else {
                    MergeConflictKind::DistinctTypesStage
                }),
                auto_merged: false,
            });
            let theirs_worktree =
                Some((theirs_mode, merge_worktree_bytes(db, theirs_mode, &theirs_oid)?));
            paths.push(MergedPath {
                path: theirs_path,
                stages: MergeStages {
                    base: theirs_base,
                    ours: None,
                    theirs: Some((theirs_mode, theirs_oid)),
                },
                result: Some((theirs_mode, theirs_oid)),
                worktree: theirs_worktree,
                conflict: Some(if ours_carries_message {
                    MergeConflictKind::DistinctTypesStage
                } else {
                    distinct
                }),
                auto_merged: false,
            });
        } else {
            // add/add of non-files, mode changes on same-type entries, etc. Keep
            // the surviving side's content and record a generic content conflict.
            clean = false;
            let add_add = base.is_none();
            let surviving = ours.or(theirs);
            let worktree = match &surviving {
                Some((mode, oid)) => Some((*mode, merge_worktree_bytes(db, *mode, oid)?)),
                None => None,
            };
            if let Some(entry) = surviving {
                leaves.insert(path.clone(), entry);
            }
            paths.push(MergedPath {
                path: path.clone(),
                stages: stages_for(&base, &ours, &theirs),
                result: surviving,
                worktree,
                conflict: Some(MergeConflictKind::Content { add_add }),
                auto_merged: false,
            });
        }
    }

    if !renames.rename_rename_one_to_two.is_empty() {
        apply_rename_rename_one_to_two_conflicts(
            db,
            base_map,
            &eff_ours,
            &eff_theirs,
            &renames.rename_rename_one_to_two,
            &mut paths,
            &mut leaves,
            options,
        )?;
        clean = false;
    }

    if !dir_rename_two_to_one.is_empty() {
        apply_dir_rename_two_to_one_conflicts(
            db,
            &eff_ours,
            &eff_theirs,
            &dir_rename_two_to_one,
            &mut paths,
            &mut leaves,
            options,
        )?;
        clean = false;
    }

    // Rename/rename(2to1) and rename/add: two distinct contents collide on one
    // destination (and the rename source(s) are consumed). Detected from the full
    // per-side rename sets, applied here so the destination carries both sides'
    // content-merged stages instead of the path-keyed core's raw add/add.
    if !renames.rename_rename_two_to_one.is_empty() || !renames.rename_adds.is_empty() {
        apply_rename_two_to_one_and_add_conflicts(
            db,
            base_map,
            ours_map,
            theirs_map,
            &renames,
            &mut paths,
            &mut leaves,
            options,
        )?;
        clean = false;
    }

    // Rename/delete conflicts: a file renamed on one side whose source the other
    // side deleted. The merge core resolved the destination cleanly (only the
    // renaming side has it), but git flags this as a conflict — keep the renamed
    // content in the tree, record higher-order stages, and mark the merge dirty.
    if !renames.rename_deletes.is_empty() {
        for (dest, rd) in &renames.rename_deletes {
            // Skip if another conflict already claimed this destination.
            let Some(slot) = paths.iter_mut().find(|p| &p.path == dest) else {
                continue;
            };
            if slot.conflict.is_some() {
                continue;
            }
            let base_entry = base_map.get(&rd.source).copied();
            let renamed_entry = slot.result;
            // The renamed content sits on the renaming side; the deleting side
            // contributes no stage at the destination.
            let (ours_stage, theirs_stage) = match rd.side {
                RenameSide::Ours => (renamed_entry, None),
                RenameSide::Theirs => (None, renamed_entry),
            };
            let (renamed_in, deleted_in) = match rd.side {
                RenameSide::Ours => (
                    options.ours_label.to_string(),
                    options.theirs_label.to_string(),
                ),
                RenameSide::Theirs => (
                    options.theirs_label.to_string(),
                    options.ours_label.to_string(),
                ),
            };
            let worktree = match &renamed_entry {
                Some((mode, oid)) => Some((*mode, merge_worktree_bytes(db, *mode, oid)?)),
                None => None,
            };
            slot.stages = MergeStages {
                base: base_entry,
                ours: ours_stage,
                theirs: theirs_stage,
            };
            slot.worktree = worktree;
            slot.conflict = Some(MergeConflictKind::RenameDelete {
                old_path: rd.source.clone(),
                renamed_in,
                deleted_in,
            });
            clean = false;
        }
    }

    // Directory-rename outcomes that make the merge dirty. A collision/split
    // detected while re-homing (two paths onto one destination, an ambiguous
    // split source, or a file in the way) marks the merge unclean regardless of
    // mode. In =conflict mode, every silently re-homed path is *also* reported
    // as a location conflict: the tree keeps the re-homed content but git wants
    // the user to confirm the suggested move.
    if dir_rename_dirty {
        clean = false;
    }
    // Implicit-directory-rename collisions (a directory rename would put a path
    // onto an existing file/dir, or N paths onto one destination). git emits
    // `CONFLICT (implicit dir rename): Existing file/dir at <dest> in the way ...`
    // regardless of mode, and the merge is unclean. Attach the conflict to the
    // blocked destination path (which keeps its original content).
    for collision in &dir_rename_collisions {
        clean = false;
        if let Some(slot) = paths.iter_mut().find(|p| p.path == collision.dest)
            && slot.conflict.is_none()
        {
            slot.conflict = Some(MergeConflictKind::DirRenameImplicitCollision {
                sources: collision.sources.clone(),
            });
        } else if !paths.iter().any(|p| p.path == collision.dest) {
            paths.push(MergedPath {
                path: collision.dest.clone(),
                stages: MergeStages::default(),
                result: None,
                worktree: None,
                conflict: Some(MergeConflictKind::DirRenameImplicitCollision {
                    sources: collision.sources.clone(),
                }),
                auto_merged: false,
            });
        }
    }
    for source_dir in &dir_rename_splits {
        clean = false;
        paths.push(MergedPath {
            path: source_dir.clone(),
            stages: MergeStages::default(),
            result: None,
            worktree: None,
            conflict: Some(MergeConflictKind::DirRenameSplit {
                source_dir: source_dir.clone(),
            }),
            auto_merged: false,
        });
    }
    if !dir_rename_conflict_paths.is_empty() {
        clean = false;
        for (dest, infos) in &dir_rename_conflict_paths {
            for info in [&infos.ours, &infos.theirs].into_iter().flatten() {
                let (added_in, dir_renamed_in) = if info.added_on_ours {
                    // The path was added/renamed by ours, into a dir theirs renamed.
                    (
                        options.ours_label.to_string(),
                        options.theirs_label.to_string(),
                    )
                } else {
                    (
                        options.theirs_label.to_string(),
                        options.ours_label.to_string(),
                    )
                };
                // Rename-to-self via a directory rename (merge-ort 12i2): the
                // re-home landed the file back on its own base source path where
                // the other side modified it. git records this UNMERGED (UU) even
                // though the trivial 3-way at the destination resolves cleanly
                // (the renamed side's content equals base). Stage the three
                // versions so the index carries the conflict.
                let back_to_self = dir_rename_back_to_self.contains(dest);
                if let Some(slot) = paths.iter_mut().find(|p| &p.path == dest)
                    && slot.conflict.is_none()
                {
                    if back_to_self {
                        slot.stages = MergeStages {
                            base: eff_base.get(dest).copied(),
                            ours: eff_ours.get(dest).copied(),
                            theirs: eff_theirs.get(dest).copied(),
                        };
                        slot.worktree = match &slot.result {
                            Some((mode, oid)) => {
                                Some((*mode, merge_worktree_bytes(db, *mode, oid)?))
                            }
                            None => slot.worktree.clone(),
                        };
                    }
                    slot.conflict = Some(MergeConflictKind::DirRenameLocation {
                        old_path: info.old_path.clone(),
                        renamed_from: info.renamed_from.clone(),
                        added_in,
                        dir_renamed_in,
                        back_to_self,
                    });
                } else {
                    info_messages.push(MergeInfoMessage::DirRenameLocationConflict {
                        old_path: info.old_path.clone(),
                        new_path: dest.clone(),
                        renamed_from: info.renamed_from.clone(),
                        added_in,
                        dir_renamed_in,
                    });
                }
            }
        }
    }

    // Directory/file (D/F) conflict resolution (merge-ort `process_entry`): a
    // path that ends up as a *file* in the merged result while another result
    // path lives *under* it (so the path is simultaneously a directory) cannot
    // coexist. git keeps the directory at the original path and moves the file
    // aside to `path~<branch>` via `unique_path`, where `<branch>` is the side
    // that contributed the file. We resolve this on the flattened `leaves` after
    // every per-path decision is made, so renames/dir-renames have settled first.
    resolve_directory_file_conflicts(
        db,
        &mut paths,
        &mut leaves,
        &mut clean,
        &eff_ours,
        &eff_theirs,
        options,
        &mut info_messages,
    )?;

    let tree = write_merged_tree(db, &leaves)?;

    cleanup_paths.retain(|path| !leaves.contains_key(path));

    Ok(MergeTreesResult {
        tree,
        paths,
        clean,
        cleanup_paths: cleanup_paths.into_iter().collect(),
        info_messages,
    })
}

/// Flatten a branch label the way git's `add_flattened_path` does for
/// `unique_path`: any `/` in the branch name becomes `_` so the synthesized
/// `path~branch` stays a single path component family.
fn flatten_branch_label(branch: &str) -> String {
    branch.replace('/', "_")
}

/// Pick a `path~<branch>` name not already present in `leaves` (or claimed by an
/// existing `paths` entry), mirroring merge-ort's `unique_path`: start from
/// `path~branch`, then append `_0`, `_1`, … on collision.
fn unique_df_path(
    path: &[u8],
    branch: &str,
    leaves: &MergeEntryMap,
    paths: &[MergedPath],
) -> Vec<u8> {
    let mut base = path.to_vec();
    base.push(b'~');
    base.extend_from_slice(flatten_branch_label(branch).as_bytes());
    let taken = |candidate: &[u8]| {
        leaves.contains_key(candidate) || paths.iter().any(|p| p.path == candidate)
    };
    if !taken(&base) {
        return base;
    }
    let mut suffix = 0usize;
    loop {
        let mut candidate = base.clone();
        candidate.push(b'_');
        candidate.extend_from_slice(suffix.to_string().as_bytes());
        if !taken(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Resolve directory/file collisions in the merged leaf set. For every file leaf
/// whose path is also a directory (some other leaf lives under `path/`), move the
/// file to `path~<branch>` and record a [`MergeConflictKind::FileDirectory`].
#[allow(clippy::too_many_arguments)]
fn resolve_directory_file_conflicts(
    db: &FileObjectDatabase,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
    clean: &mut bool,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    options: &MergeTreesOptions<'_>,
    info_messages: &mut Vec<MergeInfoMessage>,
) -> Result<()> {
    // A path is a "directory" in the result iff some leaf key has it as a strict
    // `path/` prefix. Collect every such directory prefix once.
    let mut directory_prefixes: BTreeSet<Vec<u8>> = BTreeSet::new();
    for key in leaves.keys() {
        let mut idx = 0;
        while let Some(pos) = key[idx..].iter().position(|b| *b == b'/') {
            let end = idx + pos;
            directory_prefixes.insert(key[..end].to_vec());
            idx = end + 1;
        }
    }
    if directory_prefixes.is_empty() {
        return Ok(());
    }

    // File leaves that collide with a directory of the same name.
    let colliding: Vec<Vec<u8>> = leaves
        .keys()
        .filter(|key| directory_prefixes.contains(*key))
        .cloned()
        .collect();

    for original in colliding {
        let Some(entry) = leaves.remove(&original) else {
            continue;
        };
        // The moved-aside file must be materialized in the worktree at its new
        // path; read its blob bytes once so the porcelain has worktree content.
        let moved_bytes = merge_worktree_bytes(db, entry.0, &entry.1)?;
        // Which side contributed the file? git keys off `dirmask`: the file lives
        // on the side that is NOT the directory. We read it off the effective side
        // maps — whichever side has this path as a plain file. When only theirs has
        // it, use the theirs label; otherwise (ours has it, or both do) ours wins,
        // matching git's index-1 bias for the moved-aside name.
        let ours_has_file = eff_ours.contains_key(&original);
        let theirs_has_file = eff_theirs.contains_key(&original);
        let from_ours = ours_has_file || !theirs_has_file;
        let branch = if from_ours {
            options.ours_label
        } else {
            options.theirs_label
        };
        let new_path = unique_df_path(&original, branch, leaves, paths);
        leaves.insert(new_path.clone(), entry);
        *clean = false;

        // Relocate the path's MergedPath: update its destination and stamp the D/F
        // conflict. If the path had no MergedPath (defensive), synthesize one.
        if let Some(slot) = paths.iter_mut().find(|p| p.path == original) {
            if let Some(MergeConflictKind::RenameDelete {
                old_path,
                renamed_in,
                deleted_in,
            }) = &slot.conflict
            {
                info_messages.push(MergeInfoMessage::RenameDeleteConflict {
                    old_path: old_path.clone(),
                    new_path: original.clone(),
                    renamed_in: renamed_in.clone(),
                    deleted_in: deleted_in.clone(),
                });
            }
            slot.path = new_path.clone();
            slot.result = Some(entry);
            // Preserve any pre-existing higher-order stages; a clean file leaf has
            // none, so seed ours/theirs from the effective maps for `ls-files -u`.
            if slot.stages.base.is_none()
                && slot.stages.ours.is_none()
                && slot.stages.theirs.is_none()
            {
                slot.stages = MergeStages {
                    base: None,
                    ours: if from_ours { Some(entry) } else { None },
                    theirs: if from_ours { None } else { Some(entry) },
                };
            }
            // Keep the slot's existing `auto_merged`: git only emits
            // `Auto-merging <new_path>` for the moved file when a real content
            // merge ran (a rename or both-sides change drives filemask>=6 through
            // handle_content_merge). A plain one-sided add (filemask 2/4) is moved
            // aside silently, so we must NOT force the flag on here.
            slot.worktree = Some((entry.0, moved_bytes));
            slot.conflict = Some(MergeConflictKind::FileDirectory {
                original_path: original.clone(),
                moved_from: branch.to_string(),
            });
        } else {
            paths.push(MergedPath {
                path: new_path.clone(),
                stages: MergeStages {
                    base: None,
                    ours: if from_ours { Some(entry) } else { None },
                    theirs: if from_ours { None } else { Some(entry) },
                },
                result: Some(entry),
                worktree: Some((entry.0, moved_bytes)),
                conflict: Some(MergeConflictKind::FileDirectory {
                    original_path: original.clone(),
                    moved_from: branch.to_string(),
                }),
                auto_merged: false,
            });
        }
    }

    // Keep `paths` sorted by destination path (callers and tests assume order).
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// Construct a clean (non-conflicted) [`MergedPath`].
fn clean_path(path: Vec<u8>, result: Option<(u32, ObjectId)>) -> MergedPath {
    clean_path_auto(path, result, false)
}

/// Like [`clean_path`] but records whether the path went through a textual
/// 3-way content merge (for the "Auto-merging" message).
fn clean_path_auto(
    path: Vec<u8>,
    result: Option<(u32, ObjectId)>,
    auto_merged: bool,
) -> MergedPath {
    MergedPath {
        path,
        stages: MergeStages::default(),
        result,
        worktree: None,
        conflict: None,
        auto_merged,
    }
}

/// Snapshot the present stages for a conflicted path.
fn stages_for(
    base: &Option<(u32, ObjectId)>,
    ours: &Option<(u32, ObjectId)>,
    theirs: &Option<(u32, ObjectId)>,
) -> MergeStages {
    MergeStages {
        base: *base,
        ours: *ours,
        theirs: *theirs,
    }
}

/// Read a blob's raw bytes, requiring it to be a blob object.
fn merge_blob_bytes(reader: &impl ObjectReader, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            oid,
            object.object_type.as_str()
        )));
    }
    Ok(object.body.clone())
}

fn merge_worktree_bytes(reader: &impl ObjectReader, mode: u32, oid: &ObjectId) -> Result<Vec<u8>> {
    if sley_index::is_gitlink(mode) {
        Ok(Vec::new())
    } else {
        merge_blob_bytes(reader, oid)
    }
}

/// 3-way merge of a file mode. Returns the resolved mode and whether the modes
/// conflict (both sides changed it to different non-base values).
fn merge_file_modes(base: Option<u32>, ours: u32, theirs: u32) -> (u32, bool) {
    if ours == theirs {
        return (ours, false);
    }
    match base {
        Some(base) if ours == base => (theirs, false),
        Some(base) if theirs == base => (ours, false),
        _ => (ours, true),
    }
}

/// Build a top-level tree object from a flat map of `path -> (mode, oid)`
/// leaves, writing every (sub)tree object to `db`.
fn write_merged_tree(db: &FileObjectDatabase, leaves: &MergeEntryMap) -> Result<ObjectId> {
    let mut root = MergeTreeNode::default();
    for (path, (mode, oid)) in leaves {
        root.insert(path, *mode, *oid);
    }
    root.write(db)
}

#[derive(Default)]
struct MergeTreeNode {
    blobs: BTreeMap<Vec<u8>, (u32, ObjectId)>,
    subtrees: BTreeMap<Vec<u8>, MergeTreeNode>,
}

impl MergeTreeNode {
    fn insert(&mut self, path: &[u8], mode: u32, oid: ObjectId) {
        match path.iter().position(|byte| *byte == b'/') {
            Some(slash) => {
                let component = path[..slash].to_vec();
                let rest = &path[slash + 1..];
                self.subtrees
                    .entry(component)
                    .or_default()
                    .insert(rest, mode, oid);
            }
            None => {
                self.blobs.insert(path.to_vec(), (mode, oid));
            }
        }
    }

    fn write(&self, db: &FileObjectDatabase) -> Result<ObjectId> {
        let mut entries: Vec<TreeEntry> = Vec::new();
        for (name, (mode, oid)) in &self.blobs {
            entries.push(TreeEntry {
                mode: *mode,
                name: BString::from(name.clone()),
                oid: *oid,
            });
        }
        for (name, subtree) in &self.subtrees {
            let oid = subtree.write(db)?;
            entries.push(TreeEntry {
                mode: 0o040000,
                name: BString::from(name.clone()),
                oid,
            });
        }
        entries.sort_by_key(merge_tree_sort_key);
        let tree = Tree { entries };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
    }
}

fn merge_tree_sort_key(entry: &TreeEntry) -> Vec<u8> {
    let mut key = entry.name.as_bytes().to_vec();
    if entry.mode == 0o040000 {
        key.push(b'/');
    }
    key
}

// --- Rename-aware non-recursive merge -------------------------------------

/// Which side of the merge performed a rename.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameSide {
    Ours,
    Theirs,
}

/// One detected one-sided rename: its source path and which side renamed it.
#[derive(Clone)]
struct MergeRename {
    source: Vec<u8>,
    side: RenameSide,
}

/// A file renamed on one side whose source was *deleted* on the other side — a
/// rename/delete conflict. git keeps the renamed content at the destination but
/// flags the merge as conflicted.
#[derive(Clone)]
struct RenameDelete {
    /// The pre-rename source path (deleted on the other side).
    source: Vec<u8>,
    /// Which side performed the rename (the other side deleted the source).
    side: RenameSide,
}

/// The rename pairings discovered for one merge: which destination paths came
/// from which source path, and which side renamed (so the other side's change
/// can follow the rename and conflict labels can be path-qualified like git).
#[derive(Default)]
struct MergeRenames {
    /// One-sided renames keyed by *destination* path. Only renames where the
    /// OTHER side kept/modified the source in place are recorded (the case
    /// where the modification must follow the rename).
    dest_to_source: BTreeMap<Vec<u8>, MergeRename>,
    /// Rename/delete conflicts: a file renamed on one side whose source the
    /// other side deleted. Keyed by destination path.
    rename_deletes: BTreeMap<Vec<u8>, RenameDelete>,
    /// Rename/rename(1to2) conflicts keyed by source path.
    rename_rename_one_to_two: BTreeMap<Vec<u8>, RenameRenameOneToTwo>,
    /// Rename/rename(2to1) conflicts keyed by the shared *destination* path:
    /// ours renamed `ours_source`->dest and theirs renamed `theirs_source`->dest.
    rename_rename_two_to_one: BTreeMap<Vec<u8>, RenameRenameTwoToOne>,
    /// Rename/add conflicts keyed by *destination*: one side renamed a file to
    /// `dest` while the other side added a different file at the same `dest`.
    rename_adds: BTreeMap<Vec<u8>, RenameAdd>,
}

#[derive(Clone)]
struct RenameRenameOneToTwo {
    ours_dest: Vec<u8>,
    theirs_dest: Vec<u8>,
}

/// A rename/rename(2to1): two distinct sources renamed onto one destination, one
/// rename per side. Each side's content at the destination is the 3-way merge of
/// its rename (the other side's change to that source follows the rename).
#[derive(Clone)]
struct RenameRenameTwoToOne {
    /// The source ours renamed onto the destination.
    ours_source: Vec<u8>,
    /// The source theirs renamed onto the destination.
    theirs_source: Vec<u8>,
}

/// A rename/add: one side renamed a file onto `dest`, the other side added an
/// unrelated file at `dest`. The renaming side's content is the 3-way merge of
/// its rename; the adding side contributes its added blob verbatim.
#[derive(Clone)]
struct RenameAdd {
    /// The pre-rename source path on the renaming side.
    source: Vec<u8>,
    /// Which side performed the rename (the other side added at `dest`).
    side: RenameSide,
}

/// Every file rename observed on one side (base->side), as `(old, new)` pairs.
/// Unlike [`MergeRenames`] this is the *complete* rename set on a side — it is
/// the input to directory-rename inference, which needs to see all the per-file
/// moves between directories, not just the ones the other side kept in place.
struct SideRenames {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Detect one-sided renames usable for a non-recursive merge: a path present in
/// `base`, deleted on one side and present (renamed) at a new path on that same
/// side, while the OTHER side still has the original path (modified or
/// unchanged). Such a rename lets the other side's change move to the
/// destination.
///
/// Also returns the complete per-side rename set so the caller can infer
/// directory renames (which need every file move, not just the merge-relevant
/// ones).
fn detect_merge_renames(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<(MergeRenames, SideRenames, SideRenames)> {
    let mut renames = MergeRenames::default();

    // Renames on ours: the other side that must carry its change is theirs.
    let ours_side = collect_side_renames(
        db,
        format,
        base_map,
        ours_map,
        theirs_map,
        RenameSide::Ours,
        options.rename_threshold,
        options.rename_limit,
        &mut renames,
    )?;
    // Renames on theirs: the other side that carries its change is ours.
    let theirs_side = collect_side_renames(
        db,
        format,
        base_map,
        theirs_map,
        ours_map,
        RenameSide::Theirs,
        options.rename_threshold,
        options.rename_limit,
        &mut renames,
    )?;

    collect_rename_rename_one_to_two(&mut renames, &ours_side, &theirs_side);
    collect_rename_rename_two_to_one_and_adds(
        &mut renames,
        &ours_side,
        &theirs_side,
        base_map,
        ours_map,
        theirs_map,
    );

    Ok((renames, ours_side, theirs_side))
}

/// Detect rename/rename(2to1) and rename/add conflicts from the complete per-side
/// rename sets. Both arise when a one-sided rename's destination is *occupied* on
/// the other side (so [`collect_side_renames`] left it out of `dest_to_source`):
///
/// * 2to1 — both sides renamed (distinct sources) onto the same destination.
/// * rename/add — one side renamed onto a path the other side *added* fresh
///   (the destination is new to the other side, not a base path it kept and not
///   itself a rename destination on that side).
fn collect_rename_rename_two_to_one_and_adds(
    renames: &mut MergeRenames,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
) {
    let ours_by_dest: BTreeMap<&[u8], &[u8]> = ours_side
        .pairs
        .iter()
        .map(|(old, new)| (new.as_slice(), old.as_slice()))
        .collect();
    let theirs_by_dest: BTreeMap<&[u8], &[u8]> = theirs_side
        .pairs
        .iter()
        .map(|(old, new)| (new.as_slice(), old.as_slice()))
        .collect();

    // 2to1: a destination that is a rename target on BOTH sides from different
    // sources. (Same source on both sides is a rename/rename(1to1), handled by
    // the path-keyed core; same source to two dests is the 1to2 case above.)
    for (dest, ours_src) in &ours_by_dest {
        let Some(theirs_src) = theirs_by_dest.get(dest) else {
            continue;
        };
        if ours_src == theirs_src {
            continue;
        }
        // Don't disturb a destination the 1to2 pass already claimed.
        if renames.rename_rename_one_to_two.contains_key(*dest) {
            continue;
        }
        renames.rename_rename_two_to_one.insert(
            dest.to_vec(),
            RenameRenameTwoToOne {
                ours_source: ours_src.to_vec(),
                theirs_source: theirs_src.to_vec(),
            },
        );
    }

    // rename/add on ours: ours renamed onto `dest`, which theirs added (present
    // on theirs, absent from base, and not a theirs rename target).
    for (dest, ours_src) in &ours_by_dest {
        if renames.rename_rename_two_to_one.contains_key(*dest)
            || renames.rename_rename_one_to_two.contains_key(*dest)
        {
            continue;
        }
        if theirs_map.contains_key(*dest)
            && !base_map.contains_key(*dest)
            && !theirs_by_dest.contains_key(dest)
        {
            renames.rename_adds.insert(
                dest.to_vec(),
                RenameAdd {
                    source: ours_src.to_vec(),
                    side: RenameSide::Ours,
                },
            );
        }
    }
    // rename/add on theirs: symmetric.
    for (dest, theirs_src) in &theirs_by_dest {
        if renames.rename_rename_two_to_one.contains_key(*dest)
            || renames.rename_rename_one_to_two.contains_key(*dest)
            || renames.rename_adds.contains_key(*dest)
        {
            continue;
        }
        if ours_map.contains_key(*dest)
            && !base_map.contains_key(*dest)
            && !ours_by_dest.contains_key(dest)
        {
            renames.rename_adds.insert(
                dest.to_vec(),
                RenameAdd {
                    source: theirs_src.to_vec(),
                    side: RenameSide::Theirs,
                },
            );
        }
    }
}

fn collect_rename_rename_one_to_two(
    renames: &mut MergeRenames,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
) {
    let ours_by_source: BTreeMap<&[u8], &[u8]> = ours_side
        .pairs
        .iter()
        .map(|(old, new)| (old.as_slice(), new.as_slice()))
        .collect();
    for (old, theirs_new) in &theirs_side.pairs {
        let Some(ours_new) = ours_by_source.get(old.as_slice()) else {
            continue;
        };
        if *ours_new == theirs_new.as_slice() {
            continue;
        }
        renames.rename_deletes.remove(*ours_new);
        renames.rename_deletes.remove(theirs_new);
        renames.dest_to_source.remove(*ours_new);
        renames.dest_to_source.remove(theirs_new);
        renames.rename_rename_one_to_two.insert(
            old.clone(),
            RenameRenameOneToTwo {
                ours_dest: (*ours_new).to_vec(),
                theirs_dest: theirs_new.clone(),
            },
        );
    }
}

/// Collect renames that occurred on `side` (relative to `base`). Records the
/// merge-relevant subset (renames the `other` side still references) into
/// `renames`, and returns the *complete* per-side rename set for directory-rename
/// inference. `db`/`format` resolve blob bytes for similarity scoring.
#[allow(clippy::too_many_arguments)]
fn collect_side_renames(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base_map: &MergeEntryMap,
    side_map: &MergeEntryMap,
    other_map: &MergeEntryMap,
    side: RenameSide,
    threshold: u8,
    rename_limit: usize,
    renames: &mut MergeRenames,
) -> Result<SideRenames> {
    // Diff base->side with inexact rename detection; the resulting `Renamed`
    // entries name (old_path -> new_path) pairs on this side.
    let base_tree = entry_map_as_tracked(base_map);
    let side_tree = entry_map_as_tracked(side_map);
    let options = RenameDetectionOptions {
        base: DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: false,
        },
        detect_inexact: true,
        rename_threshold: threshold,
        copy_threshold: threshold,
        rename_limit,
    };
    let changes = diff_name_status_maps_with_renames(
        &base_tree,
        &side_tree,
        base_tree.keys().chain(side_tree.keys()),
        options,
        |oid| merge_blob_bytes(db, oid).ok(),
    )?;

    let mut pairs = Vec::new();
    for change in changes {
        let NameStatus::Renamed(_) = change.status else {
            continue;
        };
        let Some(old_path) = change.old_path.as_ref() else {
            continue;
        };
        let old = old_path.as_bytes().to_vec();
        let new = change.path.as_bytes().to_vec();
        // Complete rename set, fed to directory-rename inference.
        pairs.push((old.clone(), new.clone()));

        // Only act when the destination is genuinely new (not already present
        // in either side from a different origin) and the OTHER side still
        // references the source path — i.e. the other side modified/kept `old`,
        // and its change should follow the rename to `new`.
        if !other_map.contains_key(&old) {
            // The source path is gone on the other side. If it existed in base
            // (so the other side *deleted* it) and the other side did not also
            // produce `new`, this is a rename/delete conflict: this side renamed
            // the file, the other side deleted its source.
            if base_map.contains_key(&old) && !other_map.contains_key(&new) {
                renames
                    .rename_deletes
                    .entry(new.clone())
                    .or_insert(RenameDelete {
                        source: old.clone(),
                        side,
                    });
            }
            continue;
        }
        // If the other side ALSO renamed/created `new`, that is a rename/rename
        // or rename/add corner case we leave to the path-keyed core (stage-b).
        if other_map.contains_key(&new) {
            continue;
        }
        // Skip if both sides renamed the same source to the same dest (already
        // recorded) or to anything (first writer wins; the path-keyed core then
        // sees identical dest entries and resolves trivially).
        renames
            .dest_to_source
            .entry(new)
            .or_insert(MergeRename { source: old, side });
    }

    let _ = format;
    Ok(SideRenames { pairs })
}

/// Rewrite the three side maps so that each detected one-sided rename old->new
/// presents the OTHER side's `old` entry at `new`, and removes `old` from
/// every side. The path-keyed merge core then performs the 3-way content merge
/// at `new` with base=base[old], one side = the renaming side's new content,
/// the other side = the modifying side's old content.
fn apply_merge_renames(
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    renames: &MergeRenames,
) -> (MergeEntryMap, MergeEntryMap, MergeEntryMap) {
    if renames.dest_to_source.is_empty() {
        return (base_map.clone(), ours_map.clone(), theirs_map.clone());
    }
    let mut base = base_map.clone();
    let mut ours = ours_map.clone();
    let mut theirs = theirs_map.clone();

    for (new, rename) in &renames.dest_to_source {
        let old = &rename.source;
        // Move base[old] to base[new] so the destination has a proper ancestor.
        if let Some(entry) = base.remove(old) {
            base.entry(new.clone()).or_insert(entry);
        }
        // For each side, if it still has `old`, move that entry to `new`.
        for side in [&mut ours, &mut theirs] {
            if let Some(entry) = side.remove(old) {
                side.entry(new.clone()).or_insert(entry);
            }
        }
    }
    (base, ours, theirs)
}

// --- Directory-rename detection -------------------------------------------

/// The parent directory of `path`, or `None` for a top-level path.
fn parent_dir(path: &[u8]) -> Option<&[u8]> {
    path.iter().rposition(|b| *b == b'/').map(|i| &path[..i])
}

/// Apply a directory rename `old_dir -> new_dir` to `path` (which must live
/// under `old_dir`). E.g. `old_dir=z`, `new_dir=y`, `path=z/d` -> `y/d`; an
/// empty `new_dir` (rename into the repo root) drops the directory prefix.
fn apply_dir_rename(old_dir: &[u8], new_dir: &[u8], path: &[u8]) -> Vec<u8> {
    // The portion of `path` after `old_dir/` (handle root-target by stepping
    // past the separator, exactly as git's apply_dir_rename does).
    let rest_start = if new_dir.is_empty() {
        old_dir.len() + 1
    } else {
        old_dir.len()
    };
    let mut out = new_dir.to_vec();
    out.extend_from_slice(&path[rest_start..]);
    out
}

/// Find the longest renamed ancestor directory of `path`: walk parent dirs from
/// the deepest up and return the first one present in `dir_renames`. Mirrors
/// merge-ort's `check_dir_renamed`.
fn check_dir_renamed<'a>(
    path: &[u8],
    dir_renames: &'a BTreeMap<Vec<u8>, Vec<u8>>,
) -> Option<(&'a [u8], &'a [u8])> {
    let mut cur = parent_dir(path);
    while let Some(dir) = cur {
        if let Some((old_dir, new_dir)) = dir_renames.get_key_value(dir) {
            return Some((old_dir.as_slice(), new_dir.as_slice()));
        }
        cur = parent_dir(dir);
    }
    None
}

/// The provisional directory renames computed for both sides, plus the source
/// directories whose rename was ambiguous (a "split").
struct DirectoryRenameMaps {
    /// `old_dir -> new_dir` directory renames detected on ours' side. A path
    /// added/renamed by theirs under `old_dir` re-homes into `new_dir`.
    ours: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Directory renames detected on theirs' side.
    theirs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Source directories whose split was unclear on ours' side (no unique
    /// majority target); paths on theirs that would need to follow such a rename
    /// are a conflict, not silent.
    ours_split: BTreeSet<Vec<u8>>,
    /// Source directories whose split was unclear on theirs' side.
    theirs_split: BTreeSet<Vec<u8>>,
}

/// Infer directory renames from the complete per-side file-rename sets, mirroring
/// merge-ort's `get_provisional_directory_renames` + `handle_directory_level_conflicts`.
/// For every file moved `.../old_dir/x -> .../new_dir/x`, the ancestor pairs are
/// tallied (`dir_rename_count`) and collapsed to `old_dir -> best_new_dir` where
/// `best` is the unique highest count. A tie marks the source directory as a
/// "split". A rename is only kept if the source directory was *entirely removed*
/// on that side (the `dirs_removed` gate). A directory renamed on BOTH sides is
/// dropped from both maps (ambiguous).
fn compute_directory_renames(
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
) -> DirectoryRenameMaps {
    let ours = compute_side_dir_renames(&ours_side.pairs, ours_map);
    let theirs = compute_side_dir_renames(&theirs_side.pairs, theirs_map);

    // A directory renamed on BOTH sides (to whatever target) is ambiguous;
    // git's handle_directory_level_conflicts drops it from both maps so neither
    // side's directory rename is applied.
    let mut ours_map_out = ours.renames;
    let mut theirs_map_out = theirs.renames;
    let dup: Vec<Vec<u8>> = ours_map_out
        .keys()
        .filter(|k| theirs_map_out.contains_key(*k))
        .cloned()
        .collect();
    for k in dup {
        ours_map_out.remove(&k);
        theirs_map_out.remove(&k);
    }

    DirectoryRenameMaps {
        ours: ours_map_out,
        theirs: theirs_map_out,
        ours_split: ours.split,
        theirs_split: theirs.split,
    }
}

/// Per-side directory-rename computation result.
struct SideDirRenames {
    renames: BTreeMap<Vec<u8>, Vec<u8>>,
    split: BTreeSet<Vec<u8>>,
}

/// Compute one side's `old_dir -> new_dir` map from its file renames, gated on
/// the source directory being fully removed on that side.
fn compute_side_dir_renames(
    pairs: &[(Vec<u8>, Vec<u8>)],
    side_map: &MergeEntryMap,
) -> SideDirRenames {
    // dir_rename_count: count[old_dir][new_dir]. Built by walking every rename's
    // ancestor directories while the *trailing* path components match, exactly
    // as merge-ort's update_dir_rename_counts does. For
    //   a/b/c/d/e/foo.c -> a/b/some/thing/else/e/foo.c
    // this records both
    //   a/b/c/d/e => a/b/some/thing/else/e   AND   a/b/c/d => a/b/some/thing/else
    // but stops once the trailing components diverge.
    let mut counts: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, usize>> = BTreeMap::new();
    for (old, new) in pairs {
        update_dir_rename_counts(&mut counts, old, new);
    }

    let mut renames = BTreeMap::new();
    let mut split = BTreeSet::new();
    for (old_dir, targets) in counts {
        let mut max = 0usize;
        let mut bad_max = 0usize;
        let mut best: Option<Vec<u8>> = None;
        for (target, count) in &targets {
            if *count == max {
                bad_max = max;
            } else if *count > max {
                max = *count;
                best = Some(target.clone());
            }
        }
        if max == 0 {
            continue;
        }
        if bad_max == max {
            split.insert(old_dir);
            continue;
        }
        // dirs_removed gate: the source directory must be entirely gone on this
        // side. New files that recreate the old directory count too; otherwise
        // cases like "both sides renamed z/ -> y/, but one side added z/d"
        // incorrectly look like both sides performed a whole-directory rename.
        if let Some(best) = best
            && directory_fully_removed(&old_dir, side_map)
        {
            renames.insert(old_dir, best);
        }
    }

    SideDirRenames { renames, split }
}

/// Tally the ancestor directory-rename pairs implied by a single file rename
/// `old -> new`, mirroring merge-ort's `update_dir_rename_counts`. Starting from
/// the immediate parent dirs, we strip one trailing component at a time and
/// record `old_ancestor -> new_ancestor` as long as the *remaining* trailing
/// suffix still matches between the two paths.
fn update_dir_rename_counts(
    counts: &mut BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, usize>>,
    old: &[u8],
    new: &[u8],
) {
    // Work on owned copies we progressively truncate at each '/'.
    let mut old_dir = old.to_vec();
    let mut new_dir = new.to_vec();
    let mut first = true;
    loop {
        // Strip the trailing component (basename on the first pass, then a dir
        // each pass) to ascend one level.
        let old_has = dir_munge(&mut old_dir);
        let new_has = dir_munge(&mut new_dir);

        // On the first pass we only stripped the basename; the dirs need not
        // match. On later passes the *trailing* components must agree, otherwise
        // the rename no longer implies this ancestor pairing.
        if !first {
            let old_sub = trailing_component(old, &old_dir);
            let new_sub = trailing_component(new, &new_dir);
            if old_sub != new_sub {
                break;
            }
        }

        if old_dir == new_dir {
            // Same directory at this level — no rename implied, and no deeper
            // ancestor can differ usefully either.
            break;
        }
        *counts
            .entry(old_dir.clone())
            .or_default()
            .entry(new_dir.clone())
            .or_default() += 1;

        first = false;
        // Hitting the toplevel ("") on either side ends the ascent.
        if old_dir.is_empty() || new_dir.is_empty() {
            break;
        }
        // If the two ancestors are identical from here up, stop (git stops once
        // the suffix-equal walk reaches a common prefix).
        if !old_has || !new_has {
            break;
        }
    }
}

/// Truncate `buf` at its last '/', leaving the parent directory (or empty for a
/// toplevel name). Returns whether a '/' was present (i.e. there is a deeper
/// ancestor to ascend into).
fn dir_munge(buf: &mut Vec<u8>) -> bool {
    match buf.iter().rposition(|b| *b == b'/') {
        Some(i) => {
            buf.truncate(i);
            true
        }
        None => {
            buf.clear();
            false
        }
    }
}

/// The trailing path component that was stripped from `full` to reach `dir`
/// (i.e. the suffix of `full` after `dir/`). Used to compare whether the two
/// sides of a rename share the same trailing directory chain.
fn trailing_component<'a>(full: &'a [u8], dir: &[u8]) -> &'a [u8] {
    if dir.is_empty() {
        full
    } else {
        // full = dir + "/" + suffix
        &full[dir.len() + 1..]
    }
}

/// True when no path under `dir/` exists on `side` (the directory was entirely
/// removed there). Mirrors merge-ort's `dirs_removed` precondition.
fn directory_fully_removed(dir: &[u8], side_map: &MergeEntryMap) -> bool {
    let mut prefix = dir.to_vec();
    prefix.push(b'/');
    for path in side_map.keys() {
        if path.starts_with(&prefix) {
            return false;
        }
    }
    true
}

/// A path on one side whose location is rewritten by a directory rename the
/// *other* side performed. The rewrite applies equally to a freshly added file
/// and to a file the side itself renamed (a transitive rename).
struct DirRenameMove {
    /// The path as it currently sits in the side's effective map (the side's own
    /// rename, if any, already applied).
    from: Vec<u8>,
    /// The re-homed destination, after applying the other side's directory rename.
    to: Vec<u8>,
    /// `Some(source)` when `from` is a rename destination produced by this side
    /// (transitive rename); `None` for a fresh add. Drives git's
    /// "renamed to"/"added in" message wording.
    renamed_from: Option<Vec<u8>>,
}

struct DirRenameTwoToOne {
    dest: Vec<u8>,
    ours_source: Vec<u8>,
    theirs_source: Vec<u8>,
    ours_label_path: Vec<u8>,
    theirs_label_path: Vec<u8>,
}

/// Provenance of a re-homed path, for `=conflict`-mode `CONFLICT (file location)`
/// reporting.
#[derive(Clone)]
struct RehomeInfo {
    /// The pre-re-home path on the adding/renaming side.
    old_path: Vec<u8>,
    /// `Some(source)` for a transitive rename, `None` for a fresh add.
    renamed_from: Option<Vec<u8>>,
    /// Whether the *adding/renaming* side was ours (true) or theirs (false). The
    /// caller resolves this to a branch label.
    added_on_ours: bool,
}

/// Per-side provenance for a destination created by directory-rename rehoming.
#[derive(Clone, Default)]
struct RehomeSides {
    ours: Option<RehomeInfo>,
    theirs: Option<RehomeInfo>,
}

/// An implicit-directory-rename collision: one or more paths a directory rename
/// would re-home onto `dest`, which is blocked because `dest` is already
/// occupied (a file in the way) or because multiple sources map to it. git emits
/// `CONFLICT (implicit dir rename): Existing file/dir at <dest> in the way ...`.
struct DirRenameCollision {
    /// The blocked destination path (the file/dir already there).
    dest: Vec<u8>,
    /// The source path(s) the directory rename tried to move onto `dest`.
    sources: Vec<Vec<u8>>,
}

/// Outcome of applying directory renames to all three effective maps.
struct DirRenameOutcome {
    /// Rewritten base/ours/theirs maps with re-homed paths moved to their
    /// destinations. `base` moves too so a re-homed content-merge keeps its
    /// ancestor at the new location.
    base: MergeEntryMap,
    ours: MergeEntryMap,
    theirs: MergeEntryMap,
    /// Re-homed destination path -> provenance (for `=conflict`-mode reporting).
    rehomed: BTreeMap<Vec<u8>, RehomeSides>,
    /// Implicit-dir-rename collisions (file in the way / N-to-1), for the
    /// `CONFLICT (implicit dir rename)` message; always conflicts regardless of
    /// mode.
    collisions: Vec<DirRenameCollision>,
    /// Split source dirs that were relevant to a path on the other side.
    splits: BTreeSet<Vec<u8>>,
    /// Destinations where a directory rename moved a file back onto its own base
    /// source path (rename-to-self) and the other side modified that path. git
    /// records these as an unmerged file-location conflict (`UU`) rather than a
    /// clean auto-resolution; the trivial 3-way at the destination would
    /// otherwise resolve cleanly because the renamed side's content equals base.
    back_to_self: BTreeSet<Vec<u8>>,
    /// True if a directory-level collision or split made the merge dirty even in
    /// `=true` mode (e.g. two paths re-homed onto one destination).
    dirty: bool,
    info_messages: Vec<MergeInfoMessage>,
}

/// Apply directory renames to both sides' effective maps.
///
/// This mirrors merge-ort's `collect_renames` + `check_for_directory_rename` +
/// `apply_directory_rename_modifications`: every path a side *added* or *renamed*
/// that lives under a directory the OTHER side renamed has its destination
/// rewritten to follow that rename — making the directory rename a property of
/// the rename-detection pass that every path consults, not a per-file special
/// case. Handles:
///   - transitive renames (a file the side renamed into a dir the other side
///     renamed follows on into the final directory),
///   - `dir_rename_exclusions` (never re-home into a directory THIS side itself
///     renamed — that would create a spurious rename/rename(1to2)),
///   - collisions (N paths mapping to one destination -> conflict),
///   - splits (a source dir with no majority target -> conflict, leave in place).
#[allow(clippy::too_many_arguments)]
fn apply_directory_renames(
    base_map: &MergeEntryMap,
    eff_base: &MergeEntryMap,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
    dir_renames: &DirectoryRenameMaps,
    file_rename_dests: &BTreeMap<Vec<u8>, MergeRename>,
) -> DirRenameOutcome {
    let mut base = eff_base.clone();
    let mut ours = eff_ours.clone();
    let mut theirs = eff_theirs.clone();
    let mut rehomed = BTreeMap::new();
    let mut collisions = Vec::new();
    let mut splits = BTreeSet::new();
    let mut back_to_self = BTreeSet::new();
    let mut info_messages = Vec::new();
    let mut dirty = false;

    // Ours' paths follow THEIRS' directory renames; the exclusions are OURS' own
    // renamed-into dirs (never re-home a path into a directory this same side
    // renamed). Symmetrically for theirs.
    let ours_excl = exclusion_dirs(&dir_renames.ours);
    let theirs_excl = exclusion_dirs(&dir_renames.theirs);

    // Plan ours' moves (following theirs' dir-renames) and theirs' moves
    // (following ours' dir-renames). Planning before applying lets us detect
    // collisions (N paths onto one destination) across the whole side.
    let ours_moves = plan_rehome(
        base_map,
        &ours,
        ours_side,
        &dir_renames.theirs,
        &ours_excl,
        &dir_renames.theirs_split,
        &mut collisions,
        &mut splits,
        &mut info_messages,
        &mut dirty,
    );
    let theirs_moves = plan_rehome(
        base_map,
        &theirs,
        theirs_side,
        &dir_renames.ours,
        &theirs_excl,
        &dir_renames.ours_split,
        &mut collisions,
        &mut splits,
        &mut info_messages,
        &mut dirty,
    );

    apply_rehome_moves(
        base_map,
        file_rename_dests,
        &mut base,
        &mut ours,
        &mut theirs,
        ours_moves,
        true,
        &mut rehomed,
        &mut collisions,
        &mut back_to_self,
        &mut dirty,
    );
    apply_rehome_moves(
        base_map,
        file_rename_dests,
        &mut base,
        &mut ours,
        &mut theirs,
        theirs_moves,
        false,
        &mut rehomed,
        &mut collisions,
        &mut back_to_self,
        &mut dirty,
    );

    DirRenameOutcome {
        base,
        ours,
        theirs,
        rehomed,
        collisions,
        splits,
        back_to_self,
        dirty,
        info_messages,
    }
}

/// The set of *source* directories a side renamed away from. A directory rename
/// the other side wants to apply into one of these dirs is skipped (it would
/// produce a spurious rename/rename(1to2)); git's `dir_rename_exclusions`.
fn exclusion_dirs(side_dir_renames: &BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeSet<Vec<u8>> {
    side_dir_renames.keys().cloned().collect()
}

/// Re-home `target`'s added/renamed paths that fall under a directory the other
/// side renamed (`renamer_dirs`: `old_dir -> new_dir`).
///
/// Candidates are paths present on this side and absent in base — i.e. both
/// Plan the directory-rename moves for one side: which of its added/renamed
/// paths re-home where, following `renamer_dirs` (the OTHER side's dir-renames).
///
/// Candidates are paths present on this side and absent in base — both freshly
/// added files AND this side's own rename destinations (the latter give the
/// transitive-rename behaviour). A candidate whose target directory is in
/// `exclusions` (a dir this side itself renamed) is skipped. Splits mark the
/// merge dirty; N-to-1 collisions (multiple sources onto one destination) record
/// a `DirRenameCollision` and yield no move. Returns the surviving single moves
/// (one per destination).
#[allow(clippy::too_many_arguments)]
fn plan_rehome(
    base_map: &MergeEntryMap,
    side: &MergeEntryMap,
    side_renames: &SideRenames,
    renamer_dirs: &BTreeMap<Vec<u8>, Vec<u8>>,
    exclusions: &BTreeSet<Vec<u8>>,
    split_dirs: &BTreeSet<Vec<u8>>,
    collisions: &mut Vec<DirRenameCollision>,
    splits: &mut BTreeSet<Vec<u8>>,
    info_messages: &mut Vec<MergeInfoMessage>,
    dirty: &mut bool,
) -> Vec<DirRenameMove> {
    if renamer_dirs.is_empty() && split_dirs.is_empty() {
        return Vec::new();
    }

    // This side's rename destinations -> sources; eligible for a transitive
    // rewrite and carry the original source for message wording.
    let side_rename_src: BTreeMap<&[u8], &[u8]> = side_renames
        .pairs
        .iter()
        .map(|(o, n)| (n.as_slice(), o.as_slice()))
        .collect();

    let candidates: Vec<Vec<u8>> = side
        .keys()
        .filter(|p| !base_map.contains_key(*p) || side_rename_src.contains_key(p.as_slice()))
        .cloned()
        .collect();

    // dest -> the moves wanting to land there (collision detection).
    let mut planned: BTreeMap<Vec<u8>, Vec<DirRenameMove>> = BTreeMap::new();
    for path in candidates {
        if let Some(split_dir) = check_dir_split(&path, split_dirs) {
            splits.insert(split_dir.to_vec());
            *dirty = true;
            continue;
        }
        let Some((old_dir, new_dir)) = check_dir_renamed(&path, renamer_dirs) else {
            continue;
        };
        // dir_rename_exclusions: don't apply a rename INTO a directory this side
        // itself renamed; that would cause a spurious rename/rename(1to2). The
        // file instead follows this side's own rename, so leave it.
        let new_dir_is_exclusion = exclusions.contains(new_dir);
        let new_dir_inside_exclusion = exclusions
            .iter()
            .any(|dir| directory_contains_proper(dir, new_dir));
        if new_dir_is_exclusion
            || (new_dir_inside_exclusion
                && !side_has_pure_add_under_dir(side, base_map, &side_rename_src, old_dir))
        {
            info_messages.push(MergeInfoMessage::DirRenameSkippedDueToRerename {
                old_dir: old_dir.to_vec(),
                path: path.clone(),
                new_dir: new_dir.to_vec(),
            });
            continue;
        }
        let dest = apply_dir_rename(old_dir, new_dir, &path);
        if dest == path {
            // Directory rename causes a rename-to-self: already in place.
            continue;
        }
        let renamed_from = side_rename_src.get(path.as_slice()).map(|s| s.to_vec());
        planned
            .entry(dest.clone())
            .or_default()
            .push(DirRenameMove {
                from: path,
                to: dest,
                renamed_from,
            });
    }

    let mut moves = Vec::new();
    for (dest, group) in planned {
        if group.len() > 1 {
            // Multiple paths map to one destination: an implicit-dir-rename
            // collision. git leaves all of them in place and conflicts.
            *dirty = true;
            collisions.push(DirRenameCollision {
                dest,
                sources: group.into_iter().map(|m| m.from).collect(),
            });
            continue;
        }
        moves.push(group.into_iter().next().expect("non-empty"));
    }
    moves
}

fn check_dir_split<'a>(path: &[u8], split_dirs: &'a BTreeSet<Vec<u8>>) -> Option<&'a [u8]> {
    let mut dir = parent_dir(path)?;
    loop {
        if let Some(split_dir) = split_dirs.get(dir) {
            return Some(split_dir);
        }
        dir = parent_dir(dir)?;
    }
}

fn directory_contains_proper(parent: &[u8], child: &[u8]) -> bool {
    !parent.is_empty()
        && child.len() > parent.len()
        && child.starts_with(parent)
        && child[parent.len()] == b'/'
}

fn side_has_pure_add_under_dir(
    side: &MergeEntryMap,
    base_map: &MergeEntryMap,
    side_rename_src: &BTreeMap<&[u8], &[u8]>,
    dir: &[u8],
) -> bool {
    side.keys().any(|path| {
        path_is_under_dir(path, dir)
            && !base_map.contains_key(path)
            && !side_rename_src.contains_key(path.as_slice())
    })
}

fn path_is_under_dir(path: &[u8], dir: &[u8]) -> bool {
    !dir.is_empty() && path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

/// Apply a side's planned re-home moves to all three effective maps.
///
/// `side_is_ours` says whether the moves originate from ours' (true) or theirs'
/// (false) paths — used both for `=conflict`-mode provenance and to decide which
/// side's entry the move primarily belongs to. A move whose source is a
/// content-merge path (present on the other side and in base too) re-homes
/// across `base`/`ours`/`theirs` together, so the 3-way merge follows it to the
/// new location; a pure add re-homes only its own side.
#[allow(clippy::too_many_arguments)]
fn apply_rehome_moves(
    original_base: &MergeEntryMap,
    file_rename_dests: &BTreeMap<Vec<u8>, MergeRename>,
    base: &mut MergeEntryMap,
    ours: &mut MergeEntryMap,
    theirs: &mut MergeEntryMap,
    moves: Vec<DirRenameMove>,
    side_is_ours: bool,
    rehomed: &mut BTreeMap<Vec<u8>, RehomeSides>,
    collisions: &mut Vec<DirRenameCollision>,
    back_to_self: &mut BTreeSet<Vec<u8>>,
    dirty: &mut bool,
) {
    for mv in moves {
        // A file in the way at the destination is only a blocker when it is
        // present on this same side (or in base). If the other side already
        // occupies the destination, applying this move produces the normal
        // two-sided conflict at that path (e.g. t6423 1d's rename/rename(2to1)).
        let occupied_on_this_side = if side_is_ours {
            ours.contains_key(&mv.to) || map_has_directory_at(ours, &mv.to)
        } else {
            theirs.contains_key(&mv.to) || map_has_directory_at(theirs, &mv.to)
        };
        let occupied_by_cross_rename =
            file_rename_dests
                .get(&mv.to)
                .is_some_and(|rename| match (side_is_ours, rename.side) {
                    (true, RenameSide::Theirs) | (false, RenameSide::Ours) => true,
                    (true, RenameSide::Ours) | (false, RenameSide::Theirs) => false,
                });
        let base_entry_at_dest = original_base.get(&mv.to).copied();
        let base_entry_at_source = original_base.get(&mv.from).copied();
        let other_side_entry_at_dest = if side_is_ours {
            theirs.get(&mv.to).copied()
        } else {
            ours.get(&mv.to).copied()
        };
        let other_side_entry_at_source = if side_is_ours {
            theirs.get(&mv.from).copied()
        } else {
            ours.get(&mv.from).copied()
        };
        let base_entry_for_shifted_source = base_entry_at_source.or(base_entry_at_dest);
        let rename_back_to_modified_source = mv
            .renamed_from
            .as_ref()
            .is_some_and(|source| source == &mv.to)
            && base_entry_at_dest.is_some()
            && (other_side_entry_at_dest.is_some_and(|entry| Some(entry) != base_entry_at_dest)
                || other_side_entry_at_source
                    .is_some_and(|entry| Some(entry) != base_entry_for_shifted_source));
        if ((base_entry_at_dest.is_some() && !rename_back_to_modified_source)
            || (occupied_on_this_side && !occupied_by_cross_rename))
            && mv.to != mv.from
        {
            *dirty = true;
            collisions.push(DirRenameCollision {
                dest: mv.to.clone(),
                sources: vec![mv.from.clone()],
            });
            continue;
        }
        let mut moved = false;
        if occupied_by_cross_rename {
            base.remove(&mv.from);
            if side_is_ours {
                if let Some(entry) = ours.remove(&mv.from) {
                    ours.insert(mv.to.clone(), entry);
                    moved = true;
                }
                theirs.remove(&mv.from);
            } else {
                ours.remove(&mv.from);
                if let Some(entry) = theirs.remove(&mv.from) {
                    theirs.insert(mv.to.clone(), entry);
                    moved = true;
                }
            }
        } else {
            // Move the path on every map that holds it (base for the ancestor,
            // and whichever sides carry content at the path). This keeps a
            // content-merge keyed consistently at the re-homed destination.
            for m in [&mut *base, &mut *ours, &mut *theirs] {
                if let Some(entry) = m.remove(&mv.from) {
                    m.insert(mv.to.clone(), entry);
                    moved = true;
                }
            }
        }
        if moved {
            if rename_back_to_modified_source {
                back_to_self.insert(mv.to.clone());
            }
            let info = RehomeInfo {
                old_path: mv.from.clone(),
                renamed_from: mv.renamed_from.clone(),
                added_on_ours: side_is_ours,
            };
            let entry = rehomed.entry(mv.to.clone()).or_default();
            if side_is_ours {
                entry.ours = Some(info);
            } else {
                entry.theirs = Some(info);
            }
        }
    }
}

fn collect_dir_rename_two_to_one(
    renames: &MergeRenames,
    rehomed: &BTreeMap<Vec<u8>, RehomeSides>,
) -> Vec<DirRenameTwoToOne> {
    let mut conflicts = Vec::new();
    for (dest, sides) in rehomed {
        let Some(file_rename) = renames.dest_to_source.get(dest) else {
            continue;
        };
        match file_rename.side {
            RenameSide::Ours => {
                let Some(info) = sides.theirs.as_ref() else {
                    continue;
                };
                let Some(theirs_source) = info.renamed_from.as_ref() else {
                    continue;
                };
                conflicts.push(DirRenameTwoToOne {
                    dest: dest.clone(),
                    ours_source: file_rename.source.clone(),
                    theirs_source: theirs_source.clone(),
                    ours_label_path: dest.clone(),
                    theirs_label_path: info.old_path.clone(),
                });
            }
            RenameSide::Theirs => {
                let Some(info) = sides.ours.as_ref() else {
                    continue;
                };
                let Some(ours_source) = info.renamed_from.as_ref() else {
                    continue;
                };
                conflicts.push(DirRenameTwoToOne {
                    dest: dest.clone(),
                    ours_source: ours_source.clone(),
                    theirs_source: file_rename.source.clone(),
                    ours_label_path: info.old_path.clone(),
                    theirs_label_path: dest.clone(),
                });
            }
        }
    }
    conflicts
}

fn map_has_directory_at(map: &MergeEntryMap, path: &[u8]) -> bool {
    let mut prefix = path.to_vec();
    prefix.push(b'/');
    map.keys().any(|candidate| candidate.starts_with(&prefix))
}

fn remap_rename_destinations(renames: &mut MergeRenames, rehomed: &BTreeMap<Vec<u8>, RehomeSides>) {
    if rehomed.is_empty() {
        return;
    }
    let mut remapped_deletes = BTreeMap::new();
    for (dest, rd) in std::mem::take(&mut renames.rename_deletes) {
        let new_dest = rehomed
            .iter()
            .find_map(|(new_dest, sides)| {
                let moved = sides
                    .ours
                    .as_ref()
                    .is_some_and(|info| info.old_path == dest)
                    || sides
                        .theirs
                        .as_ref()
                        .is_some_and(|info| info.old_path == dest);
                moved.then(|| new_dest.clone())
            })
            .unwrap_or(dest);
        remapped_deletes.insert(new_dest, rd);
    }
    renames.rename_deletes = remapped_deletes;

    for rename in renames.rename_rename_one_to_two.values_mut() {
        for (dest, sides) in rehomed {
            if sides
                .ours
                .as_ref()
                .is_some_and(|info| info.old_path == rename.ours_dest)
            {
                rename.ours_dest = dest.clone();
            }
            if sides
                .theirs
                .as_ref()
                .is_some_and(|info| info.old_path == rename.theirs_dest)
            {
                rename.theirs_dest = dest.clone();
            }
        }
    }
}

fn drop_collapsed_rename_rename_conflicts(renames: &mut MergeRenames) {
    renames
        .rename_rename_one_to_two
        .retain(|_, rename| rename.ours_dest != rename.theirs_dest);
}

fn apply_dir_rename_two_to_one_conflicts(
    db: &FileObjectDatabase,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    conflicts: &[DirRenameTwoToOne],
    paths: &mut [MergedPath],
    leaves: &mut MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<()> {
    for conflict in conflicts {
        let Some(slot) = paths.iter_mut().find(|path| path.path == conflict.dest) else {
            continue;
        };
        let ours_entry = eff_ours.get(&conflict.dest).copied();
        let theirs_entry = eff_theirs.get(&conflict.dest).copied();
        let (Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) =
            (ours_entry, theirs_entry)
        else {
            continue;
        };
        let ours_bytes = merge_blob_bytes(db, &ours_oid)?;
        let theirs_bytes = merge_blob_bytes(db, &theirs_oid)?;
        let (resolved_mode, mode_conflict) = merge_file_modes(None, ours_mode, theirs_mode);
        let result = if is_mergeable_file_mode(ours_mode) && is_mergeable_file_mode(theirs_mode) {
            merge_blobs(
                &[],
                &ours_bytes,
                &theirs_bytes,
                &MergeBlobOptions {
                    ours_label: &qualify_label(options.ours_label, &conflict.ours_label_path),
                    theirs_label: &qualify_label(options.theirs_label, &conflict.theirs_label_path),
                    base_label: options.ancestor_label,
                    style: options.style,
                    favor: options.favor,
                },
            )
        } else {
            MergeBlobResult {
                content: ours_bytes.clone(),
                conflicted: true,
            }
        };
        let oid = db.write_object(EncodedObject::new(ObjectType::Blob, result.content.clone()))?;
        leaves.insert(conflict.dest.clone(), (resolved_mode, oid));
        slot.stages = MergeStages {
            base: None,
            ours: ours_entry,
            theirs: theirs_entry,
        };
        slot.result = Some((resolved_mode, oid));
        slot.worktree = Some((
            if ours_mode == theirs_mode {
                ours_mode
            } else {
                0o100644
            },
            result.content,
        ));
        slot.conflict = Some(MergeConflictKind::RenameRenameTwoToOne {
            ours_path: conflict.ours_source.clone(),
            theirs_path: conflict.theirs_source.clone(),
        });
        slot.auto_merged = !mode_conflict;
    }
    Ok(())
}

/// 3-way merge one rename's content into a single leaf entry: `base` is the
/// source's ancestor blob, `ours`/`theirs` the two sides' content (one of which
/// is the renamed file, the other the other side's change to the source). Both
/// present and differing → a real content merge; otherwise the surviving side's
/// entry is carried as-is.
fn rename_merged_leaf(
    db: &FileObjectDatabase,
    base: Option<(u32, ObjectId)>,
    ours: Option<(u32, ObjectId)>,
    theirs: Option<(u32, ObjectId)>,
    options: &MergeTreesOptions<'_>,
) -> Result<Option<(u32, ObjectId)>> {
    match (ours, theirs) {
        (None, None) => Ok(None),
        (Some(entry), None) | (None, Some(entry)) => Ok(Some(entry)),
        (Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) => {
            if (ours_mode, ours_oid) == (theirs_mode, theirs_oid) {
                return Ok(Some((ours_mode, ours_oid)));
            }
            if !is_mergeable_file_mode(ours_mode) || !is_mergeable_file_mode(theirs_mode) {
                return Ok(Some((ours_mode, ours_oid)));
            }
            let base_bytes = match base {
                Some((_, oid)) => merge_blob_bytes(db, &oid)?,
                None => Vec::new(),
            };
            let result = merge_blobs(
                &base_bytes,
                &merge_blob_bytes(db, &ours_oid)?,
                &merge_blob_bytes(db, &theirs_oid)?,
                &MergeBlobOptions {
                    ours_label: options.ours_label,
                    theirs_label: options.theirs_label,
                    base_label: options.ancestor_label,
                    style: options.style,
                    favor: options.favor,
                },
            );
            let (mode, _) = merge_file_modes(base.map(|(mode, _)| mode), ours_mode, theirs_mode);
            let oid = db.write_object(EncodedObject::new(ObjectType::Blob, result.content))?;
            Ok(Some((mode, oid)))
        }
    }
}

/// Apply rename/rename(2to1) and rename/add conflicts: two distinct contents
/// land on one destination path. Each side's content at the destination is the
/// 3-way merge of its own rename (so the other side's change to the renamed
/// source follows the rename); the two results become stages 2 and 3 with no
/// common ancestor, and the worktree holds their two-way merge. The rename
/// source paths are consumed (removed from the path set) so they don't surface as
/// a spurious modify/delete.
#[allow(clippy::too_many_arguments)]
fn apply_rename_two_to_one_and_add_conflicts(
    db: &FileObjectDatabase,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    renames: &MergeRenames,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<()> {
    let mut consumed_sources: Vec<Vec<u8>> = Vec::new();

    for (dest, conflict) in &renames.rename_rename_two_to_one {
        // Ours renamed `ours_source`->dest; theirs' change to `ours_source`
        // follows the rename. Symmetric for theirs.
        let ours_leaf = rename_merged_leaf(
            db,
            base_map.get(&conflict.ours_source).copied(),
            ours_map.get(dest).copied(),
            theirs_map.get(&conflict.ours_source).copied(),
            options,
        )?;
        let theirs_leaf = rename_merged_leaf(
            db,
            base_map.get(&conflict.theirs_source).copied(),
            ours_map.get(&conflict.theirs_source).copied(),
            theirs_map.get(dest).copied(),
            options,
        )?;
        write_two_sided_dest_conflict(
            db,
            dest,
            ours_leaf,
            theirs_leaf,
            MergeConflictKind::RenameRenameTwoToOne {
                ours_path: conflict.ours_source.clone(),
                theirs_path: conflict.theirs_source.clone(),
            },
            options,
            paths,
            leaves,
        )?;
        consumed_sources.push(conflict.ours_source.clone());
        consumed_sources.push(conflict.theirs_source.clone());
    }

    for (dest, add) in &renames.rename_adds {
        let (ours_leaf, theirs_leaf) = match add.side {
            RenameSide::Ours => (
                rename_merged_leaf(
                    db,
                    base_map.get(&add.source).copied(),
                    ours_map.get(dest).copied(),
                    theirs_map.get(&add.source).copied(),
                    options,
                )?,
                theirs_map.get(dest).copied(),
            ),
            RenameSide::Theirs => (
                ours_map.get(dest).copied(),
                rename_merged_leaf(
                    db,
                    base_map.get(&add.source).copied(),
                    ours_map.get(&add.source).copied(),
                    theirs_map.get(dest).copied(),
                    options,
                )?,
            ),
        };
        write_two_sided_dest_conflict(
            db,
            dest,
            ours_leaf,
            theirs_leaf,
            MergeConflictKind::Content { add_add: true },
            options,
            paths,
            leaves,
        )?;
        consumed_sources.push(add.source.clone());
    }

    // The rename source paths are consumed by the rename: the other side's
    // change to them followed the rename to the destination, so they resolve to
    // a clean deletion (not the path-keyed core's modify/delete). Marking them
    // `Resolved(None)` lets the worktree writer remove the now-stale source file
    // rather than leaving it as a stray untracked file.
    for source in &consumed_sources {
        leaves.remove(source);
        if let Some(slot) = paths.iter_mut().find(|path| &path.path == source) {
            slot.stages = MergeStages::default();
            slot.result = None;
            slot.worktree = None;
            slot.conflict = None;
            slot.auto_merged = false;
        } else {
            paths.push(MergedPath {
                path: source.clone(),
                stages: MergeStages::default(),
                result: None,
                worktree: None,
                conflict: None,
                auto_merged: false,
            });
        }
    }
    Ok(())
}

/// Record a destination path that holds two unmerged contents (rename/rename
/// 2to1 or rename/add): stage 2 = `ours_leaf`, stage 3 = `theirs_leaf`, no
/// common ancestor, worktree = their two-way merge. Replaces any existing slot
/// (the path-keyed core's add/add result) for the destination.
#[allow(clippy::too_many_arguments)]
fn write_two_sided_dest_conflict(
    db: &FileObjectDatabase,
    dest: &[u8],
    ours_leaf: Option<(u32, ObjectId)>,
    theirs_leaf: Option<(u32, ObjectId)>,
    kind: MergeConflictKind,
    options: &MergeTreesOptions<'_>,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
) -> Result<()> {
    let ours_bytes = match ours_leaf {
        Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
        None => None,
    };
    let theirs_bytes = match theirs_leaf {
        Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
        None => None,
    };
    let (worktree_mode, worktree_content, result_leaf) = match (&ours_bytes, &theirs_bytes) {
        (Some((ours_mode, ours_content)), Some((theirs_mode, theirs_content))) => {
            let merged = merge_blobs(
                &[],
                ours_content,
                theirs_content,
                &MergeBlobOptions {
                    ours_label: options.ours_label,
                    theirs_label: options.theirs_label,
                    base_label: options.ancestor_label,
                    style: options.style,
                    favor: options.favor,
                },
            );
            let mode = if ours_mode == theirs_mode {
                *ours_mode
            } else {
                0o100644
            };
            let oid = db.write_object(EncodedObject::new(
                ObjectType::Blob,
                merged.content.clone(),
            ))?;
            (mode, merged.content, Some((mode, oid)))
        }
        (Some((mode, content)), None) | (None, Some((mode, content))) => {
            (*mode, content.clone(), ours_leaf.or(theirs_leaf))
        }
        (None, None) => (0o100644, Vec::new(), None),
    };

    let slot = MergedPath {
        path: dest.to_vec(),
        stages: MergeStages {
            base: None,
            ours: ours_leaf,
            theirs: theirs_leaf,
        },
        result: result_leaf,
        worktree: Some((worktree_mode, worktree_content)),
        conflict: Some(kind),
        auto_merged: true,
    };
    if let Some(existing) = paths.iter_mut().find(|path| path.path == dest) {
        *existing = slot;
    } else {
        paths.push(slot);
    }
    if let Some(leaf) = result_leaf {
        leaves.insert(dest.to_vec(), leaf);
    } else {
        leaves.remove(dest);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_rename_rename_one_to_two_conflicts(
    db: &FileObjectDatabase,
    base_map: &MergeEntryMap,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    conflicts: &BTreeMap<Vec<u8>, RenameRenameOneToTwo>,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<()> {
    for (old_path, conflict) in conflicts {
        let base_entry = base_map.get(old_path).copied();
        let ours_entry = eff_ours.get(&conflict.ours_dest).copied();
        let theirs_entry = eff_theirs.get(&conflict.theirs_dest).copied();
        let theirs_add_at_ours_dest = eff_theirs.get(&conflict.ours_dest).copied();
        let ours_add_at_theirs_dest = eff_ours.get(&conflict.theirs_dest).copied();

        leaves.remove(old_path);
        leaves.remove(&conflict.ours_dest);
        leaves.remove(&conflict.theirs_dest);
        paths.retain(|path| {
            path.path != *old_path
                && path.path != conflict.ours_dest
                && path.path != conflict.theirs_dest
        });

        paths.push(MergedPath {
            path: old_path.clone(),
            stages: MergeStages {
                base: base_entry,
                ours: None,
                theirs: None,
            },
            result: None,
            worktree: None,
            conflict: Some(MergeConflictKind::RenameRenameOneToTwo {
                old_path: old_path.clone(),
                ours_path: conflict.ours_dest.clone(),
                theirs_path: conflict.theirs_dest.clone(),
                ours_label: options.ours_label.to_string(),
                theirs_label: options.theirs_label.to_string(),
            }),
            auto_merged: false,
        });

        let ours_worktree = match ours_entry {
            Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
            None => None,
        };
        paths.push(MergedPath {
            path: conflict.ours_dest.clone(),
            stages: MergeStages {
                base: None,
                ours: ours_entry,
                theirs: theirs_add_at_ours_dest,
            },
            result: None,
            worktree: ours_worktree,
            conflict: Some(MergeConflictKind::RenameRenameOneToTwoStage),
            auto_merged: false,
        });

        let theirs_worktree = match theirs_entry {
            Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
            None => None,
        };
        paths.push(MergedPath {
            path: conflict.theirs_dest.clone(),
            stages: MergeStages {
                base: None,
                ours: ours_add_at_theirs_dest,
                theirs: theirs_entry,
            },
            result: None,
            worktree: theirs_worktree,
            conflict: Some(MergeConflictKind::RenameRenameOneToTwoStage),
            auto_merged: false,
        });
    }
    Ok(())
}

/// Build a path-qualified conflict-marker label `"<label>:<path>"`, as git does
/// for renamed files (so the two sides of a conflict name their distinct paths).
fn qualify_label(label: &str, path: &[u8]) -> String {
    format!("{label}:{}", String::from_utf8_lossy(path))
}

/// Adapt a flat `path -> (mode, oid)` map into the `TrackedEntry` map the
/// name-status diff core consumes.
fn entry_map_as_tracked(map: &MergeEntryMap) -> BTreeMap<Vec<u8>, TrackedEntry> {
    map.iter()
        .map(|(path, (mode, oid))| {
            (
                path.clone(),
                TrackedEntry {
                    mode: *mode,
                    oid: *oid,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_formats::RepositoryLayout;
    use sley_object::TreeEntry;
    use sley_odb::ObjectWriter;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn name_status_reports_added_from_index() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                size: 6,
                oid,
                flags: "hello.txt".len() as u16,
                flags_extended: 0,
                path: BString::from(b"hello.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let changes = diff_name_status_head_worktree(&root, &layout.git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(changes[0].line(), "A\thello.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn index_worktree_diff_returns_staged_gitlinks() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: sley_index::GITLINK_MODE,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: "deps/sub".len() as u16,
                flags_extended: 0,
                path: BString::from(b"deps/sub"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        let diff = diff_name_status_index_worktree_with_options_and_gitlinks(
            &root,
            &layout.git_dir,
            ObjectFormat::Sha1,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");

        assert_eq!(diff.entries.len(), 1);
        let gitlinks = diff.staged_gitlinks;
        assert_eq!(gitlinks.len(), 1);
        assert_eq!(gitlinks[0].path.as_bytes(), b"deps/sub");
        assert_eq!(gitlinks[0].oid, oid);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn index_worktree_diff_ignores_untracked_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"clean\n".to_vec()))
            .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                size: 6,
                oid,
                flags: "tracked.txt".len() as u16,
                flags_extended: 0,
                path: BString::from(b"tracked.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        fs::write(root.join("tracked.txt"), b"clean\n").expect("test operation should succeed");
        symlink("missing-target", root.join("untracked-link"))
            .expect("test operation should succeed");

        let changes = diff_name_status_index_worktree_with_options(
            &root,
            &layout.git_dir,
            ObjectFormat::Sha1,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
        )
        .expect("untracked dangling symlink should be ignored");
        assert!(changes.is_empty());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn index_worktree_diff_trusts_non_racy_stat_cache() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let worktree_path = root.join("tracked.txt");
        fs::write(&worktree_path, b"clean\n").expect("test operation should succeed");
        let metadata = fs::symlink_metadata(&worktree_path).expect("test operation should succeed");
        let (mtime_seconds, mtime_nanoseconds) =
            sley_index::file_mtime_parts(&metadata).expect("test operation should succeed");
        let bogus_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: mtime_seconds as u32,
                mtime_nanoseconds: mtime_nanoseconds as u32,
                dev: 0,
                ino: 0,
                mode: sley_index::worktree_metadata_mode(&metadata),
                uid: 0,
                gid: 0,
                size: metadata.len() as u32,
                oid: bogus_oid,
                flags: "tracked.txt".len() as u16,
                flags_extended: 0,
                path: BString::from(b"tracked.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        let changes = diff_name_status_index_worktree(&root, &layout.git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            changes.is_empty(),
            "a clean non-racy stat match must reuse the cached index oid"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-diff-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test operation should succeed");
        path
    }

    // ---- line diff / blob merge tests ---------------------------------------

    fn merge_opts() -> MergeBlobOptions<'static> {
        MergeBlobOptions {
            ours_label: "ours",
            theirs_label: "theirs",
            base_label: "base",
            style: ConflictStyle::Merge,
            favor: MergeFavor::None,
        }
    }

    #[test]
    fn split_lines_preserves_content_and_newlines() {
        let lines = split_lines(b"a\nb\nc\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content, b"a\n");
        assert!(lines[0].has_newline);
        assert_eq!(lines[2].content, b"c\n");
        assert!(lines[2].has_newline);
        assert!(split_lines(b"").is_empty());
    }

    #[test]
    fn split_lines_tracks_missing_final_newline() {
        let lines = split_lines(b"a\nb");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].has_newline);
        assert!(!lines[1].has_newline);
        assert_eq!(lines[1].content, b"b");
        assert_eq!(lines[1].bytes_without_newline(), b"b");
        // A line that lost its newline must not compare equal to one that has it.
        let with_nl = split_lines(b"b\n");
        assert_ne!(lines[1], with_nl[0]);
    }

    #[test]
    fn myers_replace_single_line() {
        let old = split_lines(b"a\nb\nc\n");
        let new = split_lines(b"a\nx\nc\n");
        assert_eq!(
            myers_diff_lines(&old, &new),
            vec![
                DiffOp::Equal(1),
                DiffOp::Delete(1),
                DiffOp::Insert(1),
                DiffOp::Equal(1),
            ]
        );
    }

    #[test]
    fn myers_identical_is_single_equal() {
        let old = split_lines(b"a\nb\nc\n");
        let new = split_lines(b"a\nb\nc\n");
        assert_eq!(myers_diff_lines(&old, &new), vec![DiffOp::Equal(3)]);
    }

    #[test]
    fn myers_pure_insert_and_delete() {
        let empty = split_lines(b"");
        let two = split_lines(b"a\nb\n");
        assert_eq!(myers_diff_lines(&empty, &two), vec![DiffOp::Insert(2)]);
        assert_eq!(myers_diff_lines(&two, &empty), vec![DiffOp::Delete(2)]);

        let old = split_lines(b"a\nb\nc\nd\n");
        let new = split_lines(b"a\nc\nd\n");
        assert_eq!(
            myers_diff_lines(&old, &new),
            vec![DiffOp::Equal(1), DiffOp::Delete(1), DiffOp::Equal(2)]
        );
    }

    #[test]
    fn myers_reconstructs_new_and_is_minimal() {
        // Apply the script to `old` and confirm it yields `new`; also count edits.
        let old = split_lines(b"the\nquick\nbrown\nfox\n");
        let new = split_lines(b"the\nlazy\nbrown\ncat\n");
        let ops = myers_diff_lines(&old, &new);
        let mut oi = 0usize;
        let mut ni = 0usize;
        let mut edits = 0usize;
        let mut rebuilt: Vec<u8> = Vec::new();
        for op in &ops {
            match *op {
                DiffOp::Equal(n) => {
                    for _ in 0..n {
                        assert_eq!(old[oi], new[ni]);
                        rebuilt.extend_from_slice(old[oi].content);
                        oi += 1;
                        ni += 1;
                    }
                }
                DiffOp::Delete(n) => {
                    oi += n;
                    edits += n;
                }
                DiffOp::Insert(n) => {
                    for _ in 0..n {
                        rebuilt.extend_from_slice(new[ni].content);
                        ni += 1;
                    }
                    edits += n;
                }
            }
        }
        assert_eq!(rebuilt, b"the\nlazy\nbrown\ncat\n");
        // Two lines changed -> 2 deletes + 2 inserts is the minimal SES here.
        assert_eq!(edits, 4);
    }

    #[test]
    fn merge_non_overlapping_changes_is_clean() {
        let base = b"a\nb\nc\nd\ne\n";
        let ours = b"A\nb\nc\nd\ne\n";
        let theirs = b"a\nb\nc\nd\nE\n";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"A\nb\nc\nd\nE\n");
    }

    #[test]
    fn merge_identical_changes_no_conflict() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nX\nc\n";
        let theirs = b"a\nX\nc\n";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nX\nc\n");
    }

    #[test]
    fn merge_overlapping_change_emits_exact_markers() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nOURS\nc\n";
        let theirs = b"a\nTHEIRS\nc\n";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\nOURS\n=======\nTHEIRS\n>>>>>>> theirs\nc\n".to_vec(),
        );
    }

    #[test]
    fn merge_diff3_style_includes_base_section() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nOURS\nc\n";
        let theirs = b"a\nTHEIRS\nc\n";
        let options = MergeBlobOptions {
            style: ConflictStyle::Diff3,
            ..merge_opts()
        };
        let result = merge_blobs(base, ours, theirs, &options);
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\nOURS\n||||||| base\nb\n=======\nTHEIRS\n>>>>>>> theirs\nc\n"
                .to_vec(),
        );
    }

    #[test]
    fn merge_empty_label_omits_trailing_space() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nOURS\nc\n";
        let theirs = b"a\nTHEIRS\nc\n";
        let options = MergeBlobOptions {
            ours_label: "",
            theirs_label: "",
            base_label: "",
            style: ConflictStyle::Merge,
            favor: MergeFavor::None,
        };
        let result = merge_blobs(base, ours, theirs, &options);
        assert!(result.conflicted);
        // No trailing space after the 7 marker chars when the label is empty.
        assert_eq!(
            result.content,
            b"a\n<<<<<<<\nOURS\n=======\nTHEIRS\n>>>>>>>\nc\n".to_vec(),
        );
    }

    #[test]
    fn merge_add_add_empty_base_conflicts() {
        let result = merge_blobs(b"", b"x\ny\n", b"p\nq\n", &merge_opts());
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"<<<<<<< ours\nx\ny\n=======\np\nq\n>>>>>>> theirs\n".to_vec(),
        );
    }

    #[test]
    fn merge_add_add_empty_base_identical_is_clean() {
        let result = merge_blobs(b"", b"x\ny\n", b"x\ny\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"x\ny\n");
    }

    #[test]
    fn merge_deletion_one_side_takes_deletion() {
        // ours deletes line b; theirs leaves it -> clean, deletion wins.
        let result = merge_blobs(b"a\nb\nc\n", b"a\nc\n", b"a\nb\nc\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nc\n");
    }

    #[test]
    fn merge_deletion_vs_modification_conflicts() {
        // ours deletes b; theirs modifies b -> conflict.
        let result = merge_blobs(b"a\nb\nc\n", b"a\nc\n", b"a\nB!\nc\n", &merge_opts());
        assert!(result.conflicted);
        // ours side of the conflict is empty (the line was deleted).
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\n=======\nB!\n>>>>>>> theirs\nc\n".to_vec(),
        );
    }

    #[test]
    fn merge_missing_final_newline_marker_starts_on_own_line() {
        // Both sides drop the trailing newline AND conflict at the end. The
        // closing marker section must still begin on its own line.
        let base = b"a\nb";
        let ours = b"a\nOURS";
        let theirs = b"a\nTHEIRS";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\nOURS\n=======\nTHEIRS\n>>>>>>> theirs\n".to_vec(),
        );
    }

    #[test]
    fn merge_clean_preserves_missing_final_newline() {
        // ours removes the trailing newline; theirs is unchanged -> ours wins,
        // and the result keeps the missing newline.
        let result = merge_blobs(b"a\nb\n", b"a\nb", b"a\nb\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nb");
    }

    #[test]
    fn merge_both_append_identical_tail_is_clean() {
        let result = merge_blobs(b"a\n", b"a\nz\n", b"a\nz\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nz\n");
    }

    #[test]
    fn merge_when_ours_equals_base_yields_theirs() {
        // Regression: a side that did not change must not suppress the other
        // side's edits anywhere in the file.
        let base = b"b\na\n";
        let theirs = b"b\nb\nc\na\nc\n";
        let result = merge_blobs(base, base, theirs, &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, theirs.to_vec());
    }
    fn applied(outcome: ApplyOutcome) -> Vec<u8> {
        match outcome {
            ApplyOutcome::Applied(bytes) => bytes,
            ApplyOutcome::Rejected => panic!("expected Applied, got Rejected"),
        }
    }

    #[test]
    fn parse_multi_file_patch() {
        let patch = b"\
diff --git a/one.txt b/one.txt
index aaaaaaa..bbbbbbb 100644
--- a/one.txt
+++ b/one.txt
@@ -1,3 +1,3 @@
 alpha
-beta
+BETA
 gamma
diff --git a/two.txt b/two.txt
index ccccccc..ddddddd 100644
--- a/two.txt
+++ b/two.txt
@@ -1,2 +1,3 @@
 first
+inserted
 second
";
        let patches = parse_unified_patch(patch).expect("test operation should succeed");
        assert_eq!(patches.len(), 2);

        assert_eq!(patches[0].old_path.as_deref(), Some(b"one.txt".as_slice()));
        assert_eq!(patches[0].new_path.as_deref(), Some(b"one.txt".as_slice()));
        // The `index <a>..<b> 100644` line carries the unchanged-file mode, which
        // git's gitdiff_index records as old_mode.
        assert_eq!(patches[0].old_mode, Some(0o100644));
        assert_eq!(patches[0].old_oid_hex.as_deref(), Some(b"aaaaaaa".as_slice()));
        assert_eq!(patches[0].new_oid_hex.as_deref(), Some(b"bbbbbbb".as_slice()));
        assert_eq!(patches[0].hunks.len(), 1);
        let h = &patches[0].hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 3, 1, 3)
        );
        assert_eq!(
            h.lines,
            vec![
                HunkLine::Context(b"alpha".to_vec()),
                HunkLine::Delete(b"beta".to_vec()),
                HunkLine::Insert(b"BETA".to_vec()),
                HunkLine::Context(b"gamma".to_vec()),
            ]
        );

        assert_eq!(patches[1].new_path.as_deref(), Some(b"two.txt".as_slice()));
        assert_eq!(patches[1].hunks[0].new_len, 3);
    }

    #[test]
    fn parse_default_hunk_range_length() {
        // `@@ -1 +1,2 @@` (no comma) means a length of 1 on the old side.
        let patch = b"\
--- a/x
+++ b/x
@@ -1 +1,2 @@
 line
+added
";
        let patches = parse_unified_patch(patch).expect("test operation should succeed");
        let h = &patches[0].hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 1, 1, 2)
        );
    }

    #[test]
    fn parse_hunk_header_before_file_errors() {
        let patch = b"@@ -1,1 +1,1 @@\n context\n";
        assert!(parse_unified_patch(patch).is_err());
    }

    #[test]
    fn parse_mismatched_counts_errors() {
        // Header promises two old lines but only one is present.
        let patch = b"--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n only\n+new\n";
        assert!(parse_unified_patch(patch).is_err());
    }

    #[test]
    fn apply_clean_hunk() {
        let base = b"alpha\nbeta\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"alpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_with_line_offset() {
        // The hunk's recorded position (line 2) is a couple of lines above where
        // the matching context actually lives (line 4); the outward search must
        // find it. The hunk is NOT anchored at the file start (old_start > 1, so
        // no match_beginning) and has trailing context (`tail`, so no
        // match_end), which is exactly the shape a real drifted patch takes —
        // verified against `git apply` ("Hunk #1 succeeded at 4 (offset 2)").
        let base = b"pre1\npre2\npre3\nalpha\nbeta\ngamma\ntail\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -2,4 +2,4 @@\n alpha\n-beta\n+BETA\n gamma\n tail\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"pre1\npre2\npre3\nalpha\nBETA\ngamma\ntail\n");
    }

    #[test]
    fn apply_with_negative_line_offset() {
        // Recorded position is well past the real location; search backward.
        let base = b"alpha\nbeta\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -50,3 +50,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"alpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_multiple_hunks() {
        let base = b"a\nb\nc\nd\ne\nf\ng\nh\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n\
@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n\
@@ -6,3 +6,3 @@\n f\n-g\n+G\n h\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"a\nB\nc\nd\ne\nf\nG\nh\n");
    }

    #[test]
    fn reject_on_context_mismatch() {
        let base = b"alpha\nDIFFERENT\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn reject_when_match_end_required_but_not_at_eof() {
        // git's `apply.c`: a hunk with NO trailing context must match the END of
        // the file (`match_end`). Here the leading context (`tail`/`anchor`)
        // matches at the middle of the base, but there are further lines after
        // it, so the preimage does not reach EOF. git rejects this; the old
        // sley matcher wrongly applied it (duplicating the appended block). This
        // is the t4150-am cell-34 lever: rejection forces `am -3`'s 3-way path.
        let base = b"one\ntwo\nanchor\nalready\nappended\n";
        // Hunk: context `anchor`, then append `added1`/`added2`. No trailing
        // context => match_end. At line 3 (`anchor`) the preimage is just one
        // line and does not reach EOF, so it must be rejected.
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -3,1 +3,3 @@\n anchor\n+added1\n+added2\n")
                .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn append_at_eof_matches_when_context_reaches_end() {
        // The mirror of the rejection case: the same shape applies cleanly when
        // the matching context IS the last line of the file (preimage reaches
        // EOF), so `match_end` is satisfied.
        let base = b"one\ntwo\nanchor\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -3,1 +3,3 @@\n anchor\n+added1\n+added2\n")
                .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"one\ntwo\nanchor\nadded1\nadded2\n");
    }

    #[test]
    fn reject_when_match_beginning_required_but_not_at_start() {
        // A hunk anchored at line 1 (`old_start <= 1`) must match the START of
        // the file (`match_beginning`). If the matching context only appears
        // later, git rejects rather than wandering to it.
        let base = b"junk\nalpha\nbeta\ngamma\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n alpha\n+INSERT\n beta\n")
                .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn no_default_fuzz_rejects_on_trailing_context_mismatch() {
        // `git apply` / `git am` keep `p_context = UINT_MAX` by default, so they
        // do NOT fuzz a hunk in by dropping context. Here the trailing context
        // line (`gamma`) differs from the base (`DIVERGED`), and because the
        // anchor is line 1 the hunk must match the beginning with its FULL
        // preimage. Verified against real `git apply`: this is rejected.
        let base = b"alpha\nbeta\nDIVERGED\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn parse_and_apply_new_file() {
        let patch = parse_unified_patch(
            b"\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
",
        )
        .expect("test operation should succeed");
        assert!(patches_first_is_new(&patch));
        assert_eq!(patch[0].old_path, None);
        assert_eq!(patch[0].new_path.as_deref(), Some(b"new.txt".as_slice()));
        assert_eq!(patch[0].new_mode, Some(0o100644));
        // Base is ignored for a new file.
        let out = applied(apply_file_patch(b"garbage that is ignored", &patch[0]));
        assert_eq!(out, b"hello\nworld\n");
    }

    fn patches_first_is_new(patches: &[FilePatch]) -> bool {
        patches.first().map(|p| p.is_new).unwrap_or(false)
    }

    #[test]
    fn parse_and_apply_delete_file() {
        let patch = parse_unified_patch(
            b"\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 1111111..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-hello
-world
",
        )
        .expect("test operation should succeed");
        assert!(patch[0].is_delete);
        assert_eq!(patch[0].old_path.as_deref(), Some(b"gone.txt".as_slice()));
        assert_eq!(patch[0].new_path, None);
        assert_eq!(patch[0].old_mode, Some(0o100644));
        let out = applied(apply_file_patch(b"hello\nworld\n", &patch[0]));
        assert_eq!(out, b"");
    }

    #[test]
    fn parse_rename_headers() {
        let patch = parse_unified_patch(
            b"\
diff --git a/old/name.txt b/new/name.txt
similarity index 100%
rename from old/name.txt
rename to new/name.txt
",
        )
        .expect("test operation should succeed");
        assert!(patch[0].is_rename);
        assert_eq!(
            patch[0].old_path.as_deref(),
            Some(b"old/name.txt".as_slice())
        );
        assert_eq!(
            patch[0].new_path.as_deref(),
            Some(b"new/name.txt".as_slice())
        );
        assert!(patch[0].hunks.is_empty());
    }

    #[test]
    fn parse_mode_change_headers() {
        let patch = parse_unified_patch(
            b"\
diff --git a/script.sh b/script.sh
old mode 100644
new mode 100755
",
        )
        .expect("test operation should succeed");
        assert_eq!(patch[0].old_mode, Some(0o100644));
        assert_eq!(patch[0].new_mode, Some(0o100755));
        assert!(!patch[0].is_new);
        assert!(!patch[0].is_delete);
    }

    #[test]
    fn no_final_newline_base_preserved_when_untouched() {
        // The change is on line 1; the final line has no newline and is not
        // modified, so its no-newline state must survive. This uses the patch
        // shape real `git diff` emits for such a change — `@@ -1,3 +1,3 @@` with
        // the two unchanged lines as trailing context (the `\ No newline`
        // marker rides the last context line). A hand-rolled `@@ -1,1 +1,1 @@`
        // with NO trailing context would (correctly) be rejected by git, since
        // a no-trailing-context hunk anchored at line 1 must span the whole
        // file (`match_beginning` && `match_end`).
        let base = b"alpha\nbeta\nnotail"; // "notail" has no trailing \n
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n-alpha\n+ALPHA\n beta\n notail\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"ALPHA\nbeta\nnotail");
    }

    #[test]
    fn no_final_newline_added_by_patch() {
        // Old file ends with a newline; patch rewrites the last line to one
        // without a trailing newline.
        let base = b"alpha\nbeta\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -2,1 +2,1 @@\n-beta\n+beta-notail\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].hunks[0].new_no_newline);
        assert!(!patch[0].hunks[0].old_no_newline);
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"alpha\nbeta-notail");
    }

    #[test]
    fn no_final_newline_in_base_matched_and_kept() {
        // Both sides lack a trailing newline; context match must require the
        // base's final line to itself be newline-free.
        let base = b"alpha\nbeta"; // no trailing newline
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-alpha\n+ALPHA\n beta\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].hunks[0].old_no_newline);
        assert!(patch[0].hunks[0].new_no_newline);
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"ALPHA\nbeta");
    }

    #[test]
    fn no_final_newline_mismatch_rejected() {
        // Patch asserts the old file has no trailing newline, but the base does.
        // That must be rejected rather than silently mis-applied.
        let base = b"alpha\nbeta\n"; // HAS trailing newline
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -2,1 +2,1 @@\n-beta\n\\ No newline at end of file\n+beta2\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].hunks[0].old_no_newline);
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn delete_with_no_final_newline() {
        // Deleting the entire content of a file that had no trailing newline.
        let base = b"only line no newline";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-only line no newline\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].is_delete);
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"");
    }

    #[test]
    fn apply_pure_insertion_hunk() {
        let base = b"first\nsecond\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n first\n+middle\n second\n")
                .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"first\nmiddle\nsecond\n");
    }

    #[test]
    fn apply_pure_deletion_hunk() {
        let base = b"first\nmiddle\nsecond\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,3 +1,2 @@\n first\n-middle\n second\n")
                .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"first\nsecond\n");
    }

    #[test]
    fn apply_then_reparse_round_trip() {
        // Hand-written unified diff -> apply -> the result is exactly the new
        // file content the diff describes. Re-parsing the same patch yields an
        // identical structure (idempotent parse).
        let base = b"l1\nl2\nl3\nl4\nl5\n";
        let text = b"--- a/f\n+++ b/f\n@@ -2,3 +2,4 @@\n l2\n-l3\n+L3\n+L3b\n l4\n";
        let p1 = parse_unified_patch(text).expect("test operation should succeed");
        let p2 = parse_unified_patch(text).expect("test operation should succeed");
        assert_eq!(p1, p2);
        let out = applied(apply_file_patch(base, &p1[0]));
        assert_eq!(out, b"l1\nl2\nL3\nL3b\nl4\nl5\n");
    }

    #[test]
    fn empty_context_line_without_trailing_space() {
        // Some transports strip the single leading space from blank context
        // lines; the parser treats a wholly empty body line as blank context.
        let base = b"a\n\nb\n";
        let patch = parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n")
            .expect("test operation should succeed");
        assert_eq!(patch[0].hunks[0].lines[1], HunkLine::Context(Vec::new()));
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"a\n\nB\n");
    }

    #[test]
    fn split_blob_lines_handles_edge_cases() {
        assert!(split_blob_lines(b"").is_empty());
        let single = split_blob_lines(b"abc");
        assert_eq!(single.len(), 1);
        assert!(single[0].no_newline);
        let terminated = split_blob_lines(b"abc\n");
        assert_eq!(terminated.len(), 1);
        assert!(!terminated[0].no_newline);
        let blank_then_eof = split_blob_lines(b"x\n");
        assert_eq!(blank_then_eof.len(), 1);
    }

    // ---- content similarity & inexact rename/copy detection -----------------

    #[test]
    fn similarity_identical_and_empty_conventions() {
        // Byte-identical blobs are always 100% similar.
        assert_eq!(blob_similarity(b"hello\nworld\n", b"hello\nworld\n"), 100);
        // Two empty blobs are identical -> 100.
        assert_eq!(blob_similarity(b"", b""), 100);
        // An empty blob vs a non-empty one shares nothing -> 0.
        assert_eq!(blob_similarity(b"", b"hello\n"), 0);
        assert_eq!(blob_similarity(b"hello\n", b""), 0);
    }

    #[test]
    fn similarity_one_changed_line_is_75_and_symmetric() {
        // A = one/two/three/four/five (bytes: 4+4+6+5+5 = 24).
        // B changes "three\n" -> "THREE\n" (same total size 24).
        // Common spans: one,two,four,five = 4+4+5+5 = 18 bytes.
        // score = round(18 * 100 / max(24, 24)) = round(75) = 75.
        // Verified against `git diff -M` which reports "similarity index 75%".
        let a = b"one\ntwo\nthree\nfour\nfive\n";
        let b = b"one\ntwo\nTHREE\nfour\nfive\n";
        assert_eq!(blob_similarity(a, b), 75);
        // The metric is symmetric.
        assert_eq!(blob_similarity(b, a), 75);
    }

    #[test]
    fn similarity_one_edited_line_of_three_is_66_not_67() {
        // "a\nb\nc\n" -> "a\nB\nc\n": one of three lines edited (4 common bytes of
        // 6). git reports `R066` / "similarity index 66%". git's two-step integer
        // math is `4 * 60000 / 6 = 40000`, then `40000 * 100 / 60000 = 66` (both
        // truncated); a single rounded `4 * 100 / 6` would give 67. This pins the
        // MAX_SCORE-based rounding so it stays aligned with diffcore-rename.
        assert_eq!(blob_similarity(b"a\nb\nc\n", b"a\nB\nc\n"), 66);
        assert_eq!(blob_similarity(b"a\nB\nc\n", b"a\nb\nc\n"), 66);
    }

    #[test]
    fn similarity_small_append_is_88() {
        // A: 8 lines totalling 46 bytes. B: same 8 lines + "ADDED\n" (6 bytes) = 52.
        // Common = the 46 original bytes; score = round(46*100/52) = 88.
        // Verified against `git diff -M` -> "similarity index 88%".
        let a = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
        let b = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\nADDED\n";
        assert_eq!(blob_similarity(a, b), 88);
    }

    #[test]
    fn similarity_half_rewrite_is_50() {
        // 6 lines, last 3 rewritten. Common = l1,l2,l3 = 9 bytes; total each 18.
        // score = round(9*100/18) = 50. Verified against `git diff -M`.
        let a = b"l1\nl2\nl3\nl4\nl5\nl6\n";
        let b = b"l1\nl2\nl3\nX4\nX5\nX6\n";
        assert_eq!(blob_similarity(a, b), 50);
    }

    // ---- tree-diff based inexact detection ----------------------------------

    /// Write a blob and return its oid.
    fn write_blob(db: &mut FileObjectDatabase, bytes: &[u8]) -> ObjectId {
        db.write_object(EncodedObject::new(ObjectType::Blob, bytes.to_vec()))
            .expect("test operation should succeed")
    }

    /// Write a tree from `(name, mode, oid)` entries (sorted by name as git
    /// requires) and return its oid.
    fn write_tree(db: &mut FileObjectDatabase, entries: &[(&[u8], u32, ObjectId)]) -> ObjectId {
        let mut tree_entries: Vec<TreeEntry> = entries
            .iter()
            .map(|(name, mode, oid)| TreeEntry {
                mode: *mode,
                name: BString::from(*name),
                oid: *oid,
            })
            .collect();
        tree_entries.sort_by(|a, b| a.name.cmp(&b.name));
        let tree = Tree {
            entries: tree_entries,
        };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("test operation should succeed")
    }

    #[test]
    fn inexact_rename_detected_with_plausible_score() {
        // a.txt (one changed line vs the new b.txt) should be detected as a
        // rename with score 75 (see `similarity_one_changed_line_is_75`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(&mut db, b"one\ntwo\nthree\nfour\nfive\n");
        let new = write_blob(&mut db, b"one\ntwo\nTHREE\nfour\nfive\n");
        let left = write_tree(&mut db, &[(b"a.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"b.txt", 0o100644, new)]);

        let opts = RenameDetectionOptions {
            base: DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
        rename_limit: 0,
        };
        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");

        assert_eq!(
            entries.len(),
            1,
            "expected a single rename entry: {entries:?}"
        );
        assert_eq!(entries[0].status, NameStatus::Renamed(75));
        assert_eq!(
            entries[0].old_path.as_ref().map(|p| p.as_bytes()),
            Some(b"a.txt".as_slice())
        );
        assert_eq!(entries[0].path, b"b.txt");
        assert_eq!(entries[0].line(), "R075\ta.txt\tb.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_rename_below_threshold_not_detected() {
        // A half-rewrite scores 50%. With a 60% threshold it must NOT be paired;
        // the change shows up as a separate Add + Delete instead.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(&mut db, b"l1\nl2\nl3\nl4\nl5\nl6\n");
        let new = write_blob(&mut db, b"l1\nl2\nl3\nX4\nX5\nX6\n");
        let left = write_tree(&mut db, &[(b"a.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"b.txt", 0o100644, new)]);

        let opts = RenameDetectionOptions {
            base: DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
            detect_inexact: true,
            rename_threshold: 60,
            copy_threshold: 60,
        rename_limit: 0,
        };
        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");

        let statuses: Vec<_> = entries.iter().map(|e| e.status).collect();
        assert!(
            statuses.contains(&NameStatus::Added) && statuses.contains(&NameStatus::Deleted),
            "expected separate add/delete below threshold, got {entries:?}"
        );
        assert!(
            !statuses.iter().any(|s| matches!(s, NameStatus::Renamed(_))),
            "no rename should be reported below threshold: {entries:?}"
        );

        // Sanity: lowering the threshold to 50 *does* detect it (boundary is
        // inclusive), and the score is exactly 50.
        let opts_low = RenameDetectionOptions {
            rename_threshold: 50,
            ..opts
        };
        let entries_low = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts_low,
        )
        .expect("test operation should succeed");
        assert_eq!(entries_low.len(), 1);
        assert_eq!(entries_low[0].status, NameStatus::Renamed(50));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn exact_rename_scores_100_and_takes_priority() {
        // Identical content moved to a new path is an exact rename: score 100,
        // detected even with inexact disabled, and still 100 with it enabled.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let oid = write_blob(&mut db, b"identical\ncontent\nhere\n");
        let left = write_tree(&mut db, &[(b"old.txt", 0o100644, oid)]);
        let right = write_tree(&mut db, &[(b"new.txt", 0o100644, oid)]);

        for inexact in [false, true] {
            let opts = RenameDetectionOptions {
                base: DiffNameStatusOptions {
                    detect_renames: true,
                    detect_copies: false,
                    find_copies_harder: false,
                    rename_empty: true,
                },
                detect_inexact: inexact,
                rename_threshold: DEFAULT_RENAME_THRESHOLD,
                copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            };
            let entries = diff_name_status_trees_with_rename_options(
                &db,
                ObjectFormat::Sha1,
                &left,
                &right,
                opts,
            )
            .expect("test operation should succeed");
            assert_eq!(entries.len(), 1, "inexact={inexact}: {entries:?}");
            assert_eq!(entries[0].status, NameStatus::Renamed(100));
            assert_eq!(
                entries[0].old_path.as_ref().map(|p| p.as_bytes()),
                Some(b"old.txt".as_slice())
            );
            assert_eq!(entries[0].path, b"new.txt");
        }
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_copy_detected_with_score() {
        // orig.txt is unchanged and a near-copy (one line differs, 80% similar)
        // is added. With copy detection + find_copies_harder + inexact, the new
        // file is reported as a copy with score 80 (matches `git diff -C
        // --find-copies-harder`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let orig = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\neee\n");
        let copy = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\nEEE\n");
        let left = write_tree(&mut db, &[(b"orig.txt", 0o100644, orig.clone())]);
        let right = write_tree(
            &mut db,
            &[(b"orig.txt", 0o100644, orig), (b"copy.txt", 0o100644, copy)],
        );

        let opts = RenameDetectionOptions {
            base: DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: true,
                find_copies_harder: true,
                rename_empty: true,
            },
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
        rename_limit: 0,
        };
        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");

        let copy_entry = entries
            .iter()
            .find(|e| e.path == b"copy.txt")
            .unwrap_or_else(|| panic!("no copy.txt entry: {entries:?}"));
        assert_eq!(copy_entry.status, NameStatus::Copied(80));
        assert_eq!(
            copy_entry.old_path.as_ref().map(|p| p.as_bytes()),
            Some(b"orig.txt".as_slice())
        );
        // The source remains present (copies do not consume the original).
        assert!(
            entries.iter().all(|e| e.status != NameStatus::Deleted),
            "copy must not delete the source: {entries:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_copy_skipped_over_rename_limit() {
        // git's `too_many_rename_candidates`: when the copy matrix
        // (sources × dests) exceeds `rename_limit²`, inexact copy detection is
        // skipped wholesale and the new file is reported as a plain Add — the
        // same `A` real git emits (`git diff -C --find-copies-harder -l1` warns
        // "rename detection was skipped" and shows `A copy.txt`). A `rename_limit`
        // comfortably above the matrix still detects the copy, proving the gate
        // fires *only* over-limit and not on any positive limit.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let orig = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\neee\n");
        let extra = write_blob(&mut db, b"111\n222\n333\n444\n555\n");
        let copy = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\nEEE\n");
        // Two unchanged left files → under `--find-copies-harder` both are copy
        // sources, so the matrix is 2 (sources) × 1 (dest) = 2.
        let left = write_tree(
            &mut db,
            &[
                (b"orig.txt", 0o100644, orig.clone()),
                (b"extra.txt", 0o100644, extra.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"orig.txt", 0o100644, orig),
                (b"extra.txt", 0o100644, extra),
                (b"copy.txt", 0o100644, copy),
            ],
        );

        let opts_for = |rename_limit| RenameDetectionOptions {
            base: DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: true,
                find_copies_harder: true,
                rename_empty: true,
            },
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit,
        };

        // Over limit: 2 × 1 = 2 > 1² ⇒ copy detection skipped, copy.txt is Added.
        let over = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts_for(1),
        )
        .expect("test operation should succeed");
        let copy_over = over
            .iter()
            .find(|e| e.path == b"copy.txt")
            .unwrap_or_else(|| panic!("no copy.txt entry: {over:?}"));
        assert_eq!(
            copy_over.status,
            NameStatus::Added,
            "over rename_limit, copy must degrade to a plain Add: {over:?}"
        );

        // Under limit: 2 × 1 = 2 ≤ 4² ⇒ copy still detected (score 80).
        let under = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts_for(4),
        )
        .expect("test operation should succeed");
        let copy_under = under
            .iter()
            .find(|e| e.path == b"copy.txt")
            .unwrap_or_else(|| panic!("no copy.txt entry: {under:?}"));
        assert_eq!(
            copy_under.status,
            NameStatus::Copied(80),
            "below rename_limit, copy detection is unaffected: {under:?}"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_rename_with_small_edit_scores_88() {
        // A rename that also appends a single line scores 88% (see
        // `similarity_small_append_is_88`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(
            &mut db,
            b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n",
        );
        let new = write_blob(
            &mut db,
            b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\nADDED\n",
        );
        let left = write_tree(&mut db, &[(b"src.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"dst.txt", 0o100644, new)]);

        let opts = RenameDetectionOptions::inexact(DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
        });
        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].status, NameStatus::Renamed(88));
        assert_eq!(
            entries[0].old_path.as_ref().map(|p| p.as_bytes()),
            Some(b"src.txt".as_slice())
        );
        assert_eq!(entries[0].path, b"dst.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_disabled_default_preserves_exact_only_behavior() {
        // With RenameDetectionOptions::default() (detect_inexact == false), a
        // similar-but-not-identical pair is NOT a rename — identical to the
        // legacy exact-only path. Defaults must not silently turn on inexact.
        assert!(!RenameDetectionOptions::default().detect_inexact);
        assert_eq!(
            RenameDetectionOptions::default().rename_threshold,
            DEFAULT_RENAME_THRESHOLD
        );

        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(&mut db, b"one\ntwo\nthree\nfour\nfive\n");
        let new = write_blob(&mut db, b"one\ntwo\nTHREE\nfour\nfive\n");
        let left = write_tree(&mut db, &[(b"a.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"b.txt", 0o100644, new)]);

        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            RenameDetectionOptions::default(),
        )
        .expect("test operation should succeed");
        let statuses: Vec<_> = entries.iter().map(|e| e.status).collect();
        assert!(statuses.contains(&NameStatus::Added));
        assert!(statuses.contains(&NameStatus::Deleted));
        assert!(!statuses.iter().any(|s| matches!(s, NameStatus::Renamed(_))));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    // ---- patience / histogram diff tests ------------------------------------

    /// Apply an edit script to `old` and return the reconstructed `new` bytes.
    ///
    /// Panics (test-only) if the script ever references a line out of range or
    /// claims a line is `Equal` when the corresponding `old`/`new` lines differ
    /// — that is exactly the invariant a correct LCS diff must uphold.
    fn apply_ops(old: &[DiffLine<'_>], new: &[DiffLine<'_>], ops: &[DiffOp]) -> Vec<u8> {
        let mut oi = 0usize;
        let mut ni = 0usize;
        let mut rebuilt: Vec<u8> = Vec::new();
        for op in ops {
            match *op {
                DiffOp::Equal(n) => {
                    for _ in 0..n {
                        // Equal must mean genuinely-equal lines (LCS-correct).
                        assert_eq!(old[oi], new[ni], "Equal op covered unequal lines");
                        rebuilt.extend_from_slice(old[oi].content);
                        oi += 1;
                        ni += 1;
                    }
                }
                DiffOp::Delete(n) => oi += n,
                DiffOp::Insert(n) => {
                    for _ in 0..n {
                        rebuilt.extend_from_slice(new[ni].content);
                        ni += 1;
                    }
                }
            }
        }
        // The script must consume every line of both sides exactly once.
        assert_eq!(oi, old.len(), "script did not consume all of old");
        assert_eq!(ni, new.len(), "script did not consume all of new");
        rebuilt
    }

    /// Assert that `ops` is a valid LCS-correct script: it reconstructs `new`
    /// from `old`, and consecutive ops are coalesced (no two same-kind in a row).
    fn assert_valid_script(old_bytes: &[u8], new_bytes: &[u8], ops: &[DiffOp]) {
        let old = split_lines(old_bytes);
        let new = split_lines(new_bytes);
        let rebuilt = apply_ops(&old, &new, ops);
        assert_eq!(rebuilt, new_bytes, "script did not rebuild new");
        for pair in ops.windows(2) {
            let same_kind = matches!(
                (pair[0], pair[1]),
                (DiffOp::Equal(_), DiffOp::Equal(_))
                    | (DiffOp::Delete(_), DiffOp::Delete(_))
                    | (DiffOp::Insert(_), DiffOp::Insert(_))
            );
            assert!(!same_kind, "ops not coalesced: {:?}", ops);
        }
    }

    /// Run all three real algorithms over a byte pair and assert each produces a
    /// valid, coalesced, LCS-correct script.
    fn check_all_algorithms(old_bytes: &[u8], new_bytes: &[u8]) {
        let old = split_lines(old_bytes);
        let new = split_lines(new_bytes);
        for algo in [
            DiffAlgorithm::Myers,
            DiffAlgorithm::Minimal,
            DiffAlgorithm::Patience,
            DiffAlgorithm::Histogram,
        ] {
            let ops = diff_lines_with_algorithm(&old, &new, algo);
            assert_valid_script(old_bytes, new_bytes, &ops);
        }
    }

    #[test]
    fn patience_and_histogram_match_myers_on_simple_cases() {
        // For localized single-line edits with no repeated lines, all three
        // algorithms agree with the canonical Myers script.
        let cases: &[(&[u8], &[u8], Vec<DiffOp>)] = &[
            (
                b"a\nb\nc\n",
                b"a\nx\nc\n",
                vec![
                    DiffOp::Equal(1),
                    DiffOp::Delete(1),
                    DiffOp::Insert(1),
                    DiffOp::Equal(1),
                ],
            ),
            (b"a\nb\nc\n", b"a\nb\nc\n", vec![DiffOp::Equal(3)]),
            (b"", b"a\nb\n", vec![DiffOp::Insert(2)]),
            (b"a\nb\n", b"", vec![DiffOp::Delete(2)]),
            (
                b"a\nb\nc\nd\n",
                b"a\nc\nd\n",
                vec![DiffOp::Equal(1), DiffOp::Delete(1), DiffOp::Equal(2)],
            ),
        ];
        for (old_bytes, new_bytes, expected) in cases {
            let old = split_lines(old_bytes);
            let new = split_lines(new_bytes);
            assert_eq!(&patience_diff_lines(&old, &new), expected);
            assert_eq!(&histogram_diff_lines(&old, &new), expected);
            assert_eq!(&myers_diff_lines(&old, &new), expected);
        }
    }

    #[test]
    fn patience_handles_both_empty() {
        let empty = split_lines(b"");
        assert!(patience_diff_lines(&empty, &empty).is_empty());
        assert!(histogram_diff_lines(&empty, &empty).is_empty());
    }

    #[test]
    fn patience_aligns_unique_anchors_across_moved_block() {
        // Reordering two unique blocks: patience anchors on the unique lines and
        // produces a delete-then-insert (or insert-then-delete) that still
        // reconstructs `new`. Validity is the contract; exact shape may differ
        // from Myers, so we only assert reconstruction here.
        check_all_algorithms(
            b"alpha\nbeta\ngamma\ndelta\n",
            b"gamma\ndelta\nalpha\nbeta\n",
        );
    }

    #[test]
    fn histogram_differs_from_myers_keeping_block_contiguous() {
        // A case where histogram diverges from Myers. With old = "b a" and a new
        // that surrounds an intact "b a" with inserted "b" lines, Myers splits
        // the common run into two single-line Equals (matching the leading and
        // trailing `b`/`a` separately), while histogram anchors on the rare line
        // and keeps the original two lines together as one Equal(2) block.
        let old = b"b\na\n";
        let new = b"a\nb\nb\na\nb\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);

        let myers = myers_diff_lines(&old_l, &new_l);
        let histogram = histogram_diff_lines(&old_l, &new_l);

        // All variants must reconstruct `new`.
        assert_valid_script(old, new, &myers);
        assert_valid_script(old, new, &histogram);

        // Exact, pinned shapes: Myers interleaves single-line equals; histogram
        // keeps "b\na\n" contiguous.
        assert_eq!(
            myers,
            vec![
                DiffOp::Insert(1),
                DiffOp::Equal(1),
                DiffOp::Insert(1),
                DiffOp::Equal(1),
                DiffOp::Insert(1),
            ]
        );
        assert_eq!(
            histogram,
            vec![DiffOp::Insert(2), DiffOp::Equal(2), DiffOp::Insert(1)]
        );
        // The contract the task calls out: histogram differs from Myers here.
        assert_ne!(myers, histogram);
    }

    #[test]
    fn patience_differs_from_myers_on_repeated_lines() {
        // A case where patience diverges from Myers. old = "b a", new = "a a b".
        // Myers deletes the leading `b` and appends; patience anchors on the
        // single unique-in-both line `a`... but `a` occurs twice in `new`, so it
        // is NOT unique there; patience instead falls through to its recursive
        // structure and produces the mirror script. Both reconstruct `new`.
        let old = b"b\na\n";
        let new = b"a\na\nb\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);

        let myers = myers_diff_lines(&old_l, &new_l);
        let patience = patience_diff_lines(&old_l, &new_l);

        assert_valid_script(old, new, &myers);
        assert_valid_script(old, new, &patience);

        assert_eq!(
            myers,
            vec![DiffOp::Delete(1), DiffOp::Equal(1), DiffOp::Insert(2)]
        );
        assert_eq!(
            patience,
            vec![DiffOp::Insert(2), DiffOp::Equal(1), DiffOp::Delete(1)]
        );
        assert_ne!(myers, patience);
    }

    #[test]
    fn realistic_function_insertion_all_valid() {
        // A more lifelike example: a new function is inserted ahead of an
        // existing one that shares structural lines ("}", blank line). We don't
        // pin exact shapes (they depend on trim interactions) but every
        // algorithm must produce a valid LCS-correct script.
        let old = b"int f() {\n    return 1;\n}\n";
        let new = b"int g() {\n    return 2;\n}\n\nint f() {\n    return 1;\n}\n";
        check_all_algorithms(old, new);
    }

    #[test]
    fn histogram_anchors_on_rare_line_when_no_unique_line_exists() {
        // No line is globally unique on both sides (every distinct line repeats
        // on at least one side), so plain patience would fall straight to Myers.
        // Histogram still anchors on the least-frequent shared line. We assert
        // both produce valid, reconstructing scripts.
        check_all_algorithms(b"x\nx\nmid\nx\nx\n", b"x\nmid\nx\nx\nx\n");
        check_all_algorithms(
            b"dup\ndup\nrare\ndup\ndup\n",
            b"dup\nrare\ndup\ndup\ndup\ndup\n",
        );
    }

    #[test]
    fn all_algorithms_treat_missing_final_newline_as_change() {
        // "b" (no newline) vs "b\n" is a real change for every algorithm.
        let old = split_lines(b"a\nb");
        let new = split_lines(b"a\nb\n");
        for algo in [
            DiffAlgorithm::Myers,
            DiffAlgorithm::Minimal,
            DiffAlgorithm::Patience,
            DiffAlgorithm::Histogram,
        ] {
            let ops = diff_lines_with_algorithm(&old, &new, algo);
            assert_eq!(
                ops,
                vec![DiffOp::Equal(1), DiffOp::Delete(1), DiffOp::Insert(1)],
                "algorithm {:?} mishandled missing final newline",
                algo
            );
        }
    }

    #[test]
    fn dispatcher_routes_each_variant() {
        let old = split_lines(b"a\nb\nc\n");
        let new = split_lines(b"a\nx\nc\n");
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Myers),
            myers_diff_lines(&old, &new)
        );
        // Minimal aliases Myers (the Myers search is already a minimal SES).
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Minimal),
            myers_diff_lines(&old, &new)
        );
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Patience),
            patience_diff_lines(&old, &new)
        );
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Histogram),
            histogram_diff_lines(&old, &new)
        );
    }

    #[test]
    fn patience_recurses_into_gaps_between_anchors() {
        // Unique anchors `head`/`tail` bracket an inner edit; patience must
        // recurse into the middle gap and diff `mid1`->`MID` there.
        let old = b"head\nmid1\nmid2\ntail\n";
        let new = b"head\nMID\nmid2\ntail\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);
        let ops = patience_diff_lines(&old_l, &new_l);
        assert_eq!(
            ops,
            vec![
                DiffOp::Equal(1),
                DiffOp::Delete(1),
                DiffOp::Insert(1),
                DiffOp::Equal(2),
            ]
        );
        assert_valid_script(old, new, &ops);
    }

    #[test]
    fn patience_falls_back_to_myers_with_no_unique_lines() {
        // Every line is duplicated within its own side, so there are no
        // unique-in-both anchors; patience must defer to Myers but still return
        // a valid script.
        let old = b"a\na\nb\nb\n";
        let new = b"a\na\na\nb\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);
        let ops = patience_diff_lines(&old_l, &new_l);
        // The contract for the fallback path is validity, not minimality: after
        // the greedy prefix/suffix trim (which git's patience does too) the
        // leftover block is handed to Myers, and the whole script must still
        // reconstruct `new`.
        assert_valid_script(old, new, &ops);
    }

    #[test]
    fn algorithms_agree_with_myers_when_all_lines_distinct() {
        // When every line is globally unique, patience's anchor set is the full
        // LCS, so patience and histogram must produce exactly the Myers script.
        let cases: &[(&[u8], &[u8])] = &[
            (b"a\nb\nc\nd\ne\n", b"a\nc\nd\nf\ne\n"),
            (b"1\n2\n3\n4\n5\n6\n", b"1\n3\n2\n4\n6\n5\n"),
            (b"q\nw\ne\nr\nt\ny\n", b"q\nw\nx\nr\nt\nz\n"),
        ];
        for (old_bytes, new_bytes) in cases {
            let old = split_lines(old_bytes);
            let new = split_lines(new_bytes);
            let myers = myers_diff_lines(&old, &new);
            assert_eq!(
                patience_diff_lines(&old, &new),
                myers,
                "patience must equal Myers when all lines are distinct: {:?}",
                old_bytes
            );
            assert_eq!(
                histogram_diff_lines(&old, &new),
                myers,
                "histogram must equal Myers when all lines are distinct: {:?}",
                old_bytes
            );
        }
    }

    #[test]
    fn fuzz_all_algorithms_reconstruct_new() {
        // A small deterministic LCG drives many random small inputs over a tiny
        // alphabet (so lines repeat and exercise the anchor/fallback paths).
        // Every algorithm must produce a valid LCS-correct script for each pair.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let alphabet = [b"a\n", b"b\n", b"c\n", b"d\n"];
        let build = |rng: &mut dyn FnMut() -> u32| -> Vec<u8> {
            let len = (rng() % 9) as usize; // 0..=8 lines
            let mut buf = Vec::new();
            for _ in 0..len {
                let pick = (rng() % alphabet.len() as u32) as usize;
                buf.extend_from_slice(alphabet[pick]);
            }
            // Occasionally drop the trailing newline to exercise that path.
            if !buf.is_empty() && rng().is_multiple_of(4) {
                buf.pop();
            }
            buf
        };
        for _ in 0..400 {
            let old_bytes = build(&mut next);
            let new_bytes = build(&mut next);
            check_all_algorithms(&old_bytes, &new_bytes);
        }
    }

    #[test]
    fn exhaustive_small_inputs_all_algorithms_reconstruct() {
        // Brute force over a 3-symbol alphabet up to 5 lines per side: every
        // algorithm must produce a valid LCS-correct script for *every* pair.
        // This is the strongest correctness net for the recursion/fallback
        // paths; apply_ops asserts both reconstruction and Equal-correctness.
        let syms = [b"a\n".to_vec(), b"b\n".to_vec(), b"c\n".to_vec()];
        let make = |n: usize, mut code: usize| -> Vec<u8> {
            let mut v = Vec::new();
            for _ in 0..n {
                v.extend_from_slice(&syms[code % 3]);
                code /= 3;
            }
            v
        };
        for la in 0..=5usize {
            for lb in 0..=5usize {
                for ca in 0..3usize.pow(la as u32) {
                    for cb in 0..3usize.pow(lb as u32) {
                        let ob = make(la, ca);
                        let nb = make(lb, cb);
                        let ol = split_lines(&ob);
                        let nl = split_lines(&nb);
                        assert_eq!(apply_ops(&ol, &nl, &myers_diff_lines(&ol, &nl)), nb);
                        assert_eq!(apply_ops(&ol, &nl, &patience_diff_lines(&ol, &nl)), nb);
                        assert_eq!(apply_ops(&ol, &nl, &histogram_diff_lines(&ol, &nl)), nb);
                    }
                }
            }
        }
    }

    #[test]
    fn fuzz_distinct_lines_patience_histogram_equal_myers() {
        // When inputs are permutations/subsequences of globally-unique lines,
        // patience and histogram must match Myers exactly. We generate sequences
        // of distinct tokens to guarantee global uniqueness on both sides.
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for _ in 0..200 {
            // Random subset+order of tokens "0\n".."9\n" for each side; tokens
            // are globally unique, so any common line is unique in both.
            let pick_subseq = |rng: &mut dyn FnMut() -> u32| -> Vec<u8> {
                let mut buf = Vec::new();
                for t in 0..10u32 {
                    if rng().is_multiple_of(2) {
                        buf.extend_from_slice(format!("{t}\n").as_bytes());
                    }
                }
                buf
            };
            let old_bytes = pick_subseq(&mut next);
            let new_bytes = pick_subseq(&mut next);
            let old = split_lines(&old_bytes);
            let new = split_lines(&new_bytes);
            let myers = myers_diff_lines(&old, &new);
            assert_eq!(patience_diff_lines(&old, &new), myers);
            assert_eq!(histogram_diff_lines(&old, &new), myers);
        }
    }

    // ===================================================================
    // Subtree-skip-by-OID tree-diff optimization: the pruned simultaneous
    // walk (`changed_tree_entries`) must produce byte-identical name-status
    // output to the legacy "flatten both sides fully" walk
    // (`collect_full_tree_pair`) on every representative diff shape.
    // ===================================================================

    /// Format a name-status result into stable, comparable lines.
    fn status_lines(entries: &[NameStatusEntry]) -> Vec<String> {
        entries.iter().map(|entry| entry.line()).collect()
    }

    /// Assert the pruned walk and the full flatten agree, both as raw map diffs
    /// and through the public tree-diff entry points, for the given options.
    fn assert_tree_diff_matches_full(
        db: &FileObjectDatabase,
        left: &ObjectId,
        right: &ObjectId,
        options: DiffNameStatusOptions,
    ) {
        // Reference ("old") behaviour: fully flatten both trees, then diff.
        let (full_left, full_right) = collect_full_tree_pair(db, ObjectFormat::Sha1, left, right)
            .expect("test operation should succeed");
        let reference = diff_name_status_maps(
            &full_left,
            &full_right,
            full_left.keys().chain(full_right.keys()),
            options,
        )
        .expect("test operation should succeed");

        // Optimized ("new") behaviour: prune identical subtrees, then diff.
        let (pruned_left, pruned_right) = changed_tree_entries(db, ObjectFormat::Sha1, left, right)
            .expect("test operation should succeed");
        let pruned = diff_name_status_maps(
            &pruned_left,
            &pruned_right,
            pruned_left.keys().chain(pruned_right.keys()),
            options,
        )
        .expect("test operation should succeed");

        assert_eq!(
            status_lines(&reference),
            status_lines(&pruned),
            "pruned map diff diverged from full map diff for {options:?}"
        );

        // And the public entry point (which itself selects pruned vs full) must
        // match the reference too.
        let public =
            diff_name_status_trees_with_options(db, ObjectFormat::Sha1, left, right, options)
                .expect("test operation should succeed");
        assert_eq!(
            status_lines(&reference),
            status_lines(&public),
            "public tree diff diverged from full map diff for {options:?}"
        );

        // The pruned maps must be a subset of the full maps and must contain
        // exactly the paths that actually changed (no identical entries leak in,
        // no changed entries get dropped).
        for (path, tracked) in &pruned_left {
            assert_eq!(
                full_left.get(path),
                Some(tracked),
                "pruned left entry not present (or differs) in full left map: {:?}",
                String::from_utf8_lossy(path)
            );
        }
        for (path, tracked) in &pruned_right {
            assert_eq!(
                full_right.get(path),
                Some(tracked),
                "pruned right entry not present (or differs) in full right map: {:?}",
                String::from_utf8_lossy(path)
            );
        }
        // Every path the full diff reports as changed must survive pruning on
        // whichever side(s) it lives.
        for entry in &reference {
            let path = entry.path.as_bytes();
            match entry.status {
                NameStatus::Added => assert!(
                    pruned_right.contains_key(path),
                    "added path dropped by pruning: {:?}",
                    String::from_utf8_lossy(path)
                ),
                NameStatus::Deleted => assert!(
                    pruned_left.contains_key(path),
                    "deleted path dropped by pruning: {:?}",
                    String::from_utf8_lossy(path)
                ),
                NameStatus::Modified => {
                    assert!(
                        pruned_left.contains_key(path) && pruned_right.contains_key(path),
                        "modified path dropped by pruning: {:?}",
                        String::from_utf8_lossy(path)
                    );
                }
                _ => {}
            }
        }
    }

    /// Run the equivalence assertion across the option matrix that the pruned
    /// path serves (everything except `--find-copies-harder`, which uses the
    /// full maps and is checked separately).
    fn assert_tree_diff_matches_full_all_modes(
        db: &FileObjectDatabase,
        left: &ObjectId,
        right: &ObjectId,
    ) {
        for detect_renames in [false, true] {
            for detect_copies in [false, true] {
                let options = DiffNameStatusOptions {
                    detect_renames,
                    detect_copies,
                    find_copies_harder: false,
                    rename_empty: true,
                };
                assert_tree_diff_matches_full(db, left, right, options);
            }
        }
    }

    /// Build a DB pre-seeded with a fixed bank of blobs for the structural tests.
    fn structural_db() -> (PathBuf, FileObjectDatabase) {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        (root, db)
    }

    #[test]
    fn pruned_walk_skips_identical_subtree_and_matches_full() {
        // A large shared subtree (`shared/`) is byte-identical on both sides; the
        // only change lives in `app/`. The pruned walk must skip `shared/`
        // entirely yet still produce the exact same diff as flattening it.
        let (root, mut db) = structural_db();

        // shared/ — identical on both sides, several nested files.
        let s1 = write_blob(&mut db, b"shared one\n");
        let s2 = write_blob(&mut db, b"shared two\n");
        let s3 = write_blob(&mut db, b"deep nested\n");
        let shared_inner = write_tree(&mut db, &[(b"c.txt", 0o100644, s3.clone())]);
        let shared = write_tree(
            &mut db,
            &[
                (b"a.txt", 0o100644, s1.clone()),
                (b"b.txt", 0o100644, s2.clone()),
                (b"inner", 0o040000, shared_inner.clone()),
            ],
        );

        // app/ — one file modified between sides.
        let app_old = write_blob(&mut db, b"version 1\n");
        let app_new = write_blob(&mut db, b"version 2\n");
        let app_left = write_tree(&mut db, &[(b"main.rs", 0o100644, app_old)]);
        let app_right = write_tree(&mut db, &[(b"main.rs", 0o100644, app_new)]);

        let left = write_tree(
            &mut db,
            &[
                (b"app", 0o040000, app_left),
                (b"shared", 0o040000, shared.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[(b"app", 0o040000, app_right), (b"shared", 0o040000, shared)],
        );

        // Sanity: the only change is the nested app/main.rs modification.
        let (pruned_left, pruned_right) =
            changed_tree_entries(&db, ObjectFormat::Sha1, &left, &right)
                .expect("test operation should succeed");
        assert_eq!(
            pruned_left.keys().collect::<Vec<_>>(),
            vec![&b"app/main.rs".to_vec()],
            "pruning should leave only the changed path on the left"
        );
        assert_eq!(
            pruned_right.keys().collect::<Vec<_>>(),
            vec![&b"app/main.rs".to_vec()],
            "pruning should leave only the changed path on the right"
        );
        assert!(
            !pruned_left.contains_key(b"shared/a.txt".as_slice()),
            "identical shared subtree must not appear in pruned maps"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_add_delete_modify_nested() {
        // Mixed shape: a top-level add, a top-level delete, a nested modify, and
        // an untouched nested subtree that must be skipped.
        let (root, mut db) = structural_db();

        let keep = write_blob(&mut db, b"unchanged\n");
        let untouched_dir = write_tree(&mut db, &[(b"keep.txt", 0o100644, keep.clone())]);

        let nested_old = write_blob(&mut db, b"nested old\n");
        let nested_new = write_blob(&mut db, b"nested new\n");
        let dir_left = write_tree(
            &mut db,
            &[
                (b"changed.txt", 0o100644, nested_old),
                (b"stable.txt", 0o100644, keep.clone()),
            ],
        );
        let dir_right = write_tree(
            &mut db,
            &[
                (b"changed.txt", 0o100644, nested_new),
                (b"stable.txt", 0o100644, keep.clone()),
            ],
        );

        let only_left = write_blob(&mut db, b"will be deleted\n");
        let only_right = write_blob(&mut db, b"freshly added\n");

        let left = write_tree(
            &mut db,
            &[
                (b"dir", 0o040000, dir_left),
                (b"gone.txt", 0o100644, only_left),
                (b"untouched", 0o040000, untouched_dir.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"dir", 0o040000, dir_right),
                (b"new.txt", 0o100644, only_right),
                (b"untouched", 0o040000, untouched_dir),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec![
                "M\tdir/changed.txt".to_string(),
                "D\tgone.txt".to_string(),
                "A\tnew.txt".to_string(),
            ],
            "unexpected raw status for mixed nested diff"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_rename_across_dirs() {
        // An exact rename (same blob oid) moving between directories. Rename
        // detection runs on the pruned add/delete set and must match the full
        // walk's result exactly.
        let (root, mut db) = structural_db();

        let moved = write_blob(&mut db, b"i get moved across directories\n");
        let companion = write_blob(&mut db, b"i stay put\n");
        let stable_dir = write_tree(&mut db, &[(b"keep.txt", 0o100644, companion.clone())]);

        let src_dir = write_tree(&mut db, &[(b"file.txt", 0o100644, moved.clone())]);
        let dst_dir = write_tree(&mut db, &[(b"renamed.txt", 0o100644, moved.clone())]);

        let left = write_tree(
            &mut db,
            &[
                (b"src", 0o040000, src_dir),
                (b"stable", 0o040000, stable_dir.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"dst", 0o040000, dst_dir),
                (b"stable", 0o040000, stable_dir),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec!["R100\tsrc/file.txt\tdst/renamed.txt".to_string()],
            "rename across dirs should be detected on pruned set"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_binary_and_mode_change() {
        // Binary blob modification plus an executable-bit (mode) change on an
        // otherwise-identical blob. Mode-only changes must still register as a
        // Modify (the pruned walk compares mode + oid, like the full map).
        let (root, mut db) = structural_db();

        let bin_old = write_blob(&mut db, &[0u8, 159, 146, 150, 0, 255, 1, 2, 3]);
        let bin_new = write_blob(&mut db, &[0u8, 159, 146, 150, 0, 254, 9, 8, 7]);
        let script = write_blob(&mut db, b"#!/bin/sh\necho hi\n");

        let left = write_tree(
            &mut db,
            &[
                (b"image.bin", 0o100644, bin_old),
                (b"run.sh", 0o100644, script.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"image.bin", 0o100644, bin_new),
                // same blob oid, executable bit flipped on
                (b"run.sh", 0o100755, script),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec!["M\timage.bin".to_string(), "M\trun.sh".to_string()],
            "binary edit and mode-only change should both be Modify"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_dir_replaced_by_file() {
        // A name that is a directory on the left and a regular file on the right
        // (and vice versa). The flattened paths differ (`thing/...` vs `thing`),
        // so the pruned walk must treat them as unrelated add/delete pairs,
        // exactly as the full flatten does.
        let (root, mut db) = structural_db();

        let inner_a = write_blob(&mut db, b"inner a\n");
        let inner_b = write_blob(&mut db, b"inner b\n");
        let thing_dir = write_tree(
            &mut db,
            &[(b"a.txt", 0o100644, inner_a), (b"b.txt", 0o100644, inner_b)],
        );
        let thing_file = write_blob(&mut db, b"now i am a file\n");

        // other/ is a file on the left, a directory on the right.
        let other_file = write_blob(&mut db, b"i was a file\n");
        let other_inner = write_blob(&mut db, b"now nested\n");
        let other_dir = write_tree(&mut db, &[(b"x.txt", 0o100644, other_inner)]);

        let left = write_tree(
            &mut db,
            &[
                (b"other", 0o100644, other_file),
                (b"thing", 0o040000, thing_dir),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"other", 0o040000, other_dir),
                (b"thing", 0o100644, thing_file),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec![
                "D\tother".to_string(),
                "A\tother/x.txt".to_string(),
                "A\tthing".to_string(),
                "D\tthing/a.txt".to_string(),
                "D\tthing/b.txt".to_string(),
            ],
            "dir<->file swap should flatten to independent adds/deletes"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_identical_trees() {
        // Two identical root trees: zero changes, and the root must be skipped
        // without reading anything below it.
        let (root, mut db) = structural_db();

        let blob = write_blob(&mut db, b"same\n");
        let sub = write_tree(&mut db, &[(b"f.txt", 0o100644, blob.clone())]);
        let tree = write_tree(
            &mut db,
            &[(b"sub", 0o040000, sub), (b"top.txt", 0o100644, blob)],
        );

        let (pruned_left, pruned_right) =
            changed_tree_entries(&db, ObjectFormat::Sha1, &tree, &tree)
                .expect("test operation should succeed");
        assert!(
            pruned_left.is_empty() && pruned_right.is_empty(),
            "identical trees must produce no changed entries"
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &tree,
            &tree,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");
        assert!(entries.is_empty(), "identical trees must produce no diff");

        assert_tree_diff_matches_full_all_modes(&db, &tree, &tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn find_copies_harder_uses_full_left_map_and_finds_unchanged_source() {
        // `--find-copies-harder` must still see an *unchanged* file as a copy
        // source. This is the case where the public entry point deliberately
        // falls back to the full flatten; verify the full-map fallback both
        // behaves correctly and matches a direct full-map computation.
        let (root, mut db) = structural_db();

        // `template.txt` is unchanged between sides (lives in an untouched
        // subtree), and `copy.txt` is added on the right with the same content.
        let template = write_blob(&mut db, b"reusable boilerplate content\n");
        let lib_dir = write_tree(&mut db, &[(b"template.txt", 0o100644, template.clone())]);

        let trigger_old = write_blob(&mut db, b"trigger old\n");
        let trigger_new = write_blob(&mut db, b"trigger new\n");

        let left = write_tree(
            &mut db,
            &[
                (b"lib", 0o040000, lib_dir.clone()),
                (b"trigger.txt", 0o100644, trigger_old),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"copy.txt", 0o100644, template.clone()),
                (b"lib", 0o040000, lib_dir),
                (b"trigger.txt", 0o100644, trigger_new),
            ],
        );

        let options = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: true,
            find_copies_harder: true,
            rename_empty: true,
        };

        // Reference via the full flatten (the old algorithm).
        let (full_left, full_right) =
            collect_full_tree_pair(&db, ObjectFormat::Sha1, &left, &right)
                .expect("test operation should succeed");
        let reference = diff_name_status_maps(
            &full_left,
            &full_right,
            full_left.keys().chain(full_right.keys()),
            options,
        )
        .expect("test operation should succeed");

        let public =
            diff_name_status_trees_with_options(&db, ObjectFormat::Sha1, &left, &right, options)
                .expect("test operation should succeed");
        assert_eq!(
            status_lines(&reference),
            status_lines(&public),
            "find-copies-harder public diff must match full-map reference"
        );
        // The copy must be detected from the unchanged template source.
        assert!(
            public
                .iter()
                .any(|entry| matches!(entry.status, NameStatus::Copied(_))
                    && entry.old_path.as_ref().map(|p| p.as_bytes())
                        == Some(b"lib/template.txt".as_slice())
                    && entry.path == b"copy.txt"),
            "copy from unchanged source must be found with find_copies_harder: {public:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_with_inexact_rename_options() {
        // Exercise the rename-options entry point (which also selects pruned vs
        // full) with inexact detection enabled, across an untouched subtree.
        let (root, mut db) = structural_db();

        let untouched = write_blob(&mut db, b"untouched file\n");
        let untouched_dir = write_tree(&mut db, &[(b"u.txt", 0o100644, untouched.clone())]);

        // a.txt -> b.txt with one changed line (a 75% inexact rename).
        let old = write_blob(&mut db, b"one\ntwo\nthree\nfour\nfive\n");
        let new = write_blob(&mut db, b"one\ntwo\nTHREE\nfour\nfive\n");

        let left = write_tree(
            &mut db,
            &[
                (b"a.txt", 0o100644, old),
                (b"keep", 0o040000, untouched_dir.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"b.txt", 0o100644, new),
                (b"keep", 0o040000, untouched_dir),
            ],
        );

        let options = RenameDetectionOptions {
            base: DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
            },
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
        rename_limit: 0,
        };

        // Reference: full flatten + same detection.
        let (full_left, full_right) =
            collect_full_tree_pair(&db, ObjectFormat::Sha1, &left, &right)
                .expect("test operation should succeed");
        let reference = diff_name_status_maps_with_renames(
            &full_left,
            &full_right,
            full_left.keys().chain(full_right.keys()),
            options,
            |oid| read_blob_bytes(&db, oid),
        )
        .expect("test operation should succeed");

        let public = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            options,
        )
        .expect("test operation should succeed");

        assert_eq!(
            status_lines(&reference),
            status_lines(&public),
            "inexact rename via pruned walk must match full-map reference"
        );
        assert_eq!(
            status_lines(&public),
            vec!["R075\ta.txt\tb.txt".to_string()],
            "expected a 75% inexact rename"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }
}
