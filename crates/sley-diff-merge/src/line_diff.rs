//! Line-level diff algorithms (Myers, patience, histogram).

use std::collections::HashMap;

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
// Myers' diagonal coordinates intentionally index the old and new inputs with
// different offsets.
#[allow(clippy::suspicious_operation_groupings)]
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
    /// No whitespace-ignore flavour active (the exact, byte-for-byte comparison).
    pub const EMPTY: Self = Self {
        all_space: false,
        space_change: false,
        space_at_eol: false,
        cr_at_eol: false,
    };

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

pub(crate) fn canonicalize_line(line: &[u8], ignore: WsIgnore) -> Vec<u8> {
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
pub(crate) fn line_is_blank(line: &[u8], ignore: WsIgnore) -> bool {
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
/// flags, lines are canonicalized (see `canonicalize_line`) for the equality
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
    patience_diff_lines_anchored(old, new, &[])
}

/// As [`patience_diff_lines`], but pins lines whose content has any of `anchors`
/// as a byte prefix into the common subsequence (git's `--anchored=<text>`).
///
/// Mirrors xdiff's `xpatience.c`: an anchor line that is unique in both ranges is
/// forced to remain aligned (so *other* lines are moved instead), taken greedily
/// in old-side order; an anchor that would break the increasing order with an
/// already-pinned anchor is dropped. Anchors that are non-unique or absent have
/// no effect, exactly as in git. With `anchors` empty this is plain patience.
pub fn patience_diff_lines_anchored(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    anchors: &[Vec<u8>],
) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();
    patience_recurse(old, new, 0, old.len(), 0, new.len(), anchors, &mut ops);
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
// The two ranges have independent bounds and offsets by design.
#[allow(clippy::suspicious_operation_groupings)]
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
///
/// `anchors` carries the `--anchored=<text>` prefixes (empty for plain
/// patience); they are re-evaluated at every recursion level, since a line that
/// is non-unique in the whole file can become unique within a sub-range.
#[allow(clippy::too_many_arguments)]
fn patience_recurse(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
    anchors: &[Vec<u8>],
    out: &mut Vec<DiffOp>,
) {
    if emit_trivial_range(a0, a1, b0, b1, out) {
        return;
    }
    let (a0, a1, b0, b1, suffix) = trim_common(old, new, a0, a1, b0, b1, out);
    if !emit_trivial_range(a0, a1, b0, b1, out) {
        match patience_anchors(old, new, a0, a1, b0, b1, anchors) {
            Some(aligned) => {
                // Walk the aligned anchors in order, recursing into each gap
                // before emitting the anchor line as Equal.
                let mut cur_a = a0;
                let mut cur_b = b0;
                for (ai, bi) in aligned {
                    patience_recurse(old, new, cur_a, ai, cur_b, bi, anchors, out);
                    out.push(DiffOp::Equal(1));
                    cur_a = ai + 1;
                    cur_b = bi + 1;
                }
                // Tail after the last anchor.
                patience_recurse(old, new, cur_a, a1, cur_b, b1, anchors, out);
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
    anchors: &[Vec<u8>],
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
    // anchors increasing in both coordinates. With `--anchored` text(s) present,
    // pin the matching (unique-in-both) lines into the subsequence instead.
    let lis = if anchors.is_empty() {
        longest_increasing_by_new(&pairs)
    } else {
        let is_anchor: Vec<bool> = pairs
            .iter()
            .map(|&(_, nj)| line_matches_anchor(new[nj].content, anchors))
            .collect();
        longest_increasing_by_new_anchored(&pairs, &is_anchor)
    };
    if lis.is_empty() { None } else { Some(lis) }
}

/// Whether `line` begins with any of the `--anchored` prefixes (git's
/// `is_anchor`: a byte-prefix `strncmp` against the line's content, trailing
/// newline included). An empty anchor prefix matches every line, matching git.
fn line_matches_anchor(line: &[u8], anchors: &[Vec<u8>]) -> bool {
    anchors.iter().any(|anchor| line.starts_with(anchor))
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

/// Longest increasing subsequence of `pairs` (sorted by old-index, keyed on the
/// new-index) that is *forced* to pass through every includible anchor.
///
/// A direct port of git's anchored `find_longest_common_sequence`
/// (xdiff/xpatience.c): entries are processed in old-index order and placed into
/// the patience-sort `sequence` by their new-index. When an anchor entry
/// (`is_anchor[i]`) is placed at position `k`, `anchor_i` is pinned to `k` and
/// the running length is forced to `k + 1`; thereafter positions `<= anchor_i`
/// can never be overridden, so the result must contain that anchor. A later
/// anchor whose placement would fall at or before `anchor_i` is skipped, exactly
/// matching git's greedy handling of mutually-incompatible anchors.
fn longest_increasing_by_new_anchored(
    pairs: &[(usize, usize)],
    is_anchor: &[bool],
) -> Vec<(usize, usize)> {
    if pairs.is_empty() {
        return Vec::new();
    }
    // sequence[k] = index into `pairs` of the smallest-new-index tail of an
    // increasing subsequence of length k+1; `prev` links to the predecessor.
    let mut sequence: Vec<usize> = Vec::with_capacity(pairs.len());
    let mut prev: Vec<Option<usize>> = vec![None; pairs.len()];
    let mut longest: usize = 0;
    let mut anchor_i: isize = -1;
    for (e, &(_, val)) in pairs.iter().enumerate() {
        // i = largest position in sequence[0..longest] whose new-index < val,
        // or -1 if none (git's fast-path + `binary_search`).
        let i: isize = if longest == 0 || val > pairs[sequence[longest - 1]].1 {
            longest as isize - 1
        } else {
            let mut lo = 0usize;
            let mut hi = longest;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if pairs[sequence[mid]].1 < val {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo as isize - 1
        };
        prev[e] = if i < 0 {
            None
        } else {
            Some(sequence[i as usize])
        };
        let pos = (i + 1) as usize;
        if (pos as isize) <= anchor_i {
            continue;
        }
        if pos == sequence.len() {
            sequence.push(e);
        } else {
            sequence[pos] = e;
        }
        if is_anchor[e] {
            anchor_i = pos as isize;
            longest = pos + 1;
        } else if pos == longest {
            longest += 1;
        }
    }
    if longest == 0 {
        return Vec::new();
    }
    let mut result: Vec<(usize, usize)> = Vec::with_capacity(longest);
    let mut cur = Some(sequence[longest - 1]);
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
// Candidate runs intentionally compare independently offset input ranges.
#[allow(clippy::suspicious_operation_groupings)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAlgorithm {
    Myers,
    Minimal,
    Patience,
    Histogram,
}

/// Line-level diff with git's `xdl_cleanup_records` pre-pass plus the
/// bidirectional middle-snake search (`xdl_recs_cmp` / `xdl_split`).
///
/// Lines that appear only on one side are forced into the change set; Myers
/// then runs only over lines present on both sides (the KEEP /
/// `reference_index` set). Word-diff depends on this for SES choice among
/// equal-cost alignments (bibtex brace matching in `t4034-diff-words.sh`).
///
/// The multi-match INVESTIGATE path (`xdl_clean_mmatch`) is approximated by
/// always KEEPing lines that appear on both sides: word-diff buffers stay
/// under the `bogosqrt` multi-match threshold, so this matches git there.
pub fn myers_diff_lines_prepared(old: &[DiffLine<'_>], new: &[DiffLine<'_>]) -> Vec<DiffOp> {
    if old.is_empty() && new.is_empty() {
        return Vec::new();
    }
    if old.is_empty() {
        return vec![DiffOp::Insert(new.len())];
    }
    if new.is_empty() {
        return vec![DiffOp::Delete(old.len())];
    }

    // Count occurrences by the same equality DiffLine uses (bytes + newline flag).
    let mut new_counts: HashMap<(&[u8], bool), usize> = HashMap::new();
    for line in new {
        *new_counts
            .entry((line.content, line.has_newline))
            .or_insert(0) += 1;
    }
    let mut old_counts: HashMap<(&[u8], bool), usize> = HashMap::new();
    for line in old {
        *old_counts
            .entry((line.content, line.has_newline))
            .or_insert(0) += 1;
    }

    let mut old_changed = vec![false; old.len()];
    let mut new_changed = vec![false; new.len()];
    let mut old_keep: Vec<usize> = Vec::new();
    let mut new_keep: Vec<usize> = Vec::new();

    for (i, line) in old.iter().enumerate() {
        let key = (line.content, line.has_newline);
        if new_counts.get(&key).copied().unwrap_or(0) == 0 {
            old_changed[i] = true;
        } else {
            old_keep.push(i);
        }
    }
    for (i, line) in new.iter().enumerate() {
        let key = (line.content, line.has_newline);
        if old_counts.get(&key).copied().unwrap_or(0) == 0 {
            new_changed[i] = true;
        } else {
            new_keep.push(i);
        }
    }

    // Bidirectional middle-snake over KEEP lines, marking unmatched KEEP
    // entries as changed. DISCARD lines are already changed.
    let old_ref: Vec<DiffLine<'_>> = old_keep.iter().map(|&i| old[i]).collect();
    let new_ref: Vec<DiffLine<'_>> = new_keep.iter().map(|&i| new[i]).collect();
    let mut ref_old_changed = vec![false; old_ref.len()];
    let mut ref_new_changed = vec![false; new_ref.len()];
    middle_snake_mark_changed(
        &old_ref,
        &new_ref,
        &mut ref_old_changed,
        &mut ref_new_changed,
    );
    for (ref_i, &full_i) in old_keep.iter().enumerate() {
        if ref_old_changed[ref_i] {
            old_changed[full_i] = true;
        }
    }
    for (ref_i, &full_i) in new_keep.iter().enumerate() {
        if ref_new_changed[ref_i] {
            new_changed[full_i] = true;
        }
    }

    // git always runs `xdl_change_compact` after the main search (word-diff
    // uses flags=0, so indent heuristic is off). Slide change groups to the
    // same canonical positions git does.
    change_compact_no_indent(old, new, &mut old_changed, &mut new_changed);

    // Rebuild a coalesced op script from the two changed[] arrays, the same
    // way `xdl_build_script` walks them after the main search.
    ops_from_changed(&old_changed, &new_changed)
}

/// Rebuild a coalesced [`DiffOp`] script from two parallel `changed[]` flags.
fn ops_from_changed(old_changed: &[bool], new_changed: &[bool]) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    let n_old = old_changed.len();
    let n_new = new_changed.len();
    while i < n_old || j < n_new {
        if i < n_old && old_changed[i] {
            let mut run = 0usize;
            while i < n_old && old_changed[i] {
                run += 1;
                i += 1;
            }
            ops.push(DiffOp::Delete(run));
        } else if j < n_new && new_changed[j] {
            let mut run = 0usize;
            while j < n_new && new_changed[j] {
                run += 1;
                j += 1;
            }
            ops.push(DiffOp::Insert(run));
        } else {
            let mut run = 0usize;
            while i < n_old && j < n_new && !old_changed[i] && !new_changed[j] {
                run += 1;
                i += 1;
                j += 1;
            }
            if run == 0 {
                break;
            }
            ops.push(DiffOp::Equal(run));
        }
    }
    coalesce_ops(ops)
}

/// Port of git's `xdl_change_compact` with indent heuristic disabled (the
/// word-diff path sets `xpp.flags = 0`).
fn change_compact_no_indent(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    old_changed: &mut [bool],
    new_changed: &mut [bool],
) {
    struct File<'a> {
        recs: &'a [DiffLine<'a>],
        changed: &'a mut [bool],
    }
    struct Group {
        start: usize,
        end: usize,
    }
    fn nrec(f: &File<'_>) -> usize {
        f.recs.len()
    }
    fn ch(f: &File<'_>, i: isize) -> bool {
        if i < 0 || i as usize >= f.changed.len() {
            false
        } else {
            f.changed[i as usize]
        }
    }
    fn set_ch(f: &mut File<'_>, i: usize, v: bool) {
        f.changed[i] = v;
    }
    fn match_at(f: &File<'_>, a: usize, b: usize) -> bool {
        a < f.recs.len() && b < f.recs.len() && f.recs[a] == f.recs[b]
    }
    fn group_init(f: &File<'_>) -> Group {
        let mut end = 0usize;
        while end < nrec(f) && ch(f, end as isize) {
            end += 1;
        }
        Group { start: 0, end }
    }
    fn group_next(f: &File<'_>, g: &mut Group) -> bool {
        if g.end == nrec(f) {
            return false;
        }
        g.start = g.end + 1;
        g.end = g.start;
        while g.end < nrec(f) && ch(f, g.end as isize) {
            g.end += 1;
        }
        true
    }
    fn group_previous(f: &File<'_>, g: &mut Group) -> bool {
        if g.start == 0 {
            return false;
        }
        g.end = g.start - 1;
        g.start = g.end;
        while g.start > 0 && ch(f, g.start as isize - 1) {
            g.start -= 1;
        }
        true
    }
    fn slide_down(f: &mut File<'_>, g: &mut Group) -> bool {
        if g.end < nrec(f) && match_at(f, g.start, g.end) {
            set_ch(f, g.start, false);
            set_ch(f, g.end, true);
            g.start += 1;
            g.end += 1;
            while g.end < nrec(f) && ch(f, g.end as isize) {
                g.end += 1;
            }
            true
        } else {
            false
        }
    }
    fn slide_up(f: &mut File<'_>, g: &mut Group) -> bool {
        if g.start > 0 && match_at(f, g.start - 1, g.end - 1) {
            g.start -= 1;
            g.end -= 1;
            set_ch(f, g.start, true);
            set_ch(f, g.end, false);
            while g.start > 0 && ch(f, g.start as isize - 1) {
                g.start -= 1;
            }
            true
        } else {
            false
        }
    }
    fn compact_one(xdf: &mut File<'_>, xdfo: &mut File<'_>) {
        let mut g = group_init(xdf);
        let mut go = group_init(xdfo);
        loop {
            if g.end == g.start {
                if !group_next(xdf, &mut g) {
                    break;
                }
                if !group_next(xdfo, &mut go) {
                    break;
                }
                continue;
            }
            loop {
                let groupsize = g.end - g.start;
                let mut end_matching_other: Option<usize> = None;

                while slide_up(xdf, &mut g) {
                    let _ = group_previous(xdfo, &mut go);
                }
                let earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = Some(g.end);
                }
                loop {
                    if !slide_down(xdf, &mut g) {
                        break;
                    }
                    let _ = group_next(xdfo, &mut go);
                    if go.end > go.start {
                        end_matching_other = Some(g.end);
                    }
                }
                if groupsize == g.end - g.start {
                    // Slide done for this size.
                    if g.end != earliest_end && end_matching_other.is_some() {
                        while go.end == go.start {
                            let _ = slide_up(xdf, &mut g);
                            let _ = group_previous(xdfo, &mut go);
                        }
                        // indent heuristic omitted (word-diff flags=0)
                    }
                    break;
                }
            }
            if !group_next(xdf, &mut g) {
                break;
            }
            if !group_next(xdfo, &mut go) {
                break;
            }
        }
    }

    let mut f1 = File {
        recs: old,
        changed: old_changed,
    };
    let mut f2 = File {
        recs: new,
        changed: new_changed,
    };
    // git compacts old then new.
    compact_one(&mut f1, &mut f2);
    compact_one(&mut f2, &mut f1);
}

/// Port of `xdl_recs_cmp` + `xdl_split` (need_min path only): shrink common
/// prefix/suffix, then recursively split at the middle snake. Marks lines that
/// lie off the common subsequence as changed.
fn middle_snake_mark_changed(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    old_changed: &mut [bool],
    new_changed: &mut [bool],
) {
    #[allow(clippy::too_many_arguments, clippy::suspicious_operation_groupings)]
    fn recs_cmp(
        old: &[DiffLine<'_>],
        new: &[DiffLine<'_>],
        mut off1: usize,
        mut lim1: usize,
        mut off2: usize,
        mut lim2: usize,
        old_changed: &mut [bool],
        new_changed: &mut [bool],
    ) {
        while off1 < lim1 && off2 < lim2 && old[off1] == new[off2] {
            off1 += 1;
            off2 += 1;
        }
        while off1 < lim1 && off2 < lim2 && old[lim1 - 1] == new[lim2 - 1] {
            lim1 -= 1;
            lim2 -= 1;
        }
        if off1 == lim1 {
            for changed in &mut new_changed[off2..lim2] {
                *changed = true;
            }
            return;
        }
        if off2 == lim2 {
            for changed in &mut old_changed[off1..lim1] {
                *changed = true;
            }
            return;
        }
        let (mid1, mid2) = xdl_split_middle(old, new, off1, lim1, off2, lim2);
        recs_cmp(old, new, off1, mid1, off2, mid2, old_changed, new_changed);
        recs_cmp(old, new, mid1, lim1, mid2, lim2, old_changed, new_changed);
    }
    recs_cmp(
        old,
        new,
        0,
        old.len(),
        0,
        new.len(),
        old_changed,
        new_changed,
    );
}

/// Port of git's `xdl_split` with `need_min` forced true (no heuristic early
/// exit). Returns the `(i1, i2)` split point of the middle snake.
fn xdl_split_middle(
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    off1: usize,
    lim1: usize,
    off2: usize,
    lim2: usize,
) -> (usize, usize) {
    // Diagonal k = i1 - i2. Use a map so k can be negative without offset math.
    let dmin = off1 as isize - lim2 as isize;
    let dmax = lim1 as isize - off2 as isize;
    let fmid = off1 as isize - off2 as isize;
    let bmid = lim1 as isize - lim2 as isize;
    let odd = ((fmid - bmid) & 1) != 0;

    let mut fmin = fmid;
    let mut fmax = fmid;
    let mut bmin = bmid;
    let mut bmax = bmid;
    let mut kvdf: HashMap<isize, isize> = HashMap::new();
    let mut kvdb: HashMap<isize, isize> = HashMap::new();
    kvdf.insert(fmid, off1 as isize);
    kvdb.insert(bmid, lim1 as isize);

    let mut ec = 0isize;
    loop {
        ec += 1;

        // Extend forward domain.
        if fmin > dmin {
            fmin -= 1;
            kvdf.insert(fmin - 1, -1);
        } else {
            fmin += 1;
        }
        if fmax < dmax {
            fmax += 1;
            kvdf.insert(fmax + 1, -1);
        } else {
            fmax -= 1;
        }

        let mut d = fmax;
        while d >= fmin {
            let i1 = if kvdf.get(&(d - 1)).copied().unwrap_or(-1)
                >= kvdf.get(&(d + 1)).copied().unwrap_or(-1)
            {
                kvdf.get(&(d - 1)).copied().unwrap_or(-1) + 1
            } else {
                kvdf.get(&(d + 1)).copied().unwrap_or(-1)
            };
            let mut i1 = i1;
            let mut i2 = i1 - d;
            while (i1 as usize) < lim1
                && (i2 as usize) < lim2
                && old[i1 as usize] == new[i2 as usize]
            {
                i1 += 1;
                i2 += 1;
            }
            kvdf.insert(d, i1);
            if odd
                && bmin <= d
                && d <= bmax
                && let Some(&bd) = kvdb.get(&d)
                && bd <= i1
            {
                return (i1 as usize, i2 as usize);
            }
            d -= 2;
        }

        // Extend backward domain.
        if bmin > dmin {
            bmin -= 1;
            kvdb.insert(bmin - 1, isize::MAX / 4);
        } else {
            bmin += 1;
        }
        if bmax < dmax {
            bmax += 1;
            kvdb.insert(bmax + 1, isize::MAX / 4);
        } else {
            bmax -= 1;
        }

        let mut d = bmax;
        while d >= bmin {
            let i1 = if kvdb.get(&(d - 1)).copied().unwrap_or(isize::MAX / 4)
                < kvdb.get(&(d + 1)).copied().unwrap_or(isize::MAX / 4)
            {
                kvdb.get(&(d - 1)).copied().unwrap_or(isize::MAX / 4)
            } else {
                kvdb.get(&(d + 1)).copied().unwrap_or(isize::MAX / 4) - 1
            };
            let mut i1 = i1;
            let mut i2 = i1 - d;
            while i1 > off1 as isize
                && i2 > off2 as isize
                && old[(i1 - 1) as usize] == new[(i2 - 1) as usize]
            {
                i1 -= 1;
                i2 -= 1;
            }
            kvdb.insert(d, i1);
            if !odd
                && fmin <= d
                && d <= fmax
                && let Some(&fd) = kvdf.get(&d)
                && i1 <= fd
            {
                return (i1 as usize, i2 as usize);
            }
            d -= 2;
        }

        // Safety: always terminates for finite boxes (edit cost ≤ N+M).
        if ec > (lim1 - off1 + lim2 - off2) as isize + 2 {
            // Fallback split at half the remaining box.
            return (off1 + (lim1 - off1) / 2, off2 + (lim2 - off2) / 2);
        }
    }
}

#[cfg(test)]
mod prepared_diff_tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<DiffLine<'static>> {
        items
            .iter()
            .map(|w| {
                let owned = w.as_bytes().to_vec();
                let leaked: &'static [u8] = Box::leak(owned.into_boxed_slice());
                DiffLine {
                    content: leaked,
                    has_newline: true,
                }
            })
            .collect()
    }

    fn fmt_ops(ops: &[DiffOp]) -> String {
        ops.iter()
            .map(|op| match op {
                DiffOp::Equal(n) => format!("E{n}"),
                DiffOp::Delete(n) => format!("D{n}"),
                DiffOp::Insert(n) => format!("I{n}"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn bibtex_year_block_matches_git_xdiff_alignment() {
        // Token sequence from t4034 bibtex year/note change group.
        let minus = words(&["year", "=", "{", "1987", "}", ","]);
        let plus = words(&[
            "year",
            "=",
            "1987,",
            "note",
            "=",
            "{",
            "This",
            "is",
            "in",
            "fact",
            "a",
            "rather",
            "funny",
            "read",
            "since",
            "ethernet",
            "works",
            "well",
            "in",
            "practice.",
            "The",
            "{",
            "\\em",
            "pre",
            "}",
            "reference",
            "is",
            "the",
            "right",
            "one,",
            "however.",
            "}",
        ]);
        let ops = myers_diff_lines_prepared(&minus, &plus);
        // Git xdiff (cleanup + middle-snake + compact): equal `year=`; insert
        // through `The`; equal `{`; delete `1987`; insert `\em pre } reference
        // ... however.`; equal `}`; delete `,`.
        assert_eq!(fmt_ops(&ops), "E2 I19 E1 D1 I9 E1 D1");
    }

    #[test]
    fn prepared_diff_identical_is_single_equal() {
        let lines = words(&["a", "b", "c"]);
        assert_eq!(
            myers_diff_lines_prepared(&lines, &lines),
            vec![DiffOp::Equal(3)]
        );
    }
}
