//! Three-way blob merge (diff3 conflict markers).

use crate::line_diff::{
    DiffAlgorithm, DiffLine, DiffOp, WsIgnore, canonicalize_line, diff_lines_with_algorithm,
    myers_diff_lines, myers_diff_lines_ws, split_lines,
};

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

/// Which conflict-marker style [`merge_blobs`] emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictStyle {
    /// Standard two-section markers (`<<<<<<<` / `=======` / `>>>>>>>`).
    #[default]
    Merge,
    /// `diff3` style: also include the common-ancestor section between `ours`
    /// and the `=======` divider, delimited by `|||||||`.
    Diff3,
    /// `zdiff3` style: include the common-ancestor section like [`Self::Diff3`],
    /// but hoist lines shared at the beginning and end of both sides out of the
    /// conflict block. This is git's zealous diff3 rendering.
    ZDiff3,
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
    /// lines with no markers; ours/theirs select that side's conflict content.
    pub favor: MergeFavor,
    /// Whitespace-insensitivity for the 3-way line matching, mirroring
    /// `-Xignore-space-change`/`-Xignore-all-space`/`-Xignore-space-at-eol` (git's
    /// `ll_opts.xdl_opts`). When non-empty, regions that differ only by ignored
    /// whitespace are not conflicts, and unchanged spans emit ours' actual bytes
    /// (xdl_merge copies the common parts from file1). Empty (the default) is the
    /// exact, byte-for-byte merge.
    pub ws_ignore: WsIgnore,
    /// Number of marker bytes in `<<<<<<<` / `=======` / `>>>>>>>` lines.
    pub marker_size: usize,
}

impl Default for MergeBlobOptions<'_> {
    fn default() -> Self {
        Self {
            ours_label: "ours",
            theirs_label: "theirs",
            base_label: "base",
            style: ConflictStyle::Merge,
            favor: MergeFavor::None,
            ws_ignore: WsIgnore::EMPTY,
            marker_size: 7,
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

/// Typed engine request for standalone `merge-file` semantics.
#[derive(Debug, Clone, Copy)]
pub struct MergeFileOptions<'a> {
    pub ours_label: &'a str,
    pub base_label: &'a str,
    pub theirs_label: &'a str,
    pub style: ConflictStyle,
    pub favor: MergeFavor,
    pub marker_size: usize,
    pub algorithm: DiffAlgorithm,
}

impl Default for MergeFileOptions<'_> {
    fn default() -> Self {
        Self {
            ours_label: "ours",
            base_label: "base",
            theirs_label: "theirs",
            style: ConflictStyle::Merge,
            favor: MergeFavor::None,
            marker_size: 7,
            algorithm: DiffAlgorithm::Myers,
        }
    }
}

/// Standalone file-merge result, including the exact number of unresolved
/// conflict sections used as `merge-file`'s exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeFileOutcome {
    pub content: Vec<u8>,
    pub conflicts: usize,
}

/// Merge three file/blob payloads without repository or terminal concerns.
pub fn merge_file(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    options: &MergeFileOptions<'_>,
) -> MergeFileOutcome {
    let blob_options = MergeBlobOptions {
        ours_label: options.ours_label,
        base_label: options.base_label,
        theirs_label: options.theirs_label,
        style: options.style,
        favor: options.favor,
        ws_ignore: WsIgnore::EMPTY,
        marker_size: options.marker_size,
    };
    let (result, conflicts) =
        merge_blobs_internal(base, ours, theirs, &blob_options, options.algorithm);
    MergeFileOutcome {
        content: result.content,
        conflicts,
    }
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
    merge_blobs_internal(base, ours, theirs, options, DiffAlgorithm::Myers).0
}

fn merge_blobs_internal(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    options: &MergeBlobOptions<'_>,
    algorithm: DiffAlgorithm,
) -> (MergeBlobResult, usize) {
    let base_lines = split_lines(base);
    let ours_lines = split_lines(ours);
    let theirs_lines = split_lines(theirs);

    // Per-side matched (equal) base regions, paired with the corresponding side
    // ranges, computed via Myers. Under `ws_ignore`, lines that differ only by
    // ignored whitespace match, so whitespace-only changes are absorbed into the
    // stable spans rather than surfacing as conflicts.
    let ours_matches = matching_regions(&base_lines, &ours_lines, options.ws_ignore, algorithm);
    let theirs_matches = matching_regions(&base_lines, &theirs_lines, options.ws_ignore, algorithm);

    // Intersect the two match lists to get segments of base that are unchanged
    // on BOTH sides, each carrying the exact aligned side indices. Between these
    // common-stable segments lie the (potentially conflicting) changed regions.
    let stable = common_stable_segments(&ours_matches, &theirs_matches);
    let stable = if options.style == ConflictStyle::Merge {
        simplify_conflict_separators(
            stable,
            &base_lines,
            &ours_lines,
            &theirs_lines,
            options.ws_ignore,
        )
    } else {
        stable
    };

    let mut writer = MergeWriter::new(options, ours, base);
    // Cursors: next unconsumed line in base, ours, theirs.
    let mut base_idx = 0usize;
    let mut our_idx = 0usize;
    let mut their_idx = 0usize;

    for seg in &stable {
        // Unstable (changed) region preceding this stable segment.
        let base_region = &base_lines[base_idx..seg.base_start];
        let our_region = &ours_lines[our_idx..seg.ours_start];
        let their_region = &theirs_lines[their_idx..seg.theirs_start];
        emit_region(
            &mut writer,
            base_region,
            our_region,
            their_region,
            options.ws_ignore,
        );

        // The stable segment matched on both sides. Emit ours' actual bytes
        // (xdl_merge copies common spans from file1): identical to base under an
        // exact match, and ours' whitespace under `ws_ignore`.
        writer.emit_lines(&ours_lines[seg.ours_start..seg.ours_start + seg.len]);

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
        options.ws_ignore,
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
    ws_ignore: WsIgnore,
) {
    if our_region.is_empty() && their_region.is_empty() {
        return;
    }
    // Under `ws_ignore`, "changed" means changed beyond ignored whitespace; with
    // the empty default the comparison is exact byte equality.
    let our_changed = !regions_match(our_region, base_region, ws_ignore);
    let their_changed = !regions_match(their_region, base_region, ws_ignore);
    match (our_changed, their_changed) {
        (false, false) => writer.emit_lines(our_region),
        (true, false) => writer.emit_lines(our_region),
        (false, true) => writer.emit_lines(their_region),
        (true, true) => {
            if regions_match(our_region, their_region, ws_ignore) {
                // Both sides made the same change (up to ignored whitespace): no
                // conflict. xdl_merge keeps ours' bytes.
                writer.emit_lines(our_region);
            } else {
                writer.emit_conflict_refined(our_region, base_region, their_region);
            }
        }
    }
}

/// Whether two line slices are equal, exactly when `ws_ignore` is empty and up to
/// the active whitespace-ignore canonicalization otherwise.
fn regions_match(a: &[DiffLine<'_>], b: &[DiffLine<'_>], ws_ignore: WsIgnore) -> bool {
    if ws_ignore.is_empty() {
        return a == b;
    }
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            canonicalize_line(x.content, ws_ignore) == canonicalize_line(y.content, ws_ignore)
        })
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
                let small = ours[range.clone()]
                    .iter()
                    .filter(|line| line.content.iter().any(u8::is_ascii_alphanumeric))
                    .count()
                    <= 3;
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
fn matching_regions(
    base: &[DiffLine<'_>],
    side: &[DiffLine<'_>],
    ws_ignore: WsIgnore,
    algorithm: DiffAlgorithm,
) -> Vec<MatchRegion> {
    let ops = if ws_ignore.is_empty() {
        diff_lines_with_algorithm(base, side, algorithm)
    } else {
        // The 3-way content merge uses the Myers line diff (git's ll-merge xdl
        // default); the whitespace flags affect only the equality test.
        myers_diff_lines_ws(base, side, ws_ignore, algorithm)
    };
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

/// Git's `xdl_simplify_non_conflicts(XDL_MERGE_ZEALOUS_ALNUM)` absorbs a common
/// span between two genuine conflicts when at most three of its lines contain
/// alphanumeric content. Removing that span from the stable anchors lets the
/// existing side-to-side refinement fold it into one larger conflict. Blank or
/// punctuation-only lines therefore have zero separator cost.
fn simplify_conflict_separators<'a>(
    stable: Vec<StableSegment>,
    base: &[DiffLine<'a>],
    ours: &[DiffLine<'a>],
    theirs: &[DiffLine<'a>],
    ws_ignore: WsIgnore,
) -> Vec<StableSegment> {
    stable
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| {
            let previous = index.checked_sub(1).and_then(|index| stable.get(index));
            let next = stable.get(index + 1);
            let before = changed_span_conflicts(
                &base[previous.map_or(0, |item| item.base_start + item.len)..segment.base_start],
                &ours[previous.map_or(0, |item| item.ours_start + item.len)..segment.ours_start],
                &theirs
                    [previous.map_or(0, |item| item.theirs_start + item.len)..segment.theirs_start],
                ws_ignore,
            );
            let after = changed_span_conflicts(
                &base[segment.base_start + segment.len
                    ..next.map_or(base.len(), |item| item.base_start)],
                &ours[segment.ours_start + segment.len
                    ..next.map_or(ours.len(), |item| item.ours_start)],
                &theirs[segment.theirs_start + segment.len
                    ..next.map_or(theirs.len(), |item| item.theirs_start)],
                ws_ignore,
            );
            let alnum_lines = base[segment.base_start..segment.base_start + segment.len]
                .iter()
                .filter(|line| line.content.iter().any(u8::is_ascii_alphanumeric))
                .count();
            (!(before && after && alnum_lines <= 3)).then_some(*segment)
        })
        .collect()
}

fn changed_span_conflicts(
    base: &[DiffLine<'_>],
    ours: &[DiffLine<'_>],
    theirs: &[DiffLine<'_>],
    ws_ignore: WsIgnore,
) -> bool {
    !regions_match(ours, base, ws_ignore)
        && !regions_match(theirs, base, ws_ignore)
        && !regions_match(ours, theirs, ws_ignore)
}

/// Accumulates merged output and renders conflict markers byte-for-byte like
/// upstream git.
struct MergeWriter<'a> {
    out: Vec<u8>,
    conflicted: bool,
    conflicts: usize,
    /// Whether the *current* conflict markers should end in CRLF. Computed per
    /// conflict via git's `is_cr_needed` (both post-images + ancestor agree).
    crlf: bool,
    /// Ancestor bytes (for the first-line CRLF probe in `is_cr_needed`).
    base_blob: &'a [u8],
    options: &'a MergeBlobOptions<'a>,
}

impl<'a> MergeWriter<'a> {
    fn new(options: &'a MergeBlobOptions<'a>, _ours: &[u8], base: &'a [u8]) -> Self {
        Self {
            out: Vec::new(),
            conflicted: false,
            conflicts: 0,
            // Default LF; set per conflict in `emit_conflict`.
            crlf: false,
            base_blob: base,
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
        if self.options.favor == MergeFavor::Ours {
            self.emit_section(ours);
            return;
        }
        if self.options.favor == MergeFavor::Theirs {
            self.emit_section(theirs);
            return;
        }
        // Union: keep both sides' lines (ours then theirs) with no markers, and do
        // NOT flag a conflict — git's `XDL_MERGE_FAVOR_UNION`.
        if self.options.favor == MergeFavor::Union {
            self.emit_section(ours);
            self.ensure_newline();
            self.emit_section(theirs);
            return;
        }

        // zdiff3 keeps the diff3 ancestor section, but moves context common to
        // both sides at either edge outside the conflict markers. The base
        // section deliberately remains the whole base region; that is how xdiff
        // retains the useful ancestor context while shrinking only the side
        // sections.
        let (prefix, suffix) = if self.options.style == ConflictStyle::ZDiff3 {
            let prefix = common_prefix_len(ours, theirs);
            let suffix = common_suffix_len(ours, theirs, prefix);
            (prefix, suffix)
        } else {
            (0, 0)
        };
        if prefix != 0 {
            self.emit_section(&ours[..prefix]);
        }
        let ours_inner = &ours[prefix..ours.len() - suffix];
        let theirs_inner = &theirs[prefix..theirs.len() - suffix];

        // git's `is_cr_needed`: markers use CRLF only when both post-images and
        // the ancestor agree the surrounding lines end in CR/LF. Mixed endings
        // (e.g. CR-at-eol on ours only under `--ignore-space-at-eol`) fall back
        // to LF-only markers, matching xdiff/xmerge.c.
        self.crlf = conflict_markers_need_cr(ours, theirs, self.base_blob);

        self.conflicted = true;
        self.conflicts += 1;
        self.write_marker(b'<', self.options.ours_label);
        self.emit_section(ours_inner);
        if matches!(
            self.options.style,
            ConflictStyle::Diff3 | ConflictStyle::ZDiff3
        ) {
            self.ensure_newline();
            self.write_marker(b'|', self.options.base_label);
            self.emit_section(base);
        }
        self.ensure_newline();
        self.write_divider();
        self.emit_section(theirs_inner);
        self.ensure_newline();
        self.write_marker(b'>', self.options.theirs_label);
        if suffix != 0 {
            self.emit_section(&ours[ours.len() - suffix..]);
        }
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
        if matches!(
            self.options.style,
            ConflictStyle::Diff3 | ConflictStyle::ZDiff3
        ) || self.options.favor != MergeFavor::None
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
            self.write_line_terminator();
        }
    }

    /// Write a marker line: N copies of `ch`, then (if the label is non-empty)
    /// a space and the label, then a newline. No trailing space for an empty
    /// label — byte-for-byte with upstream git.
    fn write_marker(&mut self, ch: u8, label: &str) {
        for _ in 0..self.options.marker_size {
            self.out.push(ch);
        }
        if !label.is_empty() {
            self.out.push(b' ');
            self.out.extend_from_slice(label.as_bytes());
        }
        self.write_line_terminator();
    }

    /// Write the `=======` divider line (never labelled).
    fn write_divider(&mut self) {
        for _ in 0..self.options.marker_size {
            self.out.push(b'=');
        }
        self.write_line_terminator();
    }

    fn write_line_terminator(&mut self) {
        if self.crlf {
            self.out.extend_from_slice(b"\r\n");
        } else {
            self.out.push(b'\n');
        }
    }

    fn finish(self) -> (MergeBlobResult, usize) {
        let result = MergeBlobResult {
            content: self.out,
            conflicted: self.conflicted,
        };
        (result, self.conflicts)
    }
}

/// git's `is_cr_needed` / `is_eol_crlf`: true only when both post-image sides
/// and the ancestor's first line end in CR/LF. Returns false (LF markers) when
/// any side is LF-only or the style is indeterminate.
fn conflict_markers_need_cr(
    ours: &[DiffLine<'_>],
    theirs: &[DiffLine<'_>],
    base_blob: &[u8],
) -> bool {
    fn line_is_crlf(line: &DiffLine<'_>) -> Option<bool> {
        let bytes = line.content;
        if bytes.is_empty() {
            return None;
        }
        if bytes.last() == Some(&b'\n') {
            return Some(bytes.len() > 1 && bytes[bytes.len() - 2] == b'\r');
        }
        // No-newline final line: indeterminate from this line alone.
        None
    }
    fn side_is_crlf(side: &[DiffLine<'_>]) -> Option<bool> {
        // Prefer the first line of the conflict region (git probes the
        // preceding line when available; for a refined single-line conflict
        // that is the conflict line itself).
        side.first().and_then(line_is_crlf)
    }
    fn ancestor_first_is_crlf(base_blob: &[u8]) -> Option<bool> {
        if base_blob.is_empty() {
            return None;
        }
        let end = base_blob
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(base_blob.len());
        let line = &base_blob[..end];
        if line.last() == Some(&b'\n') {
            Some(line.len() > 1 && line[line.len() - 2] == b'\r')
        } else {
            None
        }
    }
    match (
        side_is_crlf(ours),
        side_is_crlf(theirs),
        ancestor_first_is_crlf(base_blob),
    ) {
        (Some(true), Some(true), Some(true)) => true,
        (Some(true), Some(true), None) => true,
        _ => false,
    }
}

fn common_prefix_len(a: &[DiffLine<'_>], b: &[DiffLine<'_>]) -> usize {
    a.iter()
        .zip(b)
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix_len(a: &[DiffLine<'_>], b: &[DiffLine<'_>], prefix: usize) -> usize {
    let max = a.len().min(b.len()).saturating_sub(prefix);
    let mut len = 0;
    while len < max && a[a.len() - 1 - len] == b[b.len() - 1 - len] {
        len += 1;
    }
    len
}

#[cfg(test)]
mod merge_file_tests {
    use super::*;

    fn options() -> MergeFileOptions<'static> {
        MergeFileOptions {
            ours_label: "ours",
            base_label: "base",
            theirs_label: "theirs",
            ..MergeFileOptions::default()
        }
    }

    #[test]
    fn zealous_merge_absorbs_small_alphanumeric_separator() {
        let base = b"a\nbase-one\ncontext\nbase-two\nz\n";
        let ours = b"a\nours-one\ncontext\nours-two\nz\n";
        let theirs = b"a\ntheirs-one\ncontext\ntheirs-two\nz\n";
        let outcome = merge_file(base, ours, theirs, &options());
        assert_eq!(outcome.conflicts, 1);
        assert_eq!(
            outcome
                .content
                .windows(7)
                .filter(|window| *window == b"=======")
                .count(),
            1
        );
    }

    #[test]
    fn zealous_alnum_absorbs_more_than_three_blank_lines() {
        let base = b"a\nbase-one\n\n\n\n\nbase-two\nz\n";
        let ours = b"a\nours-one\n\n\n\n\nours-two\nz\n";
        let theirs = b"a\ntheirs-one\n\n\n\n\ntheirs-two\nz\n";
        let outcome = merge_file(base, ours, theirs, &options());
        assert_eq!(outcome.conflicts, 1);
    }

    #[test]
    fn diff3_does_not_apply_zealous_conflict_coalescing() {
        let base = b"a\nbase-one\ncontext\nbase-two\nz\n";
        let ours = b"a\nours-one\ncontext\nours-two\nz\n";
        let theirs = b"a\ntheirs-one\ncontext\ntheirs-two\nz\n";
        let mut options = options();
        options.style = ConflictStyle::Diff3;
        let outcome = merge_file(base, ours, theirs, &options);
        assert_eq!(outcome.conflicts, 2);
    }

    #[test]
    fn conflict_markers_follow_crlf_input() {
        let base = b"one\r\nbase\r\nthree";
        let ours = b"one\r\nours\r\nthree";
        let theirs = b"one\r\ntheirs\r\nthree";
        let outcome = merge_file(base, ours, theirs, &options());
        assert!(outcome.content.windows(9).any(|line| line == b"<<<<<<< o"));
        assert!(
            outcome
                .content
                .split(|byte| *byte == b'\n')
                .filter(|line| line.starts_with(b"<<<<<<<")
                    || line.starts_with(b"=======")
                    || line.starts_with(b">>>>>>>"))
                .all(|line| line.ends_with(b"\r"))
        );
    }
}
