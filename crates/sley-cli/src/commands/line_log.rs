//! `git log -L<range>:<file>` — the line-log engine.
//!
//! A behavioural port of git's `line-log.c` + `line-range.c` + the `diff.c`
//! `line_range_*` hunk-restriction callbacks. Given one or more
//! `-L<start>,<end>:<file>` / `-L:<funcname>:<file>` arguments, it:
//!
//! 1. Parses each argument into a 0-based half-open `[start, end)` line range
//!    against the tip commit's blob (`parse_lines`).
//! 2. Walks history in topological order, mapping the tracked ranges back
//!    across each commit's diff to its parent and recording, per commit, the
//!    hunks that touched a tracked range (`process_ranges_*`).
//! 3. Emits each surviving commit with the standard log header followed by a
//!    `diff --git` whose hunks are clipped to the tracked ranges (the
//!    `line_ranges` hook in `sley_diff_merge::render`).
//!
//! The range-mapping core (`range_set_map_across_diff`,
//! `diff_ranges_filter_touched`, `range_set_shift_diff`) is a direct port; the
//! per-commit diff is computed with sley's tree-name-status + blob reads, and
//! the per-line diff that drives range mapping uses sley's Myers diff.

use crate::*;
use sley::ObjectDatabase as FileObjectDatabase;
use sley::plumbing::sley_diff_merge::render::LineRange;
use sley::plumbing::sley_rev::{CommitRecord, resolve_tree_path_entry};
use std::collections::HashMap;

/// One `-L` argument before resolution: the raw `<range>:<file>` string.
#[derive(Debug, Clone)]
pub(crate) struct LineLogArg {
    pub(crate) raw: String,
}

/// A half-open `[start, end)` line range, 0-based. Mirrors diff.c's
/// `struct range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    start: i64,
    end: i64,
}

/// A sorted, disjoint set of [`Range`]s (git's `range_set`).
#[derive(Debug, Clone, Default)]
struct RangeSet {
    ranges: Vec<Range>,
}

impl RangeSet {
    fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// `range_set_append`: must begin at or after the end of the last range.
    /// Skips empty ranges (git's `range_set_append` asserts `a <= b` but real
    /// range sets never store empties; the diff-ranges parallel arrays use
    /// [`append_raw`] to keep zero-width sides).
    fn append(&mut self, start: i64, end: i64) {
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
    fn sort_and_merge(&mut self) {
        self.ranges.sort_by_key(|r| r.start);
        let mut out: Vec<Range> = Vec::with_capacity(self.ranges.len());
        for r in self.ranges.drain(..) {
            if r.start == r.end {
                continue;
            }
            if let Some(last) = out.last_mut() {
                if r.start <= last.end {
                    if r.end > last.end {
                        last.end = r.end;
                    }
                    continue;
                }
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
        let mut push = |out: &mut RangeSet, r: Range| {
            if let Some(last) = out.ranges.last_mut() {
                if r.start <= last.end {
                    if r.end > last.end {
                        last.end = r.end;
                    }
                    return;
                }
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
struct FileRange {
    path: String,
    ranges: RangeSet,
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
    fn new(path: String) -> Self {
        Self {
            path,
            ranges: RangeSet::new(),
            pair: None,
        }
    }
}

/// The per-commit tracked range list (git's `line_log_data` linked list, kept
/// sorted by path).
type RangeList = Vec<FileRange>;

/// Read a blob's bytes by oid.
fn read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = db.read_object(oid)?;
    Ok(object.body.to_vec())
}

/// 0-based line-start offsets of `data`: `nth_line(n)` returns `&data[ends[n]..]`.
/// `ends[0] = 0`; `ends[k]` is the byte offset of the start of line `k`.
/// `lines` is the number of lines (git's `fill_line_ends`).
fn line_ends(data: &[u8]) -> (Vec<usize>, i64) {
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

/// `nth_line(n)` for the parse-loc regex search: the start of line `n`
/// (0-based). Returns the slice from that offset to end.
fn nth_line<'a>(data: &'a [u8], ends: &[usize], n: i64) -> &'a [u8] {
    let n = n.max(0) as usize;
    let off = ends.get(n).copied().unwrap_or(data.len());
    &data[off.min(data.len())..]
}

/// Default funcname-line classifier (`def_ff` / `match_funcname` with no
/// driver): first byte is a letter, `_`, or `$`.
fn is_funcname_line(line: &[u8]) -> bool {
    match line.first() {
        Some(&b) => b.is_ascii_alphabetic() || b == b'_' || b == b'$',
        None => false,
    }
}

/// The line at offset `off` (the bytes from `off` to the next '\n' inclusive,
/// or end).
fn line_at(data: &[u8], off: usize) -> &[u8] {
    let end = data[off..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| off + p + 1)
        .unwrap_or(data.len());
    &data[off..end]
}

/// Error type for `-L` parsing failures that should print git's exact message.
fn line_log_fatal(msg: impl AsRef<str>) -> GitError {
    eprintln!("fatal: {}", msg.as_ref());
    GitError::Exit(128)
}

/// Outcome of parsing the leading `<range>` portion of a `-L` argument.
struct ParsedRange {
    /// 1-based begin / end (git's human-terms output before the `begin--`).
    begin: i64,
    end: i64,
}

/// Split `arg` (`<range>:<file>` or `:<funcname>:<file>` or `^:<funcname>:<file>`)
/// into `(range_part, file_part)`. Mirrors `skip_range_arg`: the file name is
/// everything after the range, which itself ends at the `:` that the funcname /
/// loc parser stops at.
fn split_range_and_file(arg: &str) -> Result<(String, String)> {
    let bytes = arg.as_bytes();
    // Funcname form: ":funcname:file" or "^:funcname:file".
    let funcname_form = bytes.first() == Some(&b':')
        || (bytes.first() == Some(&b'^') && bytes.get(1) == Some(&b':'));
    if funcname_form {
        // The funcname pattern ends at the next unescaped ':'.
        let colon1 = if bytes[0] == b'^' { 1 } else { 0 };
        // bytes[colon1] == ':'
        let mut i = colon1 + 1;
        while i < bytes.len() && bytes[i] != b':' {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 1;
            }
            i += 1;
        }
        if i == colon1 + 1 || i >= bytes.len() {
            return Err(line_log_fatal(format!(
                "-L argument not 'start,end:file' or ':funcname:file': {arg}"
            )));
        }
        let range_part = arg[..i].to_string();
        let file_part = arg[i + 1..].to_string();
        if file_part.is_empty() {
            return Err(line_log_fatal(format!(
                "-L argument not 'start,end:file' or ':funcname:file': {arg}"
            )));
        }
        return Ok((range_part, file_part));
    }
    // Numeric / regex loc form: scan the range portion up to the ':' that
    // separates it from the file (a '/regex/' may itself contain ':' but not
    // unescaped). We mimic skip_range_arg by scanning loc[,loc] then the ':'.
    let mut i = 0usize;
    // first loc
    i = skip_loc(bytes, i);
    if i < bytes.len() && bytes[i] == b',' {
        i = skip_loc(bytes, i + 1);
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return Err(line_log_fatal(format!(
            "-L argument not 'start,end:file' or ':funcname:file': {arg}"
        )));
    }
    let range_part = arg[..i].to_string();
    let file_part = arg[i + 1..].to_string();
    if file_part.is_empty() {
        return Err(line_log_fatal(format!(
            "-L argument not 'start,end:file' or ':funcname:file': {arg}"
        )));
    }
    Ok((range_part, file_part))
}

/// Skip one loc spec (number, `+N`/`-N`, `/regex/`, `^/regex/`) and return the
/// new index. Mirrors line-range.c parse_loc's scan-only behaviour.
fn skip_loc(bytes: &[u8], mut i: usize) -> usize {
    if i >= bytes.len() {
        return i;
    }
    // +N / -N
    if bytes[i] == b'+' || bytes[i] == b'-' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > i + 1 {
            return j;
        }
    }
    // number
    {
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > i {
            return j;
        }
    }
    // optional leading '^' for regex
    if bytes[i] == b'^' {
        i += 1;
    }
    // /regex/
    if i < bytes.len() && bytes[i] == b'/' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != b'/' {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 1;
            }
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'/' {
            return j + 1;
        }
    }
    i
}

/// Parse one loc (the part before/after a `,`). `begin` is the anchor context:
/// for the start loc it is `-anchor`, for the end loc it is `start_begin + 1`.
/// Returns the resolved 1-based line number (git's `*ret`).
fn parse_loc(spec: &str, data: &[u8], ends: &[usize], lines: i64, begin: i64) -> Result<i64> {
    let bytes = spec.as_bytes();
    // "+N" / "-N" relative form (only when begin >= 1).
    if begin >= 1 && (bytes.first() == Some(&b'+') || bytes.first() == Some(&b'-')) {
        let sign_minus = bytes[0] == b'-';
        let digits: String = spec[1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            let mut num: i64 = digits.parse().unwrap_or(0);
            if num == 0 {
                return Err(line_log_fatal("-L invalid empty range"));
            }
            if sign_minus {
                num = -num;
            }
            let ret = if num > 0 {
                begin + num - 2
            } else {
                (begin + num).max(1)
            };
            return Ok(ret);
        }
        // fall through to numeric on no digits
    }
    // bare number
    {
        let digits: String = spec.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let num: i64 = digits.parse().unwrap_or(0);
            if num <= 0 {
                return Err(line_log_fatal(format!("-L invalid line number: {num}")));
            }
            return Ok(num);
        }
    }
    // regex form: '^/.../' or '/.../'.
    let mut anchor = begin;
    let mut spec = spec;
    if anchor < 0 {
        if !spec.starts_with('^') {
            anchor = -anchor;
        } else {
            anchor = 1;
            spec = &spec[1..];
        }
    }
    if !spec.starts_with('/') {
        return Err(line_log_fatal(format!(
            "-L invalid line specification: {spec}"
        )));
    }
    // Extract the regex between the first '/' and the next unescaped '/'.
    let sbytes = spec.as_bytes();
    let mut j = 1usize;
    while j < sbytes.len() && sbytes[j] != b'/' {
        if sbytes[j] == b'\\' && j + 1 < sbytes.len() {
            j += 1;
        }
        j += 1;
    }
    if j >= sbytes.len() || sbytes[j] != b'/' {
        return Err(line_log_fatal(format!(
            "-L invalid line specification: {spec}"
        )));
    }
    let pattern = &spec[1..j];
    // begin-- (human terms → 0-based), search from that line.
    let search_from = anchor - 1;
    let regex = sley_grep::Regex::compile_bytes(
        pattern.as_bytes(),
        sley_grep::RegexMode::Bre,
        false,
        false,
    )
    .map_err(|_| line_log_fatal(format!("-L parameter '{pattern}': invalid regex")))?;
    // git runs the regex with REG_NEWLINE (multiline `^`/`$`) over the buffer
    // from `search_from`, then maps the first match offset to a line. sley's
    // regex engine lacks multiline anchors, so scan line-by-line: the first
    // line (>= search_from) the pattern matches is git's result.
    match find_regex_line(&regex, data, ends, lines, search_from) {
        Some(line0) => Ok(line0 + 1),
        None => Err(line_log_fatal(format!(
            "-L parameter '{}' starting at line {}: no match",
            pattern,
            search_from + 1
        ))),
    }
}

/// Find the first line index (0-based, `>= from0`) whose bytes the `regex`
/// matches anywhere — git's REG_NEWLINE multiline search reduced to a per-line
/// scan (the engine here has no multiline `^`/`$`, so we feed each line slice
/// individually, which makes `^`/`$` anchor at the line boundaries). Returns
/// `None` when no line in `[from0, lines)` matches.
fn find_regex_line(
    regex: &sley_grep::Regex,
    data: &[u8],
    ends: &[usize],
    lines: i64,
    from0: i64,
) -> Option<i64> {
    let mut idx = from0.max(0);
    while idx < lines {
        let lo = ends.get(idx as usize).copied().unwrap_or(data.len());
        let hi = ends.get((idx + 1) as usize).copied().unwrap_or(data.len());
        // Match against the line WITHOUT the trailing '\n' so `$` anchors at the
        // line's logical end (git's REG_NEWLINE treats '\n' as the boundary).
        let mut slice = &data[lo.min(data.len())..hi.min(data.len())];
        if slice.last() == Some(&b'\n') {
            slice = &slice[..slice.len() - 1];
        }
        if regex.find_from(slice, 0).is_some() {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

/// Parse a `:funcname:` (or `^:funcname:`) spec into (begin, end), 1-based.
fn parse_range_funcname(
    range_part: &str,
    data: &[u8],
    ends: &[usize],
    lines: i64,
    anchor_in: i64,
) -> Result<ParsedRange> {
    let bytes = range_part.as_bytes();
    let mut anchor = anchor_in;
    let mut idx = 0usize;
    if bytes[idx] == b'^' {
        anchor = 1;
        idx += 1;
    }
    // bytes[idx] == ':'
    let pattern = &range_part[idx + 1..];
    let anchor0 = anchor - 1; // human → 0-based
    let start_off = ends.get(anchor0.max(0) as usize).copied().unwrap_or(0);
    let regex = sley_grep::Regex::compile_bytes(
        pattern.as_bytes(),
        sley_grep::RegexMode::Bre,
        false,
        false,
    )
    .map_err(|_| line_log_fatal(format!("-L parameter '{pattern}': invalid regex")))?;
    // find_funcname_matching_regexp: scan forward from the anchor line for the
    // first line that (a) the regex matches and (b) is a funcname line (git's
    // `match_funcname` over the matched line). Per-line scan gives the correct
    // multiline-anchor semantics that the engine lacks. `p` is the byte offset
    // of that line's start.
    let _ = start_off;
    let mut found: Option<usize> = None;
    let mut idx = anchor0.max(0);
    while idx < lines {
        let lo = ends.get(idx as usize).copied().unwrap_or(data.len());
        let hi = ends.get((idx + 1) as usize).copied().unwrap_or(data.len());
        let mut slice = &data[lo.min(data.len())..hi.min(data.len())];
        if slice.last() == Some(&b'\n') {
            slice = &slice[..slice.len() - 1];
        }
        if regex.find_from(slice, 0).is_some() && is_funcname_line(line_at(data, lo)) {
            found = Some(lo);
            break;
        }
        idx += 1;
    }
    let p = found.ok_or_else(|| {
        line_log_fatal(format!(
            "-L parameter '{}' starting at line {}: no match",
            pattern,
            anchor0 + 1
        ))
    })?;
    // *begin = 0; while p > nth_line(begin) begin++  (0-based line of p)
    let mut begin0 = 0i64;
    while begin0 < lines {
        let off = ends.get(begin0 as usize).copied().unwrap_or(data.len());
        if p <= off {
            break;
        }
        begin0 += 1;
    }
    // git: while (p > nth_line(begin)) begin++ — stop when nth_line(begin) >= p.
    // Recompute precisely.
    begin0 = 0;
    loop {
        let off = ends.get(begin0 as usize).copied().unwrap_or(data.len());
        if off >= p || begin0 >= lines {
            break;
        }
        begin0 += 1;
    }
    if begin0 >= lines {
        return Err(line_log_fatal(format!(
            "-L parameter '{pattern}' matches at EOF"
        )));
    }
    // end = begin+1; advance until the next funcname line.
    let mut end0 = begin0 + 1;
    while end0 < lines {
        let bol = ends.get(end0 as usize).copied().unwrap_or(data.len());
        if is_funcname_line(line_at(data, bol)) {
            break;
        }
        end0 += 1;
    }
    // compensate for 1-based numbering: (*begin)++
    Ok(ParsedRange {
        begin: begin0 + 1,
        end: end0,
    })
}

/// Parse a full `<range>` spec (numeric, `+N`, regex, funcname) into 1-based
/// begin/end. Mirrors `parse_range_arg`.
fn parse_range_arg(
    range_part: &str,
    data: &[u8],
    ends: &[usize],
    lines: i64,
    anchor_in: i64,
) -> Result<ParsedRange> {
    let mut anchor = anchor_in;
    if anchor < 1 {
        anchor = 1;
    }
    if anchor > lines {
        anchor = lines + 1;
    }
    let bytes = range_part.as_bytes();
    if bytes.first() == Some(&b':') || (bytes.first() == Some(&b'^') && bytes.get(1) == Some(&b':'))
    {
        return parse_range_funcname(range_part, data, ends, lines, anchor);
    }
    // loc[,loc]
    let (first, rest) = match range_part.find(',') {
        Some(pos) => (&range_part[..pos], Some(&range_part[pos + 1..])),
        None => (range_part, None),
    };
    let begin = if first.is_empty() {
        0
    } else {
        parse_loc(first, data, ends, lines, -anchor)?
    };
    let end = match rest {
        Some(end_spec) if !end_spec.is_empty() => {
            parse_loc(end_spec, data, ends, lines, begin + 1)?
        }
        _ => 0,
    };
    let (mut begin, mut end) = (begin, end);
    if begin != 0 && end != 0 && end < begin {
        std::mem::swap(&mut begin, &mut end);
    }
    Ok(ParsedRange { begin, end })
}

/// Insert a 0-based `[begin, end)` range for `path` into the range list
/// (keeping it sorted by path, merging ranges for the same path).
fn range_list_insert(list: &mut RangeList, path: &str, begin: i64, end: i64) {
    if let Some(fr) = list.iter_mut().find(|f| f.path == path) {
        fr.ranges.append(begin, end);
        return;
    }
    let mut fr = FileRange::new(path.to_string());
    fr.ranges.append(begin, end);
    // keep sorted by path
    let pos = list
        .iter()
        .position(|f| f.path.as_str() > path)
        .unwrap_or(list.len());
    list.insert(pos, fr);
}

/// Parse all `-L` args against the tip commit, producing the initial range
/// list. Mirrors `parse_lines` + `line_log_init`.
fn parse_lines(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip_tree: &ObjectId,
    args: &[LineLogArg],
) -> Result<RangeList> {
    let mut list: RangeList = Vec::new();
    for arg in args {
        let (range_part, file_part) = split_range_and_file(&arg.raw)?;
        let entry = resolve_tree_path_entry(db, format, tip_tree, &file_part)
            .ok_or_else(|| line_log_fatal(format!("There is no path {file_part} in the commit")))?;
        if entry.object_type != ObjectType::Blob {
            return Err(line_log_fatal(format!(
                "There is no path {file_part} in the commit"
            )));
        }
        let data = read_blob(db, &entry.oid)?;
        let (ends, lines) = line_ends(&data);
        // anchor: end of the last range already parsed for this path + 1, else 1.
        let anchor = list
            .iter()
            .find(|f| f.path == file_part)
            .and_then(|f| f.ranges.ranges.last())
            .map(|r| r.end + 1)
            .unwrap_or(1);
        let ParsedRange { begin, end } = parse_range_arg(&range_part, &data, &ends, lines, anchor)?;
        if (lines == 0 && (begin != 0 || end != 0)) || lines < begin {
            return Err(line_log_fatal(format!(
                "file {file_part} has only {lines} lines"
            )));
        }
        let begin = if begin < 1 { 1 } else { begin };
        let end = if end < 1 || lines < end { lines } else { end };
        // begin-- (1-based → 0-based start); end stays as the exclusive bound.
        range_list_insert(&mut list, &file_part, begin - 1, end);
    }
    for fr in &mut list {
        fr.ranges.sort_and_merge();
    }
    Ok(list)
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
                ..Default::default()
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
        Some(p) => Some(parent_tree_oid(db, format, p)?),
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
fn parent_tree_oid(
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
pub(crate) struct PrintedFile {
    pub(crate) old_path: String,
    pub(crate) new_path: String,
    pub(crate) old_oid: Option<ObjectId>,
    pub(crate) new_oid: Option<ObjectId>,
    pub(crate) old_mode: Option<u32>,
    pub(crate) new_mode: Option<u32>,
    pub(crate) status: sley_diff_merge::NameStatus,
    pub(crate) line_ranges: Vec<LineRange>,
}

/// The output of the line-log walk: the ordered interesting commits (newest
/// first, topo order) and, per commit oid, the files + ranges to render.
pub(crate) struct LineLogResult {
    pub(crate) interesting: Vec<ObjectId>,
    pub(crate) printed: HashMap<ObjectId, Vec<PrintedFile>>,
}

/// Run the line-log walk over `ordered` (commits in topological order, child
/// before parent — git's `revs->commits` after topo sort). `tip` is the single
/// commit `-L` resolves its initial ranges against (git requires exactly one
/// positive tip). Returns the interesting commits + per-commit printed ranges.
///
/// Mirrors `line_log_filter`: ranges propagate lazily from child to parent via
/// the per-commit decoration map; a commit is interesting iff it changed a
/// tracked range.
pub(crate) fn run_line_log(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    ordered: &[CommitRecord],
    tip: &ObjectId,
    args: &[LineLogArg],
    detect_renames: bool,
    first_parent: bool,
) -> Result<LineLogResult> {
    // Resolve initial ranges against the tip's tree.
    let tip_tree = parent_tree_oid(db, format, tip)?;
    let initial = parse_lines(db, format, &tip_tree, args)?;

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
        let parent_tree = parent_tree_oid(db, format, parent)?;
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
