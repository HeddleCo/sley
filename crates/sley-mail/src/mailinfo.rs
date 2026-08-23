//! `git mailinfo` / `git mailsplit` text engines: mbox + mboxrd splitting,
//! RFC822 mail-header parsing, RFC 2047 encoded-word decoding, RFC 2822 /
//! asctime date parsing, subject cleanup, and stgit/hg patch-to-mail
//! conversion. Extracted verbatim from `sley-cli/src/commands/am.rs`; all of
//! it is repo-independent byte processing (deps: sley-core date helpers +
//! encoding_rs).
#![allow(clippy::unwrap_used)]

use encoding_rs::{self, UTF_8};
use sley_core::Result;

/// Charset-name resolution for RFC 2047 encoded words (git's `convert_to_utf8`
/// label lookup). Mirrors the sley-pretty `encoding_for_name` helper: git's
/// UTF-8/Latin-1 aliases first, a few legacy mail charsets, then encoding_rs'
/// WHATWG label table. Duplicated from sley-pretty because this crate must
/// depend only on sley-core + external crates.
pub fn encoding_for_name(name: &str) -> Option<&'static encoding_rs::Encoding> {
    let n = name.trim();
    if n.is_empty() || n.eq_ignore_ascii_case("utf-8") || n.eq_ignore_ascii_case("utf8") {
        return Some(UTF_8);
    }
    if matches!(
        n.to_ascii_lowercase().as_str(),
        "iso8859-1" | "iso-8859-1" | "latin1" | "latin-1" | "8859-1"
    ) {
        return Some(encoding_rs::WINDOWS_1252);
    }
    let compact = n
        .bytes()
        .filter(|byte| !matches!(*byte, b'-' | b'_' | b' '))
        .map(|byte| byte.to_ascii_uppercase())
        .collect::<Vec<_>>();
    match compact.as_slice() {
        b"EUCJP" => return Some(encoding_rs::EUC_JP),
        b"ISO2022JP" => return Some(encoding_rs::ISO_2022_JP),
        _ => {}
    }
    encoding_rs::Encoding::for_label(n.as_bytes())
}

/// How a patch's `Subject:` header should be cleaned, mirroring git's mailinfo
/// `keep_subject` (`-k`) and `keep_non_patch_brackets_in_subject` (`-b`).
#[derive(Clone, Copy, Default)]
pub struct SubjectCleanup {
    /// `-k`/`--keep`: keep the subject verbatim, no cleanup at all.
    pub keep_subject: bool,
    /// `-b`/`--keep-non-patch`: strip `[PATCH]` brackets but keep other `[…]`.
    pub keep_non_patch_brackets: bool,
    /// `--scissors`: discard everything before the scissors cut line.
    pub scissors: bool,
}

/// A single message extracted from an mbox: identity, message, and raw diff.
pub struct MailMessage {
    /// Author name from the `From:` header.
    pub author_name: Vec<u8>,
    /// Author email from the `From:` header.
    pub author_email: Vec<u8>,
    /// Charset of `author_name` / `author_email`.
    pub author_encoding: String,
    /// Author date from the `Date:` header, already normalised to
    /// `"<seconds> <±HHMM>"`. `None` when the header was absent or unparsable
    /// (the committer/env date is then used).
    pub author_date: Option<String>,
    /// Original `Date:` header text, preserved verbatim for the author-script.
    pub author_date_raw: Option<String>,
    /// Cleaned subject line (with any `[PATCH …]` prefix stripped).
    pub subject: String,
    /// Full commit message (subject + blank line + body), newline-terminated.
    pub message: Vec<u8>,
    /// Charset declared by the mail message for the commit message body.
    pub message_encoding: String,
    /// The raw `Message-ID:` header value (including the surrounding angle
    /// brackets, e.g. `<...@example.com>`), if the message carried one. Appended
    /// to the commit message when `--message-id`/`am.messageid` is set.
    pub message_id: Option<String>,
    /// The unified diff body (everything from the first `diff`/`---` onward).
    pub diff: Vec<u8>,
}

/// Convert a subject line out of a full commit message (first line).
pub fn subject_of_message(message: &[u8]) -> String {
    let end = message
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(message.len());
    String::from_utf8_lossy(&message[..end]).into_owned()
}

/// Strip a trailing CR from every CRLF in the buffer (git's default
/// `--no-keep-cr` mailsplit behaviour). Only `\r` immediately before a `\n` is
/// removed, so a lone `\r` mid-line (rare in mail) is preserved.
pub fn strip_cr(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut iter = input.iter().peekable();
    while let Some(&byte) = iter.next() {
        if byte == b'\r' && iter.peek() == Some(&&b'\n') {
            continue;
        }
        out.push(byte);
    }
    out
}

pub fn stgit_patch_to_mail(input: &[u8]) -> Vec<u8> {
    let lines = split_keep_newline(input);
    let mut out = Vec::new();
    let mut idx = 0;
    let mut subject_printed = false;
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        let text = String::from_utf8_lossy(line);
        if text.trim().is_empty() {
            idx += 1;
            continue;
        } else if let Some(rest) = text.strip_prefix("Author:") {
            out.extend_from_slice(format!("From:{rest}\n").as_bytes());
        } else if text.starts_with("From") || text.starts_with("Date") {
            out.extend_from_slice(line);
            out.push(b'\n');
        } else if !subject_printed {
            out.extend_from_slice(b"Subject: ");
            out.extend_from_slice(line);
            out.push(b'\n');
            subject_printed = true;
        } else {
            out.push(b'\n');
            out.extend_from_slice(line);
            out.push(b'\n');
            idx += 1;
            break;
        }
        idx += 1;
    }
    for line in &lines[idx..] {
        out.extend_from_slice(line);
    }
    out
}

pub fn hg_patch_to_mail(input: &[u8]) -> Vec<u8> {
    let lines = split_keep_newline(input);
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        let text = String::from_utf8_lossy(line);
        if let Some(rest) = text.strip_prefix("# User ") {
            out.extend_from_slice(format!("From: {rest}\n").as_bytes());
        } else if let Some(rest) = text.strip_prefix("# Date ") {
            if let Some(date) = parse_hg_date(rest) {
                out.extend_from_slice(format!("Date: {date}\n").as_bytes());
            }
        } else if text.starts_with("# ") {
            // Mercurial metadata/comment line.
        } else {
            out.push(b'\n');
            out.extend_from_slice(line);
            out.push(b'\n');
            idx += 1;
            break;
        }
        idx += 1;
    }
    for line in &lines[idx..] {
        out.extend_from_slice(line);
    }
    out
}

pub fn parse_hg_date(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let seconds = parts.next()?;
    let tz_west: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let minutes_east = -tz_west / 60;
    let sign = if minutes_east < 0 { '-' } else { '+' };
    let abs = minutes_east.abs();
    Some(format!("{seconds} {sign}{:02}{:02}", abs / 60, abs % 60))
}

pub fn unescape_mboxrd(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for line in split_keep_newline(input) {
        let trimmed = trim_trailing_newline(&line);
        let mut gt = 0usize;
        while trimmed.get(gt) == Some(&b'>') {
            gt += 1;
        }
        if gt > 0 && trimmed[gt..].starts_with(b"From ") {
            out.extend_from_slice(&line[1..]);
        } else {
            out.extend_from_slice(&line);
        }
    }
    out
}

/// Heuristic patch-format detection for explicit mbox files, mirroring what git
/// does before splitting: the content must look like a mailbox (`From `), a mail
/// (a `Header: value` line such as `From:`/`Subject:`/`Date:`), or a diff
/// (`diff --git`, `--- `, `Index:`). Empty/whitespace-only content fails.
pub fn looks_like_patch_input(input: &[u8]) -> bool {
    for line in split_keep_newline(input) {
        let line = trim_trailing_newline(&line);
        // git's mailsplit treats leading all-whitespace lines as blank and skips
        // them before locating the first header (the t4150 "preceding
        // whitespace" patch leads with 255 spaces).
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if line.starts_with(b"From ") || is_diff_start(line) {
            return true;
        }
        // A mail header line: a non-space token, then a colon (e.g. `Subject:`).
        if let Some(colon) = line.iter().position(|byte| *byte == b':')
            && colon > 0
            && line[..colon].iter().all(|byte| byte.is_ascii_graphic())
        {
            return true;
        }
        // First non-blank line is neither a header nor a diff: not a patch.
        break;
    }
    false
}

/// Split an mbox buffer into raw message byte buffers (`From `-delimited, each
/// separator line dropped, message content kept verbatim). This is the same
/// splitting `git mailsplit` performs when writing numbered output files: a
/// buffer with no separator at all yields the whole buffer as one message, and
/// whitespace-only input yields no messages.
pub fn split_mbox(input: &[u8]) -> Vec<Vec<u8>> {
    if input.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Vec::new();
    }
    let lines = split_keep_newline(input);
    // Identify message-start indices (mbox "From " separators).
    let mut starts = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with(b"From ") {
            starts.push(idx);
        }
    }
    if starts.is_empty() {
        return vec![input.to_vec()];
    }
    let mut messages = Vec::with_capacity(starts.len());
    for (position, &start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        // Skip the leading "From " separator line itself.
        let mut message = Vec::new();
        for line in &lines[start + 1..end] {
            message.extend_from_slice(line);
        }
        messages.push(message);
    }
    messages
}

/// Split an mbox into individual messages and parse each into an [`MailMessage`].
///
/// Messages are delimited by lines beginning with `From ` (the mbox "From_"
/// separator that `git format-patch` emits as `From <sha> Mon Sep 17 …`). A
/// buffer with no separator at all is treated as a single message, matching
/// git's lenient behaviour for a lone patch. Whitespace-only input yields no
/// messages (the caller treats that as a no-op). A message that turns out to
/// carry no diff is still returned so the series driver can report the exact
/// "Patch is empty." behaviour git uses (including its hint block).
pub fn parse_mbox(input: &[u8], cleanup: SubjectCleanup) -> Result<Vec<MailMessage>> {
    let mut patches = Vec::new();
    for message in split_mbox(input) {
        let patch = parse_message(&split_keep_newline(&message), cleanup)?;
        if is_pine_internal_message(&patch) {
            continue;
        }
        patches.push(patch);
    }
    Ok(patches)
}

pub fn parse_mboxrd(input: &[u8], cleanup: SubjectCleanup) -> Result<Vec<MailMessage>> {
    let mut patches = Vec::new();
    for message in split_mbox(input) {
        let unescaped = unescape_mboxrd(&message);
        let patch = parse_message(&split_keep_newline(&unescaped), cleanup)?;
        if is_pine_internal_message(&patch) {
            continue;
        }
        patches.push(patch);
    }
    Ok(patches)
}

pub fn is_pine_internal_message(patch: &MailMessage) -> bool {
    patch.diff.is_empty()
        && patch.subject == "DON'T DELETE THIS MESSAGE -- FOLDER INTERNAL DATA"
        && patch
            .message_id
            .as_deref()
            .is_some_and(|id| id.contains("foo-0001@example.com"))
}

/// Parse a single message (headers + blank line + body + diff).
pub fn parse_message(lines: &[Vec<u8>], cleanup: SubjectCleanup) -> Result<MailMessage> {
    let mut author_name = String::new();
    let mut author_email = String::new();
    let mut author_date = None;
    let mut author_date_raw = None;
    let mut subject = String::new();
    let mut message_id = None;
    let mut message_encoding = "UTF-8".to_string();

    // Skip any leading all-whitespace lines before the headers (git's mailinfo
    // ignores blank/whitespace lines preceding the first header; the t4150
    // "preceding whitespace" patch leads with a 255-space line).
    let mut idx = 0;
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        if line.iter().all(u8::is_ascii_whitespace) {
            idx += 1;
        } else {
            break;
        }
    }

    // Phase 1: RFC822-style headers, ending at the first blank line. Continuation
    // lines (leading whitespace) extend the previous header value.
    let mut last_header: Option<String> = None;
    let mut header_values: Vec<(String, String)> = Vec::new();
    while idx < lines.len() {
        let line = trim_trailing_newline(&lines[idx]);
        if line.is_empty() {
            idx += 1;
            break;
        }
        if (line[0] == b' ' || line[0] == b'\t') && last_header.is_some() {
            if let Some((_, value)) = header_values.last_mut() {
                value.push(' ');
                value.push_str(String::from_utf8_lossy(line).trim());
            }
            idx += 1;
            continue;
        }
        if let Some(colon) = line.iter().position(|byte| *byte == b':') {
            let name = String::from_utf8_lossy(&line[..colon])
                .trim()
                .to_lowercase();
            let value = String::from_utf8_lossy(&line[colon + 1..])
                .trim()
                .to_string();
            last_header = Some(name.clone());
            header_values.push((name, value));
        } else {
            // Not a header line — treat the rest as body (lenient).
            break;
        }
        idx += 1;
    }
    for (name, value) in &header_values {
        match name.as_str() {
            "from" => {
                let (name, email) = parse_from_header(value);
                author_name = name;
                author_email = email;
            }
            "date" => {
                author_date_raw = Some(value.clone());
                // RFC 2822 is the format `git format-patch` emits, but the rebase
                // apply backend stores the commit's raw git date (`<secs> <tz>` /
                // `@<secs> <tz>`) directly; accept that too so the round-trip
                // through the state dir preserves the author date.
                author_date = parse_rfc2822_date(value)
                    .or_else(|| parse_raw_git_date_normalized(value))
                    .or_else(|| parse_git_default_date(value));
            }
            "subject" => subject = clean_subject(value, cleanup),
            "message-id" if !value.is_empty() => message_id = Some(value.clone()),
            "content-type" => {
                if let Some(charset) = content_type_charset(value) {
                    message_encoding = charset;
                }
            }
            _ => {}
        }
    }

    if cleanup.scissors
        && let Some(cut) = lines[idx..]
            .iter()
            .position(|line| is_scissors_line(trim_trailing_newline(line)))
    {
        idx += cut + 1;
        subject.clear();
    }
    consume_in_body_headers(
        lines,
        &mut idx,
        cleanup,
        &mut author_name,
        &mut author_email,
        &mut author_date,
        &mut author_date_raw,
        &mut subject,
    );

    // Phase 2: the rest of the message is one of three regions, in order:
    //   1. the commit body — until a standalone `---` separator or the diff;
    //   2. an optional diffstat — between the `---` separator and the diff,
    //      which `git format-patch` emits and `git am` discards;
    //   3. the diff itself — from the first `diff --git`/`Index:` line onward,
    //      ending at the `-- ` signature footer format-patch appends.
    #[derive(PartialEq)]
    enum Region {
        Body,
        Diffstat,
        Diff,
    }
    let mut body_lines: Vec<&[u8]> = Vec::new();
    let mut diff = Vec::new();
    let mut region = Region::Body;
    while idx < lines.len() {
        let raw = &lines[idx];
        let line = trim_trailing_newline(raw);
        match region {
            Region::Body => {
                if is_diff_start(line) {
                    region = Region::Diff;
                    diff.extend_from_slice(raw);
                } else if line == b"---" {
                    // End of the commit message; a diffstat (or the diff) follows.
                    region = Region::Diffstat;
                } else {
                    body_lines.push(raw);
                }
            }
            Region::Diffstat => {
                // Skip diffstat lines until the patch proper begins.
                if is_diff_start(line) {
                    region = Region::Diff;
                    diff.extend_from_slice(raw);
                }
            }
            Region::Diff => {
                if line == b"-- " {
                    break;
                }
                diff.extend_from_slice(raw);
            }
        }
        idx += 1;
    }

    let message = if subject.is_empty() && !body_lines.is_empty() {
        subject = String::from_utf8_lossy(trim_trailing_newline(body_lines[0]))
            .trim()
            .to_string();
        build_commit_message(&subject, &body_lines[1..])
    } else {
        build_commit_message(&subject, &body_lines)
    };

    Ok(MailMessage {
        author_name: author_name.into_bytes(),
        author_email: author_email.into_bytes(),
        author_encoding: "UTF-8".to_string(),
        author_date,
        author_date_raw,
        subject,
        message,
        message_encoding,
        message_id,
        diff,
    })
}

/// Parse a `From:` value of the form `Name <email>` (or a bare address).
pub fn parse_from_header(value: &str) -> (String, String) {
    if let Some(open) = value.rfind('<')
        && let Some(close) = value[open..].find('>')
    {
        let email = value[open + 1..open + close].trim().to_string();
        let name = decode_mime_word(value[..open].trim())
            .trim_matches('"')
            .to_string();
        return (name, email);
    }
    // Bare address: use it for both, matching git's fallback for name.
    let addr = value.trim().to_string();
    (addr.clone(), addr)
}

pub fn content_type_charset(value: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        if let Some((key, raw_value)) = part.trim().split_once('=')
            && key.trim().eq_ignore_ascii_case("charset")
        {
            let charset = raw_value.trim().trim_matches('"');
            if !charset.is_empty() {
                return Some(charset.to_string());
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub fn consume_in_body_headers(
    lines: &[Vec<u8>],
    idx: &mut usize,
    cleanup: SubjectCleanup,
    author_name: &mut String,
    author_email: &mut String,
    author_date: &mut Option<String>,
    author_date_raw: &mut Option<String>,
    subject: &mut String,
) {
    let start = *idx;
    let mut scan = start;
    let mut last_header: Option<String> = None;
    let mut header_values: Vec<(String, String)> = Vec::new();
    let mut saw_blank = false;
    while scan < lines.len() {
        let line = trim_trailing_newline(&lines[scan]);
        if line.is_empty() {
            scan += 1;
            saw_blank = true;
            break;
        }
        if (line[0] == b' ' || line[0] == b'\t') && last_header.is_some() {
            if let Some((_, value)) = header_values.last_mut() {
                value.push(' ');
                value.push_str(String::from_utf8_lossy(line).trim());
            }
            scan += 1;
            continue;
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return;
        };
        let name = String::from_utf8_lossy(&line[..colon])
            .trim()
            .to_lowercase();
        if !matches!(name.as_str(), "from" | "date" | "subject") {
            return;
        }
        let value = String::from_utf8_lossy(&line[colon + 1..])
            .trim()
            .to_string();
        last_header = Some(name.clone());
        header_values.push((name, value));
        scan += 1;
    }
    if !saw_blank || header_values.is_empty() {
        return;
    }
    for (name, value) in &header_values {
        match name.as_str() {
            "from" => {
                let (name, email) = parse_from_header(value);
                *author_name = name;
                *author_email = email;
            }
            "date" => {
                *author_date_raw = Some(value.clone());
                *author_date = parse_rfc2822_date(value)
                    .or_else(|| parse_raw_git_date_normalized(value))
                    .or_else(|| parse_git_default_date(value));
            }
            "subject" => *subject = clean_subject(value, cleanup),
            _ => {}
        }
    }
    *idx = scan;
}

pub fn is_scissors_line(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line);
    text.contains(">8") && text.contains(" - - ")
}

/// Clean a `Subject:` value the way git's mailinfo `cleanup_subject` does:
/// repeatedly strip a leading `Re:` (case-insensitive), leading spaces / tabs /
/// colons, and `[…]` brackets, then trim. A `[…]` bracket is removed unless
/// `keep_non_patch_brackets` (`-b`/`--keep-non-patch`) is set AND the bracket is
/// ≥7 chars and does NOT contain `PATCH` — those non-patch brackets (e.g.
/// `[foo]`) are kept, along with one following space. With `keep_subject`
/// (`-k`/`--keep`) the subject is kept verbatim (only MIME-decoded + trimmed).
pub fn clean_subject(value: &str, cleanup: SubjectCleanup) -> String {
    let decoded = decode_mime_word(value);
    if cleanup.keep_subject {
        return decoded.trim().to_string();
    }
    let keep_non_patch = cleanup.keep_non_patch_brackets;
    let mut bytes = decoded.trim().as_bytes().to_vec();
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'r' | b'R' => {
                // A leading "Re:" (any case) is dropped.
                if at + 3 <= bytes.len()
                    && (bytes[at + 1] == b'e' || bytes[at + 1] == b'E')
                    && bytes[at + 2] == b':'
                {
                    bytes.drain(at..at + 3);
                    continue;
                }
                break;
            }
            b' ' | b'\t' | b':' => {
                bytes.remove(at);
                continue;
            }
            b'[' => {
                let Some(rel) = bytes[at..].iter().position(|&b| b == b']') else {
                    break;
                };
                let remove = rel + 1; // length of "[...]"
                let contains_patch =
                    remove >= 7 && bytes[at..at + remove].windows(5).any(|w| w == b"PATCH");
                if !keep_non_patch || contains_patch {
                    bytes.drain(at..at + remove);
                } else {
                    at += remove;
                    if bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                        at += 1;
                    }
                }
                continue;
            }
            _ => break,
        }
    }
    let cleaned = cleanup_space_bytes(&bytes);
    String::from_utf8_lossy(&cleaned).trim().to_string()
}

pub fn cleanup_space_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx].is_ascii_whitespace() {
            out.push(b' ');
            idx += 1;
            while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
        } else {
            out.push(bytes[idx]);
            idx += 1;
        }
    }
    out
}

/// Best-effort decode of RFC 2047 encoded-words for Q or B encodings.
/// Adjacent encoded words separated only by folded whitespace are concatenated,
/// which is what `format-patch -k` uses for multiline subjects.
pub fn decode_mime_word(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut idx = 0;
    let mut previous_encoded = false;
    while idx < value.len() {
        if let Some((decoded, consumed)) = decode_mime_word_at(&value[idx..]) {
            out.push_str(&decoded);
            idx += consumed;
            previous_encoded = true;
            continue;
        }

        let byte = value.as_bytes()[idx];
        if previous_encoded && byte.is_ascii_whitespace() {
            let whitespace_start = idx;
            while idx < value.len() && value.as_bytes()[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if decode_mime_word_at(&value[idx..]).is_some() {
                continue;
            }
            out.push_str(&value[whitespace_start..idx]);
            previous_encoded = false;
            continue;
        }

        let ch = value[idx..].chars().next().unwrap();
        out.push(ch);
        idx += ch.len_utf8();
        previous_encoded = false;
    }
    out
}

pub fn decode_mime_word_at(value: &str) -> Option<(String, usize)> {
    let rest = value.strip_prefix("=?")?;
    let charset_end = rest.find('?')?;
    let charset = &rest[..charset_end];
    let after_charset = &rest[charset_end + 1..];
    let encoding_end = after_charset.find('?')?;
    let encoding = &after_charset[..encoding_end];
    let payload = &after_charset[encoding_end + 1..];
    let end = payload.find("?=")?;
    let encoded = &payload[..end];
    let consumed = 2 + charset_end + 1 + encoding_end + 1 + end + 2;

    let decoded = match encoding.to_ascii_uppercase().as_str() {
        "Q" => decode_quoted_printable_word(encoded),
        "B" => decode_base64(encoded),
        _ => return None,
    };
    match decoded {
        Some(bytes) => {
            let encoding = encoding_for_name(charset).unwrap_or(UTF_8);
            let (decoded, _, _) = encoding.decode(&bytes);
            Some((decoded.into_owned(), consumed))
        }
        None => None,
    }
}

pub fn decode_quoted_printable_word(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'_' => {
                out.push(b' ');
                idx += 1;
            }
            b'=' if idx + 2 < bytes.len() => {
                let hi = (bytes[idx + 1] as char).to_digit(16)?;
                let lo = (bytes[idx + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                idx += 3;
            }
            other => {
                out.push(other);
                idx += 1;
            }
        }
    }
    Some(out)
}

pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input.bytes().filter(|byte| *byte != b'=').collect();
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in cleaned {
        let value = value(byte)?;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// Whether `line` begins a unified diff (a git or plain patch).
pub fn is_diff_start(line: &[u8]) -> bool {
    line.starts_with(b"diff --git ")
        || line.starts_with(b"--- ")
        || line.starts_with(b"diff --cc ")
        || line.starts_with(b"Index: ")
}

/// Build the full commit message: subject, blank line, then trimmed body.
///
/// Mirrors git's `cleanup`: the subject is the first line, followed by a blank
/// line and the body with leading/trailing blank lines removed. The result is
/// newline-terminated. An empty body yields just `subject\n`.
pub fn build_commit_message(subject: &str, body_lines: &[&[u8]]) -> Vec<u8> {
    // Drop leading and trailing blank lines from the body.
    let mut start = 0;
    while start < body_lines.len() && trim_trailing_newline(body_lines[start]).is_empty() {
        start += 1;
    }
    let mut end = body_lines.len();
    while end > start && trim_trailing_newline(body_lines[end - 1]).is_empty() {
        end -= 1;
    }
    let mut message = Vec::new();
    message.extend_from_slice(subject.as_bytes());
    message.push(b'\n');
    if end > start {
        message.push(b'\n');
        for line in &body_lines[start..end] {
            let trimmed = trim_trailing_newline(line);
            message.extend_from_slice(trimmed);
            message.push(b'\n');
        }
    }
    message
}

// ===========================================================================
// RFC 2822 date parsing → raw git timestamp
// ===========================================================================

/// Parse an RFC 2822 `Date:` value (e.g. `Sun, 27 Sep 2026 11:06:40 +0200`)
/// into git's raw `"<seconds> <±HHMM>"` form. Returns `None` if the value is not
/// in the expected shape, so callers can fall back to the environment date.
pub fn parse_rfc2822_date(value: &str) -> Option<String> {
    let mut tokens: Vec<&str> = value.split_whitespace().collect();
    // Optional leading weekday with trailing comma: "Sun," or "Sun".
    if let Some(first) = tokens.first() {
        let stripped = first.trim_end_matches(',');
        if WEEKDAYS.contains(&stripped) {
            tokens.remove(0);
        }
    }
    if tokens.len() < 5 {
        return None;
    }
    let day: u32 = tokens[0].parse().ok()?;
    let month = month_index(tokens[1])?;
    let year: i64 = tokens[2].parse().ok()?;
    let (hour, minute, second) = parse_clock(tokens[3])?;
    let timezone = parse_timezone(tokens[4])?;

    let days = sley_core::date::days_from_civil(year, month, day);
    let local_seconds = days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let seconds = local_seconds - timezone.1;
    Some(format!("{seconds} {}", timezone.0))
}

/// Parse git's "default" (asctime-style) date as `mailinfo` accepts it:
/// `[<DoW>] <Mon> <day> <HH:MM:SS> <year> [<tz>]`, e.g.
/// `Thu Dec 4 16:00:00 2008 -0800`. Distinct from RFC 2822 by the
/// month-before-day token order; the timezone defaults to `+0000` when absent.
pub fn parse_git_default_date(value: &str) -> Option<String> {
    let mut tokens: Vec<&str> = value.split_whitespace().collect();
    if let Some(first) = tokens.first()
        && WEEKDAYS.contains(&first.trim_end_matches(','))
    {
        tokens.remove(0);
    }
    if tokens.len() < 4 {
        return None;
    }
    let month = month_index(tokens[0])?;
    let day: u32 = tokens[1].parse().ok()?;
    let (hour, minute, second) = parse_clock(tokens[2])?;
    let year: i64 = tokens[3].parse().ok()?;
    let timezone = tokens
        .get(4)
        .and_then(|token| parse_timezone(token))
        .unwrap_or_else(|| ("+0000".to_string(), 0));

    let days = sley_core::date::days_from_civil(year, month, day);
    let local_seconds = days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let seconds = local_seconds - timezone.1;
    Some(format!("{seconds} {}", timezone.0))
}

/// Parse a raw git date (`<seconds> <±HHMM>` or `@<seconds> <±HHMM>`) into the
/// normalised `<seconds> <±HHMM>` form `author_date` carries. Returns `None` if
/// the value is not exactly two whitespace-separated raw-date fields.
pub fn parse_raw_git_date_normalized(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let seconds = parts.next()?;
    let tz = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let seconds = seconds.strip_prefix('@').unwrap_or(seconds);
    if seconds.is_empty() || !seconds.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if tz.len() != 5
        || !matches!(tz.as_bytes()[0], b'+' | b'-')
        || !tz.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    Some(format!("{seconds} {tz}"))
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

pub fn month_index(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| month.eq_ignore_ascii_case(token))
        .map(|index| index as u32 + 1)
}

pub fn parse_clock(token: &str) -> Option<(u32, u32, u32)> {
    let mut parts = token.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = match parts.next() {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second))
}

/// Parse a timezone token (`+0200`, `-0500`, or a named zone) into its
/// canonical `±HHMM` string plus offset in seconds east of UTC. Numeric tokens
/// go through the canonical bounded parser (core::date::parse_tz_offset, the
/// bounds of git's date.c match_tz); a handful of named zones from old mail
/// (mostly UTC-equivalents) are handled here.
pub fn parse_timezone(token: &str) -> Option<(String, i64)> {
    if let Some(offset) = sley_core::date::parse_tz_offset(token) {
        return Some((token.to_string(), offset));
    }
    let offset = match token {
        "UT" | "GMT" | "UTC" | "Z" => 0,
        "EST" => -5 * 3600,
        "EDT" => -4 * 3600,
        "CST" => -6 * 3600,
        "CDT" => -5 * 3600,
        "MST" => -7 * 3600,
        "MDT" => -6 * 3600,
        "PST" => -8 * 3600,
        "PDT" => -7 * 3600,
        _ => return None,
    };
    Some((format_offset(offset), offset))
}

pub fn format_offset(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.abs();
    format!(
        "{sign}{:02}{:02}",
        magnitude / 3600,
        (magnitude % 3600) / 60
    )
}

pub fn write_stored_subject_header(out: &mut Vec<u8>, subject: &str) {
    out.extend_from_slice(b"Subject: ");
    if subject.bytes().any(stored_subject_needs_rfc2047) {
        out.extend_from_slice(b"=?UTF-8?Q?");
        for byte in subject.bytes() {
            if stored_subject_q_safe(byte) {
                out.push(byte);
            } else {
                out.extend_from_slice(format!("={byte:02X}").as_bytes());
            }
        }
        out.extend_from_slice(b"?=");
    } else {
        out.extend_from_slice(subject.as_bytes());
    }
    out.push(b'\n');
}

pub fn stored_subject_needs_rfc2047(byte: u8) -> bool {
    byte == b'\n' || byte == b'\r' || byte >= 0x80 || byte == b'='
}

pub fn stored_subject_q_safe(byte: u8) -> bool {
    byte.is_ascii_graphic() && byte != b'=' && byte != b'?' && byte != b'_' && byte < 0x80
}

/// Return the commit body (everything after the subject line and its trailing
/// blank line). Empty when the message is subject-only.
pub fn commit_message_body_after_subject(message: &[u8], subject: &str) -> Vec<u8> {
    let subject = subject.as_bytes();
    if message.starts_with(subject) && message.get(subject.len()) == Some(&b'\n') {
        let mut start = subject.len() + 1;
        if message.get(start) == Some(&b'\n') {
            start += 1;
        }
        return message[start..].to_vec();
    }

    let Some(first_lf) = message.iter().position(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let mut start = first_lf + 1;
    if message.get(start) == Some(&b'\n') {
        start += 1;
    }
    message[start..].to_vec()
}

// ===========================================================================

/// Split a buffer into lines, each retaining its trailing `\n` (the final line
/// keeps whatever terminator it had, or none).
pub fn split_keep_newline(input: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(input[start..=idx].to_vec());
            start = idx + 1;
        }
    }
    if start < input.len() {
        lines.push(input[start..].to_vec());
    }
    lines
}

/// A line without its trailing `\r?\n`.
pub fn trim_trailing_newline(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}
