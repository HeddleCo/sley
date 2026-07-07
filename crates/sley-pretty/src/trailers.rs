use crate::ForEachRefTrailerOptions;

struct TrailerItem {
    /// `Some(token)` for a real trailer; `None` for a preserved non-trailer line.
    token: Option<String>,
    value: String,
}

/// Render `%(trailers)` for `message` under `options`, mirroring git's
/// `format_trailers_from_commit` + `format_trailers`.
pub fn format_trailers_from_commit(
    message: &[u8],
    options: &ForEachRefTrailerOptions,
) -> Vec<u8> {
    let text = String::from_utf8_lossy(message);
    let block = trailer_block(&text);
    // Fast path: unmodified whole block.
    if !options.only
        && !options.unfold
        && options.filter.is_none()
        && options.separator.is_none()
        && !options.key_only
        && !options.value_only
        && options.key_value_separator.is_none()
    {
        return block.as_bytes().to_vec();
    }
    let items = trailer_parse_trailer_items(&block, options);
    let mut out = String::new();
    let orig_len = out.len();
    for item in &items {
        match &item.token {
            Some(token) => {
                let mut value = item.value.clone();
                if options.unfold {
                    value = trailer_unfold(&value);
                }
                if let Some(filter) = &options.filter
                    && !filter.iter().any(|key| key.eq_ignore_ascii_case(token))
                {
                    continue;
                }
                if let Some(sep) = &options.separator
                    && out.len() != orig_len
                {
                    out.push_str(sep);
                }
                if !options.value_only {
                    out.push_str(token);
                }
                if !options.key_only && !options.value_only {
                    if let Some(kvsep) = &options.key_value_separator {
                        out.push_str(kvsep);
                    } else {
                        // git appends "%c " using separators[0] (':') only when
                        // the token doesn't already end with a separator char.
                        let last = token.trim_end().chars().last();
                        if last != Some(':') {
                            out.push_str(": ");
                        }
                    }
                }
                if !options.key_only {
                    out.push_str(&value);
                }
                if options.separator.is_none() {
                    out.push('\n');
                }
            }
            None => {
                if options.only {
                    continue;
                }
                if let Some(sep) = &options.separator
                    && out.len() != orig_len
                {
                    out.push_str(sep);
                }
                out.push_str(&item.value);
                if options.separator.is_some() {
                    while out.ends_with([' ', '\t', '\n', '\r']) {
                        out.pop();
                    }
                } else {
                    out.push('\n');
                }
            }
        }
    }
    out.into_bytes()
}

/// The trailer block text (`[start, end)`) of a message, with `no_divider=1`
/// (the whole message is the log region).
fn trailer_block(message: &str) -> String {
    let bytes = message.as_bytes();
    let len = bytes.len();
    let start = trailer_find_trailer_block_start(message, len);
    message[start..].to_string()
}

/// Port of trailer.c `find_trailer_block_start` (no comment prefix; default
/// `:` separator; `Signed-off-by: ` / `(cherry picked from commit ` prefixes).
fn trailer_find_trailer_block_start(buf: &str, len: usize) -> usize {
    let bytes = buf.as_bytes();
    // Skip the title paragraph up to the first blank line.
    let mut s = 0usize;
    while s < len {
        if trailer_is_blank_line(bytes, s) {
            break;
        }
        s = trailer_next_line(bytes, s, len);
    }
    let end_of_title = s;

    let mut only_spaces = true;
    let mut recognized_prefix = false;
    let mut trailer_lines = 0i64;
    let mut non_trailer_lines = 0i64;
    let mut possible_continuation = 0i64;

    let mut maybe_l = trailer_last_line(bytes, len);
    while let Some(l) = maybe_l {
        if l < end_of_title {
            break;
        }
        if trailer_is_blank_line(bytes, l) {
            if only_spaces {
                // trailing blank; keep scanning upward
            } else {
                non_trailer_lines += possible_continuation;
                if (recognized_prefix && trailer_lines * 3 >= non_trailer_lines)
                    || (trailer_lines > 0 && non_trailer_lines == 0)
                {
                    return trailer_next_line(bytes, l, len);
                }
                return len;
            }
        } else {
            only_spaces = false;
            let line = trailer_line_text(buf, l, len);
            if line.starts_with("Signed-off-by: ")
                || line.starts_with("(cherry picked from commit ")
            {
                trailer_lines += 1;
                possible_continuation = 0;
                recognized_prefix = true;
            } else if trailer_find_separator(line).is_some_and(|pos| pos >= 1)
                && !bytes[l].is_ascii_whitespace()
            {
                trailer_lines += 1;
                possible_continuation = 0;
            } else if bytes[l].is_ascii_whitespace() {
                possible_continuation += 1;
            } else {
                non_trailer_lines += 1;
                non_trailer_lines += possible_continuation;
                possible_continuation = 0;
            }
        }
        if l == 0 {
            break;
        }
        maybe_l = trailer_last_line(bytes, l);
    }
    len
}

/// Parse the trailer block into items, joining continuation lines (git's
/// `trailer_block_get` split + `parse_trailers`).
fn trailer_parse_trailer_items(
    block: &str,
    options: &ForEachRefTrailerOptions,
) -> Vec<TrailerItem> {
    // Split on '\n' keeping each line; fold continuation lines (leading
    // whitespace) into the previous line *only if it had a separator*.
    let mut lines: Vec<String> = Vec::new();
    let mut last_had_sep = false;
    for raw in block.split_inclusive('\n') {
        if last_had_sep
            && raw.starts_with([' ', '\t'])
            && let Some(prev) = lines.last_mut()
        {
            prev.push_str(raw);
            continue;
        }
        let has_sep = trailer_find_separator(raw).is_some_and(|pos| pos >= 1);
        last_had_sep = has_sep;
        lines.push(raw.to_string());
    }

    let mut items = Vec::new();
    for line in &lines {
        // Trim a single trailing newline for separator analysis / raw value.
        let trimmed_nl = line.strip_suffix('\n').unwrap_or(line);
        match trailer_find_separator(line).filter(|pos| *pos >= 1) {
            Some(sep) => {
                let token = line[..sep].trim().to_string();
                let value = line[sep + 1..].trim().to_string();
                items.push(TrailerItem {
                    token: Some(token),
                    value,
                });
            }
            None => {
                if !options.only {
                    items.push(TrailerItem {
                        token: None,
                        value: trimmed_nl.to_string(),
                    });
                }
            }
        }
    }
    items
}

/// git `find_separator` restricted to the default `:` separator.
fn trailer_find_separator(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut whitespace_found = false;
    for (idx, &c) in bytes.iter().enumerate() {
        if c == b':' {
            return Some(idx);
        }
        if !whitespace_found && (c.is_ascii_alphanumeric() || c == b'-') {
            continue;
        }
        if idx != 0 && (c == b' ' || c == b'\t') {
            whitespace_found = true;
            continue;
        }
        break;
    }
    None
}

/// git `unfold_value`: a newline plus following whitespace run collapses to one
/// space; result is trimmed.
fn trailer_unfold(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn trailer_is_blank_line(bytes: &[u8], pos: usize) -> bool {
    let mut idx = pos;
    while idx < bytes.len() && bytes[idx] != b'\n' {
        if !bytes[idx].is_ascii_whitespace() {
            return false;
        }
        idx += 1;
    }
    true
}

fn trailer_next_line(bytes: &[u8], pos: usize, len: usize) -> usize {
    match bytes[pos..len].iter().position(|&b| b == b'\n') {
        Some(rel) => pos + rel + 1,
        None => len,
    }
}

/// The byte offset of the start of the last line within `bytes[..len]`.
fn trailer_last_line(bytes: &[u8], len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    // If the region ends with '\n', that newline terminates the prior line.
    let end = if bytes[len - 1] == b'\n' {
        len - 1
    } else {
        len
    };
    if end == 0 {
        return Some(0);
    }
    match bytes[..end].iter().rposition(|&b| b == b'\n') {
        Some(nl) => Some(nl + 1),
        None => Some(0),
    }
}

fn trailer_line_text(buf: &str, pos: usize, len: usize) -> &str {
    let bytes = buf.as_bytes();
    let end = match bytes[pos..len].iter().position(|&b| b == b'\n') {
        Some(rel) => pos + rel,
        None => len,
    };
    &buf[pos..end]
}
