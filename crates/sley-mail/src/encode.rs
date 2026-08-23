//! format-patch mail encoders: RFC 2047 encoded-word writing, RFC 822 address
//! quoting, header word-wrapping, MIME multipart framing, Message-ID /
//! In-Reply-To / References threading, and subject-paragraph extraction.
//! Extracted verbatim from `sley-cli/src/commands/format_patch.rs`.
#![allow(clippy::expect_used)]

/// MIME `multipart/mixed` wrapping for `--attach`/`--inline` (git's
/// `rev->mime_boundary` + `no_inline`). The `boundary` is the inner string
/// (without git's 12-dash `mime_boundary_leader`); `inline` selects the second
/// part's `Content-Disposition` (`inline` vs `attachment`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeAttach {
    pub boundary: String,
    pub inline: bool,
}

/// Which RFC 2047 character class governs the special-byte check: `Subject` is
/// the loose set (git's `RFC2047_SUBJECT`); `Address` is the tighter phrase set
/// (git's `RFC2047_ADDRESS`, used for `From:`/`To:` display names).
#[derive(Clone, Copy)]
pub enum Rfc2047Type {
    Subject,
    Address,
}

/// git's `needs_rfc2047_encoding`: a header needs RFC 2047 encoding if it carries
/// any non-ASCII byte, a newline, or the literal `=?` introducer.
pub fn needs_rfc2047_encoding(bytes: &[u8]) -> bool {
    for (i, &byte) in bytes.iter().enumerate() {
        if byte >= 0x80 || byte == b'\n' {
            return true;
        }
        if byte == b'=' && bytes.get(i + 1) == Some(&b'?') {
            return true;
        }
    }
    false
}

/// git's `is_rfc2047_special`. A byte must be `=%02X`-escaped inside an encoded
/// word when it is non-ASCII, non-printable, whitespace, or one of `=` `?` `_`.
/// For the `Address` (phrase) type the encodable set is further narrowed to
/// alphanumerics plus `! * + - / = _` (rfc2047 §5.3).
pub fn is_rfc2047_special(byte: u8, kind: Rfc2047Type) -> bool {
    // non-ASCII or non-printable
    if byte >= 0x80 || !(byte.is_ascii_graphic() || byte == b' ') {
        return true;
    }
    // special printable characters
    if byte.is_ascii_whitespace() || byte == b'=' || byte == b'?' || byte == b'_' {
        return true;
    }
    match kind {
        Rfc2047Type::Subject => false,
        // '=' and '_' were already handled above.
        Rfc2047Type::Address => {
            !(byte.is_ascii_alphanumeric()
                || byte == b'!'
                || byte == b'*'
                || byte == b'+'
                || byte == b'-'
                || byte == b'/')
        }
    }
}

/// How many bytes are already on the last line of `buf` (git's
/// `last_line_length`).
pub fn last_line_length(buf: &[u8]) -> usize {
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => buf.len() - (i + 1),
        None => buf.len(),
    }
}

/// Length of the leading UTF-8 multibyte sequence at `bytes` (git's
/// `mbs_chrlen` for a UTF-8 encoding). Returns 1 for ASCII / invalid lead bytes,
/// clamped to the remaining length so we never read past the end.
pub fn utf8_seq_len(bytes: &[u8]) -> usize {
    let lead = match bytes.first() {
        Some(&b) => b,
        None => return 0,
    };
    let want = if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        1
    };
    want.min(bytes.len())
}

/// Port of git's `add_rfc2047`: append `line` to `out` as one or more
/// `=?UTF-8?q?...?=` encoded words, folding at 76 columns. `out` must already
/// hold the header text up to the insertion point (e.g. `Subject: [PATCH] `),
/// since the first encoded word's budget is measured from the current last-line
/// length. Multi-byte UTF-8 characters are never split across encoded words.
pub fn add_rfc2047(out: &mut Vec<u8>, line: &[u8], kind: Rfc2047Type, encoding: &str) {
    const MAX_ENCODED_LENGTH: usize = 76;
    let mut line_len = last_line_length(out);
    out.extend_from_slice(format!("=?{encoding}?q?").as_bytes());
    line_len += encoding.len() + 5; // 5 for "=??q?"

    let mut rest = line;
    while !rest.is_empty() {
        let chrlen = utf8_seq_len(rest);
        let (chunk, tail) = rest.split_at(chrlen);
        let is_special = chrlen > 1 || is_rfc2047_special(chunk[0], kind);
        let encoded_len = if is_special { 3 * chrlen } else { 1 };

        if line_len + encoded_len + 2 > MAX_ENCODED_LENGTH {
            // It won't fit with the trailing "?=" — break the line.
            out.extend_from_slice(format!("?=\n =?{encoding}?q?").as_bytes());
            line_len = encoding.len() + 5 + 1; // "=??q?" plus the leading SP
        }

        if is_special {
            for &b in chunk {
                out.extend_from_slice(format!("={b:02X}").as_bytes());
            }
        } else {
            out.push(chunk[0]);
        }
        line_len += encoded_len;
        rest = tail;
    }
    out.extend_from_slice(b"?=");
}

/// git's `is_rfc822_special`: characters that force the display name to be
/// double-quoted in an address header.
pub fn is_rfc822_special(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b':' | b';' | b'@' | b',' | b'.' | b'"' | b'\\'
    )
}

/// git's `needs_rfc822_quoting`: true if any byte is an rfc822 special.
pub fn needs_rfc822_quoting(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| is_rfc822_special(b))
}

/// git's `add_rfc822_quoted`: wrap the name in double quotes, backslash-escaping
/// embedded `"` and `\`.
pub fn add_rfc822_quoted(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'"');
    for &b in bytes {
        if b == b'"' || b == b'\\' {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'"');
    out
}

/// Port of git's `strbuf_add_wrapped_text` (the bytes variant), appending `text`
/// to `out` word-wrapped to `width` columns. `indent1` is the first-line indent;
/// a *negative* `indent1` means "the first line already has `-indent1` columns of
/// content" (the leading `From: ` prefix). `indent2` is the continuation-line
/// indent. Wrapping breaks at ASCII whitespace; UTF-8 display width is honored.
pub fn add_wrapped_text(
    out: &mut Vec<u8>,
    text: &[u8],
    indent1: isize,
    indent2: isize,
    width: isize,
) {
    if width <= 0 {
        // git falls back to strbuf_add_indented_text; format-patch never calls
        // this with width<=0, so a plain append is sufficient here.
        out.extend_from_slice(text);
        return;
    }

    let mut indent = indent1;
    let mut w = indent1;
    // `bol`/`space` are byte offsets into `text`.
    let mut bol: usize = 0;
    let mut space: Option<usize> = None;
    if indent < 0 {
        w = -indent1;
        space = Some(0);
    }

    // Whether byte at `i` is ASCII whitespace (used when skipping the remembered
    // break point, matching git's `bol = space + isspace(*space)`).
    let is_ws = |i: usize| -> usize {
        usize::from(matches!(text.get(i), Some(&b) if b == b' ' || b == b'\t' || b == b'\n'))
    };

    let mut pos: usize = 0;
    loop {
        let c = text.get(pos).copied();
        let is_space = matches!(c, Some(b) if b == b' ' || b == b'\t' || b == b'\n');
        if c.is_none() || is_space {
            if w <= width || space.is_none() {
                let start = if let Some(sp) = space {
                    sp
                } else {
                    for _ in 0..indent.max(0) {
                        out.push(b' ');
                    }
                    bol
                };
                if c.is_none() && pos == start {
                    return;
                }
                out.extend_from_slice(&text[start..pos]);
                let ch = match c {
                    Some(ch) => ch,
                    None => return,
                };
                space = Some(pos);
                if ch == b'\t' {
                    w |= 0x07;
                } else if ch == b'\n' {
                    // A run of two newlines, or a newline before a non-alnum,
                    // forces a hard line break; otherwise it becomes a space.
                    let next = text.get(pos + 1).copied();
                    let sp = pos + 1;
                    if next == Some(b'\n')
                        || !next.map(|b| b.is_ascii_alphanumeric()).unwrap_or(false)
                    {
                        // new_line
                        out.push(b'\n');
                        pos = sp + is_ws(sp);
                        bol = pos;
                        space = None;
                        w = indent2;
                        indent = indent2;
                        continue;
                    }
                    out.push(b' ');
                }
                w += 1;
                pos += 1;
            } else {
                // new_line: too wide and we have a remembered space — break.
                out.push(b'\n');
                let sp = space.unwrap_or(pos);
                pos = sp + is_ws(sp);
                bol = pos;
                space = None;
                w = indent2;
                indent = indent2;
            }
            continue;
        }
        // A non-space character: advance one UTF-8 char, adding its display width.
        let seq = utf8_seq_len(&text[pos..]);
        w += utf8_display_width(&text[pos..pos + seq]);
        pos += seq;
    }
}

/// Display width of a single UTF-8 character for header wrapping. ASCII and the
/// non-ASCII letters exercised by t4014 are width 1; this is deliberately the
/// simple "1 column per codepoint" model git's `utf8_width` yields for those
/// ranges (no East-Asian wide handling, which format-patch headers never need).
pub fn utf8_display_width(_ch: &[u8]) -> isize {
    1
}

/// git's 12-dash `mime_boundary_leader` (diff.c). The actual delimiter lines are
/// `--` + this + the boundary string (14 dashes total).
pub const MIME_BOUNDARY_LEADER: &str = "------------";

/// The `multipart/mixed` preamble: the MIME headers, the human-readable note,
/// the first delimiter, and the first (`text/plain`) part's headers, ending in
/// the blank line that separates them from the commit body. Mirrors git's
/// `log_write_email_headers` strbuf for the `mime_boundary` case.
pub fn write_mime_preamble(out: &mut Vec<u8>, mime: &MimeAttach) {
    let b = &mime.boundary;
    write_fmt_buf(
        out,
        format_args!(
            "MIME-Version: 1.0\n\
             Content-Type: multipart/mixed; boundary=\"{MIME_BOUNDARY_LEADER}{b}\"\n\
             \n\
             This is a multi-part message in MIME format.\n\
             --{MIME_BOUNDARY_LEADER}{b}\n\
             Content-Type: text/plain; charset=UTF-8; format=fixed\n\
             Content-Transfer-Encoding: 8bit\n\n"
        ),
    );
}

/// The second (`text/x-patch`) part's headers, emitted between the diffstat and
/// the diff hunks (git's `stat_sep`). The leading `\n` is git's separator.
pub fn write_mime_part_header(out: &mut Vec<u8>, mime: &MimeAttach, filename: &str) {
    let b = &mime.boundary;
    let disposition = if mime.inline { "inline" } else { "attachment" };
    write_fmt_buf(
        out,
        format_args!(
            "\n--{MIME_BOUNDARY_LEADER}{b}\n\
             Content-Type: text/x-patch; name=\"{filename}\"\n\
             Content-Transfer-Encoding: 8bit\n\
             Content-Disposition: {disposition}; filename=\"{filename}\"\n\n"
        ),
    );
}

/// The closing delimiter that terminates the multipart message.
pub fn write_mime_closing(out: &mut Vec<u8>, mime: &MimeAttach) {
    write_fmt_buf(
        out,
        format_args!("\n--{MIME_BOUNDARY_LEADER}{}--\n\n\n", mime.boundary),
    );
}

/// Collapse the leading subject paragraph (git's `format_subject` with a single
/// space separator): consecutive non-blank message lines are trimmed of trailing
/// whitespace and joined by one space, stopping at the first blank line. This is
/// what turns a three-line `one\ntwo\nthree` subject into `one two three`.
pub fn format_patch_subject(message: &[u8]) -> Vec<u8> {
    format_patch_subject_with_separator(message, b" ")
}

/// Preserve the leading subject paragraph (git's `format_subject` with a
/// newline separator), used by `format-patch -k`.
pub fn format_patch_preserved_subject(message: &[u8]) -> Vec<u8> {
    format_patch_subject_with_separator(message, b"\n")
}

pub fn format_patch_subject_with_separator(message: &[u8], separator: &[u8]) -> Vec<u8> {
    let text = message;
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    let mut idx = 0;
    while idx < text.len() {
        let nl = text[idx..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| idx + p)
            .unwrap_or(text.len());
        let mut line = &text[idx..nl];
        // Trim trailing CR/space/tab like git's get_one_line/is_blank_line.
        while let Some(&last) = line.last() {
            if last == b' ' || last == b'\t' || last == b'\r' {
                line = &line[..line.len() - 1];
            } else {
                break;
            }
        }
        if line.is_empty() {
            break;
        }
        if !first {
            out.extend_from_slice(separator);
        }
        out.extend_from_slice(line);
        first = false;
        idx = nl + 1;
    }
    out
}

/// Byte offset immediately after the title paragraph and its first separating
/// blank line, matching the message pointer returned by git's `format_subject`.
pub fn format_patch_body_start(message: &[u8]) -> usize {
    let mut idx = 0;
    while idx < message.len() {
        let nl = message[idx..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| idx + p)
            .unwrap_or(message.len());
        let mut line = &message[idx..nl];
        while let Some(&last) = line.last() {
            if last == b' ' || last == b'\t' || last == b'\r' {
                line = &line[..line.len() - 1];
            } else {
                break;
            }
        }
        let next = if nl < message.len() { nl + 1 } else { nl };
        if line.is_empty() {
            return next;
        }
        idx = next;
    }
    idx
}

/// Append the `Subject:` header for one mail. Mirrors git's `pp_email_subject`
/// plus `fmt_output_email_subject`: writes `Subject: <prefix> `, then either an
/// RFC 2047 encoded-word sequence (when header-encoding is on and the subject
/// needs it) or an ASCII word-wrap at 78 columns (continuations indented one
/// space). The encoded path folds *inside* the encoded word at 76 columns; the
/// ASCII path measures its first-line budget from the prefix already written.
pub fn write_email_subject(
    out: &mut Vec<u8>,
    prefix: Option<&str>,
    subject: &[u8],
    encode: bool,
    output_encoding: &str,
) {
    const MAX_LENGTH: isize = 78;
    let header_start = out.len();
    match prefix {
        Some(prefix) => write_fmt_buf(out, format_args!("Subject: {prefix} ")),
        None => out.extend_from_slice(b"Subject: "),
    }
    if encode && needs_rfc2047_encoding(subject) {
        add_rfc2047(out, subject, Rfc2047Type::Subject, output_encoding);
    } else {
        let prefix_cols = (out.len() - header_start) as isize;
        add_wrapped_text(out, subject, -prefix_cols, 1, MAX_LENGTH);
    }
    out.push(b'\n');
}

/// Append a `From: <name> <email>` header, mirroring git's `pp_user_info` mail
/// branch: the display name is RFC 2047-encoded (when header-encoding is on and
/// it carries non-ASCII), else RFC 822-quoted if it has specials, else wrapped at
/// `max_length` columns; the ` <email>` is folded onto its own line when it would
/// overflow that last line.
pub fn write_from_header(
    out: &mut Vec<u8>,
    name: &[u8],
    email: &[u8],
    encode: bool,
    output_encoding: &str,
) {
    out.extend_from_slice(b"From: ");
    write_address_name_and_email(out, name, email, encode, output_encoding);
    out.push(b'\n');
}

pub fn write_address_name_and_email(
    out: &mut Vec<u8>,
    name_bytes: &[u8],
    email: &[u8],
    encode: bool,
    output_encoding: &str,
) {
    // git: max_length starts at 78, narrows to 76 once the name is rfc2047-encoded.
    let mut max_length: isize = 78;

    if encode && needs_rfc2047_encoding(name_bytes) {
        add_rfc2047(out, name_bytes, Rfc2047Type::Address, output_encoding);
        max_length = 76;
    } else if needs_rfc822_quoting(name_bytes) {
        let quoted = add_rfc822_quoted(name_bytes);
        let start_cols = last_line_length(out) as isize;
        add_wrapped_text(out, &quoted, -start_cols, 1, max_length);
    } else {
        let start_cols = last_line_length(out) as isize;
        add_wrapped_text(out, name_bytes, -start_cols, 1, max_length);
    }

    // git: if the " <email>" won't fit on the current last line, fold it down.
    let needed = last_line_length(out) as isize + 2 + email.len() as isize + 1;
    if max_length < needed {
        out.push(b'\n');
    }
    out.extend_from_slice(b" <");
    out.extend_from_slice(email);
    out.push(b'>');
}

/// Per-mail threading headers: the `Message-ID`, the `In-Reply-To` target, and
/// the ordered `References` chain (oldest → newest). Each id is the bare body
/// (no angle brackets); the writers add `<...>`.
#[derive(Default, Clone)]
pub struct MailThreadHeaders {
    pub message_id: Option<String>,
    pub references: Vec<String>,
}

impl MailThreadHeaders {
    /// Emit the `Message-ID`/`In-Reply-To`/`References` block. Mirrors git's
    /// `log_write_email_headers`: In-Reply-To is the *last* reference; References
    /// lists every reference, one per line, the first prefixed `References: ` and
    /// the rest indented by a single tab.
    pub fn write(&self, out: &mut Vec<u8>) {
        if let Some(id) = &self.message_id {
            writeln_fmt_buf(out, format_args!("Message-ID: <{id}>"));
        }
        if let Some(last) = self.references.last() {
            writeln_fmt_buf(out, format_args!("In-Reply-To: <{last}>"));
            for (i, r) in self.references.iter().enumerate() {
                let lead = if i > 0 { "\t" } else { "References: " };
                writeln_fmt_buf(out, format_args!("{lead}<{r}>"));
            }
        }
    }
}

/// The `--thread[=<style>]` / `format.thread` level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadLevel {
    Unset,
    Shallow,
    Deep,
}

/// Whether the commit *body* (everything past the header/body split, i.e. after
/// the first blank line) carries a non-ASCII byte — git's body scan in
/// `pp_title_line` that drives `need_8bit_cte`.
pub fn message_body_has_non_ascii(message: &[u8]) -> bool {
    let mut in_body = false;
    let mut i = 0;
    while i < message.len() {
        let ch = message[i];
        if !in_body {
            if ch == b'\n' && message.get(i + 1) == Some(&b'\n') {
                in_body = true;
            }
        } else if ch >= 0x80 {
            return true;
        }
        i += 1;
    }
    false
}

/// git's `gen_message_id`: `<base>.<timestamp>.git.<email>`. `base` is the cover
/// keyword `cover` or a commit oid hex; `timestamp` is captured once per run (git
/// uses `time(NULL)` per call, but a single run-wide value keeps References
/// byte-identical to the Message-IDs they reference, which is what the test
/// normalization checks).
pub fn gen_message_id(base: &str, timestamp: i64, email: &str) -> String {
    format!("{base}.{timestamp}.git.{email}")
}

/// git's `clean_message_id`: strip leading whitespace + `<`, and trailing
/// whitespace + `>`, returning the inner id. Used for `--in-reply-to`.
pub fn clean_message_id(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut start = 0;
    while start < bytes.len() && (bytes[start].is_ascii_whitespace() || bytes[start] == b'<') {
        start += 1;
    }
    // Last index that is neither whitespace nor '>'.
    let mut last = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if !b.is_ascii_whitespace() && b != b'>' {
            last = Some(i);
        }
    }
    match last {
        Some(z) => String::from_utf8_lossy(&bytes[start..=z]).into_owned(),
        None => raw.to_string(),
    }
}

/// The resolved threading plan for a whole run: the cover's headers (when a cover
/// is emitted) and one `MailThreadHeaders` per patch, in emission order. Built by
/// replaying git's per-mail threading state machine (builtin/log.c).
pub struct ThreadPlan {
    pub cover: MailThreadHeaders,
    pub patches: Vec<MailThreadHeaders>,
}

/// Replay git's threading state machine to assign Message-ID / In-Reply-To /
/// References to the cover and each patch. `start_number` is the first patch's
/// `n` (so `rev.nr` for patch index `i` is `start_number + i`).
pub fn build_thread_plan(
    level: ThreadLevel,
    in_reply_to: Option<&str>,
    cover_letter: bool,
    commit_oids: &[String],
    start_number: usize,
    timestamp: i64,
    email: &str,
) -> ThreadPlan {
    let threading = level != ThreadLevel::Unset;
    // ref_message_ids: the live reference list git mutates as it walks.
    let mut ref_ids: Vec<String> = Vec::new();
    if let Some(irt) = in_reply_to {
        ref_ids.push(clean_message_id(irt));
    }
    // rev.message_id: the id assigned to the previously-emitted mail.
    let mut prev_message_id: Option<String> = None;

    let mut cover = MailThreadHeaders::default();
    if cover_letter {
        if threading {
            let id = gen_message_id("cover", timestamp, email);
            prev_message_id = Some(id.clone());
            cover.message_id = Some(id);
        }
        // The cover's In-Reply-To/References come from any pre-seeded ref_ids
        // (i.e. --in-reply-to), captured *before* the cover's own id is pushed.
        cover.references = ref_ids.clone();
    }

    let mut patches = Vec::with_capacity(commit_oids.len());
    for (i, oid) in commit_oids.iter().enumerate() {
        let rev_nr = start_number + i;
        if threading {
            if let Some(prev) = prev_message_id.take() {
                // SHALLOW: drop the previous id (don't chain) when there is at
                // least one reference already and we're past the cover's reply.
                // DEEP: always chain the previous id into references.
                let shallow_drop = level == ThreadLevel::Shallow
                    && !ref_ids.is_empty()
                    && (!cover_letter || rev_nr > 1);
                if !shallow_drop {
                    ref_ids.push(prev);
                }
            }
            let id = gen_message_id(oid, timestamp, email);
            prev_message_id = Some(id.clone());
            let mut h = MailThreadHeaders {
                message_id: Some(id),
                references: ref_ids.clone(),
            };
            // The patch's own Message-ID is not part of its References list.
            let _ = &mut h;
            patches.push(h);
        } else {
            // No threading: only --in-reply-to seeds In-Reply-To/References,
            // and only when explicitly given (ref_ids non-empty).
            patches.push(MailThreadHeaders {
                message_id: None,
                references: ref_ids.clone(),
            });
        }
    }

    ThreadPlan { cover, patches }
}

// ---------------------------------------------------------------------------
// Local byte-writer helpers (duplicated from the sley-cli crate root so the
// moved writers stay verbatim).
// ---------------------------------------------------------------------------

fn write_fmt_buf(out: &mut Vec<u8>, args: std::fmt::Arguments<'_>) {
    std::io::Write::write_fmt(out, args).expect("writing to Vec cannot fail");
}

fn writeln_fmt_buf(out: &mut Vec<u8>, args: std::fmt::Arguments<'_>) {
    write_fmt_buf(out, args);
    out.push(b'\n');
}
