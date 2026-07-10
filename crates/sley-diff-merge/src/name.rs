//! Patch filename extraction, ported from git's `apply.c`.
//!
//! `git apply` derives the affected pathname from several sources — the
//! `diff --git` line, the `---`/`+++` headers, and `rename from`/`copy to`
//! extended headers — applying a `-p<n>` strip and a `--directory=<root>`
//! prefix to each. Traditional (non-git) diffs additionally *guess* the strip
//! count and recognise GNU `1969-12-31`/`1970-01-01` epoch timestamps as
//! creation/deletion markers. This module reproduces that logic byte-for-byte
//! (`find_name`, `find_name_traditional`, `find_name_gnu`, `git_header_name`,
//! `skip_tree_prefix`, `guess_p_value`, `has_epoch_timestamp`,
//! `diff_timestamp_len`, `unquote_c_style`).

/// Terminate name scanning at a space (`TERM_SPACE`).
pub(crate) const TERM_SPACE: u8 = 1;
/// Terminate name scanning at a tab (`TERM_TAB`).
pub(crate) const TERM_TAB: u8 = 2;

/// `git apply`'s `is_dev_null`: the name is `/dev/null` optionally followed by
/// whitespace (here, end-of-line counts as the terminating newline git sees).
pub(crate) fn is_dev_null(line: &[u8]) -> bool {
    if let Some(rest) = line.strip_prefix(b"/dev/null".as_slice()) {
        rest.is_empty() || rest[0].is_ascii_whitespace()
    } else {
        false
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn name_terminate(c: u8, terminate: u8) -> bool {
    if c == b' ' && (terminate & TERM_SPACE) == 0 {
        return false;
    }
    if c == b'\t' && (terminate & TERM_TAB) == 0 {
        return false;
    }
    true
}

/// Collapse runs of `/` into a single `/` (git's `squash_slash`), which lets
/// `--index` work with doubled-slash filenames.
fn squash_slash(mut name: Vec<u8>) -> Vec<u8> {
    let mut j = 0usize;
    let mut i = 0usize;
    while i < name.len() {
        let c = name[i];
        name[j] = c;
        j += 1;
        i += 1;
        if c == b'/' {
            while i < name.len() && name[i] == b'/' {
                i += 1;
            }
        }
    }
    name.truncate(j);
    name
}

/// Decode a C-quoted string (git's `unquote_c_style`). `line` must start with a
/// double quote. Returns the decoded bytes plus the index in `line` just past
/// the closing quote, or `None` on a malformed quote.
pub(crate) fn unquote_c_style(line: &[u8]) -> Option<(Vec<u8>, usize)> {
    if line.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1usize; // skip opening quote
    while i < line.len() {
        let c = line[i];
        i += 1;
        match c {
            b'"' => return Some((out, i)),
            b'\\' => {}
            _ => {
                out.push(c);
                continue;
            }
        }
        // Escape sequence.
        let &esc = line.get(i)?;
        i += 1;
        match esc {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' | b'"' => out.push(esc),
            b'0'..=b'7' => {
                // Up to three octal digits, the first already consumed.
                let mut value = (esc - b'0') as u32;
                let mut digits = 1;
                while digits < 3 {
                    match line.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            value = (value << 3) | (d - b'0') as u32;
                            i += 1;
                            digits += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            _ => return None,
        }
    }
    // No closing quote.
    None
}

/// Skip `p_value` leading path components from `line`; reject absolute paths
/// (git's `skip_tree_prefix`). Returns the byte offset of the remainder, or
/// `None`.
fn skip_tree_prefix(p_value: usize, line: &[u8]) -> Option<usize> {
    if p_value == 0 {
        return if !line.is_empty() && line[0] == b'/' {
            None
        } else {
            Some(0)
        };
    }
    let mut nslash = p_value;
    for (i, &ch) in line.iter().enumerate() {
        if ch == b'/' {
            nslash -= 1;
            if nslash == 0 {
                return if i == 0 { None } else { Some(i + 1) };
            }
        }
    }
    None
}

/// git's `find_name_gnu`: decode a quoted name, strip `p_value` components and
/// prepend `root`.
fn find_name_gnu(line: &[u8], p_value: usize, root: &[u8]) -> Option<Vec<u8>> {
    let (name, _) = unquote_c_style(line)?;
    let mut cp = 0usize;
    let mut pv = p_value;
    while pv > 0 {
        let rel = name[cp..].iter().position(|&b| b == b'/')?;
        cp += rel + 1;
        pv -= 1;
    }
    let mut result = Vec::with_capacity(root.len() + name.len() - cp);
    result.extend_from_slice(root);
    result.extend_from_slice(&name[cp..]);
    Some(squash_slash(result))
}

// --- traditional-diff timestamp recognition (git's diff_timestamp_len) ---

fn sane_tz_len(line: &[u8]) -> usize {
    let len = line.len();
    let need = b" +0500".len();
    if len < need || line[len - need] != b' ' {
        return 0;
    }
    let tz = &line[len - need..];
    if tz[1] != b'+' && tz[1] != b'-' {
        return 0;
    }
    for &b in &tz[2..] {
        if !b.is_ascii_digit() {
            return 0;
        }
    }
    need
}

fn tz_with_colon_len(line: &[u8]) -> usize {
    let len = line.len();
    let need = b" +08:00".len();
    if len < need || line[len - b":00".len()] != b':' {
        return 0;
    }
    let tz = &line[len - need..];
    if tz[0] != b' ' || (tz[1] != b'+' && tz[1] != b'-') {
        return 0;
    }
    // tz: ' ', sign, digit, digit, ':', digit, digit
    if !tz[2].is_ascii_digit()
        || !tz[3].is_ascii_digit()
        || tz[4] != b':'
        || !tz[5].is_ascii_digit()
        || !tz[6].is_ascii_digit()
    {
        return 0;
    }
    need
}

fn date_len(line: &[u8]) -> usize {
    let len = line.len();
    let need = b"72-02-05".len();
    if len < need || line[len - b"-05".len()] != b'-' {
        return 0;
    }
    let d = &line[len - need..];
    if !d[0].is_ascii_digit()
        || !d[1].is_ascii_digit()
        || d[2] != b'-'
        || !d[3].is_ascii_digit()
        || !d[4].is_ascii_digit()
        || d[5] != b'-'
        || !d[6].is_ascii_digit()
        || !d[7].is_ascii_digit()
    {
        return 0;
    }
    let mut date_start = len - need;
    // Optional 4-digit year: two more leading digits.
    if date_start >= b"19".len()
        && line[date_start - 1].is_ascii_digit()
        && line[date_start - 2].is_ascii_digit()
    {
        date_start -= b"19".len();
    }
    len - date_start
}

fn short_time_len(line: &[u8]) -> usize {
    let len = line.len();
    let need = b" 07:01:32".len();
    if len < need || line[len - b":32".len()] != b':' {
        return 0;
    }
    let t = &line[len - need..];
    if t[0] != b' '
        || !t[1].is_ascii_digit()
        || !t[2].is_ascii_digit()
        || t[3] != b':'
        || !t[4].is_ascii_digit()
        || !t[5].is_ascii_digit()
        || t[6] != b':'
        || !t[7].is_ascii_digit()
        || !t[8].is_ascii_digit()
    {
        return 0;
    }
    need
}

fn fractional_time_len(line: &[u8]) -> usize {
    let len = line.len();
    if len == 0 || !line[len - 1].is_ascii_digit() {
        return 0;
    }
    let mut p = len - 1;
    while p > 0 && line[p].is_ascii_digit() {
        p -= 1;
    }
    if line[p] != b'.' {
        return 0;
    }
    let n = short_time_len(&line[..p]);
    if n == 0 {
        return 0;
    }
    (len - p) + n
}

fn trailing_spaces_len(line: &[u8]) -> usize {
    let len = line.len();
    if len == 0 || line[len - 1] != b' ' {
        return 0;
    }
    let mut p = len;
    while p != 0 {
        p -= 1;
        if line[p] != b' ' {
            return len - (p + 1);
        }
    }
    len
}

/// Length of a trailing diff timestamp (git's `diff_timestamp_len`), 0 if none.
fn diff_timestamp_len(line: &[u8]) -> usize {
    let mut end = line.len();
    if end == 0 || !line[end - 1].is_ascii_digit() {
        return 0;
    }
    let mut n = sane_tz_len(&line[..end]);
    if n == 0 {
        n = tz_with_colon_len(&line[..end]);
    }
    end -= n;

    let mut n = short_time_len(&line[..end]);
    if n == 0 {
        n = fractional_time_len(&line[..end]);
    }
    end -= n;

    let n = date_len(&line[..end]);
    if n == 0 {
        return 0;
    }
    end -= n;

    if end == 0 {
        return 0;
    }
    if line[end - 1] == b'\t' {
        end -= 1;
        return line.len() - end;
    }
    if line[end - 1] != b' ' {
        return 0;
    }
    end -= trailing_spaces_len(&line[..end]);
    line.len() - end
}

/// git's `find_name_common`. `end` bounds the scan (traditional diffs pass the
/// post-timestamp length); when `None` the scan runs to a `\n`/terminator.
fn find_name_common(
    line: &[u8],
    def: Option<&[u8]>,
    p_value: usize,
    end: Option<usize>,
    terminate: u8,
    root: &[u8],
) -> Option<Vec<u8>> {
    let limit = end.unwrap_or(line.len()).min(line.len());
    let mut start: Option<usize> = if p_value == 0 { Some(0) } else { None };
    let mut pv = p_value;
    let mut i = 0usize;
    while i < limit {
        let c = line[i];
        if end.is_none() && is_space(c) {
            if c == b'\n' {
                break;
            }
            if name_terminate(c, terminate) {
                break;
            }
        }
        i += 1;
        if c == b'/' && pv > 0 {
            pv -= 1;
            if pv == 0 {
                start = Some(i);
            }
        }
    }
    let start = match start {
        Some(s) => s,
        None => return def.map(|d| squash_slash(d.to_vec())),
    };
    let len = i - start;
    if len == 0 {
        return def.map(|d| squash_slash(d.to_vec()));
    }
    // Prefer the shorter `def` when the found name is just `def` with a suffix.
    if let Some(def) = def {
        let deflen = def.len();
        if deflen < len && &line[start..start + deflen] == def {
            return Some(squash_slash(def.to_vec()));
        }
    }
    let mut result = Vec::with_capacity(root.len() + len);
    result.extend_from_slice(root);
    result.extend_from_slice(&line[start..start + len]);
    Some(squash_slash(result))
}

/// git's `find_name`, for `---`/`+++`/rename/copy lines in a git diff.
pub(crate) fn find_name(
    line: &[u8],
    def: Option<&[u8]>,
    p_value: usize,
    terminate: u8,
    root: &[u8],
) -> Option<Vec<u8>> {
    if line.first() == Some(&b'"')
        && let Some(name) = find_name_gnu(line, p_value, root)
    {
        return Some(name);
    }
    find_name_common(line, def, p_value, None, terminate, root)
}

/// git's `find_name_traditional`, for the `---`/`+++` of a non-git diff.
pub(crate) fn find_name_traditional(
    line: &[u8],
    def: Option<&[u8]>,
    p_value: usize,
    root: &[u8],
) -> Option<Vec<u8>> {
    if line.first() == Some(&b'"')
        && let Some(name) = find_name_gnu(line, p_value, root)
    {
        return Some(name);
    }
    let len = line.len();
    let date = diff_timestamp_len(&line[..len]);
    if date == 0 {
        return find_name_common(line, def, p_value, None, TERM_TAB, root);
    }
    find_name_common(line, def, p_value, Some(len - date), 0, root)
}

/// git's `guess_p_value`: returns the strip count implied by a `---`/`+++`
/// name, or `None` when it cannot be determined.
pub(crate) fn guess_p_value(nameline: &[u8], root: &[u8], prefix: &[u8]) -> Option<usize> {
    if is_dev_null(nameline) {
        return None;
    }
    let name = find_name_traditional(nameline, None, 0, root)?;
    let Some(slash) = name.iter().position(|&b| b == b'/') else {
        return Some(0);
    };
    // git's `guess_p_value`: when running from a subdirectory (`state->prefix`),
    // a name that begins with the prefix (or with the prefix after its first
    // component, e.g. `a/sub/dir/file`) reveals how many leading components to
    // strip so the rest matches our directory. Without a prefix an embedded
    // slash leaves the strip count unknown.
    if prefix.is_empty() {
        return None;
    }
    if name.starts_with(prefix) {
        return Some(count_slashes(prefix));
    }
    if name[slash + 1..].starts_with(prefix) {
        return Some(count_slashes(prefix) + 1);
    }
    None
}

/// Count the `/` separators in a path, matching git's `count_slashes`.
fn count_slashes(path: &[u8]) -> usize {
    path.iter().filter(|&&b| b == b'/').count()
}

/// git's `has_epoch_timestamp`: does the `---`/`+++` line carry a GNU epoch
/// timestamp (signalling a creation/deletion)?
pub(crate) fn has_epoch_timestamp(nameline: &[u8]) -> bool {
    // Timestamp begins after the last tab.
    let mut timestamp: Option<usize> = None;
    for (i, &c) in nameline.iter().enumerate() {
        if c == b'\n' {
            break;
        }
        if c == b'\t' {
            timestamp = Some(i + 1);
        }
    }
    let Some(ts) = timestamp else {
        return false;
    };
    let rest = &nameline[ts..];
    let (after_date, epoch_hour) = if let Some(r) = rest.strip_prefix(b"1969-12-31 ".as_slice()) {
        (r, 24i64)
    } else if let Some(r) = rest.strip_prefix(b"1970-01-01 ".as_slice()) {
        (r, 0i64)
    } else {
        return false;
    };
    // Match: HH:MM:00(.0+)? SP [-+]HH[:]?MM  then end-of-line.
    parse_epoch_time(after_date, epoch_hour)
}

fn parse_epoch_time(s: &[u8], epoch_hour: i64) -> bool {
    // ^[0-2][0-9]:([0-5][0-9]):00(\.0+)? ([-+][0-2][0-9]:?[0-5][0-9])$
    let mut p = 0usize;
    let get = |p: usize| s.get(p).copied();
    if !matches!(get(p), Some(b'0'..=b'2')) {
        return false;
    }
    p += 1;
    if !matches!(get(p), Some(b'0'..=b'9')) {
        return false;
    }
    let hour = (s[0] - b'0') as i64 * 10 + (s[1] - b'0') as i64;
    p += 1;
    if get(p) != Some(b':') {
        return false;
    }
    p += 1;
    if !matches!(get(p), Some(b'0'..=b'5')) {
        return false;
    }
    if !matches!(get(p + 1), Some(b'0'..=b'9')) {
        return false;
    }
    let minute = (s[p] - b'0') as i64 * 10 + (s[p + 1] - b'0') as i64;
    p += 2;
    if get(p) != Some(b':') || get(p + 1) != Some(b'0') || get(p + 2) != Some(b'0') {
        return false;
    }
    p += 3;
    // Optional fractional `.0+`
    if get(p) == Some(b'.') {
        p += 1;
        let mut any = false;
        while get(p) == Some(b'0') {
            p += 1;
            any = true;
        }
        if !any {
            return false;
        }
    }
    if get(p) != Some(b' ') {
        return false;
    }
    p += 1;
    let sign = match get(p) {
        Some(b'+') => 1i64,
        Some(b'-') => -1i64,
        _ => return false,
    };
    p += 1;
    if !matches!(get(p), Some(b'0'..=b'2')) || !matches!(get(p + 1), Some(b'0'..=b'9')) {
        return false;
    }
    let zh = (s[p] - b'0') as i64 * 10 + (s[p + 1] - b'0') as i64;
    p += 2;
    let zm;
    if get(p) == Some(b':') {
        p += 1;
        if !matches!(get(p), Some(b'0'..=b'5')) || !matches!(get(p + 1), Some(b'0'..=b'9')) {
            return false;
        }
        zm = (s[p] - b'0') as i64 * 10 + (s[p + 1] - b'0') as i64;
        p += 2;
    } else {
        if !matches!(get(p), Some(b'0'..=b'5')) || !matches!(get(p + 1), Some(b'0'..=b'9')) {
            return false;
        }
        zm = (s[p] - b'0') as i64 * 10 + (s[p + 1] - b'0') as i64;
        p += 2;
    }
    // Must consume to end of line.
    if p != s.len() {
        return false;
    }
    let zoneoffset = sign * (zh * 60 + zm);
    hour * 60 + minute - zoneoffset == epoch_hour * 60
}

/// git's `git_header_name`: extract the common name from a `diff --git` line
/// for use as `def_name`. `rest` is the line after `diff --git `.
pub(crate) fn git_header_name(p_value: usize, rest: &[u8]) -> Option<Vec<u8>> {
    if rest.first() == Some(&b'"') {
        // Quoted first name.
        let (first, second_off) = unquote_c_style(rest)?;
        let cp = skip_tree_prefix(p_value, &first)?;
        let first_stripped = first[cp..].to_vec();

        // Skip whitespace to the second name.
        let mut second = second_off;
        while second < rest.len() && is_space(rest[second]) {
            second += 1;
        }
        if second >= rest.len() {
            return None;
        }
        if rest[second] == b'"' {
            let (sp, _) = unquote_c_style(&rest[second..])?;
            let cp2 = skip_tree_prefix(p_value, &sp)?;
            if sp[cp2..] != first_stripped[..] {
                return None;
            }
            return Some(first_stripped);
        }
        // Unquoted second.
        let tail = &rest[second..];
        let cp2 = skip_tree_prefix(p_value, tail)?;
        if tail[cp2..] != first_stripped[..] {
            return None;
        }
        return Some(first_stripped);
    }

    // Unquoted first name.
    let name_off = skip_tree_prefix(p_value, rest)?;
    let name = &rest[name_off..];

    // If a dquote appears, it begins the second name.
    if let Some(rel) = name.iter().position(|&b| b == b'"') {
        let second = &name[rel..];
        let (sp, _) = unquote_c_style(second)?;
        let np = skip_tree_prefix(p_value, &sp)?;
        let np_slice = &sp[np..];
        let len = np_slice.len();
        if len < rel && &name[..len] == np_slice && is_space(name[len]) {
            return Some(np_slice.to_vec());
        }
        return None;
    }

    // Accept the name only if it appears twice in identical form, separated by
    // a single SP/HT (git's "name shows up twice" check). Our line slice has no
    // trailing newline, so a postimage name ending the slice plays the role of
    // git's `second[len] == '\n'`.
    let mut len = 0usize;
    loop {
        match name.get(len) {
            None => return None,
            Some(&b'\t') | Some(&b' ') => {
                name.get(len + 1)?; // no postimage name
                let after = &name[len + 1..];
                if let Some(off) = skip_tree_prefix(p_value, after) {
                    let second = &after[off..];
                    if second.len() == len && second == &name[..len] {
                        return Some(name[..len].to_vec());
                    }
                }
                len += 1;
            }
            Some(_) => {
                len += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquote_basic() {
        assert_eq!(
            unquote_c_style(br#""qu\157tefile""#).map(|(v, _)| v),
            Some(b"quotefile".to_vec())
        );
        assert_eq!(
            unquote_c_style(br#""post\timage.txt""#).map(|(v, _)| v),
            Some(b"post\timage.txt".to_vec())
        );
        assert_eq!(
            unquote_c_style(br#""post\\image.txt""#).map(|(v, _)| v),
            Some(b"post\\image.txt".to_vec())
        );
        assert_eq!(
            unquote_c_style(br#""\"postimage\".txt""#).map(|(v, _)| v),
            Some(b"\"postimage\".txt".to_vec())
        );
    }

    #[test]
    fn skip_prefix_components() {
        assert_eq!(skip_tree_prefix(1, b"a/sub/file1"), Some(2));
        assert_eq!(skip_tree_prefix(2, b"a/sub/file1"), Some(6));
        assert_eq!(skip_tree_prefix(3, b"a/sub/file1"), None);
        assert_eq!(skip_tree_prefix(0, b"a/b"), Some(0));
        assert_eq!(skip_tree_prefix(0, b"/abs"), None); // absolute rejected with p0
        assert_eq!(skip_tree_prefix(1, b"/abs"), None); // leading slash -> i==0 -> None
    }

    #[test]
    fn header_name_modechange() {
        assert_eq!(
            git_header_name(2, b"a/sub/file1 b/sub/file1"),
            Some(b"file1".to_vec())
        );
        assert_eq!(
            git_header_name(1, b"a/sub/file1 b/sub/file1"),
            Some(b"sub/file1".to_vec())
        );
        // rename: names differ -> None
        assert_eq!(git_header_name(2, b"a/sub/file1 b/sub/file2"), None);
    }

    #[test]
    fn traditional_with_spaces() {
        // diff-with spaces: "--- post image.txt.orig\t<ts>" / "+++ post image.txt\t<ts>"
        let first = b"post image.txt.orig\t2010-08-18 20:13:31.544002255 -0500";
        let second = b"post image.txt\t2010-08-18 20:13:31.544002255 -0500";
        assert_eq!(guess_p_value(first, b"", b""), Some(0));
        let fname = find_name_traditional(first, None, 0, b"");
        let name = find_name_traditional(second, fname.as_deref(), 0, b"");
        assert_eq!(name, Some(b"post image.txt".to_vec()));
    }

    #[test]
    fn epoch_detection() {
        assert!(has_epoch_timestamp(
            b"a/post image.txt\t1969-12-31 18:00:00.000000000 -0600"
        ));
        assert!(has_epoch_timestamp(
            b"linux/post image.txt\t1969-12-31 16:00:00 -08:00"
        ));
        assert!(!has_epoch_timestamp(
            b"linux/post image.txt\t2005-04-12 02:14:06 -07:00"
        ));
    }
}
