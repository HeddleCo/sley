use git_core::{object_id_for_bytes, GitError, ObjectFormat, ObjectId, RepoPath, Result};
use git_formats::{Commit, EncodedObject, Index, ObjectType, Tree};
use git_odb::{FileObjectDatabase, ObjectReader};
use git_refs::{FileRefStore, RefTarget};
use std::collections::{BTreeMap, BTreeSet};
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
            Self::Renamed(score) => format!("R{score}"),
            Self::Copied(score) => format!("C{score}"),
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

fn diff_name_status_maps<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
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

fn is_empty_blob_oid(oid: &ObjectId) -> bool {
    object_id_for_bytes(oid.format(), "blob", b"").is_ok_and(|empty| empty == *oid)
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

#[cfg(test)]
mod tests {
    use super::*;
    use git_formats::RepositoryLayout;
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
            entries: vec![git_formats::IndexEntry {
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
}
