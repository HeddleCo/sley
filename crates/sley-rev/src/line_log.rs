//! The line-log history-simplification core (`git log -L`, git's `line-log.c`).
//!
//! Given the initial tracked line ranges for one or more paths (resolved
//! against a tip blob above the seam by the `-L` argument parser), this module
//! walks an already-ordered commit list, maps each commit's tracked ranges back
//! across its diff to its parent(s), and records which commits changed a
//! tracked range together with the per-file diff pairs and post-image ranges
//! needed to render range-clipped patches.
//!
//! The range-mapping core (`range_set_map_across_diff`,
//! `diff_ranges_filter_touched`, `range_set_shift_diff`) is a direct port; the
//! per-commit diff is computed with sley's tree-name-status + blob reads, and
//! the per-line diff that drives range mapping uses sley's Myers diff.

use std::collections::HashMap;

use sley_core::{ObjectFormat, ObjectId, Result};
use sley_diff_merge::render::LineRange;
use sley_object::Commit;
use sley_odb::{FileObjectDatabase, ObjectReader};

use crate::CommitRecord;

/// A half-open `[start, end)` line range, 0-based. Mirrors diff.c's
/// `struct range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: i64,
    pub end: i64,
}

/// A sorted, disjoint set of [`Range`]s (git's `range_set`).
#[derive(Debug, Clone, Default)]
pub struct RangeSet {
    pub ranges: Vec<Range>,
}

impl RangeSet {
    pub fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// `range_set_append`: must begin at or after the end of the last range.
    /// Skips empty ranges (git's `range_set_append` asserts `a <= b` but real
    /// range sets never store empties; the diff-ranges parallel arrays use
    /// [`append_raw`] to keep zero-width sides).
    pub fn append(&mut self, start: i64, end: i64) {
        if start >= end {
            return;
        }
        self.ranges.push(Range { start, end });
    }

    /// Append a possibly-empty range (git's `range_set_append_unsafe` with
    /// `a == b` allowed). Used by `collect_diff` to keep the parent/target
    /// arrays parallel — an insert hunk has a zero-width parent side, a delete
    /// hunk a zero-width target side, but both arrays must stay index-aligned.
    fn append_raw(&mut self, start: i64, end: i64) {
        self.ranges.push(Range { start, end });
    }

    /// `sort_and_merge_range_set`: sort by start, then merge overlapping /
    /// touching ranges into a disjoint set.
    pub fn sort_and_merge(&mut self) {
        self.ranges.sort_by_key(|r| r.start);
        let mut out: Vec<Range> = Vec::with_capacity(self.ranges.len());
        for r in self.ranges.drain(..) {
            if r.start == r.end {
                continue;
            }
            if let Some(last) = out.last_mut()
                && r.start <= last.end
            {
                if r.end > last.end {
                    last.end = r.end;
                }
                continue;
            }
            out.push(r);
        }
        self.ranges = out;
    }

    /// `range_set_union`: merge two sorted disjoint sets.
    fn union(a: &RangeSet, b: &RangeSet) -> RangeSet {
        let mut out = RangeSet::new();
        let mut i = 0usize;
        let mut j = 0usize;
        // git interleaves by start, appending and coalescing.
        let push = |out: &mut RangeSet, r: Range| {
            if let Some(last) = out.ranges.last_mut()
                && r.start <= last.end
            {
                if r.end > last.end {
                    last.end = r.end;
                }
                return;
            }
            out.ranges.push(r);
        };
        while i < a.ranges.len() || j < b.ranges.len() {
            let take_a = match (a.ranges.get(i), b.ranges.get(j)) {
                (Some(ra), Some(rb)) => ra.start <= rb.start,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_a {
                push(&mut out, a.ranges[i]);
                i += 1;
            } else {
                push(&mut out, b.ranges[j]);
                j += 1;
            }
        }
        out
    }

    /// `range_set_difference`: out = rs - remove. Both inputs are sorted and
    /// disjoint; the output is too.
    fn difference(rs: &RangeSet, remove: &RangeSet) -> RangeSet {
        let mut out = RangeSet::new();
        for &Range { start, end } in &rs.ranges {
            let mut cur = start;
            for rk in &remove.ranges {
                if rk.end <= cur {
                    continue;
                }
                if rk.start >= end {
                    break;
                }
                // rk overlaps [cur, end).
                if rk.start > cur {
                    out.append(cur, rk.start);
                }
                cur = rk.end.max(cur);
                if cur >= end {
                    break;
                }
            }
            if cur < end {
                out.append(cur, end);
            }
        }
        out
    }
}

/// A diff encoded as parallel parent/target range sets — one pair per hunk.
/// Mirrors diff.c's `struct diff_ranges`.
#[derive(Debug, Clone, Default)]
struct DiffRanges {
    parent: RangeSet,
    target: RangeSet,
}

/// `ranges_overlap`.
fn ranges_overlap(a: &Range, b: &Range) -> bool {
    !(a.end <= b.start || b.end <= a.start)
}

/// `diff_ranges_filter_touched`: select the hunks of `diff` whose target side
/// overlaps a tracked range in `rs`.
fn diff_ranges_filter_touched(out: &mut DiffRanges, diff: &DiffRanges, rs: &RangeSet) {
    if rs.is_empty() {
        return;
    }
    let mut j = 0usize;
    for i in 0..diff.target.ranges.len() {
        while diff.target.ranges[i].start >= rs.ranges[j].end {
            j += 1;
            if j == rs.ranges.len() {
                return;
            }
        }
        if ranges_overlap(&diff.target.ranges[i], &rs.ranges[j]) {
            out.parent
                .append_raw(diff.parent.ranges[i].start, diff.parent.ranges[i].end);
            out.target
                .append_raw(diff.target.ranges[i].start, diff.target.ranges[i].end);
        }
    }
}

/// `range_set_shift_diff`: shift the line numbers in `rs` to account for the
/// lines added/removed by `diff` (mapping target-side positions to parent-side
/// positions).
fn range_set_shift_diff(out: &mut RangeSet, rs: &RangeSet, diff: &DiffRanges) {
    let mut j = 0usize;
    let mut offset: i64 = 0;
    for src in &rs.ranges {
        while j < diff.target.ranges.len() && src.start >= diff.target.ranges[j].start {
            offset += (diff.parent.ranges[j].end - diff.parent.ranges[j].start)
                - (diff.target.ranges[j].end - diff.target.ranges[j].start);
            j += 1;
        }
        out.append(src.start + offset, src.end + offset);
    }
}

/// `range_set_map_across_diff`: map `rs` across `diff`, returning the parent-side
/// ranges and the set of touched hunks (for output).
fn range_set_map_across_diff(rs: &RangeSet, diff: &DiffRanges) -> (RangeSet, DiffRanges) {
    let mut touched = DiffRanges::default();
    diff_ranges_filter_touched(&mut touched, diff, rs);
    let tmp1 = RangeSet::difference(rs, &touched.target);
    let mut tmp2 = RangeSet::new();
    range_set_shift_diff(&mut tmp2, &tmp1, diff);
    let out = RangeSet::union(&tmp2, &touched.parent);
    (out, touched)
}

/// Per-file tracked range state for one commit (git's `line_log_data`).
#[derive(Debug, Clone)]
pub struct FileRange {
    pub path: String,
    pub ranges: RangeSet,
    /// The diff pair (old_oid/new_oid + status) that produced `diff`, set when
    /// this file changed in the commit being printed.
    pair: Option<DiffPair>,
}

/// The diff pair to render for a printed file (old/new blob + status + paths).
#[derive(Debug, Clone)]
struct DiffPair {
    old_path: String,
    new_path: String,
    old_oid: Option<ObjectId>,
    new_oid: Option<ObjectId>,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    status: sley_diff_merge::NameStatus,
}

impl FileRange {
    pub fn new(path: String) -> Self {
        Self {
            path,
            ranges: RangeSet::new(),
            pair: None,
        }
    }
}

/// The per-commit tracked range list (git's `line_log_data` linked list, kept
/// sorted by path).
pub type RangeList = Vec<FileRange>;

/// Read a blob's bytes by oid.
fn read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = db.read_object(oid)?;
    Ok(object.body.to_vec())
}

/// 0-based line-start offsets of `data`: `nth_line(n)` returns `&data[ends[n]..]`.
/// `ends[0] = 0`; `ends[k]` is the byte offset of the start of line `k`.
/// `lines` is the number of lines (git's `fill_line_ends`).
pub fn line_ends(data: &[u8]) -> (Vec<usize>, i64) {
    let mut ends = vec![0usize];
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            ends.push(i + 1);
        }
    }
    // git counts a trailing partial line. ends has one entry per line start
    // plus a sentinel for the position after the last '\n'. The number of
    // lines is ends.len()-1 when the data ends in '\n', else ends.len().
    let lines = if data.is_empty() {
        0
    } else if data.last() == Some(&b'\n') {
        (ends.len() - 1) as i64
    } else {
        ends.len() as i64
    };
    (ends, lines)
}

/// Default funcname-line classifier (`def_ff` / `match_funcname` with no
/// driver): first byte is a letter, `_`, or `$`.
pub fn is_funcname_line(line: &[u8]) -> bool {
    match line.first() {
        Some(&b) => b.is_ascii_alphabetic() || b == b'_' || b == b'$',
        None => false,
    }
}

/// The line at offset `off` (the bytes from `off` to the next '\n' inclusive,
/// or end).
pub fn line_at(data: &[u8], off: usize) -> &[u8] {
    let end = data[off..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| off + p + 1)
        .unwrap_or(data.len());
    &data[off..end]
}

/// Compute the per-line diff hunks between `parent` and `target` blobs as a
/// [`DiffRanges`] (git's `collect_diff`: ctxlen=0, one hunk = one change). The
/// ranges are 0-based half-open on each side.
fn collect_diff(parent: &[u8], target: &[u8]) -> DiffRanges {
    let old = sley_diff_merge::split_lines(parent);
    let new = sley_diff_merge::split_lines(target);
    let ops = sley_diff_merge::myers_diff_lines(&old, &new);
    let mut out = DiffRanges::default();
    let mut old_idx = 0i64;
    let mut new_idx = 0i64;
    // Walk the edit script; each maximal change run becomes one hunk with
    // count-0 sides appended as empty (git appends both sides per hunk; an
    // unchanged region is skipped).
    let mut i = 0usize;
    while i < ops.len() {
        match ops[i] {
            sley_diff_merge::DiffOp::Equal(n) => {
                old_idx += n as i64;
                new_idx += n as i64;
                i += 1;
            }
            _ => {
                // Accumulate a contiguous Delete/Insert run.
                let old_start = old_idx;
                let new_start = new_idx;
                while i < ops.len() {
                    match ops[i] {
                        sley_diff_merge::DiffOp::Delete(n) => {
                            old_idx += n as i64;
                            i += 1;
                        }
                        sley_diff_merge::DiffOp::Insert(n) => {
                            new_idx += n as i64;
                            i += 1;
                        }
                        sley_diff_merge::DiffOp::Equal(_) => break,
                    }
                }
                out.parent.append_raw(old_start, old_idx);
                out.target.append_raw(new_start, new_idx);
            }
        }
    }
    out
}

/// Process one parent diff for one commit, mapping the tracked ranges of `fr`
/// back across the diff. Returns the touched-hunk DiffRanges if any hunk
/// touched a tracked range. Destructively updates `fr.ranges` to the parent
/// side. Mirrors `process_diff_filepair`.
fn process_file_diff(
    parent_blob: &[u8],
    target_blob: &[u8],
    fr: &mut FileRange,
) -> Option<DiffRanges> {
    if fr.ranges.is_empty() {
        return None;
    }
    let diff = collect_diff(parent_blob, target_blob);
    let (mapped, touched) = range_set_map_across_diff(&fr.ranges, &diff);
    fr.ranges = mapped;
    if !touched.parent.is_empty() || !touched.target.is_empty() {
        Some(touched)
    } else {
        None
    }
}

/// Lookup a name-status entry for `path` (matching git's `pair->two->path`).
fn find_entry<'a>(
    entries: &'a [sley_diff_merge::NameStatusEntry],
    path: &str,
) -> Option<&'a sley_diff_merge::NameStatusEntry> {
    entries
        .iter()
        .find(|e| e.path.as_bytes() == path.as_bytes())
}

/// Produce the per-commit name-status entries against a parent tree.
fn commit_name_status(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    parent_tree: Option<&ObjectId>,
    tree: &ObjectId,
    detect_renames: bool,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    let base = sley_diff_merge::DiffNameStatusOptions {
        detect_renames,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: true,
        ..Default::default()
    };
    let entries = match (parent_tree, detect_renames) {
        (Some(parent), true) => sley_diff_merge::diff_name_status_trees_with_options(
            db,
            format,
            parent,
            tree,
            sley_diff_merge::DiffNameStatusOptions {
                detect_renames,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
                detect_inexact: true,
                rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
                rename_limit: 0,
            },
        )?,
        (Some(parent), false) => {
            sley_diff_merge::diff_name_status_trees_with_options(db, format, parent, tree, base)?
        }
        (None, _) => {
            sley_diff_merge::diff_name_status_empty_tree_with_options(db, format, tree, base)?
        }
    };
    Ok(entries)
}

/// Blob bytes for a name-status side; `None` for an absent (added/deleted) side.
fn entry_side_blob(db: &FileObjectDatabase, oid: Option<&ObjectId>) -> Result<Option<Vec<u8>>> {
    match oid {
        Some(oid) => Ok(Some(read_blob(db, oid)?)),
        None => Ok(None),
    }
}

/// Convert a [`RangeSet`] to the renderer's `[LineRange]`.
fn to_line_ranges(rs: &RangeSet) -> Vec<LineRange> {
    rs.ranges
        .iter()
        .map(|r| LineRange {
            start: r.start,
            end: r.end,
        })
        .collect()
}

/// Per-commit processing result. Mirrors `process_ranges_ordinary_commit` +
/// `process_all_files`.
struct ProcessResult {
    /// Whether the commit changed a tracked range (git's `changed`).
    changed: bool,
    /// The range list to attach to the (first) parent, after mapping back.
    parent_ranges: RangeList,
    /// The committing range list with diff pairs filled in, for rendering this
    /// commit's restricted patch.
    printed: RangeList,
}

/// Process an ordinary (single-parent or root) commit. Mirrors
/// `process_ranges_ordinary_commit` + `process_all_files`.
fn process_ranges_ordinary(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &CommitRecord,
    range: &RangeList,
    detect_renames: bool,
) -> Result<ProcessResult> {
    let parent = record.parents.first();
    let parent_tree = match parent {
        Some(p) => Some(tree_of_commit(db, format, p)?),
        None => None,
    };
    let entries = commit_name_status(
        db,
        format,
        parent_tree.as_ref(),
        &record.commit.tree,
        detect_renames,
    )?;
    // `parent_ranges` is a copy of the committing range list; we map each file's
    // ranges back across its diff (so it can be propagated to the parent) and
    // record the touched-commit diff pair on `printed` (the input commit's list)
    // for rendering.
    let mut parent_ranges: RangeList = range.clone();
    let mut printed: RangeList = range.clone();
    let mut changed = false;
    for fr in &mut parent_ranges {
        let entry = match find_entry(&entries, &fr.path) {
            Some(e) => e,
            None => continue,
        };
        let target_blob = entry_side_blob(db, entry.new_oid.as_ref())?.unwrap_or_default();
        let parent_blob = entry_side_blob(db, entry.old_oid.as_ref())?.unwrap_or_default();
        let old_path = entry
            .old_path
            .as_ref()
            .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
            .unwrap_or_else(|| fr.path.clone());
        let printed_path = fr.path.clone();
        if process_file_diff(&parent_blob, &target_blob, fr).is_some() {
            changed = true;
            if let Some(prf) = printed.iter_mut().find(|f| f.path == printed_path) {
                prf.pair = Some(DiffPair {
                    old_path: old_path.clone(),
                    new_path: entry_path_string(entry),
                    old_oid: entry.old_oid,
                    new_oid: entry.new_oid,
                    old_mode: entry.old_mode,
                    new_mode: entry.new_mode,
                    status: entry.status,
                });
            }
        }
        // Rename following: the parent-side path becomes the old path.
        fr.path = old_path;
    }
    parent_ranges.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ProcessResult {
        changed,
        parent_ranges,
        printed,
    })
}

/// Tree oid of a commit oid.
fn tree_of_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(oid)?;
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

fn entry_path_string(entry: &sley_diff_merge::NameStatusEntry) -> String {
    String::from_utf8_lossy(entry.path.as_bytes()).into_owned()
}

/// One file's restricted patch to render for a printed commit: the diff pair
/// plus the post-image line ranges to clip hunks to.
#[derive(Debug, Clone)]
pub struct PrintedFile {
    pub old_path: String,
    pub new_path: String,
    pub old_oid: Option<ObjectId>,
    pub new_oid: Option<ObjectId>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub status: sley_diff_merge::NameStatus,
    pub line_ranges: Vec<LineRange>,
}

/// The output of the line-log walk: the ordered interesting commits (newest
/// first, topo order) and, per commit oid, the files + ranges to render.
pub struct LineLogResult {
    pub interesting: Vec<ObjectId>,
    pub printed: HashMap<ObjectId, Vec<PrintedFile>>,
}

/// Run the line-log walk over `ordered` (commits in topological order, child
/// before parent — git's `revs->commits` after topo sort). `tip` is the single
/// commit the `-L` arguments resolved their initial ranges against (git
/// requires exactly one positive tip); `initial` carries those parsed ranges.
/// Returns the interesting commits + per-commit printed ranges.
///
/// Mirrors `line_log_filter`: ranges propagate lazily from child to parent via
/// the per-commit decoration map; a commit is interesting iff it changed a
/// tracked range.
pub fn run_line_log(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    ordered: &[CommitRecord],
    tip: &ObjectId,
    initial: RangeList,
    detect_renames: bool,
    first_parent: bool,
) -> Result<LineLogResult> {
    // Per-commit tracked ranges (git's line_log_data decoration). Seeded on the
    // tip; propagated to parents as we process each commit in topo order.
    let mut ranges_by_commit: HashMap<ObjectId, RangeList> = HashMap::new();
    ranges_by_commit.insert(*tip, initial);

    let mut interesting: Vec<ObjectId> = Vec::new();
    let mut printed: HashMap<ObjectId, Vec<PrintedFile>> = HashMap::new();
    // Index records by oid for parent lookups.
    let by_oid: HashMap<ObjectId, &CommitRecord> = ordered.iter().map(|r| (r.oid, r)).collect();

    for record in ordered {
        let range = match ranges_by_commit.remove(&record.oid) {
            Some(r) if !r.is_empty() && r.iter().any(|f| !f.ranges.is_empty()) => r,
            _ => continue,
        };

        let is_merge = record.parents.len() > 1 && !first_parent;
        let result = if is_merge {
            process_ranges_merge(db, format, record, &range, detect_renames)?
        } else {
            let parent = record.parents.first().copied();
            process_ranges_ordinary(db, format, record, &range, detect_renames)?.into_merge(parent)
        };

        if result.changed {
            interesting.push(record.oid);
            // Build the printed files for this commit from result.printed.
            let mut files = Vec::new();
            for fr in &result.printed {
                if let Some(pair) = &fr.pair {
                    files.push(PrintedFile {
                        old_path: pair.old_path.clone(),
                        new_path: pair.new_path.clone(),
                        old_oid: pair.old_oid,
                        new_oid: pair.new_oid,
                        old_mode: pair.old_mode,
                        new_mode: pair.new_mode,
                        status: pair.status,
                        line_ranges: to_line_ranges(&fr.ranges),
                    });
                }
            }
            printed.insert(record.oid, files);
        }

        // Propagate the mapped ranges to the named parents (first parent for an
        // ordinary commit; every candidate parent for an unexplained merge, or
        // the single blamed parent when one explains it).
        for (parent_oid, parent_ranges) in result.merge_parent_ranges {
            if by_oid.contains_key(&parent_oid) {
                merge_into_commit(&mut ranges_by_commit, parent_oid, parent_ranges);
            }
        }
    }

    Ok(LineLogResult {
        interesting,
        printed,
    })
}

/// Merge `add` into the range list stored for `commit` (git's `add_line_range`
/// → `line_log_data_merge`: union per-path).
fn merge_into_commit(map: &mut HashMap<ObjectId, RangeList>, commit: ObjectId, add: RangeList) {
    let entry = map.entry(commit).or_default();
    for fr in add {
        if fr.ranges.is_empty() {
            continue;
        }
        if let Some(existing) = entry.iter_mut().find(|f| f.path == fr.path) {
            existing.ranges = RangeSet::union(&existing.ranges, &fr.ranges);
        } else {
            entry.push(fr);
        }
    }
    entry.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Process a merge commit. Mirrors `process_ranges_merge_commit`: try each
/// parent; if one parent fully explains the ranges (no change), blame it and
/// stop. Otherwise every parent gets the mapped candidate ranges.
fn process_ranges_merge(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &CommitRecord,
    range: &RangeList,
    detect_renames: bool,
) -> Result<MergeResult> {
    let mut cands: Vec<(ObjectId, RangeList)> = Vec::new();
    for parent in &record.parents {
        let parent_tree = tree_of_commit(db, format, parent)?;
        let entries = commit_name_status(
            db,
            format,
            Some(&parent_tree),
            &record.commit.tree,
            detect_renames,
        )?;
        let mut parent_ranges: RangeList = range.clone();
        let mut changed = false;
        for fr in &mut parent_ranges {
            let entry = match find_entry(&entries, &fr.path) {
                Some(e) => e,
                None => continue,
            };
            let target_blob = entry_side_blob(db, entry.new_oid.as_ref())?.unwrap_or_default();
            let parent_blob = entry_side_blob(db, entry.old_oid.as_ref())?.unwrap_or_default();
            let old_path = entry
                .old_path
                .as_ref()
                .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
                .unwrap_or_else(|| fr.path.clone());
            if process_file_diff(&parent_blob, &target_blob, fr).is_some() {
                changed = true;
            }
            fr.path = old_path;
        }
        parent_ranges.sort_by(|a, b| a.path.cmp(&b.path));
        if !changed {
            // This parent fully explains the ranges — blame it alone.
            return Ok(MergeResult {
                changed: false,
                merge_parent_ranges: vec![(*parent, parent_ranges)],
                printed: range.clone(),
            });
        }
        cands.push((*parent, parent_ranges));
    }
    // No single parent explained it — every parent gets its candidate ranges.
    Ok(MergeResult {
        changed: true,
        merge_parent_ranges: cands,
        printed: range.clone(),
    })
}

/// Result of processing a commit (ordinary or merge) for the driver's needs.
struct MergeResult {
    changed: bool,
    merge_parent_ranges: Vec<(ObjectId, RangeList)>,
    printed: RangeList,
}

// Adapt the ordinary result into the driver's unified shape.
impl ProcessResult {
    fn into_merge(self, parent: Option<ObjectId>) -> MergeResult {
        MergeResult {
            changed: self.changed,
            merge_parent_ranges: match parent {
                Some(p) => vec![(p, self.parent_ranges)],
                None => Vec::new(),
            },
            printed: self.printed,
        }
    }
}
