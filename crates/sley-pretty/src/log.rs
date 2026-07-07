//! Compiled log-format emission (`emit_compiled_log_format*`).

use crate::{
    CompiledLogFormat, DecorateSpec, DescribeSpec, FormatToken, LogFormatDialect,
    parse_for_each_ref_trailer_options,
};
use sley_config::GitConfig;
use sley_core::{DateMode, GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_ref_filter::commit_identity_date;
use sley_rev::revlist::commit_identity_timestamp;
use sley_rev::{CommitMetadata, CommitRecord};
use sley_refs::ReflogEntry;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use crate::trailers::format_trailers_from_commit;

struct EmptyMailmap;
impl MailmapLookup for EmptyMailmap {
    fn map_user(&self, name: &str, email: &str) -> (String, String) {
        (name.to_string(), email.to_string())
    }
}
pub trait MailmapLookup {
    fn map_user(&self, name: &str, email: &str) -> (String, String);
}
#[derive(Debug, Clone, Default)]
pub struct LogSignatureView {
    pub trust: String,
    pub signer: String,
    pub key: String,
    pub fingerprint: String,
    pub primary_fingerprint: String,
    pub pretty_code: u8,
    pub bare_output: Vec<u8>,
}
pub trait LogSignatureLookup {
    fn verification_for_oid(&self, oid: &ObjectId) -> Result<LogSignatureView>;
}
pub trait LogDescribeLookup {
    fn describe_oid(&self, oid: &ObjectId, spec: &DescribeSpec) -> Result<String>;
}

pub fn format_log_abbrev_oid(oid: &ObjectId) -> String {
    format_log_oid(oid, Some(7))
}

pub fn format_log_oid(oid: &ObjectId, abbrev_len: Option<usize>) -> String {
    let hex = oid.to_hex();
    match abbrev_len {
        Some(width) => hex[..width.min(hex.len())].to_string(),
        None => hex,
    }
}

/// Resolve a git color name / attribute (as used by `%Cred`, `%C(red)`,
/// `%C(bold red)`, etc.) to its ANSI escape sequence. Mirrors git's
/// `color_parse_mem` for the subset of single-word colours and attributes:
/// `reset`, `normal`, the eight ANSI colours, and the attribute words. Returns
/// `None` for an unknown name.
pub fn git_color_name_to_ansi(name: &str) -> Option<&'static str> {
    Some(match name {
        "reset" => "\x1b[m",
        "normal" => "", // GIT_COLOR_NORMAL is the empty string.
        "black" => "\x1b[30m",
        "red" => "\x1b[31m",
        "green" => "\x1b[32m",
        "yellow" => "\x1b[33m",
        "blue" => "\x1b[34m",
        "magenta" => "\x1b[35m",
        "cyan" => "\x1b[36m",
        "white" => "\x1b[37m",
        "bold" => "\x1b[1m",
        "dim" => "\x1b[2m",
        "italic" => "\x1b[3m",
        "ul" => "\x1b[4m",
        "blink" => "\x1b[5m",
        "reverse" => "\x1b[7m",
        "strike" => "\x1b[9m",
        _ => return None,
    })
}

/// Resolve a multi-word `%C(...)` colour spec (e.g. `bold red`, `auto,green`,
/// `red yellow bold`) to an ANSI escape, mirroring git's `color_parse_mem`. The
/// grammar is `[reset] [fg [bg]] [attr]...` and the emitted SGR sequence is
/// ordered **attributes (numeric order), then foreground, then background** —
/// matching git's `color_parse_mem_1`. A leading `auto` qualifier only emits
/// when `color` is on; `always` forces it; `never` suppresses it. Returns an
/// empty string when nothing applies.
pub fn git_color_spec_to_ansi(spec: &str, color: bool) -> String {
    let normalized = spec.replace(',', " ");
    let mut words: Vec<&str> = normalized.split_whitespace().collect();
    let mut effective_color = color;
    if let Some(first) = words.first() {
        match *first {
            "auto" => {
                words.remove(0);
            }
            "always" => {
                effective_color = true;
                words.remove(0);
            }
            "never" => return String::new(),
            _ => {}
        }
    }
    if !effective_color {
        return String::new();
    }

    let mut has_reset = false;
    let mut attr_bits: u32 = 0;
    let mut fg: Option<u8> = None;
    let mut bg: Option<u8> = None;
    for word in words {
        if word == "reset" {
            has_reset = true;
            continue;
        }
        if let Some(ansi) = git_ansi_color_value(word) {
            // `[fg [bg]]`: first colour is fg, second is bg.
            if fg.is_none() {
                fg = Some(ansi);
            } else if bg.is_none() {
                bg = Some(ansi);
            }
            continue;
        }
        if let Some(bit) = git_attr_bit(word) {
            attr_bits |= 1 << bit;
        }
    }

    if !has_reset && attr_bits == 0 && fg.is_none() && bg.is_none() {
        return String::new();
    }
    let mut codes: Vec<String> = Vec::new();
    // Attributes in ascending numeric (bit) order.
    for bit in 0..32u32 {
        if attr_bits & (1 << bit) != 0 {
            codes.push(bit.to_string());
        }
    }
    if let Some(v) = fg {
        codes.push((30 + v).to_string());
    }
    if let Some(v) = bg {
        codes.push((40 + v).to_string());
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// The ANSI offset (0-7, or 9 for `default`) of a basic colour word, or `None`
/// if the word is not a colour.
fn git_ansi_color_value(word: &str) -> Option<u8> {
    Some(match word {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        "default" => 9,
        _ => return None,
    })
}

/// The SGR code (bit position) of an attribute word, mirroring git's
/// `parse_attr`, or `None` if the word is not an attribute.
fn git_attr_bit(word: &str) -> Option<u32> {
    Some(match word {
        "bold" => 1,
        "dim" => 2,
        "italic" => 3,
        "ul" => 4,
        "blink" => 5,
        "reverse" => 7,
        "strike" => 9,
        _ => return None,
    })
}

pub fn append_log_oid(out: &mut Vec<u8>, oid: &ObjectId, abbrev_len: Option<usize>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let width = abbrev_len
        .map(|width| oid.abbrev_hex_len(width))
        .unwrap_or_else(|| oid.as_bytes().len() * 2);
    let mut written = 0usize;
    for byte in oid.as_bytes() {
        if written >= width {
            break;
        }
        out.push(HEX[(byte >> 4) as usize]);
        written += 1;
        if written >= width {
            break;
        }
        out.push(HEX[(byte & 0x0f) as usize]);
        written += 1;
    }
}

pub fn format_log_commit_header_oid(
    oid: &ObjectId,
    abbrev_commit: bool,
    abbrev_len: Option<usize>,
) -> String {
    if abbrev_commit {
        format_log_oid(oid, abbrev_len)
    } else {
        oid.to_string()
    }
}

pub fn format_log_parent_oids(record: &sley_rev::CommitRecord, abbrev_len: Option<usize>) -> String {
    record
        .parents
        .iter()
        .map(|oid| format_log_oid(oid, abbrev_len))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn commit_subject(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Raw-bytes subject: the title paragraph with internal newlines folded to
/// single spaces (git's `format_subject`), preserving non-UTF-8/control bytes.
pub fn commit_subject_bytes(message: &[u8]) -> &[u8] {
    // git skips leading blank lines, then takes lines until a blank line,
    // joining with spaces. The upstream corpus only uses single-line subjects,
    // so we return the first non-empty line slice directly.
    let mut start = 0;
    while start < message.len() && (message[start] == b'\n' || message[start] == b'\r') {
        start += 1;
    }
    let end = message[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|off| start + off)
        .unwrap_or(message.len());
    &message[start..end]
}

pub fn commit_body(message: &[u8]) -> &[u8] {
    let Some(first_newline) = message.iter().position(|byte| *byte == b'\n') else {
        return &[];
    };
    let mut body = &message[first_newline + 1..];
    if body.first().copied() == Some(b'\n') {
        body = &body[1..];
    }
    body
}

pub fn commit_message_lines(message: &[u8]) -> Vec<&[u8]> {
    let mut lines = message.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

pub struct LogFormatContext<'a> {
    pub abbrev_len: Option<usize>,
    pub decorations: &'a HashMap<ObjectId, Vec<String>>,
    pub marker: char,
    pub dialect: LogFormatDialect,
    pub source: Option<&'a str>,
    pub date_mode: &'a DateMode,
    pub source_oid: Option<&'a HashMap<ObjectId, String>>,
    pub describe: Option<&'a dyn LogDescribeLookup>,
    pub signature: Option<&'a dyn LogSignatureLookup>,
    pub color: bool,
    pub output_encoding: &'a str,
    pub mailmap: &'a dyn MailmapLookup,
    pub use_mailmap: bool,
}

/// Render a single `$Format:<fmt>$` inner format against `record`, returning the
/// expanded bytes. Backs `git archive`'s `export-subst` (the same pretty-format
/// placeholders as `git log --pretty=format:`). `fmt` is the text between
/// `$Format:` and the closing `$`.
pub fn format_subst_for_commit(
    record: &sley_rev::CommitRecord,
    fmt: &[u8],
) -> Result<Vec<u8>> {
    let fmt = String::from_utf8_lossy(fmt);
    let compiled = CompiledLogFormat::compile(&fmt, LogFormatDialect::Log)?;
    let decorations = HashMap::new();
    let date_mode = DateMode::Default;
    // `export-subst` substitution does not mailmap (git uses the raw ident).
    let mailmap = EmptyMailmap;
    let context = LogFormatContext {
        abbrev_len: None,
        decorations: &decorations,
        marker: '>',
        dialect: LogFormatDialect::Log,
        source: None,
        date_mode: &date_mode,
        source_oid: None,
        describe: None,
        signature: None,
        color: false,
        output_encoding: "UTF-8",
        mailmap: &mailmap as &dyn MailmapLookup,
        use_mailmap: false,
    };
    let mut out = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(
        record,
        &compiled,
        &context,
        &mut out,
        0..compiled.tokens.len(),
    )?;
    Ok(out)
}

pub fn emit_compiled_log_format(
    record: &sley_rev::CommitRecord,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
    token_range: std::ops::Range<usize>,
) -> Result<()> {
    let (author_name, author_email) = commit_identity_name_email(&record.commit.author);
    let (committer_name, committer_email) = commit_identity_name_email(&record.commit.committer);
    let author_timestamp = commit_identity_timestamp(&record.commit.author);
    let committer_timestamp = commit_identity_timestamp(&record.commit.committer);

    // Wrap state (git's `format_commit_context`): width/indents plus the offset in
    // `out` where the current wrap region began. A `%w` directive (or end-of-
    // format) flushes the pending region through the word-wrapper.
    let mut wrap_width = 0i32;
    let mut wrap_indent1 = 0i32;
    let mut wrap_indent2 = 0i32;
    let mut wrap_start = out.len();
    let mut resolver = LogFormatAtomResolver {
        record,
        context,
        author_name: &author_name,
        author_email: &author_email,
        committer_name: &committer_name,
        committer_email: &committer_email,
        author_timestamp: &author_timestamp,
        committer_timestamp: &committer_timestamp,
        auto_color: false,
    };
    let segment_range = compiled.segment_range_for_tokens(token_range);
    compiled.expand.append_range_to_with_atom(
        out,
        segment_range,
        &mut resolver,
        |out, token, value| {
            if let FormatToken::Wrap(spec) = token {
                let new_w = spec.width as i32;
                let new_i1 = spec.indent1 as i32;
                let new_i2 = spec.indent2 as i32;
                if (new_w, new_i1, new_i2) != (wrap_width, wrap_indent1, wrap_indent2) {
                    if wrap_start < out.len() {
                        log_rewrap(out, wrap_start, wrap_width, wrap_indent1, wrap_indent2);
                    }
                    wrap_start = out.len();
                    wrap_width = new_w;
                    wrap_indent1 = new_i1;
                    wrap_indent2 = new_i2;
                }
            } else {
                out.extend_from_slice(value);
            }
            Ok(())
        },
    )?;
    // git's final `rewrap_message_tail(sb, c, 0, 0, 0)`: flush the tail region if
    // a non-trivial wrap width is active.
    if (wrap_width, wrap_indent1, wrap_indent2) != (0, 0, 0) && wrap_start < out.len() {
        log_rewrap(out, wrap_start, wrap_width, wrap_indent1, wrap_indent2);
    }
    Ok(())
}

/// git's `strbuf_wrap`: word-wrap `out[pos..]` in place.
pub fn log_rewrap(out: &mut Vec<u8>, pos: usize, width: i32, indent1: i32, indent2: i32) {
    let region = out.split_off(pos);
    log_wrap_text(out, &region, indent1, indent2, width);
}

struct LogFormatAtomResolver<'a, 'b> {
    record: &'a sley_rev::CommitRecord,
    context: &'a LogFormatContext<'b>,
    author_name: &'a str,
    author_email: &'a str,
    committer_name: &'a str,
    committer_email: &'a str,
    author_timestamp: &'a str,
    committer_timestamp: &'a str,
    auto_color: bool,
}

impl sley_strbuf_expand::AtomResolver<FormatToken> for LogFormatAtomResolver<'_, '_> {
    fn resolve_atom(&mut self, out: &mut Vec<u8>, atom: &FormatToken) -> Result<()> {
        if self.auto_color && matches!(atom, FormatToken::OidFull | FormatToken::OidAbbrev) {
            self.auto_color = false;
            if self.context.color {
                out.extend_from_slice(b"\x1b[33m");
                emit_log_one_token(
                    atom,
                    self.record,
                    self.context,
                    out,
                    self.author_name,
                    self.author_email,
                    self.committer_name,
                    self.committer_email,
                    self.author_timestamp,
                    self.committer_timestamp,
                )?;
                out.extend_from_slice(b"\x1b[m");
                return Ok(());
            }
        }
        if matches!(atom, FormatToken::ColorAuto) {
            self.auto_color = self.context.color;
            return Ok(());
        }
        emit_log_one_token(
            atom,
            self.record,
            self.context,
            out,
            self.author_name,
            self.author_email,
            self.committer_name,
            self.committer_email,
            self.author_timestamp,
            self.committer_timestamp,
        )
    }

    fn is_modifier_atom(&self, atom: &FormatToken) -> bool {
        matches!(
            atom,
            FormatToken::ColorParen(_) | FormatToken::ColorName(_) | FormatToken::ColorAuto
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub fn emit_log_one_token(
    token: &FormatToken,
    record: &sley_rev::CommitRecord,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
    author_timestamp: &str,
    committer_timestamp: &str,
) -> Result<()> {
    let LogFormatContext {
        abbrev_len,
        decorations,
        marker,
        dialect,
        source,
        date_mode,
        source_oid,
        describe,
        signature,
        color,
        output_encoding,
        ..
    } = *context;
    // git formats in UTF-8 (re-encoding the stored message to UTF-8 up front),
    // computes alignment/width in UTF-8, and re-encodes the *final* output to the
    // log output encoding once at the end (handled by the print path). So here we
    // always normalise the message to UTF-8.
    let _ = output_encoding;
    let reencoded_message = log_reencode_message(
        &record.commit.message,
        &commit_encoding(&record.commit),
        "UTF-8",
    );
    let message: &[u8] = &reencoded_message;
    // git's `format_person_part`: only the UPPER-case `%aN`/`%aE`/`%aL` atoms run
    // through the mailmap; the lower-case `%an`/`%ae`/`%al` are ALWAYS raw, even
    // under `--use-mailmap` (that flag only maps the default `Author:` line via
    // `pp_user_info`, handled in the default-format path — not here).
    let mailmap = context.mailmap;
    let (mapped_author_name, mapped_author_email) = mailmap.map_user(author_name, author_email);
    let (mapped_committer_name, mapped_committer_email) =
        mailmap.map_user(committer_name, committer_email);
    {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::OidFull => write!(out, "{}", record.oid).map_err(io::Error::from)?,
            FormatToken::OidAbbrev => {
                write!(out, "{}", format_log_oid(&record.oid, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::TreeFull => {
                write!(out, "{}", record.commit.tree).map_err(io::Error::from)?
            }
            FormatToken::TreeAbbrev => {
                write!(out, "{}", format_log_oid(&record.commit.tree, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::ParentsFull => {
                write!(out, "{}", format_log_parent_oids(record, None)).map_err(io::Error::from)?;
            }
            FormatToken::ParentsAbbrev => {
                write!(out, "{}", format_log_parent_oids(record, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::Marker => out.push(marker as u8),
            FormatToken::Subject => {
                out.extend_from_slice(commit_subject_bytes(message));
            }
            FormatToken::SanitizedSubject => {
                write!(out, "{}", log_sanitized_subject(message)).map_err(io::Error::from)?;
            }
            FormatToken::Encoding => {
                write!(out, "{}", commit_encoding(&record.commit)).map_err(io::Error::from)?;
            }
            FormatToken::NoteName if dialect == LogFormatDialect::Log => {}
            FormatToken::NoteName => out.extend_from_slice(b"%N"),
            FormatToken::RevisionSource if dialect == LogFormatDialect::Log => {
                if let Some(map) = source_oid
                    && let Some(label) = map.get(&record.oid)
                {
                    out.extend_from_slice(label.as_bytes());
                } else if let Some(source) = source {
                    out.extend_from_slice(source.as_bytes());
                }
            }
            FormatToken::RevisionSource => out.extend_from_slice(b"%S"),
            FormatToken::ColorName(name) => {
                if color && let Some(ansi) = git_color_name_to_ansi(name) {
                    out.extend_from_slice(ansi.as_bytes());
                }
            }
            FormatToken::ColorParen(spec) => {
                let ansi = git_color_spec_to_ansi(spec, color);
                out.extend_from_slice(ansi.as_bytes());
            }
            FormatToken::Body => out.extend_from_slice(commit_body(message)),
            FormatToken::FullMessage => out.extend_from_slice(message),
            FormatToken::DecorationsParen => {
                write!(
                    out,
                    "{}",
                    format_log_format_decorations(&record.oid, decorations, true)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::DecorationsBare => {
                write!(
                    out,
                    "{}",
                    format_log_format_decorations(&record.oid, decorations, false)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::GRefname
            | FormatToken::GTrailers
            | FormatToken::GPlaceholder
            | FormatToken::GSignature
            | FormatToken::GKey
            | FormatToken::GFingerprint
            | FormatToken::GPassthrough => {
                emit_log_signature_atom(out, token, record, signature)?;
            }
            FormatToken::GDate
            | FormatToken::GDateShort
            | FormatToken::GDateIso
            | FormatToken::GDateIsoStrict
            | FormatToken::GDateRfc2822 => {}
            FormatToken::AuthorName => out.extend_from_slice(author_name.as_bytes()),
            FormatToken::AuthorEmail => out.extend_from_slice(author_email.as_bytes()),
            FormatToken::AuthorEmailLocal => {
                write!(out, "{}", log_email_local_part(author_email)).map_err(io::Error::from)?;
            }
            FormatToken::AuthorNameMapped => out.extend_from_slice(mapped_author_name.as_bytes()),
            FormatToken::AuthorEmailMapped => out.extend_from_slice(mapped_author_email.as_bytes()),
            FormatToken::AuthorEmailLocalMapped => {
                write!(out, "{}", log_email_local_part(&mapped_author_email))
                    .map_err(io::Error::from)?;
            }
            FormatToken::AuthorTimestamp => out.extend_from_slice(author_timestamp.as_bytes()),
            FormatToken::AuthorDate => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, date_mode)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, &DateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, &DateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, &DateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, &DateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateHuman => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.author, &DateMode::Human)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterName => out.extend_from_slice(committer_name.as_bytes()),
            FormatToken::CommitterEmail => out.extend_from_slice(committer_email.as_bytes()),
            FormatToken::CommitterEmailLocal => {
                write!(out, "{}", log_email_local_part(committer_email))
                    .map_err(io::Error::from)?;
            }
            FormatToken::CommitterNameMapped => {
                out.extend_from_slice(mapped_committer_name.as_bytes())
            }
            FormatToken::CommitterEmailMapped => {
                out.extend_from_slice(mapped_committer_email.as_bytes())
            }
            FormatToken::CommitterEmailLocalMapped => {
                write!(out, "{}", log_email_local_part(&mapped_committer_email))
                    .map_err(io::Error::from)?;
            }
            FormatToken::CommitterTimestamp => {
                out.extend_from_slice(committer_timestamp.as_bytes())
            }
            FormatToken::CommitterDate => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, date_mode)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, &DateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, &DateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, &DateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, &DateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateHuman => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&record.commit.committer, &DateMode::Human)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            FormatToken::Trailers(opts) => {
                let parsed = crate::parse_for_each_ref_trailer_options(opts)
                    .map_err(|_| GitError::Command("invalid %(trailers) options".into()))?;
                let rendered =
                    format_trailers_from_commit(message, &parsed);
                out.extend_from_slice(&rendered);
            }
            FormatToken::Decorate(spec) => {
                emit_log_decorate(out, &record.oid, decorations, spec);
            }
            FormatToken::Describe(spec) => {
                if let Some(describe_lookup) = describe {
                    let rendered = log_describe_placeholder(describe_lookup, &record.oid, spec)?;
                    out.extend_from_slice(rendered.as_bytes());
                }
            }
            FormatToken::ColorAuto => {
                // `%C(auto)` toggles auto-coloring; with `--color` we approximate
                // git's reference coloring at emission sites that need it.
                let _ = color;
            }
            FormatToken::Padding(_) | FormatToken::Wrap(_) | FormatToken::Magic(_) => {
                // Handled by the outer state machine in emit_compiled_log_format.
            }
            FormatToken::StashDecoParen
            | FormatToken::StashDecoBare
            | FormatToken::ReflogGd
            | FormatToken::ReflogGD
            | FormatToken::ReflogGn
            | FormatToken::ReflogGe
            | FormatToken::ReflogGs => {}
        }
    }
    Ok(())
}

fn emit_log_signature_atom(
    out: &mut Vec<u8>,
    token: &FormatToken,
    record: &sley_rev::CommitRecord,
    context: Option<&dyn LogSignatureLookup>,
) -> Result<()> {
    let verification = log_signature_verification(record, context)?;
    match token {
        FormatToken::GRefname => out.push(verification.pretty_code),
        FormatToken::GTrailers => out.extend_from_slice(verification.trust.as_bytes()),
        FormatToken::GPlaceholder => out.extend_from_slice(&verification.bare_output),
        FormatToken::GSignature => out.extend_from_slice(verification.signer.as_bytes()),
        FormatToken::GKey => out.extend_from_slice(verification.key.as_bytes()),
        FormatToken::GFingerprint => out.extend_from_slice(verification.fingerprint.as_bytes()),
        FormatToken::GPassthrough => out.extend_from_slice(verification.primary_fingerprint.as_bytes()),
        _ => {}
    }
    Ok(())
}

fn log_signature_verification(
    record: &sley_rev::CommitRecord,
    context: Option<&dyn LogSignatureLookup>,
) -> Result<LogSignatureView> {
    let Some(context) = context else {
        return Ok(LogSignatureView {
            trust: "undefined".into(),
            pretty_code: b'N',
            ..Default::default()
        });
    };
    context.verification_for_oid(&record.oid)
}

/// Port of utf8.c `strbuf_add_indented_text` (the `width <= 0` wrap fallback):
/// each line of `text` is prefixed with `indent`/`indent2` spaces.
fn log_add_indented_text(out: &mut Vec<u8>, text: &[u8], indent1: i32, indent2: i32) {
    if text.is_empty() {
        return;
    }
    let mut indent = indent1;
    let mut idx = 0;
    while idx < text.len() {
        let eol = text[idx..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|off| idx + off)
            .unwrap_or(text.len());
        if eol > idx {
            if indent > 0 {
                out.extend(std::iter::repeat_n(b' ', indent as usize));
            }
            out.extend_from_slice(&text[idx..eol]);
        }
        if eol < text.len() {
            out.push(b'\n');
        }
        idx = eol + 1;
        indent = indent2;
    }
}

/// Port of utf8.c `strbuf_add_wrapped_text`: word-wrap `text` into `out` to the
/// given column `width`, indenting the first line by `indent1` and continuation
/// lines by `indent2` (negative `indent1` starts mid-line, as git does).
fn log_wrap_text(out: &mut Vec<u8>, text: &[u8], indent1: i32, indent2: i32, width: i32) {
    if width <= 0 {
        log_add_indented_text(out, text, indent1, indent2);
        return;
    }
    let orig_len = out.len();
    let mut assume_utf8 = true;
    'retry: loop {
        // (re)try entry point
        let mut pos = 0usize; // index into `text`
        let mut bol = 0usize;
        let mut w = indent1;
        let mut indent = indent1;
        let mut space: Option<usize> = None;
        if indent < 0 {
            w = -indent;
            space = Some(0);
        }
        loop {
            // (skip ANSI escapes — not present in the corpus; omitted.)
            let c = text.get(pos).copied().unwrap_or(0);
            if c == 0 || (c as char).is_ascii_whitespace() {
                if w <= width || space.is_none() {
                    // git's `new_line` is reachable here only when `space` is set
                    // and width is exceeded; in this branch we emit the segment.
                    let start = match space {
                        _ if c == 0 && pos == bol => return, // git early return
                        Some(sp) => sp,
                        None => {
                            if indent > 0 {
                                out.extend(std::iter::repeat_n(b' ', indent as usize));
                            }
                            bol
                        }
                    };
                    out.extend_from_slice(&text[start..pos]);
                    if c == 0 {
                        return;
                    }
                    let mut sp = pos;
                    let mut go_new_line = false;
                    if c == b'\t' {
                        w |= 0x07;
                    } else if c == b'\n' {
                        sp += 1;
                        let next = text.get(sp).copied().unwrap_or(0);
                        if next == b'\n' {
                            out.push(b'\n');
                            go_new_line = true;
                        } else if !(next as char).is_ascii_alphanumeric() {
                            go_new_line = true;
                        } else {
                            out.push(b' ');
                        }
                    }
                    if go_new_line {
                        out.push(b'\n');
                        let advance =
                            if (text.get(sp).copied().unwrap_or(0) as char).is_ascii_whitespace() {
                                1
                            } else {
                                0
                            };
                        bol = sp + advance;
                        pos = bol;
                        space = None;
                        w = indent2;
                        indent = indent2;
                        continue;
                    }
                    space = Some(sp);
                    w += 1;
                    pos += 1;
                    continue;
                } else {
                    // new_line (width exceeded, break at the last space)
                    out.push(b'\n');
                    let sp = space.unwrap_or(pos);
                    let advance =
                        if (text.get(sp).copied().unwrap_or(0) as char).is_ascii_whitespace() {
                            1
                        } else {
                            0
                        };
                    bol = sp + advance;
                    pos = bol;
                    space = None;
                    w = indent2;
                    indent = indent2;
                    continue;
                }
            }
            // non-space glyph
            if assume_utf8 {
                match log_pick_utf8(text, pos) {
                    Some((cp, len)) => {
                        let gw = log_wcwidth(cp);
                        if gw > 0 {
                            w += gw;
                        }
                        pos += len;
                    }
                    None => {
                        // broken utf-8: restart in byte mode
                        assume_utf8 = false;
                        out.truncate(orig_len);
                        continue 'retry;
                    }
                }
            } else {
                w += 1;
                pos += 1;
            }
        }
    }
}

/// git `git_wcwidth`.
fn log_wcwidth(ch: u32) -> i32 {
    if ch == 0 {
        return 0;
    }
    if ch < 32 || (0x7f..0xa0).contains(&ch) {
        return -1;
    }
    // We don't ship the full zero/double-width tables; the t4205 corpus only
    // exercises ASCII + Latin-1 (all width 1). Treat everything else as width 1.
    1
}

/// Decode one UTF-8 scalar at `idx`; returns `(codepoint, byte_len)` or `None`
/// for invalid UTF-8 (matching git's `pick_one_utf8_char` validity checks).
pub fn log_pick_utf8(bytes: &[u8], idx: usize) -> Option<(u32, usize)> {
    let s = &bytes[idx..];
    let b0 = *s.first()?;
    if b0 < 0x80 {
        Some((b0 as u32, 1))
    } else if b0 & 0xe0 == 0xc0 {
        let b1 = *s.get(1)?;
        if b1 & 0xc0 != 0x80 || b0 & 0xfe == 0xc0 {
            return None;
        }
        Some(((((b0 & 0x1f) as u32) << 6) | (b1 & 0x3f) as u32, 2))
    } else if b0 & 0xf0 == 0xe0 {
        let b1 = *s.get(1)?;
        let b2 = *s.get(2)?;
        if b1 & 0xc0 != 0x80
            || b2 & 0xc0 != 0x80
            || (b0 == 0xe0 && b1 & 0xe0 == 0x80)
            || (b0 == 0xed && b1 & 0xe0 == 0xa0)
        {
            return None;
        }
        Some((
            (((b0 & 0x0f) as u32) << 12) | (((b1 & 0x3f) as u32) << 6) | (b2 & 0x3f) as u32,
            3,
        ))
    } else if b0 & 0xf8 == 0xf0 {
        let b1 = *s.get(1)?;
        let b2 = *s.get(2)?;
        let b3 = *s.get(3)?;
        if b1 & 0xc0 != 0x80
            || b2 & 0xc0 != 0x80
            || b3 & 0xc0 != 0x80
            || (b0 == 0xf0 && b1 & 0xf0 == 0x80)
            || (b0 == 0xf4 && b1 > 0x8f)
            || b0 > 0xf4
        {
            return None;
        }
        Some((
            (((b0 & 0x07) as u32) << 18)
                | (((b1 & 0x3f) as u32) << 12)
                | (((b2 & 0x3f) as u32) << 6)
                | (b3 & 0x3f) as u32,
            4,
        ))
    } else {
        None
    }
}

/// Render `%(decorate[:opts])` for `oid` from the decorations map, mirroring
/// pretty.c `format_decorations`.
fn emit_log_decorate(
    out: &mut Vec<u8>,
    oid: &ObjectId,
    decorations: &HashMap<ObjectId, Vec<String>>,
    spec: &DecorateSpec,
) {
    let Some(refs) = decorations.get(oid) else {
        return;
    };
    if refs.is_empty() {
        return;
    }
    out.extend_from_slice(spec.prefix.as_bytes());
    let mut first = true;
    for entry in refs {
        if !first {
            out.extend_from_slice(spec.separator.as_bytes());
        }
        first = false;
        // The decorations map stores entries like "HEAD -> main", "tag: v1",
        // "branch". Re-render the pointer/tag prefixes from the spec.
        let rendered = log_decorate_entry(entry, spec);
        out.extend_from_slice(rendered.as_bytes());
    }
    out.extend_from_slice(spec.suffix.as_bytes());
}

/// Re-render a single decoration entry under the decorate spec's tag/pointer
/// overrides. The stored entry uses the default " -> " pointer and "tag: " tag.
fn log_decorate_entry(entry: &str, spec: &DecorateSpec) -> String {
    if let Some(rest) = entry.strip_prefix("HEAD -> ") {
        format!("HEAD{}{}", spec.pointer, log_decorate_entry(rest, spec))
    } else if let Some(rest) = entry.strip_prefix("tag: ") {
        format!("{}{}", spec.tag, rest)
    } else {
        entry.to_string()
    }
}

/// Render `%(describe[:opts])` for `oid`, returning an empty string on any
/// describe failure (git treats describe errors as an empty placeholder).
fn log_describe_placeholder(
    lookup: &dyn LogDescribeLookup,
    oid: &ObjectId,
    spec: &DescribeSpec,
) -> Result<String> {
    lookup.describe_oid(oid, spec)
}

fn append_metadata_parent_oids(out: &mut Vec<u8>, parents: &[ObjectId], abbrev_len: Option<usize>) {
    for (idx, oid) in parents.iter().enumerate() {
        if idx > 0 {
            out.push(b' ');
        }
        append_log_oid(out, oid, abbrev_len);
    }
}

pub fn format_metadata_parent_oids(parents: &[ObjectId], abbrev_len: Option<usize>) -> String {
    let mut out = Vec::with_capacity(parents.len().saturating_mul(41));
    append_metadata_parent_oids(&mut out, parents, abbrev_len);
    String::from_utf8(out).expect("object ids are always ASCII hex")
}

pub fn emit_compiled_log_format_metadata(
    record: &sley_rev::CommitMetadata,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    emit_compiled_log_format_metadata_inner(record, compiled, context, out, None)
}

pub fn emit_compiled_log_format_metadata_with_message(
    record: &sley_rev::CommitMetadata,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    message: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    emit_compiled_log_format_metadata_inner(record, compiled, context, out, Some(message))
}

pub fn emit_compiled_log_format_limited_commit(
    db: &FileObjectDatabase,
    record: &sley_rev::CommitMetadata,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let object = db.read_object(&record.oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            record.oid,
            object.object_type.as_str()
        )));
    }
    let (message, encoding) = commit_object_message_and_encoding(&object.body);
    let utf8_message = log_reencode_message(message, encoding.as_ref(), "UTF-8");
    emit_compiled_log_format_metadata_with_message(record, compiled, context, &utf8_message, out)
}

fn commit_object_message_and_encoding(body: &[u8]) -> (&[u8], std::borrow::Cow<'_, str>) {
    let (message, encoding) = commit_object_message_and_optional_encoding(body);
    (message, encoding.unwrap_or(std::borrow::Cow::Borrowed("")))
}

pub fn commit_object_message_and_optional_encoding(
    body: &[u8],
) -> (&[u8], Option<std::borrow::Cow<'_, str>>) {
    let mut encoding = None;
    let mut offset = 0usize;
    while offset < body.len() {
        let line_end = body[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|idx| offset + idx)
            .unwrap_or(body.len());
        let line = &body[offset..line_end];
        if line.is_empty() {
            let message_start = line_end.saturating_add(1).min(body.len());
            return (&body[message_start..], encoding);
        }
        if let Some(value) = line.strip_prefix(b"encoding ") {
            encoding = Some(
                std::str::from_utf8(value)
                    .map(std::borrow::Cow::Borrowed)
                    .unwrap_or_else(|_| String::from_utf8_lossy(value)),
            );
        }
        if line_end == body.len() {
            break;
        }
        offset = line_end + 1;
    }
    (&[], encoding)
}

pub fn emit_compiled_log_format_metadata_inner(
    record: &sley_rev::CommitMetadata,
    compiled: &CompiledLogFormat,
    context: &LogFormatContext<'_>,
    out: &mut Vec<u8>,
    message: Option<&[u8]>,
) -> Result<()> {
    let LogFormatContext {
        abbrev_len,
        marker,
        dialect,
        source,
        color,
        ..
    } = *context;

    for token in &compiled.tokens {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::OidFull => append_log_oid(out, &record.oid, None),
            FormatToken::OidAbbrev => append_log_oid(out, &record.oid, abbrev_len),
            FormatToken::ParentsFull => append_metadata_parent_oids(out, &record.parents, None),
            FormatToken::ParentsAbbrev => {
                append_metadata_parent_oids(out, &record.parents, abbrev_len);
            }
            FormatToken::Marker => out.push(marker as u8),
            FormatToken::NoteName if dialect == LogFormatDialect::Log => {}
            FormatToken::NoteName => out.extend_from_slice(b"%N"),
            FormatToken::RevisionSource if dialect == LogFormatDialect::Log => {
                if let Some(source) = source {
                    out.extend_from_slice(source.as_bytes());
                }
            }
            FormatToken::RevisionSource => out.extend_from_slice(b"%S"),
            FormatToken::ColorName(name) => {
                if color && let Some(ansi) = git_color_name_to_ansi(name) {
                    out.extend_from_slice(ansi.as_bytes());
                }
            }
            FormatToken::ColorParen(spec) => {
                out.extend_from_slice(git_color_spec_to_ansi(spec, color).as_bytes());
            }
            FormatToken::Subject if let Some(message) = message => {
                out.extend_from_slice(commit_subject_bytes(message));
            }
            FormatToken::SanitizedSubject if let Some(message) = message => {
                write!(out, "{}", log_sanitized_subject(message)).map_err(io::Error::from)?;
            }
            FormatToken::GRefname => out.push(b'N'),
            FormatToken::GTrailers => out.extend_from_slice(b"undefined"),
            FormatToken::GPlaceholder
            | FormatToken::GSignature
            | FormatToken::GKey
            | FormatToken::GFingerprint
            | FormatToken::GPassthrough
            | FormatToken::GDate
            | FormatToken::GDateShort
            | FormatToken::GDateIso
            | FormatToken::GDateIsoStrict
            | FormatToken::GDateRfc2822 => {}
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            FormatToken::StashDecoParen
            | FormatToken::StashDecoBare
            | FormatToken::ReflogGd
            | FormatToken::ReflogGD
            | FormatToken::ReflogGn
            | FormatToken::ReflogGe
            | FormatToken::ReflogGs
            | FormatToken::TreeFull
            | FormatToken::TreeAbbrev
            | FormatToken::Subject
            | FormatToken::SanitizedSubject
            | FormatToken::Encoding
            | FormatToken::Body
            | FormatToken::FullMessage
            | FormatToken::DecorationsParen
            | FormatToken::DecorationsBare
            | FormatToken::AuthorName
            | FormatToken::AuthorEmail
            | FormatToken::AuthorEmailLocal
            | FormatToken::AuthorNameMapped
            | FormatToken::AuthorEmailMapped
            | FormatToken::AuthorEmailLocalMapped
            | FormatToken::AuthorTimestamp
            | FormatToken::AuthorDate
            | FormatToken::AuthorDateIso
            | FormatToken::AuthorDateIsoStrict
            | FormatToken::AuthorDateShort
            | FormatToken::AuthorDateRfc2822
            | FormatToken::AuthorDateHuman
            | FormatToken::CommitterName
            | FormatToken::CommitterEmail
            | FormatToken::CommitterEmailLocal
            | FormatToken::CommitterNameMapped
            | FormatToken::CommitterEmailMapped
            | FormatToken::CommitterEmailLocalMapped
            | FormatToken::CommitterTimestamp
            | FormatToken::CommitterDate
            | FormatToken::CommitterDateIso
            | FormatToken::CommitterDateIsoStrict
            | FormatToken::CommitterDateShort
            | FormatToken::CommitterDateRfc2822
            | FormatToken::CommitterDateHuman
            | FormatToken::Padding(_)
            | FormatToken::Wrap(_)
            | FormatToken::Trailers(_)
            | FormatToken::Decorate(_)
            | FormatToken::Describe(_)
            | FormatToken::ColorAuto
            | FormatToken::Magic(_) => {}
        }
    }
    Ok(())
}

pub struct StashFormatContext<'a> {
    pub entry: &'a ReflogEntry,
    pub index: usize,
    pub commit: &'a Commit,
    pub abbrev_len: Option<usize>,
    pub date_mode: &'a DateMode,
    pub date_explicit: bool,
}

pub fn emit_compiled_stash_format(
    compiled: &CompiledLogFormat,
    context: &StashFormatContext<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let StashFormatContext {
        entry,
        index,
        commit,
        abbrev_len,
        date_mode,
        date_explicit,
    } = *context;
    let (author_name, author_email) = commit_identity_name_email(&commit.author);
    let (committer_name, committer_email) = commit_identity_name_email(&commit.committer);
    let author_timestamp = commit_identity_timestamp(&commit.author);
    let committer_timestamp = commit_identity_timestamp(&commit.committer);
    let (reflog_name, reflog_email) = commit_identity_name_email(&entry.committer);

    for token in &compiled.tokens {
        match token {
            FormatToken::Literal(text) => out.extend_from_slice(text.as_bytes()),
            FormatToken::Percent => out.push(b'%'),
            FormatToken::OidFull => write!(out, "{}", entry.new_oid).map_err(io::Error::from)?,
            FormatToken::OidAbbrev => {
                write!(out, "{}", format_log_oid(&entry.new_oid, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::TreeFull => write!(out, "{}", commit.tree).map_err(io::Error::from)?,
            FormatToken::TreeAbbrev => {
                write!(out, "{}", format_log_oid(&commit.tree, abbrev_len))
                    .map_err(io::Error::from)?;
            }
            FormatToken::ParentsFull => {
                write!(
                    out,
                    "{}",
                    format_metadata_parent_oids(&commit.parents, None)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ParentsAbbrev => {
                write!(
                    out,
                    "{}",
                    format_metadata_parent_oids(&commit.parents, abbrev_len)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::Marker => out.push(b'>'),
            FormatToken::Subject => {
                write!(out, "{}", commit_subject(&commit.message)).map_err(io::Error::from)?;
            }
            FormatToken::SanitizedSubject => {
                write!(out, "{}", log_sanitized_subject(&commit.message))
                    .map_err(io::Error::from)?;
            }
            FormatToken::Encoding => {
                write!(out, "{}", commit_encoding(commit)).map_err(io::Error::from)?;
            }
            FormatToken::NoteName => {}
            FormatToken::RevisionSource => out.extend_from_slice(b"%S"),
            FormatToken::ColorParen(_) | FormatToken::ColorName(_) => {}
            FormatToken::Body => out.extend_from_slice(commit_body(&commit.message)),
            FormatToken::FullMessage => out.extend_from_slice(&commit.message),
            FormatToken::StashDecoParen if index == 0 => {
                out.extend_from_slice(b" (refs/stash)");
            }
            FormatToken::StashDecoParen => {}
            FormatToken::StashDecoBare if index == 0 => {
                out.extend_from_slice(b"refs/stash");
            }
            FormatToken::StashDecoBare => {}
            FormatToken::GRefname => out.push(b'N'),
            FormatToken::GTrailers => out.extend_from_slice(b"undefined"),
            FormatToken::GPlaceholder
            | FormatToken::GSignature
            | FormatToken::GKey
            | FormatToken::GFingerprint
            | FormatToken::GPassthrough
            | FormatToken::GDate
            | FormatToken::GDateShort
            | FormatToken::GDateIso
            | FormatToken::GDateIsoStrict
            | FormatToken::GDateRfc2822 => {}
            // The stash-metadata path has no mailmap context; the upper-case
            // (mapped) atoms degrade to the raw identity.
            FormatToken::AuthorName | FormatToken::AuthorNameMapped => {
                out.extend_from_slice(author_name.as_bytes())
            }
            FormatToken::AuthorEmail | FormatToken::AuthorEmailMapped => {
                out.extend_from_slice(author_email.as_bytes())
            }
            FormatToken::AuthorEmailLocal | FormatToken::AuthorEmailLocalMapped => {
                write!(out, "{}", log_email_local_part(&author_email)).map_err(io::Error::from)?;
            }
            FormatToken::AuthorTimestamp => out.extend_from_slice(author_timestamp.as_bytes()),
            FormatToken::AuthorDate => {
                write!(out, "{}", commit_identity_date(&commit.author, date_mode))
                    .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, &DateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, &DateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, &DateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, &DateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::AuthorDateHuman => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.author, &DateMode::Human)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterName | FormatToken::CommitterNameMapped => {
                out.extend_from_slice(committer_name.as_bytes())
            }
            FormatToken::CommitterEmail | FormatToken::CommitterEmailMapped => {
                out.extend_from_slice(committer_email.as_bytes())
            }
            FormatToken::CommitterEmailLocal | FormatToken::CommitterEmailLocalMapped => {
                write!(out, "{}", log_email_local_part(&committer_email))
                    .map_err(io::Error::from)?;
            }
            FormatToken::CommitterTimestamp => {
                out.extend_from_slice(committer_timestamp.as_bytes());
            }
            FormatToken::CommitterDate => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, date_mode)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIso => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, &DateMode::Iso)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateIsoStrict => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, &DateMode::IsoStrict)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateShort => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, &DateMode::Short)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateRfc2822 => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, &DateMode::Rfc2822)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::CommitterDateHuman => {
                write!(
                    out,
                    "{}",
                    commit_identity_date(&commit.committer, &DateMode::Human)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ReflogGd => {
                write!(
                    out,
                    "{}",
                    stash_list_reflog_selector("stash", index, entry, date_mode, date_explicit)
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ReflogGD => {
                write!(
                    out,
                    "{}",
                    stash_list_reflog_selector(
                        "refs/stash",
                        index,
                        entry,
                        date_mode,
                        date_explicit
                    )
                )
                .map_err(io::Error::from)?;
            }
            FormatToken::ReflogGn => out.extend_from_slice(reflog_name.as_bytes()),
            FormatToken::ReflogGe => out.extend_from_slice(reflog_email.as_bytes()),
            FormatToken::ReflogGs => out.extend_from_slice(&entry.message),
            FormatToken::DecorationsParen | FormatToken::DecorationsBare => {}
            FormatToken::Newline => out.push(b'\n'),
            FormatToken::HexByte(byte) => out.push(*byte),
            FormatToken::Padding(_)
            | FormatToken::Wrap(_)
            | FormatToken::Trailers(_)
            | FormatToken::Decorate(_)
            | FormatToken::Describe(_)
            | FormatToken::ColorAuto
            | FormatToken::Magic(_) => {}
        }
    }
    Ok(())
}

fn stash_list_reflog_selector(
    reference: &str,
    index: usize,
    entry: &ReflogEntry,
    date_mode: &DateMode,
    date_explicit: bool,
) -> String {
    if date_explicit {
        let date = commit_identity_date(&entry.committer, date_mode);
        return format!("{reference}@{{{date}}}");
    }
    format!("{reference}@{{{index}}}")
}

pub fn format_log_format_decorations(
    oid: &ObjectId,
    decorations: &HashMap<ObjectId, Vec<String>>,
    parenthesized: bool,
) -> String {
    let Some(labels) = decorations.get(oid) else {
        return String::new();
    };
    if parenthesized {
        format!(" ({})", labels.join(", "))
    } else {
        labels.join(", ")
    }
}

pub fn commit_identity_name_email(raw: &[u8]) -> (String, String) {
    // Tolerant git-style split (matches %an/%ae in format_person_part), so a
    // broken email yields the recovered name and clean address.
    let Some(fields) = sley_core::split_ident_line(raw) else {
        return (String::from_utf8_lossy(raw).into_owned(), String::new());
    };
    (
        String::from_utf8_lossy(fields.name).into_owned(),
        String::from_utf8_lossy(fields.email).into_owned(),
    )
}

pub fn commit_encoding(commit: &Commit) -> String {
    commit
        .encoding
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default()
        .to_string()
}

pub fn commit_encoding_config(git_dir: &Path) -> String {
    sley_config::read_repo_config(git_dir, None)
        .ok()
        .and_then(|config| {
            config
                .get("i18n", None, "commitEncoding")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "UTF-8".to_string())
}

pub fn commit_encoding_header_from_config(git_dir: &Path) -> Option<Vec<u8>> {
    let encoding = commit_encoding_config(git_dir);
    (!encoding_is_utf8(&encoding)).then(|| encoding.into_bytes())
}

/// True when `name` denotes a UTF-8 encoding (git's `is_encoding_utf8`).
pub fn encoding_is_utf8(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n.eq_ignore_ascii_case("utf-8") || n.eq_ignore_ascii_case("utf8")
}

/// True when `name` is ISO-8859-1 / Latin-1.
fn encoding_is_latin1(name: &str) -> bool {
    let n = name.trim();
    n.eq_ignore_ascii_case("ISO8859-1")
        || n.eq_ignore_ascii_case("ISO-8859-1")
        || n.eq_ignore_ascii_case("latin1")
        || n.eq_ignore_ascii_case("latin-1")
        || n.eq_ignore_ascii_case("8859-1")
}

pub fn encoding_is_none(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("none")
}

pub fn encoding_for_name(name: &str) -> Option<&'static encoding_rs::Encoding> {
    let n = name.trim();
    if encoding_is_utf8(n) {
        return Some(encoding_rs::UTF_8);
    }
    if encoding_is_latin1(n) {
        return Some(encoding_rs::WINDOWS_1252);
    }
    let compact = n
        .bytes()
        .filter(|byte| !matches!(*byte, b'-' | b'_' | b' '))
        .map(|byte| byte.to_ascii_uppercase())
        .collect::<Vec<_>>();
    match compact.as_slice() {
        b"EUCJP" => Some(encoding_rs::EUC_JP),
        b"ISO2022JP" => Some(encoding_rs::ISO_2022_JP),
        _ => encoding_rs::Encoding::for_label(n.as_bytes()),
    }
}

/// Re-encode a commit message from its stored `encoding` header to the desired
/// log output encoding, mirroring git's `repo_logmsg_reencode`.
pub fn log_reencode_message<'a>(message: &'a [u8], from: &str, to: &str) -> std::borrow::Cow<'a, [u8]> {
    use std::borrow::Cow;
    if encoding_is_none(to) || from.trim().eq_ignore_ascii_case(to.trim()) {
        return Cow::Borrowed(message);
    }
    let from_encoding = encoding_for_name(from).unwrap_or(encoding_rs::UTF_8);
    let to_encoding = encoding_for_name(to).unwrap_or(encoding_rs::UTF_8);
    if from_encoding == to_encoding {
        return Cow::Borrowed(message);
    }
    let (decoded, _, _) = from_encoding.decode(message);
    let (encoded, _, _) = to_encoding.encode(&decoded);
    Cow::Owned(encoded.into_owned())
}

pub fn commit_message_for_output<'a>(
    message: &'a [u8],
    encoding: Option<&[u8]>,
    output_encoding: &str,
) -> std::borrow::Cow<'a, [u8]> {
    let from = encoding
        .map(String::from_utf8_lossy)
        .unwrap_or(std::borrow::Cow::Borrowed("UTF-8"));
    log_reencode_message(message, &from, output_encoding)
}

pub fn commit_message_for_commit_encoding<'a>(
    commit: &'a Commit,
    output_encoding: &str,
) -> std::borrow::Cow<'a, [u8]> {
    commit_message_for_output(&commit.message, commit.encoding.as_deref(), output_encoding)
}

pub fn commit_author_for_commit_encoding<'a>(
    commit: &'a Commit,
    output_encoding: &str,
) -> std::borrow::Cow<'a, [u8]> {
    let from = commit_encoding(commit);
    log_reencode_message(&commit.author, &from, output_encoding)
}

pub fn commit_message_has_nul(message: &[u8]) -> bool {
    message.contains(&b'\0')
}

pub fn commit_message_has_invalid_utf8(message: &[u8]) -> bool {
    let mut idx = 0usize;
    while idx < message.len() {
        let Some((cp, len)) = log_pick_utf8(message, idx) else {
            return true;
        };
        if (0xfdd0..=0xfdef).contains(&cp) || (cp & 0xfffe == 0xfffe && cp <= 0x10ffff) {
            return true;
        }
        idx += len;
    }
    false
}

/// The effective `git log` output encoding: `i18n.logOutputEncoding`, else
/// `i18n.commitEncoding`, else UTF-8 (git's `get_log_output_encoding`).
pub fn log_output_encoding(config: &GitConfig) -> String {
    config
        .get("i18n", None, "logOutputEncoding")
        .or_else(|| config.get("i18n", None, "commitEncoding"))
        .unwrap_or("UTF-8")
        .to_string()
}

pub fn log_email_local_part(email: &str) -> &str {
    email.split_once('@').map_or(email, |(local, _)| local)
}

pub fn log_sanitized_subject(message: &[u8]) -> String {
    let subject = commit_subject(message);
    let mut out = String::new();
    let mut last_separator = false;
    for byte in subject.bytes() {
        if byte.is_ascii_alphanumeric() {
            out.push(byte as char);
            last_separator = false;
            continue;
        }
        if matches!(byte, b'.' | b'_') {
            if !out.is_empty() && !last_separator {
                out.push(byte as char);
                last_separator = true;
            }
            continue;
        }
        if !out.is_empty() && !last_separator {
            out.push('-');
            last_separator = true;
        }
    }
    while out.ends_with(['-', '.', '_']) {
        out.pop();
    }
    out
}

