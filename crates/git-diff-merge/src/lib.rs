use git_core::{object_id_for_bytes, GitError, ObjectFormat, ObjectId, RepoPath, Result};
use git_index::Index;
use git_object::{Commit, EncodedObject, ObjectType, Tree};
use git_odb::{FileObjectDatabase, ObjectReader};
use git_refs::{FileRefStore, RefTarget};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

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
fn emit_trivial_range(
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
    out: &mut Vec<DiffOp>,
) -> bool {
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
    if lis.is_empty() {
        None
    } else {
        Some(lis)
    }
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
            let better = run_count < best_count
                || (run_count == best_count && len > best_len);
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
}

impl Default for MergeBlobOptions<'_> {
    fn default() -> Self {
        Self {
            ours_label: "ours",
            theirs_label: "theirs",
            base_label: "base",
            style: ConflictStyle::Merge,
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
                writer.emit_conflict(our_region, base_region, their_region);
            }
        }
    }
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
    Renamed(u8),
    Copied(u8),
}

impl NameStatus {
    pub const fn code(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Renamed(_) => 'R',
            Self::Copied(_) => 'C',
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusEntry {
    pub status: NameStatus,
    pub path: Vec<u8>,
    pub old_path: Option<Vec<u8>>,
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
                String::from_utf8_lossy(old_path),
                String::from_utf8_lossy(&self.path)
            )
        } else {
            format!(
                "{}\t{}",
                self.status.label(),
                String::from_utf8_lossy(&self.path)
            )
        }
    }
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
}

impl Default for RenameDetectionOptions {
    fn default() -> Self {
        Self {
            base: DiffNameStatusOptions::default(),
            detect_inexact: false,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
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
    let index = read_index_entries(git_dir, format)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    let changes =
        diff_name_status_maps(&head, &worktree, head.keys().chain(index.keys()), options)?;
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
    let index = read_index_entries(git_dir, format)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    let cache = worktree_blob_cache(worktree_root, git_dir, format)?;
    let changes = diff_name_status_maps_with_renames(
        &head,
        &worktree,
        head.keys().chain(index.keys()),
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
    let index = read_index_entries(git_dir, format)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    let changes =
        diff_name_status_maps(&tree, &worktree, tree.keys().chain(index.keys()), options)?;
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
    let index = read_index_entries(git_dir, format)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    let cache = worktree_blob_cache(worktree_root, git_dir, format)?;
    let changes = diff_name_status_maps_with_renames(
        &tree,
        &worktree,
        tree.keys().chain(index.keys()),
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
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index = read_index_entries(git_dir, format)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    diff_name_status_maps(&index, &worktree, index.keys(), options)
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
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let index = read_index_entries(git_dir, format)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    let cache = worktree_blob_cache(worktree_root, git_dir, format)?;
    diff_name_status_maps_with_renames(&index, &worktree, index.keys(), options, |oid| {
        cache_or_odb_blob(&cache, &db, oid)
    })
}

pub fn diff_name_status_trees_with_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let mut left_entries = BTreeMap::new();
    collect_tree_entries(db, format, left_tree, Vec::new(), &mut left_entries)?;
    let mut right_entries = BTreeMap::new();
    collect_tree_entries(db, format, right_tree, Vec::new(), &mut right_entries)?;
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
    let mut left_entries = BTreeMap::new();
    collect_tree_entries(db, format, left_tree, Vec::new(), &mut left_entries)?;
    let mut right_entries = BTreeMap::new();
    collect_tree_entries(db, format, right_tree, Vec::new(), &mut right_entries)?;
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
        Ok(object) if object.object_type == ObjectType::Blob => Some(object.body),
        _ => None,
    }
}

/// Build the raw per-path add/delete/modify change list (before any rename or
/// copy detection) from the two entry maps and the candidate path set.
fn raw_name_status_changes<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
) -> Vec<NameStatusEntry> {
    let mut paths = BTreeSet::new();
    paths.extend(candidate_paths.cloned());

    let mut changes = Vec::new();
    for path in paths {
        let left = left_entries.get(&path);
        let right = right_entries.get(&path);
        let status = match (left, right) {
            (None, Some(_)) => Some(NameStatus::Added),
            (Some(_), None) => Some(NameStatus::Deleted),
            (Some(left), Some(right)) if left != right => Some(NameStatus::Modified),
            _ => None,
        };
        if let Some(status) = status {
            changes.push(NameStatusEntry {
                status,
                path,
                old_path: None,
                old_mode: left.map(|entry| entry.mode),
                new_mode: right.map(|entry| entry.mode),
                old_oid: left.map(|entry| entry.oid.clone()),
                new_oid: right.map(|entry| entry.oid.clone()),
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
    let mut changes = raw_name_status_changes(left_entries, right_entries, candidate_paths);
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
    let base = options.base;
    let mut changes = raw_name_status_changes(left_entries, right_entries, candidate_paths);
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
    let deleted = changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Deleted)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let mut consumed = BTreeSet::new();
    let mut renamed_old_paths = BTreeSet::new();
    let mut result = Vec::new();

    for old_path in deleted {
        let Some(left) = left_entries.get(&old_path) else {
            continue;
        };
        if let Some((idx, new_path)) = added.iter().find(|(idx, new_path)| {
            !consumed.contains(idx)
                && right_entries.get(new_path).is_some_and(|right| {
                    right.oid == left.oid && (rename_empty || !is_empty_blob_oid(&left.oid))
                })
        }) {
            consumed.insert(*idx);
            renamed_old_paths.insert(old_path.clone());
            let right = right_entries.get(new_path);
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(100),
                path: new_path.clone(),
                old_path: Some(old_path),
                old_mode: Some(left.mode),
                new_mode: right.map(|entry| entry.mode),
                old_oid: Some(left.oid.clone()),
                new_oid: right.map(|entry| entry.oid.clone()),
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
        .filter(|path| find_copies_harder || changed_sources.contains(*path))
        .cloned()
        .collect::<Vec<_>>();

    let mut result = Vec::new();
    for entry in changes {
        if entry.status != NameStatus::Added {
            result.push(entry);
            continue;
        }
        let Some(right) = right_entries.get(&entry.path) else {
            result.push(entry);
            continue;
        };
        if let Some(old_path) = source_paths.iter().find(|old_path| {
            old_path.as_slice() != entry.path.as_slice()
                && left_entries.get(*old_path).is_some_and(|left| {
                    left.oid == right.oid && (rename_empty || !is_empty_blob_oid(&left.oid))
                })
        }) {
            result.push(NameStatusEntry {
                status: NameStatus::Copied(100),
                path: entry.path,
                old_path: Some(old_path.clone()),
                old_mode: left_entries.get(old_path).map(|entry| entry.mode),
                new_mode: entry.new_mode,
                old_oid: left_entries.get(old_path).map(|entry| entry.oid.clone()),
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
#[derive(Debug, Clone, Default)]
struct RenameSourceMeta {
    path: Vec<u8>,
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

    // Score every (delete, add) pair; keep only those meeting the threshold.
    let mut pairs: Vec<ScoredPair> = Vec::new();
    for (si, (_, src_bytes)) in deleted.iter().enumerate() {
        for (di, (_, dst_bytes)) in added.iter().enumerate() {
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

    // Greedily assign each source/destination once.
    let mut src_used = vec![false; deleted.len()];
    let mut dst_used = vec![false; added.len()];
    // destination changes-index -> (source changes-index, score).
    let mut rename_of: BTreeMap<usize, (usize, u8)> = BTreeMap::new();
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
                    oid: src.old_oid.clone(),
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
            let meta = source_meta.get(src_idx).cloned().unwrap_or_default();
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
        if !(options.base.find_copies_harder || changed_sources.contains(path)) {
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
            if src_path.as_slice() == entry.path.as_slice() {
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
                old_path: Some(src_path.clone()),
                old_mode: Some(src_tracked.mode),
                new_mode: entry.new_mode,
                old_oid: Some(src_tracked.oid.clone()),
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

    let src = span_hash_counts(a);
    let dst = span_hash_counts(b);
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

/// Break `data` into spans and return, per span hash, the total number of bytes
/// covered by spans with that hash. Spans end at a newline (inclusive) or once
/// they reach [`MAX_SPAN_BYTES`] bytes — exactly git's `hash_chars()` loop.
///
/// The returned map is `hash -> total_span_bytes`. Summing all values yields
/// `data.len()`, so the byte accounting is exact.
fn span_hash_counts(data: &[u8]) -> BTreeMap<u64, usize> {
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
    if matches!(entry.status, NameStatus::Copied(_)) {
        &entry.path
    } else {
        entry.old_path.as_deref().unwrap_or(&entry.path)
    }
}

fn mark_unstaged_worktree_oids_unresolved(
    changes: Vec<NameStatusEntry>,
    index_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    worktree_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
) -> Vec<NameStatusEntry> {
    changes
        .into_iter()
        .map(|mut entry| {
            let worktree_entry = worktree_entries.get(&entry.path);
            if worktree_entry != index_entries.get(&entry.path) {
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

fn read_index_entries(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let index_path = git_dir.join("index");
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    Ok(index
        .entries
        .into_iter()
        .map(|entry| {
            (
                entry.path,
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            )
        })
        .collect())
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
    let commit = Commit::parse(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, &commit.tree, Vec::new(), &mut entries)?;
    Ok(entries)
}

fn collect_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let tree = Tree::parse(format, &object.body)?;
    for entry in tree.entries {
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        if entry.mode == 0o040000 {
            collect_tree_entries(db, format, &entry.oid, path, entries)?;
        } else {
            entries.insert(
                path,
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            );
        }
    }
    Ok(())
}

fn worktree_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    collect_worktree_entries(worktree_root, git_dir, worktree_root, format, &mut entries)?;
    Ok(entries)
}

fn collect_worktree_entries(
    root: &Path,
    git_dir: &Path,
    dir: &Path,
    format: ObjectFormat,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    if dir == git_dir {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == git_dir {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_worktree_entries(root, git_dir, &path, format, entries)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            let git_path = git_path_bytes(relative)?;
            let body = fs::read(&path)?;
            let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
            entries.insert(
                git_path,
                TrackedEntry {
                    mode: file_mode(&metadata),
                    oid,
                },
            );
        }
    }
    Ok(())
}

/// Build an `oid -> bytes` cache of every regular file under `worktree_root`,
/// keyed by the blob oid (so it lines up with the oids in the worktree
/// `TrackedEntry` map). Used to supply worktree blob content to similarity
/// scoring, since freshly-edited worktree files are generally not yet written to
/// the object database.
fn worktree_blob_cache(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<HashMap<ObjectId, Vec<u8>>> {
    let mut cache = HashMap::new();
    collect_worktree_blob_cache(git_dir, worktree_root, format, &mut cache)?;
    Ok(cache)
}

/// Recursively read every regular file under `dir` into `cache` keyed by blob
/// oid. The cache is oid-keyed (not path-keyed), so unlike
/// [`collect_worktree_entries`] no worktree-root rebasing is needed.
fn collect_worktree_blob_cache(
    git_dir: &Path,
    dir: &Path,
    format: ObjectFormat,
    cache: &mut HashMap<ObjectId, Vec<u8>>,
) -> Result<()> {
    if dir == git_dir {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == git_dir {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_worktree_blob_cache(git_dir, &path, format, cache)?;
        } else if metadata.is_file() {
            let body = fs::read(&path)?;
            let oid = EncodedObject::new(ObjectType::Blob, body.clone()).object_id(format)?;
            cache.entry(oid).or_insert(body);
        }
    }
    Ok(())
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

fn git_path_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(GitError::InvalidPath(format!(
            "invalid diff path {}",
            path.display()
        )));
    }
    Ok(path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes())
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
}

/// Outcome of applying a [`FilePatch`] to a base buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The patch applied cleanly; carries the resulting file bytes.
    Applied(Vec<u8>),
    /// At least one hunk's context/deleted lines did not match the base.
    Rejected,
}

/// Maximum number of lines git-style hunk application will search away from the
/// recorded position (in either direction) before giving up.
const MAX_HUNK_OFFSET: usize = 1_000;

/// Parse a unified/git diff into one [`FilePatch`] per file it touches.
///
/// The parser is intentionally lenient about leading commentary (commit
/// messages, `index <oid>..<oid>` lines, etc.): anything that is not part of a
/// recognised header or hunk body is skipped. It errors only on structurally
/// invalid hunks (bad `@@` headers, body lines that overflow the declared hunk
/// counts, or hunk bodies that appear with no preceding file header).
pub fn parse_unified_patch(input: &[u8]) -> Result<Vec<FilePatch>> {
    let lines = split_patch_lines(input);
    let mut parser = PatchParser {
        lines: &lines,
        index: 0,
    };
    parser.parse()
}

/// Apply a single-file patch to `base`, returning the patched bytes.
///
/// Each hunk's context and deleted lines must match `base` exactly. Application
/// first tries the line recorded in the hunk header and, if that does not
/// match, searches outward (the same offset-tolerant behaviour git uses) up to
/// [`MAX_HUNK_OFFSET`] lines in each direction. If any hunk cannot be located,
/// the whole patch is [`ApplyOutcome::Rejected`] and `base` is left untouched.
///
/// New-file patches (empty/ignored base) and the no-final-newline case are
/// handled byte-accurately.
pub fn apply_file_patch(base: &[u8], patch: &FilePatch) -> ApplyOutcome {
    // A pure deletion with no hunks yields an empty file.
    if patch.is_delete && patch.hunks.is_empty() {
        return ApplyOutcome::Applied(Vec::new());
    }
    // A new file: the only sensible base is empty; ignore whatever was passed
    // and build the result from the inserted lines.
    let base_for_match: &[u8] = if patch.is_new { b"" } else { base };

    let base_lines = split_blob_lines(base_for_match);

    // We walk the base line list, copying untouched lines and splicing hunks.
    let mut result: Vec<Line> = Vec::new();
    // Index into `base_lines` of the next line we have not yet emitted.
    let mut cursor: usize = 0;
    // Running offset applied to subsequent hunk positions (git carries the
    // offset from earlier hunks forward as a hint).
    let mut running_offset: isize = 0;

    for hunk in &patch.hunks {
        let located = match locate_hunk(&base_lines, hunk, cursor, running_offset) {
            Some(pos) => pos,
            None => return ApplyOutcome::Rejected,
        };
        if located < cursor {
            // Overlapping/out-of-order application is not representable.
            return ApplyOutcome::Rejected;
        }
        // Copy untouched lines preceding this hunk.
        for line in &base_lines[cursor..located] {
            result.push(line.clone());
        }
        // Emit the hunk: context + inserts replace context + deletes.
        let mut consumed = 0usize; // old-side lines consumed from base
        for hl in &hunk.lines {
            match hl {
                HunkLine::Context(bytes) => {
                    result.push(Line {
                        content: bytes.clone(),
                        no_newline: false,
                    });
                    consumed += 1;
                }
                HunkLine::Delete(_) => {
                    consumed += 1;
                }
                HunkLine::Insert(bytes) => {
                    result.push(Line {
                        content: bytes.clone(),
                        no_newline: false,
                    });
                }
            }
        }
        // Apply the no-newline flags to the last emitted new-side line and the
        // last consumed old-side line as appropriate.
        let new_end = located + consumed;
        if hunk.new_no_newline
            && let Some(last) = result.last_mut()
        {
            last.no_newline = true;
        }
        cursor = new_end;
        // Update running offset by how far the located position drifted from the
        // naive expectation, so later hunks search around the adjusted spot.
        let expected = expected_position(hunk, running_offset);
        running_offset += located as isize - expected;
        let _ = hunk.old_no_newline; // honoured implicitly via context matching
    }

    // Copy any trailing untouched lines. These carry their original
    // newline-state (including a no-final-newline marker on the base's last
    // line), so the trailing-newline status is preserved automatically when no
    // hunk touches the tail.
    for line in &base_lines[cursor..] {
        result.push(line.clone());
    }

    ApplyOutcome::Applied(join_lines(&result))
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

/// Locate the 0-based base-line index at which `hunk`'s old-side (context +
/// delete) lines match. Tries the expected position first, then expands the
/// search symmetrically outward. Returns `None` if no match is found within
/// [`MAX_HUNK_OFFSET`].
fn locate_hunk(
    base_lines: &[Line],
    hunk: &Hunk,
    min_pos: usize,
    running_offset: isize,
) -> Option<usize> {
    let old_side = old_side_lines(hunk);
    let expected = expected_position(hunk, running_offset);
    // Clamp the starting guess into range.
    let guess = expected.max(0) as usize;

    // Try the exact guess first.
    if guess >= min_pos && hunk_matches_at(base_lines, &old_side, hunk, guess) {
        return Some(guess);
    }
    // Expand outward.
    for delta in 1..=MAX_HUNK_OFFSET {
        // Forward.
        if let Some(pos) = guess.checked_add(delta)
            && pos >= min_pos
            && hunk_matches_at(base_lines, &old_side, hunk, pos)
        {
            return Some(pos);
        }
        // Backward.
        if let Some(pos) = guess.checked_sub(delta)
            && pos >= min_pos
            && hunk_matches_at(base_lines, &old_side, hunk, pos)
        {
            return Some(pos);
        }
    }
    None
}

/// The old-side (context + delete) line contents of a hunk, in order.
fn old_side_lines(hunk: &Hunk) -> Vec<&[u8]> {
    hunk.lines
        .iter()
        .filter_map(|hl| match hl {
            HunkLine::Context(bytes) | HunkLine::Delete(bytes) => Some(bytes.as_slice()),
            HunkLine::Insert(_) => None,
        })
        .collect()
}

/// Whether `old_side` matches `base_lines` starting at `pos`, including the
/// trailing-newline expectation when the hunk declares one.
fn hunk_matches_at(base_lines: &[Line], old_side: &[&[u8]], hunk: &Hunk, pos: usize) -> bool {
    if pos + old_side.len() > base_lines.len() {
        return false;
    }
    for (i, expected) in old_side.iter().enumerate() {
        if base_lines[pos + i].content.as_slice() != *expected {
            return false;
        }
    }
    // If the hunk asserts the old file's final line lacks a newline, the last
    // matched line must indeed be the file's terminal line and lack a newline.
    if hunk.old_no_newline && !old_side.is_empty() {
        let last = pos + old_side.len() - 1;
        if last + 1 != base_lines.len() || !base_lines[last].no_newline {
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
    /// caller); otherwise parsing starts at a `--- ` line.
    fn parse_file(&mut self, diff_line: Option<&[u8]>) -> Result<FilePatch> {
        let mut patch = FilePatch {
            old_path: None,
            new_path: None,
            old_mode: None,
            new_mode: None,
            hunks: Vec::new(),
            is_new: false,
            is_delete: false,
            is_rename: false,
        };
        // Default paths from `diff --git a/x b/x` if present (overridden by
        // `---`/`+++` lines when those carry real paths).
        if let Some(diff_line) = diff_line {
            if let Some((a, b)) = parse_diff_git_paths(diff_line) {
                patch.old_path = Some(a);
                patch.new_path = Some(b);
            }
            self.index += 1;
        }

        // Extended headers until the first `---`/`@@`/next `diff --git`.
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            if line.starts_with(b"--- ") {
                self.parse_old_file_header(line, &mut patch);
                self.index += 1;
                break;
            } else if line.starts_with(b"@@ ") {
                // No `---`/`+++` (e.g. pure rename or mode change with no body).
                break;
            } else if line.starts_with(b"diff --git ") {
                // Next file began with no body for this one.
                return Ok(patch);
            } else if let Some(rest) = strip_prefix(line, b"old mode ") {
                patch.old_mode = parse_octal(rest);
            } else if let Some(rest) = strip_prefix(line, b"new mode ") {
                patch.new_mode = parse_octal(rest);
            } else if let Some(rest) = strip_prefix(line, b"new file mode ") {
                patch.is_new = true;
                patch.new_mode = parse_octal(rest);
            } else if let Some(rest) = strip_prefix(line, b"deleted file mode ") {
                patch.is_delete = true;
                patch.old_mode = parse_octal(rest);
            } else if let Some(rest) = strip_prefix(line, b"rename from ") {
                patch.is_rename = true;
                patch.old_path = Some(rest.to_vec());
            } else if let Some(rest) = strip_prefix(line, b"rename to ") {
                patch.is_rename = true;
                patch.new_path = Some(rest.to_vec());
            } else {
                // `index ..`, `similarity index`, `copy from/to`, etc. — ignore.
                self.index += 1;
                continue;
            }
            self.index += 1;
        }

        // `+++` header (the old-file branch above already advanced past `---`).
        if self.index < self.lines.len() && self.lines[self.index].starts_with(b"+++ ") {
            self.parse_new_file_header(self.lines[self.index], &mut patch);
            self.index += 1;
        }

        // Hunks.
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            if line.starts_with(b"@@ ") {
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

        Ok(patch)
    }

    fn parse_old_file_header(&self, line: &[u8], patch: &mut FilePatch) {
        let rest = strip_prefix(line, b"--- ").unwrap_or(line);
        let path = strip_header_path(rest);
        match path {
            HeaderPath::DevNull => {
                patch.is_new = true;
                patch.old_path = None;
            }
            HeaderPath::Path(p) => {
                // Only override if we did not already learn a real path.
                if patch.old_path.is_none() || !patch.is_rename {
                    patch.old_path = Some(p);
                }
            }
        }
    }

    fn parse_new_file_header(&self, line: &[u8], patch: &mut FilePatch) {
        let rest = strip_prefix(line, b"+++ ").unwrap_or(line);
        let path = strip_header_path(rest);
        match path {
            HeaderPath::DevNull => {
                patch.is_delete = true;
                patch.new_path = None;
            }
            HeaderPath::Path(p) => {
                if patch.new_path.is_none() || !patch.is_rename {
                    patch.new_path = Some(p);
                }
            }
        }
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
        };
        let mut old_seen = 0usize;
        let mut new_seen = 0usize;

        while self.index < self.lines.len() {
            // Stop when both sides are satisfied.
            if old_seen >= old_len && new_seen >= new_len {
                break;
            }
            let line = self.lines[self.index];
            if line.is_empty() {
                // A wholly empty line in a unified diff is a context line whose
                // content is the empty string (git emits a bare ` `, but some
                // tooling/email transport strips the trailing space).
                hunk.lines.push(HunkLine::Context(Vec::new()));
                old_seen += 1;
                new_seen += 1;
                self.index += 1;
                continue;
            }
            match line[0] {
                b' ' => {
                    hunk.lines.push(HunkLine::Context(line[1..].to_vec()));
                    old_seen += 1;
                    new_seen += 1;
                }
                b'+' => {
                    hunk.lines.push(HunkLine::Insert(line[1..].to_vec()));
                    new_seen += 1;
                }
                b'-' => {
                    hunk.lines.push(HunkLine::Delete(line[1..].to_vec()));
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

        if old_seen != old_len || new_seen != new_len {
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

enum HeaderPath {
    DevNull,
    Path(Vec<u8>),
}

/// Extract the path from a `---`/`+++` header tail, stripping a leading `a/` or
/// `b/` prefix, an optional trailing timestamp (separated by a tab), and
/// recognising `/dev/null`.
fn strip_header_path(rest: &[u8]) -> HeaderPath {
    // Cut a trailing tab-delimited timestamp if present.
    let path = match rest.iter().position(|&b| b == b'\t') {
        Some(tab) => &rest[..tab],
        None => rest,
    };
    let path = trim_ascii_end(path);
    if path == b"/dev/null" {
        return HeaderPath::DevNull;
    }
    // Strip a leading `a/` or `b/` (git's default prefixes).
    let stripped = if path.starts_with(b"a/") || path.starts_with(b"b/") {
        &path[2..]
    } else {
        path
    };
    HeaderPath::Path(stripped.to_vec())
}

/// Parse the two paths out of `diff --git a/<x> b/<y>`. Returns the paths with
/// their `a/`/`b/` prefixes stripped. Returns `None` when the line cannot be
/// split unambiguously (e.g. paths containing spaces, which git would quote).
fn parse_diff_git_paths(line: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let rest = strip_prefix(line, b"diff --git ")?;
    // Quoted paths are uncommon in this engine's inputs; bail and let the
    // `---`/`+++` headers supply the names instead.
    if rest.first() == Some(&b'"') {
        return None;
    }
    // Find the split point: the boundary between the `a/...` and `b/...` halves.
    // git separates them with a single space; the simplest robust heuristic is
    // to look for ` b/` preceded by an `a/` start.
    if !rest.starts_with(b"a/") {
        return None;
    }
    let sep = find_subslice(rest, b" b/")?;
    let a = &rest[2..sep];
    let b = &rest[sep + 3..];
    Some((a.to_vec(), b.to_vec()))
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

fn parse_octal(bytes: &[u8]) -> Option<u32> {
    let trimmed = trim_ascii_end(bytes);
    if trimmed.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &b in trimmed {
        if !(b'0'..=b'7').contains(&b) {
            return None;
        }
        value = value.checked_mul(8)?.checked_add((b - b'0') as u32)?;
    }
    Some(value)
}

fn strip_prefix<'b>(line: &'b [u8], prefix: &[u8]) -> Option<&'b [u8]> {
    if line.starts_with(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use git_formats::RepositoryLayout;
    use git_object::TreeEntry;
    use git_odb::ObjectWriter;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn name_status_reports_added_from_index() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .unwrap();
        let index = Index {
            version: 2,
            entries: vec![git_index::IndexEntry {
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
                path: b"hello.txt".to_vec(),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(layout.git_dir.join("index"), index.write_v2_sha1().unwrap()).unwrap();
        fs::write(root.join("hello.txt"), b"hello\n").unwrap();
        let changes =
            diff_name_status_head_worktree(&root, &layout.git_dir, ObjectFormat::Sha1).unwrap();
        assert_eq!(changes[0].line(), "A\thello.txt");
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "git-rs-diff-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    // ---- line diff / blob merge tests ---------------------------------------

    fn merge_opts() -> MergeBlobOptions<'static> {
        MergeBlobOptions {
            ours_label: "ours",
            theirs_label: "theirs",
            base_label: "base",
            style: ConflictStyle::Merge,
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
        let patches = parse_unified_patch(patch).unwrap();
        assert_eq!(patches.len(), 2);

        assert_eq!(patches[0].old_path.as_deref(), Some(b"one.txt".as_slice()));
        assert_eq!(patches[0].new_path.as_deref(), Some(b"one.txt".as_slice()));
        assert_eq!(patches[0].old_mode, None);
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
        let patches = parse_unified_patch(patch).unwrap();
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
        .unwrap();
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"alpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_with_line_offset() {
        // The hunk header points at line 1, but the matching context actually
        // lives a few lines down; the offset search must find it.
        let base = b"pre1\npre2\npre3\nalpha\nbeta\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .unwrap();
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"pre1\npre2\npre3\nalpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_with_negative_line_offset() {
        // Recorded position is well past the real location; search backward.
        let base = b"alpha\nbeta\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -50,3 +50,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .unwrap();
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
        .unwrap();
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"a\nB\nc\nd\ne\nf\nG\nh\n");
    }

    #[test]
    fn reject_on_context_mismatch() {
        let base = b"alpha\nDIFFERENT\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .unwrap();
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
        .unwrap();
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
        .unwrap();
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
        .unwrap();
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
        .unwrap();
        assert_eq!(patch[0].old_mode, Some(0o100644));
        assert_eq!(patch[0].new_mode, Some(0o100755));
        assert!(!patch[0].is_new);
        assert!(!patch[0].is_delete);
    }

    #[test]
    fn no_final_newline_base_preserved_when_untouched() {
        // The change is on line 1; the final line has no newline and is not in
        // the hunk, so its no-newline state must survive.
        let base = b"alpha\nbeta\nnotail"; // "notail" has no trailing \n
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-alpha\n+ALPHA\n").unwrap();
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
        .unwrap();
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
        .unwrap();
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
        .unwrap();
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
        .unwrap();
        assert!(patch[0].is_delete);
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"");
    }

    #[test]
    fn apply_pure_insertion_hunk() {
        let base = b"first\nsecond\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n first\n+middle\n second\n")
                .unwrap();
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"first\nmiddle\nsecond\n");
    }

    #[test]
    fn apply_pure_deletion_hunk() {
        let base = b"first\nmiddle\nsecond\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,3 +1,2 @@\n first\n-middle\n second\n")
                .unwrap();
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
        let p1 = parse_unified_patch(text).unwrap();
        let p2 = parse_unified_patch(text).unwrap();
        assert_eq!(p1, p2);
        let out = applied(apply_file_patch(base, &p1[0]));
        assert_eq!(out, b"l1\nl2\nL3\nL3b\nl4\nl5\n");
    }

    #[test]
    fn empty_context_line_without_trailing_space() {
        // Some transports strip the single leading space from blank context
        // lines; the parser treats a wholly empty body line as blank context.
        let base = b"a\n\nb\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n").unwrap();
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
            .unwrap()
    }

    /// Write a tree from `(name, mode, oid)` entries (sorted by name as git
    /// requires) and return its oid.
    fn write_tree(db: &mut FileObjectDatabase, entries: &[(&[u8], u32, ObjectId)]) -> ObjectId {
        let mut tree_entries: Vec<TreeEntry> = entries
            .iter()
            .map(|(name, mode, oid)| TreeEntry {
                mode: *mode,
                name: name.to_vec(),
                oid: oid.clone(),
            })
            .collect();
        tree_entries.sort_by(|a, b| a.name.cmp(&b.name));
        let tree = Tree {
            entries: tree_entries,
        };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .unwrap()
    }

    #[test]
    fn inexact_rename_detected_with_plausible_score() {
        // a.txt (one changed line vs the new b.txt) should be detected as a
        // rename with score 75 (see `similarity_one_changed_line_is_75`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
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
        };
        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .unwrap();

        assert_eq!(entries.len(), 1, "expected a single rename entry: {entries:?}");
        assert_eq!(entries[0].status, NameStatus::Renamed(75));
        assert_eq!(entries[0].old_path.as_deref(), Some(b"a.txt".as_slice()));
        assert_eq!(entries[0].path, b"b.txt");
        assert_eq!(entries[0].line(), "R075\ta.txt\tb.txt");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inexact_rename_below_threshold_not_detected() {
        // A half-rewrite scores 50%. With a 60% threshold it must NOT be paired;
        // the change shows up as a separate Add + Delete instead.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
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
        };
        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .unwrap();

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
        .unwrap();
        assert_eq!(entries_low.len(), 1);
        assert_eq!(entries_low[0].status, NameStatus::Renamed(50));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_rename_scores_100_and_takes_priority() {
        // Identical content moved to a new path is an exact rename: score 100,
        // detected even with inexact disabled, and still 100 with it enabled.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let oid = write_blob(&mut db, b"identical\ncontent\nhere\n");
        let left = write_tree(&mut db, &[(b"old.txt", 0o100644, oid.clone())]);
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
            };
            let entries = diff_name_status_trees_with_rename_options(
                &db,
                ObjectFormat::Sha1,
                &left,
                &right,
                opts,
            )
            .unwrap();
            assert_eq!(entries.len(), 1, "inexact={inexact}: {entries:?}");
            assert_eq!(entries[0].status, NameStatus::Renamed(100));
            assert_eq!(entries[0].old_path.as_deref(), Some(b"old.txt".as_slice()));
            assert_eq!(entries[0].path, b"new.txt");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inexact_copy_detected_with_score() {
        // orig.txt is unchanged and a near-copy (one line differs, 80% similar)
        // is added. With copy detection + find_copies_harder + inexact, the new
        // file is reported as a copy with score 80 (matches `git diff -C
        // --find-copies-harder`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
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
        };
        let entries = diff_name_status_trees_with_rename_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .unwrap();

        let copy_entry = entries
            .iter()
            .find(|e| e.path == b"copy.txt")
            .unwrap_or_else(|| panic!("no copy.txt entry: {entries:?}"));
        assert_eq!(copy_entry.status, NameStatus::Copied(80));
        assert_eq!(copy_entry.old_path.as_deref(), Some(b"orig.txt".as_slice()));
        // The source remains present (copies do not consume the original).
        assert!(
            entries.iter().all(|e| e.status != NameStatus::Deleted),
            "copy must not delete the source: {entries:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inexact_rename_with_small_edit_scores_88() {
        // A rename that also appends a single line scores 88% (see
        // `similarity_small_append_is_88`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(&mut db, b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n");
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
        .unwrap();

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].status, NameStatus::Renamed(88));
        assert_eq!(entries[0].old_path.as_deref(), Some(b"src.txt".as_slice()));
        assert_eq!(entries[0].path, b"dst.txt");
        fs::remove_dir_all(root).unwrap();
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
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
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
        .unwrap();
        let statuses: Vec<_> = entries.iter().map(|e| e.status).collect();
        assert!(statuses.contains(&NameStatus::Added));
        assert!(statuses.contains(&NameStatus::Deleted));
        assert!(!statuses.iter().any(|s| matches!(s, NameStatus::Renamed(_))));
        fs::remove_dir_all(root).unwrap();
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
        check_all_algorithms(b"alpha\nbeta\ngamma\ndelta\n", b"gamma\ndelta\nalpha\nbeta\n");
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
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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
            if !buf.is_empty() && rng() % 4 == 0 {
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
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for _ in 0..200 {
            // Random subset+order of tokens "0\n".."9\n" for each side; tokens
            // are globally unique, so any common line is unique in both.
            let pick_subseq = |rng: &mut dyn FnMut() -> u32| -> Vec<u8> {
                let mut buf = Vec::new();
                for t in 0..10u32 {
                    if rng() % 2 == 0 {
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
}
