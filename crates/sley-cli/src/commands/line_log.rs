//! `git log -L<range>:<file>` — `-L` argument parsing + engine bridge.
//!
//! The `-L` string forms (`start,end`, `+N`/`-N` offsets, `/regex/`,
//! `:funcname`) are parsed here against the tip commit's blob, then handed to
//! the history-simplification core in `sley-rev::line_log`, which owns the
//! range-mapping walk (git's `line-log.c`). Rendering of the surviving commits
//! lives with the log output code.

use crate::*;
use sley::plumbing::sley_rev::{CommitRecord, resolve_tree_path_entry};
use sley_rev::line_log::{FileRange, RangeList, is_funcname_line, line_at, line_ends};
pub(crate) use sley_rev::line_log::{LineLogResult, PrintedFile};

/// One `-L` argument before resolution: the raw `<range>:<file>` string.
#[derive(Debug, Clone)]
pub(crate) struct LineLogArg {
    pub(crate) raw: String,
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
        let object = db.read_object(&entry.oid)?;
        let data = object.body.to_vec();
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

/// Parse all `-L` args against the tip commit's blob, then run the
/// history-simplification walk in `sley-rev::line_log`. Mirrors
/// `line_log_init` + `line_log_filter`.
pub(crate) fn run_line_log(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    ordered: &[CommitRecord],
    tip: &ObjectId,
    args: &[LineLogArg],
    detect_renames: bool,
    first_parent: bool,
) -> Result<LineLogResult> {
    let tip_tree = sley_rev::peel_to_tree(db, format, tip)?;
    let initial = parse_lines(db, format, &tip_tree, args)?;
    sley_rev::line_log::run_line_log(
        db,
        format,
        ordered,
        tip,
        initial,
        detect_renames,
        first_parent,
    )
}
