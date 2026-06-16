//! Unified-diff / patch RENDERER: turn a computed file diff (the old/new
//! blob contents) into the textual unified-diff hunk body git's `diff.c`
//! emit path produces (`emit_diff_symbol` / `fn_out_consume`).
//!
//! This is the byte-for-byte port of git's hunk emitter: `@@ -os,oc +ns,nc @@
//! <heading>` hunk headers, the `+`/`-`/context lines, and the
//! `\ No newline at end of file` marker. It owns hunk *grouping* (combining
//! changes whose context windows overlap, `xdl_get_hunk`'s `distance >
//! max_common` break) and hunk *range* computation, then emits each hunk.
//!
//! What this module deliberately does NOT own (those stay with the caller,
//! which has the repository/userdiff/config context):
//!
//! * **The per-file metainfo header** (`diff --git`, `index`, `---`/`+++`,
//!   mode/similarity lines). That is repository- and option-shaped; the
//!   renderer only produces the hunk body that follows it.
//! * **Funcname section-heading resolution.** The caller supplies a
//!   [`HeadingFn`] closure that, given a candidate line, returns its section
//!   heading (git's `def_ff` default heuristic or a userdiff `xfuncname`
//!   pattern). The renderer does the *scan upward* for the nearest heading
//!   line; the caller only classifies a single line.
//! * **Word-diff body rendering.** When [`HunkRenderOptions::word_diff`] is
//!   set, the renderer delegates each hunk's body to a [`HunkWordDiff`] hook,
//!   which the caller implements over its own word-diff machinery.
//!
//! The seams keep the byte-shaping (ranges, headers, prefixes, no-newline
//! markers, color spans) here — the part every diff-emitting command used to
//! re-derive — while leaving the repository-coupled concerns in the consumer.

use crate::{
    DiffAlgorithm, DiffLine, DiffOp, WsIgnore, line_is_blank, myers_diff_lines_ws, split_lines,
};

/// git's default hunk context (`-U3`).
pub const DEFAULT_CONTEXT: usize = 3;

/// The per-line origin marker for an emitted diff line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    /// An unchanged (` `) line, present on both sides.
    Context,
    /// A removed (`-`) line, present only on the old side.
    Delete,
    /// An added (`+`) line, present only on the new side.
    Insert,
}

/// One line of the unified diff, with its origin and 0-based positions in the
/// old/new files (used to compute hunk ranges and feed the word-diff hook).
#[derive(Clone, Copy)]
pub struct TaggedLine<'a> {
    /// Whether the line is context / a deletion / an insertion.
    pub kind: LineKind,
    /// The raw line bytes, including the trailing `\n` when present.
    pub content: &'a [u8],
    /// 0-based index of this line on the old side.
    pub old_index: usize,
    /// 0-based index of this line on the new side.
    pub new_index: usize,
}

/// ANSI color palette for a unified diff, mirroring git's `diff_get_color`
/// slots. Each field is the raw escape sequence (empty string = no color).
///
/// The renderer only consults the slots it paints in the hunk body; the
/// per-file metainfo slot (`meta`) lives with the caller's header emitter and
/// is intentionally absent here.
#[derive(Clone, Copy)]
pub struct RenderColors<'a> {
    /// `color.diff.frag` — the `@@ .. @@` span.
    pub frag: &'a str,
    /// `color.diff.func` — the section heading after the frag.
    pub func: &'a str,
    /// `color.diff.old` — removed (`-`) lines.
    pub old: &'a str,
    /// `color.diff.new` — added (`+`) lines.
    pub new: &'a str,
    /// `color.diff.context` — context (` `) lines and the no-newline marker.
    pub context: &'a str,
    /// The reset sequence terminating each colored span.
    pub reset: &'a str,
    /// `color.diff.whitespace` — the highlight for whitespace errors
    /// (`--ws-error-highlight`).
    pub whitespace: &'a str,
}

/// Resolve the section heading for one candidate line.
///
/// Returns `Some(heading)` when `line` is a heading line (git's `def_ff`
/// default heuristic or a userdiff `xfuncname` match) and `None` otherwise.
/// The renderer scans upward from each hunk's first line and uses the first
/// `Some` it finds — the caller only has to classify a single line, so it can
/// keep its userdiff-driver / config resolution out of this crate.
pub type HeadingFn<'a> = dyn FnMut(&[u8]) -> Option<Vec<u8>> + 'a;

/// A hook that renders a single hunk's body when `--word-diff` is active.
///
/// The renderer feeds the hunk's tagged lines through this in order
/// (`fn_out_consume`'s `diff_words` branch): each removed line is pushed to
/// the minus buffer, each added line to the plus buffer, and a context line
/// flushes the accumulated word diff before emitting the context line itself.
/// The implementor owns the actual word-level rendering and color spans; this
/// keeps the word-diff machinery in the consumer.
pub trait HunkWordDiff {
    /// Buffer one removed line's content for the next word-diff flush.
    fn push_minus(&mut self, content: &[u8]);
    /// Buffer one added line's content for the next word-diff flush.
    fn push_plus(&mut self, content: &[u8]);
    /// Word-diff the accumulated minus/plus buffers into `out` and reset them.
    fn flush(&mut self, out: &mut Vec<u8>);
    /// Emit one context line (the `--word-diff` context style).
    fn emit_context_line(&mut self, out: &mut Vec<u8>, content: &[u8]);
}

/// Hunk-shaping and styling options for [`render_hunks`].
///
/// Lifetimes are split so the funcname / word-diff hooks can be borrowed
/// mutably while `colors` is borrowed shared.
pub struct HunkRenderOptions<'a, 'h> {
    /// Lines of context around each change (`-U<n>`, default
    /// [`DEFAULT_CONTEXT`]).
    pub context: usize,
    /// Extra inter-hunk merging distance (`--inter-hunk-context`).
    pub interhunk: usize,
    /// Per-line section-heading classifier; `None` emits headerless hunks.
    pub heading: Option<&'a mut HeadingFn<'h>>,
    /// ANSI palette when color output is enabled.
    pub colors: Option<RenderColors<'a>>,
    /// Word-diff body hook (replaces the `+`/`-` line bodies of each hunk).
    pub word_diff: Option<&'a mut dyn HunkWordDiff>,
    /// `--ws-error-highlight` configuration: when set and colors are on, the
    /// renderer paints whitespace errors on the selected line kinds with
    /// `colors.whitespace` (git's `emit_line_ws_markup`). `None` disables it.
    pub ws_error: Option<WsErrorHighlight>,
    /// Whitespace-ignore flags (`-w`, `-b`, `--ignore-space-at-eol`,
    /// `--ignore-cr-at-eol`): applied to the line-level comparison so
    /// whitespace-only changes do not appear as diffs (git's
    /// `XDF_WHITESPACE_FLAGS`).
    pub ws_ignore: WsIgnore,
    /// The line-diff algorithm to use (Myers / patience / histogram).
    pub algorithm: DiffAlgorithm,
    /// Change-group suppression (`--ignore-blank-lines`, `-I<regex>`): when
    /// set, change groups all of whose old and new lines are blank (and/or
    /// match a `-I` regex) are dropped from hunk emission, mirroring git's
    /// `xdl_mark_ignorable_lines` / `xdl_mark_ignorable_regex` + `xdl_get_hunk`.
    pub change_ignore: Option<&'a ChangeIgnore<'a>>,
    /// `log -L`: restrict the emitted hunks to the new-side (post-image) line
    /// ranges. Each range is 0-based, `[start, end)`. When set, the renderer
    /// inflates context to the widest range span (so every change inside a
    /// range merges into one xdiff hunk), then clips the emitted lines back to
    /// the range boundaries — a port of diff.c's `line_range_*` callbacks.
    /// Ranges must be sorted and disjoint. `None` disables the filter (every
    /// non-line-log caller).
    pub line_ranges: Option<&'a [LineRange]>,
}

/// A half-open `[start, end)` line range (0-based) for `log -L` hunk
/// restriction. Mirrors diff.c's `struct range`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRange {
    /// 0-based inclusive start line (post-image).
    pub start: i64,
    /// 0-based exclusive end line (post-image).
    pub end: i64,
}

/// Configuration for change-group suppression (`--ignore-blank-lines` and
/// `-I<regex>`). A change group is *ignorable* iff every old line and every new
/// line it touches is blank (when `ignore_blank_lines`) or matches one of the
/// `-I` regexes (`regex_match`). Ignorable groups are kept out of hunk emission
/// per `xdl_get_hunk`'s leading/isolated-ignorable removal.
pub struct ChangeIgnore<'a> {
    /// `--ignore-blank-lines`: blank change groups are ignorable.
    pub ignore_blank_lines: bool,
    /// `-I<regex>`: a line is regex-ignorable when this returns `true`. The
    /// closure receives the raw line bytes (including the trailing `\n`). When
    /// `None`, no regex suppression applies.
    pub regex_match: Option<&'a dyn Fn(&[u8]) -> bool>,
}

/// Which line kinds get whitespace-error highlighting, plus the rule to check
/// against. git's `--ws-error-highlight` defaults to highlighting only new
/// (`+`) lines.
#[derive(Clone, Copy)]
pub struct WsErrorHighlight {
    /// The resolved whitespace rule to check each line against.
    pub rule: crate::ws::WsRule,
    /// Highlight errors on removed (`-`) lines.
    pub old: bool,
    /// Highlight errors on added (`+`) lines.
    pub new: bool,
    /// Highlight errors on context (` `) lines.
    pub context: bool,
}

impl Default for HunkRenderOptions<'_, '_> {
    fn default() -> Self {
        Self {
            context: DEFAULT_CONTEXT,
            interhunk: 0,
            heading: None,
            colors: None,
            word_diff: None,
            ws_error: None,
            ws_ignore: WsIgnore::default(),
            algorithm: DiffAlgorithm::Myers,
            change_ignore: None,
            line_ranges: None,
        }
    }
}

/// Render the unified-diff hunk body for a single file change into `out`.
///
/// `old_content` / `new_content` are the full blob contents (`None` for an
/// absent side — a created or deleted file). The function computes the
/// line-level Myers diff, groups changes into hunks with `options.context`
/// lines of surrounding context (merging nearby groups per
/// `options.interhunk`), and emits each hunk: the `@@` header (with git's
/// section heading), then the context / `-` / `+` lines including
/// `\ No newline at end of file` markers.
///
/// Nothing is written when the contents are identical (no changed lines).
/// This is the body *after* the per-file metainfo header the caller emits.
pub fn render_hunks(
    out: &mut Vec<u8>,
    old_content: Option<&[u8]>,
    new_content: Option<&[u8]>,
    options: &mut HunkRenderOptions<'_, '_>,
) {
    // `log -L` hunk restriction: render with inflated context into a scratch
    // buffer, then clip the emitted lines to the tracked ranges (diff.c's
    // `line_range_*` callbacks). The widest range span is the upper bound on
    // the context needed for every change in a range to land in one hunk.
    if let Some(ranges) = options.line_ranges {
        let max_span = ranges
            .iter()
            .map(|r| r.end - r.start)
            .max()
            .unwrap_or(0)
            .max(0) as usize;
        let saved_context = options.context;
        options.context = saved_context.max(max_span);
        options.line_ranges = None;
        let mut full = Vec::new();
        render_hunks(&mut full, old_content, new_content, options);
        options.context = saved_context;
        options.line_ranges = Some(ranges);
        filter_hunks_to_ranges(out, &full, ranges);
        return;
    }
    let old = split_lines(old_content.unwrap_or_default());
    let new = split_lines(new_content.unwrap_or_default());
    let ops = myers_diff_lines_ws(&old, &new, options.ws_ignore, options.algorithm);

    // Flatten the edit script into a tagged line stream carrying old/new
    // positions.
    let mut tagged: Vec<TaggedLine<'_>> = Vec::new();
    let mut old_idx = 0usize;
    let mut new_idx = 0usize;
    for op in ops {
        match op {
            DiffOp::Equal(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Context,
                        content: old[old_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    old_idx += 1;
                    new_idx += 1;
                }
            }
            DiffOp::Delete(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Delete,
                        content: old[old_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    old_idx += 1;
                }
            }
            DiffOp::Insert(n) => {
                for _ in 0..n {
                    tagged.push(TaggedLine {
                        kind: LineKind::Insert,
                        content: new[new_idx].content,
                        old_index: old_idx,
                        new_index: new_idx,
                    });
                    new_idx += 1;
                }
            }
        }
    }

    // Build the change list (git's xdchange script): each maximal run of
    // consecutive `-`/`+` tagged lines is one change, carrying its old/new line
    // ranges and the tagged-stream span it occupies.
    let changes = build_changes(&tagged);
    if changes.is_empty() {
        return;
    }

    // Mark each change ignorable when `--ignore-blank-lines` / `-I<regex>`
    // applies and every old and new line it touches is blank / regex-matched
    // (git's xdl_mark_ignorable_lines + xdl_mark_ignorable_regex).
    let mut changes = changes;
    if let Some(ci) = options.change_ignore {
        mark_ignorable_changes(&mut changes, &old, &new, options.ws_ignore, ci);
    }

    // Group changes into hunks (xdl_get_hunk): the `distance > max_common`
    // break plus leading/isolated-ignorable-change removal. Each hunk is a
    // tagged-stream `(first_change_pos, last_change_pos)` span of *real*
    // (emitted) changes.
    let groups = group_changes_into_hunks(&changes, options.context, options.interhunk);

    for (first_change, last_change) in groups {
        let hunk_start = first_change.saturating_sub(options.context);
        let hunk_end = (last_change + options.context + 1).min(tagged.len());
        render_one_hunk(out, &tagged, &old, hunk_start, hunk_end, options);
    }
}

/// State for [`filter_hunks_to_ranges`], a port of diff.c's
/// `struct line_range_callback`. We drive it over the already-rendered
/// unified-diff lines (with inflated context) rather than xdiff's raw line
/// callback, but the algorithm is the same: buffer pending removals, open a
/// range hunk when an in-range post-image line is seen, and emit the clipped
/// `@@` header + body for each range.
struct RangeFilter<'r> {
    ranges: &'r [LineRange],
    cur_range: usize,
    /// Post/pre-image 1-based line counters seeded from each `@@` header.
    lno_post: i64,
    lno_pre: i64,
    /// Function-name heading carried from the current `@@` header (the suffix
    /// after `@@ ... @@ `), reused verbatim on every emitted range hunk.
    func: Vec<u8>,
    /// Range hunk being accumulated.
    rhunk: Vec<u8>,
    rhunk_old_begin: i64,
    rhunk_old_count: i64,
    rhunk_new_begin: i64,
    rhunk_new_count: i64,
    rhunk_active: bool,
    rhunk_has_changes: bool,
    /// Removal lines not yet known to be in-range.
    pending_rm: Vec<u8>,
    pending_rm_count: i64,
    pending_rm_pre_begin: i64,
}

impl RangeFilter<'_> {
    fn discard_pending_rm(&mut self) {
        self.pending_rm.clear();
        self.pending_rm_count = 0;
    }

    /// Port of diff.c:flush_rhunk — emit the accumulated range hunk (header +
    /// body) into `out`, dropping context-only hunks.
    fn flush_rhunk(&mut self, out: &mut Vec<u8>) {
        if !self.rhunk_active {
            return;
        }
        if self.pending_rm_count != 0 {
            self.rhunk.extend_from_slice(&self.pending_rm);
            self.rhunk_old_count += self.pending_rm_count;
            self.rhunk_has_changes = true;
            self.discard_pending_rm();
        }
        if !self.rhunk_has_changes {
            self.rhunk_active = false;
            self.rhunk.clear();
            return;
        }
        // git's flush_rhunk uses `@@ -%ld,%ld +%ld,%ld @@` unconditionally —
        // the count is ALWAYS shown (unlike the normal emitter, which omits a
        // count of 1).
        out.extend_from_slice(
            format!(
                "@@ -{},{} +{},{} @@",
                self.rhunk_old_begin,
                self.rhunk_old_count,
                self.rhunk_new_begin,
                self.rhunk_new_count
            )
            .as_bytes(),
        );
        if !self.func.is_empty() {
            out.push(b' ');
            out.extend_from_slice(&self.func);
        }
        out.push(b'\n');
        out.extend_from_slice(&self.rhunk);
        self.rhunk_active = false;
        self.rhunk.clear();
    }

    /// Port of diff.c:line_range_line_fn for one rendered body line. `marker`
    /// is the first byte (`' '`/`'+'`/`'-'`/`'\\'`), `line` the full bytes.
    fn body_line(&mut self, out: &mut Vec<u8>, marker: u8, line: &[u8]) {
        if marker == b'-' {
            if self.pending_rm_count == 0 {
                self.pending_rm_pre_begin = self.lno_pre;
            }
            self.lno_pre += 1;
            self.pending_rm.extend_from_slice(line);
            self.pending_rm_count += 1;
            return;
        }
        if marker == b'\\' {
            if self.pending_rm_count != 0 {
                self.pending_rm.extend_from_slice(line);
            } else if self.rhunk_active {
                self.rhunk.extend_from_slice(line);
            }
            return;
        }
        // marker is '+' or ' '
        let lno_0 = self.lno_post - 1;
        let cur_pre = self.lno_pre;
        self.lno_post += 1;
        if marker == b' ' {
            self.lno_pre += 1;
        }

        while self.cur_range < self.ranges.len() && lno_0 >= self.ranges[self.cur_range].end {
            if self.rhunk_active {
                self.flush_rhunk(out);
            }
            self.discard_pending_rm();
            self.cur_range += 1;
        }
        if self.cur_range >= self.ranges.len() {
            self.discard_pending_rm();
            return;
        }
        let cur = self.ranges[self.cur_range];
        if lno_0 < cur.start {
            self.discard_pending_rm();
            return;
        }
        if !self.rhunk_active {
            self.rhunk_active = true;
            self.rhunk_has_changes = false;
            self.rhunk_new_begin = lno_0 + 1;
            self.rhunk_old_begin = if self.pending_rm_count != 0 {
                self.pending_rm_pre_begin
            } else {
                cur_pre
            };
            self.rhunk_old_count = 0;
            self.rhunk_new_count = 0;
            self.rhunk.clear();
        }
        if self.pending_rm_count != 0 {
            self.rhunk.extend_from_slice(&self.pending_rm);
            self.rhunk_old_count += self.pending_rm_count;
            self.rhunk_has_changes = true;
            self.discard_pending_rm();
        }
        self.rhunk.extend_from_slice(line);
        self.rhunk_new_count += 1;
        if marker == b'+' {
            self.rhunk_has_changes = true;
        } else {
            self.rhunk_old_count += 1;
        }
    }
}

/// Clip a fully-rendered unified-diff hunk body (`full`, produced with
/// inflated context) down to the tracked `ranges`, mirroring diff.c's
/// `line_range_hunk_fn` / `line_range_line_fn` / `flush_rhunk`. The renderer's
/// `@@` header already carries the funcname suffix; we parse it back out and
/// reuse it on every emitted range hunk. No-color path only (`log -L` test
/// output is uncolored).
fn filter_hunks_to_ranges(out: &mut Vec<u8>, full: &[u8], ranges: &[LineRange]) {
    if ranges.is_empty() {
        return;
    }
    let mut filter = RangeFilter {
        ranges,
        cur_range: 0,
        lno_post: 0,
        lno_pre: 0,
        func: Vec::new(),
        rhunk: Vec::new(),
        rhunk_old_begin: 0,
        rhunk_old_count: 0,
        rhunk_new_begin: 0,
        rhunk_new_count: 0,
        rhunk_active: false,
        rhunk_has_changes: false,
        pending_rm: Vec::new(),
        pending_rm_count: 0,
        pending_rm_pre_begin: 0,
    };
    for line in split_keep_newline(full) {
        if line.starts_with(b"@@ ") {
            // New xdiff hunk: any pending removals from the previous hunk are
            // left in place (diff.c does the same — the next body line decides
            // their fate), and the range hunk cursor is NOT reset across xdiff
            // hunks. Parse the begin line numbers + funcname suffix.
            if let Some((old_begin, new_begin, func)) = parse_hunk_header(line) {
                filter.lno_post = new_begin;
                filter.lno_pre = old_begin;
                filter.func = func;
            }
            continue;
        }
        let marker = line.first().copied().unwrap_or(b' ');
        filter.body_line(out, marker, line);
    }
    filter.flush_rhunk(out);
}

/// Split `buf` into lines, each INCLUDING its trailing `\n` (the final line may
/// lack one). Empty input yields no lines.
fn split_keep_newline(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut start = 0usize;
    std::iter::from_fn(move || {
        if start >= buf.len() {
            return None;
        }
        let rel = buf[start..].iter().position(|&b| b == b'\n');
        let end = match rel {
            Some(pos) => start + pos + 1,
            None => buf.len(),
        };
        let line = &buf[start..end];
        start = end;
        Some(line)
    })
}

/// Parse a rendered `@@ -o[,c] +n[,c] @@[ func]` header, returning the 1-based
/// old/new begin line numbers and the trailing funcname bytes (without the
/// leading space, without a trailing newline). Begin is the value xdiff emits:
/// 1-based when the count is non-zero, one-less when zero (unused in that case).
fn parse_hunk_header(line: &[u8]) -> Option<(i64, i64, Vec<u8>)> {
    // line = "@@ -A,B +C,D @@ func\n" (or "@@ -A +C @@\n", etc.)
    let rest = line.strip_prefix(b"@@ -")?;
    let plus = rest.iter().position(|&b| b == b'+')?;
    let old_part = &rest[..plus];
    // skip "+", parse new part up to " @@"
    let after_plus = &rest[plus + 1..];
    let close = find_subslice(after_plus, b" @@")?;
    let new_part = &after_plus[..close];
    let old_begin = parse_range_begin(old_part.split(|&b| b == b' ').next().unwrap_or(old_part))?;
    let new_begin = parse_range_begin(new_part)?;
    // Funcname suffix: everything after " @@ " (a single space separates it).
    let tail = &after_plus[close + 3..];
    let func = if let Some(f) = tail.strip_prefix(b" ") {
        let mut f = f.to_vec();
        if f.last() == Some(&b'\n') {
            f.pop();
        }
        f
    } else {
        Vec::new()
    };
    Some((old_begin, new_begin, func))
}

/// Parse the "A" or "A,B" begin field of an `@@` range side into A.
fn parse_range_begin(field: &[u8]) -> Option<i64> {
    let begin = field.split(|&b| b == b',').next().unwrap_or(field);
    std::str::from_utf8(begin).ok()?.trim().parse::<i64>().ok()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// One contiguous change in the edit script (git's `xdchange`): the old/new
/// line ranges it covers, the tagged-stream positions of its first/last
/// non-context line, and whether it is ignorable (`--ignore-blank-lines` /
/// `-I<regex>`).
#[derive(Clone, Copy)]
struct Change {
    /// 0-based first old line in this change (`i1`).
    i1: usize,
    /// Number of old lines deleted (`chg1`).
    chg1: usize,
    /// 0-based first new line in this change (`i2`).
    i2: usize,
    /// Number of new lines inserted (`chg2`).
    chg2: usize,
    /// Tagged-stream index of this change's first non-context line.
    tag_first: usize,
    /// Tagged-stream index of this change's last non-context line.
    tag_last: usize,
    /// Whether this change is ignorable (blank-only / regex-only).
    ignore: bool,
}

/// Build the change list (xdchange script) from the flattened tagged stream.
/// Each maximal run of consecutive `-`/`+` lines becomes one [`Change`].
fn build_changes(tagged: &[TaggedLine<'_>]) -> Vec<Change> {
    let mut changes: Vec<Change> = Vec::new();
    let mut idx = 0usize;
    while idx < tagged.len() {
        if tagged[idx].kind == LineKind::Context {
            idx += 1;
            continue;
        }
        let tag_first = idx;
        let i1 = tagged[idx].old_index;
        let i2 = tagged[idx].new_index;
        let mut chg1 = 0usize;
        let mut chg2 = 0usize;
        while idx < tagged.len() && tagged[idx].kind != LineKind::Context {
            match tagged[idx].kind {
                LineKind::Delete => chg1 += 1,
                LineKind::Insert => chg2 += 1,
                LineKind::Context => unreachable!(),
            }
            idx += 1;
        }
        changes.push(Change {
            i1,
            chg1,
            i2,
            chg2,
            tag_first,
            tag_last: idx - 1,
            ignore: false,
        });
    }
    changes
}

/// Mark each change ignorable when its old and new lines are all blank
/// (`--ignore-blank-lines`) or all regex-matched (`-I<regex>`), mirroring
/// git's `xdl_mark_ignorable_lines` then `xdl_mark_ignorable_regex` (the regex
/// pass never overrides a blank-marked change).
fn mark_ignorable_changes(
    changes: &mut [Change],
    old: &[DiffLine<'_>],
    new: &[DiffLine<'_>],
    ws_ignore: WsIgnore,
    ci: &ChangeIgnore<'_>,
) {
    for change in changes.iter_mut() {
        if ci.ignore_blank_lines {
            let blank = (change.i1..change.i1 + change.chg1)
                .all(|i| line_is_blank(old[i].content, ws_ignore))
                && (change.i2..change.i2 + change.chg2)
                    .all(|i| line_is_blank(new[i].content, ws_ignore));
            change.ignore = blank;
        }
        if !change.ignore {
            if let Some(regex_match) = ci.regex_match {
                let matched = (change.i1..change.i1 + change.chg1)
                    .all(|i| regex_match(old[i].content))
                    && (change.i2..change.i2 + change.chg2).all(|i| regex_match(new[i].content));
                change.ignore = matched;
            }
        }
    }
}

/// Group the change list into hunks, returning each hunk's
/// `(first_real_change_tag, last_real_change_tag)` tagged-stream span. This is
/// a behavioural port of git's `xemit.c:xdl_get_hunk` driving
/// `xdl_call_hunk_func`'s loop: it applies the `distance > max_common` hunk
/// break, drops leading ignorable changes, and excludes trailing/isolated
/// ignorable changes from the emitted span.
fn group_changes_into_hunks(
    changes: &[Change],
    context: usize,
    interhunk: usize,
) -> Vec<(usize, usize)> {
    let max_common = context.saturating_add(context).saturating_add(interhunk);
    let max_ignorable = context;

    let mut hunks: Vec<(usize, usize)> = Vec::new();
    // `start` is the index into `changes` of the first change still to emit
    // (xdl_call_hunk_func's `xch` cursor).
    let mut start = 0usize;
    while start < changes.len() {
        // Remove ignorable changes that are too far before other changes
        // (xdl_get_hunk's leading-ignorable loop). Faithful port: `xchp`
        // iterates over EVERY leading ignorable change; whenever the gap to the
        // next change is ≥ max_ignorable (or there is no next change), the hunk
        // start advances to that next change (`None` ⇒ the whole tail is
        // ignorable ⇒ no hunk). Unlike a break-on-first-keep loop, this still
        // advances past a run of ignorables when the FINAL one has no successor.
        {
            let mut xchp = start;
            while xchp < changes.len() && changes[xchp].ignore {
                let cur = &changes[xchp];
                match changes.get(xchp + 1) {
                    None => {
                        start = changes.len();
                    }
                    Some(next) => {
                        if next.i1 - (cur.i1 + cur.chg1) >= max_ignorable {
                            start = xchp + 1;
                        }
                    }
                }
                xchp += 1;
            }
        }
        if start >= changes.len() {
            break;
        }

        // Walk forward extending the hunk; `last` tracks the last *real*
        // (non-ignorable, or the very first) change that defines the hunk end.
        let mut last = start;
        let mut ignored = 0usize; // ignored new-line count accumulated
        let mut prev = start;
        let mut idx = start + 1;
        while idx < changes.len() {
            let xch = &changes[idx];
            let xchp = &changes[prev];
            let distance = xch.i1 - (xchp.i1 + xchp.chg1);
            if distance > max_common {
                break;
            }
            if distance < max_ignorable && (!xch.ignore || last == prev) {
                last = idx;
                ignored = 0;
            } else if distance < max_ignorable && xch.ignore {
                ignored += xch.chg2;
            } else if last != prev
                && xch.i1 + ignored - (changes[last].i1 + changes[last].chg1) > max_common
            {
                break;
            } else if !xch.ignore {
                last = idx;
                ignored = 0;
            } else {
                ignored += xch.chg2;
            }
            prev = idx;
            idx += 1;
        }

        let first_change = &changes[start];
        let last_change = &changes[last];
        hunks.push((first_change.tag_first, last_change.tag_last));
        start = last + 1;
    }

    hunks
}

/// Emit a single hunk covering `tagged[start..end]`: the `@@ -os,oc +ns,nc @@
/// <heading>` header followed by the context/`-`/`+` lines, including the
/// `\ No newline at end of file` markers.
fn render_one_hunk(
    out: &mut Vec<u8>,
    tagged: &[TaggedLine<'_>],
    old_lines: &[DiffLine<'_>],
    start: usize,
    end: usize,
    options: &mut HunkRenderOptions<'_, '_>,
) {
    let slice = &tagged[start..end];
    let mut old_count = 0usize;
    let mut new_count = 0usize;
    for line in slice {
        match line.kind {
            LineKind::Context => {
                old_count += 1;
                new_count += 1;
            }
            LineKind::Delete => old_count += 1,
            LineKind::Insert => new_count += 1,
        }
    }
    // 1-based starting line numbers; an empty side starts at 0.
    let old_start = if old_count == 0 {
        slice.first().map(|line| line.old_index).unwrap_or(0)
    } else {
        slice
            .iter()
            .find(|line| line.kind != LineKind::Insert)
            .map(|line| line.old_index + 1)
            .unwrap_or(1)
    };
    let new_start = if new_count == 0 {
        slice.first().map(|line| line.new_index).unwrap_or(0)
    } else {
        slice
            .iter()
            .find(|line| line.kind != LineKind::Delete)
            .map(|line| line.new_index + 1)
            .unwrap_or(1)
    };

    let heading = hunk_section_heading(
        old_lines,
        slice.first().map(|line| line.old_index),
        options.heading.as_deref_mut(),
    );
    let frag = format!(
        "@@ -{} +{} @@",
        format_hunk_range(old_start, old_count),
        format_hunk_range(new_start, new_count)
    );
    match options.colors {
        // Port of emit_hunk_header: the "@@ .. @@" span in the frag color,
        // the separating blank in the context color, the heading in the func
        // color (each reset-terminated).
        Some(colors) => {
            out.extend_from_slice(colors.frag.as_bytes());
            out.extend_from_slice(frag.as_bytes());
            out.extend_from_slice(colors.reset.as_bytes());
            if let Some(heading) = &heading {
                out.extend_from_slice(colors.context.as_bytes());
                out.push(b' ');
                out.extend_from_slice(colors.reset.as_bytes());
                out.extend_from_slice(colors.func.as_bytes());
                out.extend_from_slice(heading);
                out.extend_from_slice(colors.reset.as_bytes());
            }
            out.push(b'\n');
        }
        None => {
            out.extend_from_slice(frag.as_bytes());
            if let Some(heading) = &heading {
                out.push(b' ');
                out.extend_from_slice(heading);
            }
            out.push(b'\n');
        }
    }

    if let Some(word_diff) = options.word_diff.as_deref_mut() {
        // Word-diff rendering: minus/plus runs accumulate and flush at
        // context lines (fn_out_consume's diff_words branch); the
        // "\ No newline" markers are eaten.
        for line in slice {
            match line.kind {
                LineKind::Delete => word_diff.push_minus(line.content),
                LineKind::Insert => word_diff.push_plus(line.content),
                LineKind::Context => {
                    word_diff.flush(out);
                    word_diff.emit_context_line(out, line.content);
                }
            }
        }
        word_diff.flush(out);
        return;
    }

    for line in slice {
        let prefix = match line.kind {
            LineKind::Context => b' ',
            LineKind::Delete => b'-',
            LineKind::Insert => b'+',
        };
        match options.colors {
            Some(colors) => {
                // Whitespace-error highlighting applies to the selected line
                // kinds (default: new lines only).
                let ws_rule = options.ws_error.and_then(|ws| {
                    let enabled = match line.kind {
                        LineKind::Context => ws.context,
                        LineKind::Delete => ws.old,
                        LineKind::Insert => ws.new,
                    };
                    enabled.then_some(ws.rule)
                });
                write_patch_line_colored(out, prefix, line.content, colors, ws_rule);
            }
            None => write_patch_line(out, prefix, line.content),
        }
    }
}

/// Format one `start,count` side of an `@@` header. git omits the count when
/// it is exactly 1 (e.g. `+5` rather than `+5,1`).
fn format_hunk_range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

/// git's section heading for a hunk: the nearest line *before* the hunk's
/// first line accepted by the caller's `heading` classifier. Headings are
/// produced by the classifier (already capped/trimmed by the caller's
/// userdiff machinery). Returns `None` when no such line precedes the hunk or
/// no classifier was supplied.
fn hunk_section_heading(
    old_lines: &[DiffLine<'_>],
    first_old_index: Option<usize>,
    mut heading: Option<&mut HeadingFn<'_>>,
) -> Option<Vec<u8>> {
    let first = first_old_index?;
    let classifier = heading.as_mut()?;
    // Scan upward from the line just above the hunk.
    for idx in (0..first).rev() {
        if let Some(found) = classifier(old_lines[idx].content) {
            return Some(found);
        }
    }
    None
}

/// Write a single diff line with its `prefix` marker, appending the
/// `\ No newline at end of file` note when the source line lacks a trailing
/// LF.
fn write_patch_line(out: &mut Vec<u8>, prefix: u8, line: &[u8]) {
    out.push(prefix);
    out.extend_from_slice(line);
    if !line.ends_with(b"\n") {
        out.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

/// [`write_patch_line`] in color, optionally painting whitespace errors.
///
/// When `ws_rule` is `Some`, the line body is emitted through
/// [`crate::ws::ws_check_emit`] (git's `emit_line_ws_markup` highlighted
/// branch): the sign is painted in the line color, then the body's non-error
/// segments in the line color and its whitespace-error segments in
/// `colors.whitespace`. A clean line produces no whitespace spans, so it stays
/// visually plain.
///
/// When `ws_rule` is `None`, context/old lines paint the sign and body in one
/// span; new lines paint the sign and body as separate spans (the default
/// `ws-error-highlight` path with no rule).
fn write_patch_line_colored(
    out: &mut Vec<u8>,
    prefix: u8,
    line: &[u8],
    colors: RenderColors<'_>,
    ws_rule: Option<crate::ws::WsRule>,
) {
    let (body, terminated) = match line.split_last() {
        Some((b'\n', body)) => (body, true),
        _ => (line, false),
    };
    let color = match prefix {
        b'-' => colors.old,
        b'+' => colors.new,
        _ => colors.context,
    };

    if let Some(rule) = ws_rule {
        // Sign in the line color, then the body through ws_check_emit (no
        // trailing newline in `body`, so the emit's own LF handling is inert).
        out.extend_from_slice(color.as_bytes());
        out.push(prefix);
        out.extend_from_slice(colors.reset.as_bytes());
        let emit_colors = crate::ws::WsEmitColors {
            set: color,
            reset: colors.reset,
            ws: colors.whitespace,
        };
        crate::ws::ws_check_emit(body, rule, out, &emit_colors);
        out.push(b'\n');
        if !terminated {
            out.extend_from_slice(colors.context.as_bytes());
            out.extend_from_slice(b"\\ No newline at end of file");
            out.extend_from_slice(colors.reset.as_bytes());
            out.push(b'\n');
        }
        return;
    }

    if prefix == b'+' {
        out.extend_from_slice(color.as_bytes());
        out.push(prefix);
        out.extend_from_slice(colors.reset.as_bytes());
        if !body.is_empty() {
            out.extend_from_slice(color.as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(colors.reset.as_bytes());
        }
    } else {
        out.extend_from_slice(color.as_bytes());
        out.push(prefix);
        out.extend_from_slice(body);
        out.extend_from_slice(colors.reset.as_bytes());
    }
    out.push(b'\n');
    if !terminated {
        out.extend_from_slice(colors.context.as_bytes());
        out.extend_from_slice(b"\\ No newline at end of file");
        out.extend_from_slice(colors.reset.as_bytes());
        out.push(b'\n');
    }
}

// ===========================================================================
// Combined / merge-commit diff renderer (`-c` / `--cc`).
//
// This is the byte-for-byte port of git's `combine-diff.c`
// (`combine_diff` / `make_hunks` / `dump_sline`): the multi-parent
// `@@@ -p1 -p2 +out @@@` hunk header (with one extra `@` per parent and one
// `-pN,cN` column per parent), the per-parent prefix columns on each body
// line, and the `--cc` "dense" simplification that drops hunks whose result
// matches at least one parent ("uninteresting" hunks).
//
// The renderer is repository-agnostic, exactly like the unified-diff renderer
// above: the caller supplies the *result* blob and one blob per parent (plus
// the line-diff algorithm / whitespace flags), and we emit only the hunk body
// — the per-file `diff --cc`/`index`/`---`/`+++` metainfo header stays with the
// command, which owns the repository / oid / mode context.
//
// Data-structure correspondence with combine-diff.c:
//   * `Sline`  <-> `struct sline` (one per result line, plus a trailing
//     sentinel `sline[cnt]` whose `bol` is empty),
//   * `Sline.lost` <-> `struct lline` list (lines deleted relative to some
//     parent, hung before the surviving result line),
//   * the `flag` bitset: bit `n` set => parent `n` did NOT have this result
//     line (i.e. it was added relative to parent `n`); bit `num_parent`
//     ("mark") => the line is part of a shown hunk; bit `num_parent+1`
//     ("no_pre_delete") => suppress the leading deletions before this line.
// ===========================================================================

/// One result line in the combined-diff `sline` array, plus the deletions
/// ("lost" lines) that hang in front of it.
struct CdLine {
    /// The surviving result line bytes, WITHOUT the trailing newline.
    bol: Vec<u8>,
    /// Deletions hung before this line, in display order. `parent_map` is the
    /// bitset of parents that had the deleted line (git coalesces these across
    /// parents via an LCS; we replicate that with [`coalesce_lost`]).
    lost: Vec<CdLost>,
    /// Pre-coalesce deletions accumulated for the parent currently being
    /// folded (drained into `lost` after each parent, like git's `plost`).
    plost: Vec<Vec<u8>>,
    /// `flag` bitset (see module comment).
    flag: u64,
    /// `p_lno[n]` = 1-based line number in parent `n` at which a hunk starting
    /// at this result line begins. Sized `num_parent`.
    p_lno: Vec<u64>,
}

/// A single deleted ("lost") line and the set of parents it was removed from.
struct CdLost {
    line: Vec<u8>,
    parent_map: u64,
}

/// Options controlling the combined-diff body emission.
pub struct CombinedRenderOptions {
    /// `--cc` dense simplification (drop hunks the result shares with a parent);
    /// `-c` (plain combined) leaves it `false`.
    pub dense: bool,
    /// Unified-context line count (`-U`, default 3).
    pub context: usize,
    /// Line-diff algorithm used for each parent-vs-result 2-way diff.
    pub algorithm: DiffAlgorithm,
    /// Whitespace-ignore flags applied to each parent-vs-result 2-way diff and
    /// to the lost-line coalescing match.
    pub ws_ignore: WsIgnore,
}

impl Default for CombinedRenderOptions {
    fn default() -> Self {
        Self {
            dense: true,
            context: DEFAULT_CONTEXT,
            algorithm: DiffAlgorithm::Myers,
            ws_ignore: WsIgnore::default(),
        }
    }
}

/// Render a combined / merge diff body into `out`.
///
/// `result` is the merge-result blob; `parents` holds one blob per parent (in
/// parent order). Returns `true` when at least one hunk survives the
/// "interesting" filter — the caller uses this to decide whether to print the
/// metainfo header at all (git only prints `diff --cc <path>` + body when
/// `show_hunks || mode_differs`).
///
/// Mirrors `show_patch_diff`'s body half: build the `sline` array, fold each
/// parent into it via [`combine_one_parent`], run [`make_hunks`], then
/// [`dump_sline`].
pub fn render_combined(out: &mut Vec<u8>, result: &[u8], parents: &[&[u8]]) -> bool {
    render_combined_with(out, result, parents, &CombinedRenderOptions::default())
}

/// [`render_combined`] with explicit options.
pub fn render_combined_with(
    out: &mut Vec<u8>,
    result: &[u8],
    parents: &[&[u8]],
    options: &CombinedRenderOptions,
) -> bool {
    let num_parent = parents.len();
    debug_assert!(num_parent >= 1);

    // Split the result into lines (without trailing newline), counting an
    // unterminated final line as its own line — git's `cnt` counts '\n' plus an
    // incomplete trailing line.
    let result_lines = split_lines(result);
    let cnt = result_lines.len();

    // git allocates `cnt + 2` slines: indices `0..cnt-1` are the result lines,
    // `sline[cnt]` is the trailing sentinel (where end-of-file deletions hang),
    // and `sline[cnt+1]` carries the per-parent trailer p_lno that
    // `show_parent_lno` reads for a hunk whose end touches the last line.
    let mut sline: Vec<CdLine> = Vec::with_capacity(cnt + 2);
    for line in &result_lines {
        sline.push(CdLine {
            bol: line.bytes_without_newline().to_vec(),
            lost: Vec::new(),
            plost: Vec::new(),
            flag: 0,
            p_lno: vec![0; num_parent],
        });
    }
    for _ in 0..2 {
        sline.push(CdLine {
            bol: Vec::new(),
            lost: Vec::new(),
            plost: Vec::new(),
            flag: 0,
            p_lno: vec![0; num_parent],
        });
    }

    // Fold each parent into the sline array. git reuses an earlier parent's
    // result when two parents have the identical blob (`reuse_combine_diff`);
    // we replicate that to keep p_lno / flags identical.
    for n in 0..num_parent {
        let mut reused = None;
        for j in 0..n {
            if parents[j] == parents[n] {
                reused = Some(j);
                break;
            }
        }
        match reused {
            Some(j) => reuse_combine_diff(&mut sline, cnt, n, j),
            None => combine_one_parent(&mut sline, &result_lines, parents[n], n, options),
        }
    }

    let show_hunks = make_hunks(&mut sline, cnt, num_parent, options.dense, options.context);
    if show_hunks {
        dump_sline(out, &sline, cnt, num_parent, options.context);
    }
    show_hunks
}

/// Fold one parent's 2-way diff against the result into the `sline` array
/// (git's `combine_diff` + the consume_hunk/consume_line callbacks).
fn combine_one_parent(
    sline: &mut [CdLine],
    result_lines: &[DiffLine<'_>],
    parent: &[u8],
    n: usize,
    options: &CombinedRenderOptions,
) {
    let cnt = result_lines.len();
    let nmask = 1u64 << n;
    let parent_lines = split_lines(parent);
    let ops = myers_diff_lines_ws(&parent_lines, result_lines, options.ws_ignore, options.algorithm);

    // Walk the edit script, tracking the 1-based result line number (`lno`,
    // git's `state->lno`) and the parent line number (`p_lno`/`ob`). For each
    // hunk: deletions hang on `lost_bucket`; insertions set the nmask flag on
    // the result line; the hunk start records the parent line number into
    // `p_lno[n]` of the result line preceding the hunk.
    //
    // git groups the script into hunks (runs separated by Equal context); we
    // mirror consume_hunk by detecting the boundary at each non-Equal run.
    let mut old_idx: usize = 0; // 0-based parent line consumed
    let mut new_idx: usize = 0; // 0-based result line consumed
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            DiffOp::Equal(k) => {
                old_idx += k;
                new_idx += k;
                i += 1;
            }
            _ => {
                // Collect a maximal run of consecutive Delete/Insert ops as one
                // hunk (git's xdiff emits one @@ hunk per such run).
                let hunk_old_start = old_idx; // 0-based
                let hunk_new_start = new_idx; // 0-based
                let mut dels: Vec<&[u8]> = Vec::new();
                while i < ops.len() {
                    match ops[i] {
                        DiffOp::Delete(k) => {
                            for _ in 0..k {
                                dels.push(parent_lines[old_idx].bytes_without_newline());
                                old_idx += 1;
                            }
                            i += 1;
                        }
                        DiffOp::Insert(k) => {
                            new_idx += k;
                            i += 1;
                        }
                        DiffOp::Equal(_) => break,
                    }
                }
                let _ = hunk_old_start;

                // Lost bucket: deletions hang on the result line at the hunk
                // start (sline[hunk_new_start]). git's distinction between the
                // additions-present (`nb-1`) and pure-deletion (`nb`) cases
                // collapses to the same index here because our `hunk_new_start`
                // is the 0-based result line immediately *after* the preceding
                // context, matching git's `nb-1` for the additions case and the
                // bucket-after for the pure-deletion case once the result line
                // numbering (1-based `nb`) is accounted for. The authoritative
                // p_lno values are recomputed in the loop below, so we do not
                // record them here.
                for d in &dels {
                    sline[hunk_new_start].plost.push(d.to_vec());
                }
                // Mark inserted result lines: flag bit n set => parent n lacked
                // this line.
                for r in hunk_new_start..new_idx {
                    if r < cnt {
                        sline[r].flag |= nmask;
                    }
                }
            }
        }
    }

    // Coalesce the plost lines into lost (git's coalesce_lines), then assign
    // p_lno numbers per parent — git's second loop in combine_diff.
    let mut p_lno: u64 = 1;
    for lno in 0..=cnt {
        sline[lno].p_lno[n] = p_lno;
        if !sline[lno].plost.is_empty() {
            let plost = std::mem::take(&mut sline[lno].plost);
            coalesce_lost(&mut sline[lno].lost, plost, n, options);
        }
        // How many parent lines does this sline advance?
        for ll in &sline[lno].lost {
            if ll.parent_map & nmask != 0 {
                p_lno += 1; // '-' means parent had it
            }
        }
        if lno < cnt && (sline[lno].flag & nmask) == 0 {
            p_lno += 1; // no '+' means parent had it
        }
    }
    sline[cnt + 1].p_lno[n] = p_lno; // trailer (git's sline[cnt+1])
}

/// Coalesce a parent's freshly-collected deletions into the line's existing
/// lost list (git's `coalesce_lines` LCS merge). A deletion that matches an
/// already-present lost line (under the active whitespace flags) gets its
/// parent bit OR'd into that line's `parent_map` instead of being added again.
fn coalesce_lost(base: &mut Vec<CdLost>, newlines: Vec<Vec<u8>>, n: usize, options: &CombinedRenderOptions) {
    let pmask = 1u64 << n;
    if newlines.is_empty() {
        return;
    }
    if base.is_empty() {
        for line in newlines {
            base.push(CdLost { line, parent_map: pmask });
        }
        return;
    }

    // LCS over (base lines, new lines) by whitespace-aware equality, exactly
    // like git: MATCH => OR the parent bit into the base line; NEW => insert the
    // new line at that position; BASE => keep base line as-is.
    let m = base.len();
    let k = newlines.len();
    let mut lcs = vec![vec![0i32; k + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=k {
            if combined_lines_match(&base[i - 1].line, &newlines[j - 1], options.ws_ignore) {
                lcs[i][j] = lcs[i - 1][j - 1] + 1;
            } else if lcs[i][j - 1] >= lcs[i - 1][j] {
                lcs[i][j] = lcs[i][j - 1];
            } else {
                lcs[i][j] = lcs[i - 1][j];
            }
        }
    }

    // Backtrack, building the merged list in reverse.
    let mut merged: Vec<CdLost> = Vec::with_capacity(m + k);
    let mut i = m;
    let mut j = k;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && combined_lines_match(&base[i - 1].line, &newlines[j - 1], options.ws_ignore) {
            let mut entry = std::mem::replace(
                &mut base[i - 1],
                CdLost { line: Vec::new(), parent_map: 0 },
            );
            entry.parent_map |= pmask;
            merged.push(entry);
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || lcs[i][j - 1] >= lcs[i - 1][j]) {
            merged.push(CdLost { line: newlines[j - 1].clone(), parent_map: pmask });
            j -= 1;
        } else {
            let entry = std::mem::replace(
                &mut base[i - 1],
                CdLost { line: Vec::new(), parent_map: 0 },
            );
            merged.push(entry);
            i -= 1;
        }
    }
    merged.reverse();
    *base = merged;
}

/// Whitespace-aware line equality used by the lost-line coalescer
/// (git's `match_string_spaces`). Only the all-space / space-change flavours
/// affect the comparison; otherwise it is a byte compare.
fn combined_lines_match(a: &[u8], b: &[u8], ws: WsIgnore) -> bool {
    if ws.all_space || ws.space_change || ws.space_at_eol {
        let at = strip_trailing_ws(a);
        let bt = strip_trailing_ws(b);
        if !ws.all_space && !ws.space_change {
            return at == bt;
        }
        return ws_squash_eq(at, bt, ws.space_change);
    }
    a == b
}

fn strip_trailing_ws(s: &[u8]) -> &[u8] {
    let mut end = s.len();
    while end > 0 && (s[end - 1] == b' ' || s[end - 1] == b'\t') {
        end -= 1;
    }
    &s[..end]
}

/// Compare two lines ignoring whitespace runs (`-w`) or treating runs as a
/// single space (`-b`).
fn ws_squash_eq(a: &[u8], b: &[u8], change_only: bool) -> bool {
    let is_ws = |c: u8| c == b' ' || c == b'\t';
    let (mut ia, mut ib) = (0usize, 0usize);
    while ia < a.len() && ib < b.len() {
        let (ca, cb) = (a[ia], b[ib]);
        if is_ws(ca) || is_ws(cb) {
            if change_only && (!is_ws(ca) || !is_ws(cb)) {
                return false;
            }
            // For -b, a whitespace run on both sides counts as equal; for -w,
            // whitespace is skipped entirely. Skip the runs on both sides.
            if change_only {
                while ia < a.len() && is_ws(a[ia]) {
                    ia += 1;
                }
                while ib < b.len() && is_ws(b[ib]) {
                    ib += 1;
                }
                continue;
            } else {
                if is_ws(ca) {
                    ia += 1;
                    continue;
                }
                if is_ws(cb) {
                    ib += 1;
                    continue;
                }
            }
        }
        if ca != cb {
            return false;
        }
        ia += 1;
        ib += 1;
    }
    // Consume trailing whitespace.
    while ia < a.len() && is_ws(a[ia]) {
        ia += 1;
    }
    while ib < b.len() && is_ws(b[ib]) {
        ib += 1;
    }
    ia == a.len() && ib == b.len()
}

/// git's `reuse_combine_diff`: when parent `i` has the same blob as a
/// previously-folded parent `j`, copy `j`'s flags / lost parent-bits / p_lno
/// across instead of re-diffing.
fn reuse_combine_diff(sline: &mut [CdLine], cnt: usize, i: usize, j: usize) {
    let imask = 1u64 << i;
    let jmask = 1u64 << j;
    for lno in 0..=cnt {
        sline[lno].p_lno[i] = sline[lno].p_lno[j];
        for ll in &mut sline[lno].lost {
            if ll.parent_map & jmask != 0 {
                ll.parent_map |= imask;
            }
        }
        if sline[lno].flag & jmask != 0 {
            sline[lno].flag |= imask;
        }
    }
    // The overall trailer (sline[cnt+1]).
    sline[cnt + 1].p_lno[i] = sline[cnt + 1].p_lno[j];
}

/// Is this result line "interesting" — does any parent lack it, or does it have
/// deletions hung in front (git's `interesting`).
fn cd_interesting(sline: &CdLine, all_mask: u64) -> bool {
    (sline.flag & all_mask) != 0 || !sline.lost.is_empty()
}

/// git's `adjust_hunk_tail`.
fn adjust_hunk_tail(sline: &[CdLine], all_mask: u64, hunk_begin: usize, mut i: usize) -> usize {
    if hunk_begin + 1 <= i && (sline[i - 1].flag & all_mask) == 0 {
        i -= 1;
    }
    i
}

/// git's `find_next`.
fn find_next(
    sline: &[CdLine],
    mark: u64,
    mut i: usize,
    cnt: usize,
    look_for_uninteresting: bool,
) -> usize {
    while i <= cnt {
        let marked = (sline[i].flag & mark) != 0;
        if look_for_uninteresting {
            if !marked {
                return i;
            }
        } else if marked {
            return i;
        }
        i += 1;
    }
    i
}

/// git's `give_context`: paint context lines (and bridge small gaps) around the
/// interesting lines, using the `mark` bit. Returns whether any hunk shows.
fn give_context(sline: &mut [CdLine], cnt: usize, num_parent: usize, context: usize) -> bool {
    let all_mask = (1u64 << num_parent) - 1;
    let mark = 1u64 << num_parent;
    let no_pre_delete = 2u64 << num_parent;

    let mut i = find_next(sline, mark, 0, cnt, false);
    if cnt < i {
        return false;
    }

    while i <= cnt {
        let mut j = if context < i { i - context } else { 0 };
        // Paint a few lines before the first interesting line.
        while j < i {
            if (sline[j].flag & mark) == 0 {
                sline[j].flag |= no_pre_delete;
            }
            sline[j].flag |= mark;
            j += 1;
        }

        loop {
            // Where does the next uninteresting line start?
            j = find_next(sline, mark, i, cnt, true);
            if cnt < j {
                // The rest are all interesting.
                return true;
            }
            // Lookahead context lines.
            let k = find_next(sline, mark, j, cnt, false);
            let j2 = adjust_hunk_tail(sline, all_mask, i, j);

            if k < j2 + context {
                // Small gap: paint it interesting and continue.
                let mut jj = j2;
                while jj < k {
                    sline[jj].flag |= mark;
                    jj += 1;
                }
                i = k;
                continue;
            }

            // No overlap within context: paint the trailing edge a bit.
            i = k;
            let kk = if j2 + context < cnt + 1 { j2 + context } else { cnt + 1 };
            let mut jj = j2;
            while jj < kk {
                sline[jj].flag |= mark;
                jj += 1;
            }
            break;
        }
    }
    true
}

/// git's `make_hunks`: mark interesting lines, run the `--cc` dense
/// simplification when requested, then `give_context`.
fn make_hunks(
    sline: &mut [CdLine],
    cnt: usize,
    num_parent: usize,
    dense: bool,
    context: usize,
) -> bool {
    let all_mask = (1u64 << num_parent) - 1;
    let mark = 1u64 << num_parent;

    for i in 0..=cnt {
        if cd_interesting(&sline[i], all_mask) {
            sline[i].flag |= mark;
        } else {
            sline[i].flag &= !mark;
        }
    }
    if !dense {
        return give_context(sline, cnt, num_parent, context);
    }

    // Dense simplification: for each marked hunk, drop it when the result
    // differs from a single parent only (or matches all but one parent the
    // same way) — git's "interesting" recomputation.
    let mut i = 0;
    while i <= cnt {
        while i <= cnt && (sline[i].flag & mark) == 0 {
            i += 1;
        }
        if cnt < i {
            break;
        }
        let hunk_begin = i;
        let mut j = i + 1;
        while j <= cnt {
            if (sline[j].flag & mark) == 0 {
                // Look beyond the end for an interesting line within context.
                let mut la = adjust_hunk_tail(sline, all_mask, hunk_begin, j);
                la = if la + context < cnt + 1 { la + context } else { cnt + 1 };
                let mut contin = false;
                while la > 0 && j <= la - 1 {
                    la -= 1;
                    if (sline[la].flag & mark) != 0 {
                        contin = true;
                        break;
                    }
                }
                if !contin {
                    break;
                }
                j = la;
            }
            j += 1;
        }
        let hunk_end = j;

        // Is the hunk "really" interesting? Check whether all changed lines
        // record the same set of parents.
        let mut same_diff: u64 = 0;
        let mut has_interesting = false;
        let mut jj = i;
        while jj < hunk_end && !has_interesting {
            let this_diff = sline[jj].flag & all_mask;
            if this_diff != 0 {
                if same_diff == 0 {
                    same_diff = this_diff;
                } else if same_diff != this_diff {
                    has_interesting = true;
                    break;
                }
            }
            for ll in &sline[jj].lost {
                if has_interesting {
                    break;
                }
                let td = ll.parent_map;
                if same_diff == 0 {
                    same_diff = td;
                } else if same_diff != td {
                    has_interesting = true;
                }
            }
            jj += 1;
        }

        if !has_interesting && same_diff != all_mask {
            // Not interesting after all: unmark the whole hunk.
            for x in hunk_begin..hunk_end {
                sline[x].flag &= !mark;
            }
        }
        i = hunk_end;
    }

    give_context(sline, cnt, num_parent, context)
}

/// git's `show_parent_lno`: emit one `-l0,len` column for parent `n`.
fn show_parent_lno(out: &mut Vec<u8>, sline: &[CdLine], l0: usize, l1: usize, n: usize, null_context: u64) {
    let a = sline[l0].p_lno[n];
    let b = sline[l1].p_lno[n];
    out.extend_from_slice(format!(" -{},{}", a, b - a - null_context).as_bytes());
}

/// git's `hunk_comment_line` test (used to append a function-context comment
/// to the `@@@ ... @@@` header).
fn hunk_comment_line(bol: &[u8]) -> bool {
    if bol.is_empty() {
        return false;
    }
    let ch = bol[0];
    ch.is_ascii_alphabetic() || ch == b'_' || ch == b'$'
}

/// git's `show_line_to_eol`: emit a line, preserving a trailing CR. The bytes
/// here never include the newline; we add it.
fn show_line_to_eol(out: &mut Vec<u8>, line: &[u8]) {
    let saw_cr = line.last() == Some(&b'\r');
    if saw_cr {
        out.extend_from_slice(&line[..line.len() - 1]);
        out.push(b'\r');
    } else {
        out.extend_from_slice(line);
    }
    out.push(b'\n');
}

/// git's `dump_sline`: emit the combined-diff hunk bodies for all marked hunks.
fn dump_sline(out: &mut Vec<u8>, sline: &[CdLine], cnt: usize, num_parent: usize, context: usize) {
    let mark = 1u64 << num_parent;
    let no_pre_delete = 2u64 << num_parent;
    let mut lno: usize = 0;

    loop {
        let mut hunk_comment: Option<&[u8]> = None;
        while lno <= cnt && (sline[lno].flag & mark) == 0 {
            if hunk_comment_line(&sline[lno].bol) {
                hunk_comment = Some(&sline[lno].bol);
            }
            lno += 1;
        }
        if cnt < lno {
            break;
        }
        let mut hunk_end = lno + 1;
        while hunk_end <= cnt {
            if (sline[hunk_end].flag & mark) == 0 {
                break;
            }
            hunk_end += 1;
        }

        let mut rlines = (hunk_end - lno) as u64;
        if cnt < hunk_end {
            rlines -= 1; // pointing at the last delete hunk
        }

        let mut null_context: u64 = 0;
        if context == 0 {
            // --unified=0: count the all-blank-context result lines so the
            // header line counts exclude them.
            for sl in sline.iter().take(hunk_end).skip(lno) {
                if (sl.flag & (mark - 1)) == 0 {
                    null_context += 1;
                }
            }
            rlines -= null_context;
        }

        // Header: `@@@`... (num_parent+1 markers), one -l,c column per parent,
        // ` +out_start,out_len `, num_parent+1 markers again.
        for _ in 0..=num_parent {
            out.push(b'@');
        }
        for i in 0..num_parent {
            show_parent_lno(out, sline, lno, hunk_end, i, null_context);
        }
        out.extend_from_slice(format!(" +{},{} ", lno + 1, rlines).as_bytes());
        for _ in 0..=num_parent {
            out.push(b'@');
        }

        if let Some(comment) = hunk_comment {
            let mut comment_end = 0;
            for (idx, &ch) in comment.iter().take(40).enumerate() {
                if ch == b'\n' {
                    break;
                }
                if !ch.is_ascii_whitespace() {
                    comment_end = idx + 1;
                }
            }
            if comment_end != 0 {
                out.push(b' ');
                out.extend_from_slice(&comment[..comment_end]);
            }
        }
        out.push(b'\n');

        // Body.
        while lno < hunk_end {
            let sl = &sline[lno];
            lno += 1;
            // Lost (deleted) lines hung before this result line.
            if (sl.flag & no_pre_delete) == 0 {
                for ll in &sl.lost {
                    for j in 0..num_parent {
                        if ll.parent_map & (1u64 << j) != 0 {
                            out.push(b'-');
                        } else {
                            out.push(b' ');
                        }
                    }
                    show_line_to_eol(out, &ll.line);
                }
            }
            if cnt < lno {
                break;
            }
            if (sl.flag & (mark - 1)) == 0 {
                // This sline only existed to hang the lost lines in front.
                if context == 0 {
                    continue;
                }
            }
            let mut p_mask = 1u64;
            for _ in 0..num_parent {
                if p_mask & sl.flag != 0 {
                    out.push(b'+');
                } else {
                    out.push(b' ');
                }
                p_mask <<= 1;
            }
            show_line_to_eol(out, &sl.bol);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_plain(old: Option<&[u8]>, new: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut options = HunkRenderOptions::default();
        render_hunks(&mut out, old, new, &mut options);
        out
    }

    #[test]
    fn identical_content_renders_nothing() {
        assert!(render_plain(Some(b"a\nb\n"), Some(b"a\nb\n")).is_empty());
    }

    #[test]
    fn single_line_change_basic_hunk() {
        let out = render_plain(Some(b"alpha\nbeta\ngamma\n"), Some(b"alpha\nBETA\ngamma\n"));
        assert_eq!(
            out,
            b"@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n".to_vec(),
        );
    }

    #[test]
    fn count_omitted_when_one() {
        // A single-line file changed in place yields `-1 +1` (no `,1`).
        let out = render_plain(Some(b"old\n"), Some(b"new\n"));
        assert_eq!(out, b"@@ -1 +1 @@\n-old\n+new\n".to_vec());
    }

    #[test]
    fn no_newline_marker_on_old_side() {
        let out = render_plain(Some(b"only line no newline"), None);
        assert_eq!(
            out,
            b"@@ -1 +0,0 @@\n-only line no newline\n\\ No newline at end of file\n".to_vec(),
        );
    }

    #[test]
    fn no_newline_marker_on_new_side() {
        let out = render_plain(Some(b"beta\n"), Some(b"beta-notail"));
        assert_eq!(
            out,
            b"@@ -1 +1 @@\n-beta\n+beta-notail\n\\ No newline at end of file\n".to_vec(),
        );
    }

    #[test]
    fn pure_insertion_into_empty() {
        let out = render_plain(None, Some(b"x\ny\n"));
        assert_eq!(out, b"@@ -0,0 +1,2 @@\n+x\n+y\n".to_vec());
    }

    #[test]
    fn distant_changes_split_into_two_hunks() {
        let old: &[u8] = b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new: &[u8] = b"A\nb\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let out = render_plain(Some(old), Some(new));
        // Two changes 9 lines apart (> 2*3+1) produce two separate hunks.
        let text = String::from_utf8(out).expect("rendered output is valid UTF-8");
        assert_eq!(text.matches("@@ ").count(), 2, "expected two hunks: {text}");
    }

    #[test]
    fn heading_callback_supplies_section() {
        // The change is far enough below `fn foo()` that the funcname line
        // precedes the hunk (the heading scan looks *above* the hunk's first
        // line, so a change touching line 1 would correctly find no heading).
        let old: &[u8] =
            b"fn foo() {\n    a\n    b\n    c\n    d\n    e\n    f\n    g\n}\n";
        let new: &[u8] =
            b"fn foo() {\n    a\n    b\n    c\n    d\n    CHANGED\n    f\n    g\n}\n";
        let mut out = Vec::new();
        // Classifier accepts any line whose first byte is an ASCII letter
        // (a crude def_ff stand-in for the test).
        let mut heading_fn = |line: &[u8]| -> Option<Vec<u8>> {
            if line.first().is_some_and(u8::is_ascii_alphabetic) {
                Some(line.strip_suffix(b"\n").unwrap_or(line).to_vec())
            } else {
                None
            }
        };
        let mut options = HunkRenderOptions {
            heading: Some(&mut heading_fn),
            ..Default::default()
        };
        render_hunks(&mut out, Some(old), Some(new), &mut options);
        let text = String::from_utf8(out).expect("rendered output is valid UTF-8");
        assert!(
            text.starts_with("@@ -3,7 +3,7 @@ fn foo() {\n"),
            "expected funcname heading: {text}",
        );
    }

    fn render_cc(result: &[u8], parents: &[&[u8]], dense: bool) -> String {
        let mut out = Vec::new();
        let opts = CombinedRenderOptions {
            dense,
            ..Default::default()
        };
        render_combined_with(&mut out, result, parents, &opts);
        String::from_utf8(out).expect("combined output is valid UTF-8")
    }

    #[test]
    fn combined_two_parent_dense_header_and_columns() {
        // A merge result that adds lines on top of two parents, the t4013
        // dir/sub shape: parent0 = "A\nB\nC\nD\nE\nF\n", parent1 = "A\nB\n1\n2\n",
        // result = "A\nB\nC\nD\nE\nF\n1\n2\n". git emits one combined hunk with
        // the `@@@ -1,6 -1,4 +1,8 @@@` header and two prefix columns.
        let p0 = b"A\nB\nC\nD\nE\nF\n";
        let p1 = b"A\nB\n1\n2\n";
        let result = b"A\nB\nC\nD\nE\nF\n1\n2\n";
        let text = render_cc(result, &[p0, p1], true);
        assert_eq!(
            text,
            "@@@ -1,6 -1,4 +1,8 @@@\n  A\n  B\n +C\n +D\n +E\n +F\n+ 1\n+ 2\n",
            "combined dense output:\n{text}",
        );
    }

    #[test]
    fn combined_identical_to_one_parent_dense_drops_hunk() {
        // When the result is identical to one parent, the dense (`--cc`) filter
        // drops the hunk (the change is "interesting" only against the other
        // parent), so nothing is emitted; the non-dense (`-c`) form still shows
        // every parent.
        let p0 = b"x\ny\n";
        let p1 = b"x\nCHANGED\n";
        let result = b"x\ny\n"; // identical to p0
        assert_eq!(render_cc(result, &[p0, p1], true), "");
        // Non-dense still shows the hunk (differs from p1).
        assert!(render_cc(result, &[p0, p1], false).starts_with("@@@"));
    }

    #[test]
    fn combined_reuse_identical_parents() {
        // Two parents with the identical blob must produce identical columns
        // (git's reuse_combine_diff path); the result adds a line relative to
        // both, so both columns carry `+`.
        let parent = b"a\nb\n";
        let result = b"a\nb\nc\n";
        let text = render_cc(result, &[parent, parent], true);
        assert_eq!(
            text,
            "@@@ -1,2 -1,2 +1,3 @@@\n  a\n  b\n++c\n",
            "reuse output:\n{text}",
        );
    }
}
