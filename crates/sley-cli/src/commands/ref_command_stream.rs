//! Faithful re-implementation of git's `update-ref --stdin` command-stream
//! lexer (builtin/update-ref.c).
//!
//! git tokenizes each `--stdin` command with a stateful cursor rather than a
//! naive `split_whitespace`, which matters for three reasons:
//!
//!   1. Arguments may be C-quoted (`"..."`) with backslash escapes, octal
//!      bytes, and embedded spaces — `parse_arg` decodes them.
//!   2. Malformed input produces four *distinct* `fatal:` messages
//!      (`empty command in input`, `whitespace before command: <line>`,
//!      `badly quoted argument: <arg>`,
//!      `unexpected character after quoted argument: <...>`), plus per-command
//!      `<cmd> <ref>: extra input: <tail>` when trailing bytes remain.
//!   3. A run of spaces is *not* an argument separator — git requires exactly
//!      one `SP` (or `NUL` under `-z`) between arguments and dies with
//!      `expected SP but got: <tail>` otherwise.
//!
//! This module exposes [`ArgCursor`], a byte-cursor over a single decoded
//! command line (the `\n` path) or a stitched-together NUL record group (the
//! `-z` path), with the same `parse_refname` / `parse_next_arg` /
//! `parse_next_oid`-shaped primitives git uses, so the dispatch layer in
//! `refs.rs` can walk arguments exactly as the C builtin does.

use sley::plumbing::sley_core::{GitError, Result};

/// Terminator that ends a logical command stream record.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Terminator {
    /// `\n`-terminated text input (no `-z`): arguments are space-separated and
    /// may be C-quoted.
    Newline,
    /// `NUL`-terminated binary input (`-z`): each argument is its own record;
    /// no C-quoting is applied.
    Nul,
}

impl Terminator {
    fn byte(self) -> u8 {
        match self {
            Terminator::Newline => b'\n',
            Terminator::Nul => b'\0',
        }
    }
}

/// A cursor over a single command's bytes (everything after `<cmd>` and its
/// following separator). Mirrors git's `const char **next` walking pointer.
///
/// For the `\n` path the slice is the full line (terminator stripped); for the
/// `-z` path it is the command's NUL records stitched back together with a
/// single `\0` between them and a trailing `\0` so the cursor sees the same
/// shape git's stitched `input` strbuf has.
pub(crate) struct ArgCursor<'a> {
    buf: &'a [u8],
    pos: usize,
    term: Terminator,
}

impl<'a> ArgCursor<'a> {
    pub(crate) fn new(buf: &'a [u8], term: Terminator) -> Self {
        Self { buf, pos: 0, term }
    }

    fn cur(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    /// The record terminator byte for this cursor (`\n` or `\0`).
    pub(crate) fn terminator_byte(&self) -> u8 {
        self.term.byte()
    }

    /// The remaining bytes, as a lossy string, for `extra input:` messages.
    fn remainder_str(&self) -> String {
        String::from_utf8_lossy(&self.buf[self.pos..]).into_owned()
    }

    /// The entire remaining input from the cursor, without consuming or
    /// decoding it. Used by `option`, which git matches against the raw tail.
    /// Under `-z` the stitched buffer keeps a trailing NUL; strip it so the
    /// keyword comparison sees just the option name.
    pub(crate) fn remainder(&self) -> String {
        let mut rest = &self.buf[self.pos..];
        if self.term == Terminator::Nul {
            while rest.last() == Some(&0) {
                rest = &rest[..rest.len() - 1];
            }
        }
        String::from_utf8_lossy(rest).into_owned()
    }

    /// git's `parse_arg`: parse one whitespace- or NUL-terminated, possibly
    /// C-quoted argument starting at the cursor. Only used in the `\n` path
    /// (the `-z` path reads whole records, no quoting). Advances the cursor to
    /// the terminator and returns the decoded bytes. Dies on malformed quoting.
    fn parse_arg(&mut self) -> Result<Vec<u8>> {
        if self.cur() == Some(b'"') {
            let orig_start = self.pos;
            let mut out = Vec::new();
            let consumed = unquote_c_style(&self.buf[self.pos..], &mut out).ok_or_else(|| {
                die(format!(
                    "badly quoted argument: {}",
                    String::from_utf8_lossy(&self.buf[orig_start..])
                ))
            })?;
            self.pos += consumed;
            // git: `if (*next && !isspace(*next))` — anything other than a
            // terminator or whitespace immediately after the closing quote is
            // junk.
            if self.cur().is_some_and(|c| c != 0 && !is_space(c)) {
                return Err(die(format!(
                    "unexpected character after quoted argument: {}",
                    String::from_utf8_lossy(&self.buf[orig_start..])
                )));
            }
            Ok(out)
        } else {
            // git: `while (*next && !isspace(*next)) addch(*next++)`. A NUL byte
            // terminates the C string just like end-of-buffer.
            let start = self.pos;
            while let Some(c) = self.cur() {
                if c == 0 || is_space(c) {
                    break;
                }
                self.pos += 1;
            }
            Ok(self.buf[start..self.pos].to_vec())
        }
    }

    /// git's `parse_refname`: the argument immediately after `<cmd> SP`. In the
    /// `\n` path this is `parse_arg`; in the `-z` path it is everything up to
    /// the next NUL. Returns `None` if the argument is empty (git returns NULL),
    /// which callers translate into `missing <ref>` etc.
    pub(crate) fn parse_refname(&mut self) -> Result<Option<String>> {
        let bytes = match self.term {
            Terminator::Newline => self.parse_arg()?,
            Terminator::Nul => self.take_record(),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// git's `parse_next_refname`: skip the delimiter (SP/NUL) and then a
    /// refname. Returns `None` if there is no further argument.
    pub(crate) fn parse_next_refname(&mut self) -> Result<Option<String>> {
        if !self.skip_delimiter()? {
            return Ok(None);
        }
        self.parse_refname()
    }

    /// git's `parse_next_arg`: skip the delimiter and parse one argument.
    /// Returns `None` if there is no further argument or if it is empty.
    pub(crate) fn parse_next_arg(&mut self) -> Result<Option<String>> {
        if !self.skip_delimiter()? {
            return Ok(None);
        }
        let bytes = match self.term {
            Terminator::Newline => self.parse_arg()?,
            Terminator::Nul => self.take_record(),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// git's `parse_next_oid`. Skip the delimiter and parse an OID-shaped
    /// argument, resolving the empty-value semantics exactly as git does for
    /// the given `allow_empty` flag (PARSE_SHA1_ALLOW_EMPTY). Returns:
    ///   * `NextOid::Missing` — no further argument at all (git's `return 1`).
    ///   * `NextOid::Zero` — argument present but empty AND git treats it as
    ///     all-zeros (the `\n` path always; the `-z` path only with
    ///     `allow_empty`). The caller should record this as a present value.
    ///   * `NextOid::Value(s)` — a concrete value to resolve.
    ///   * `NextOid::Eof` — `-z` end-of-input where a value was required.
    ///
    /// When `-z` sees an empty value WITHOUT `allow_empty`, git's `ret = 1`
    /// (unspecified), so we return `Missing`.
    pub(crate) fn parse_next_oid(
        &mut self,
        command: &str,
        refname: &str,
        allow_empty: bool,
    ) -> Result<NextOid> {
        match self.term {
            Terminator::Newline => {
                // git: `if (!**next || **next == line_termination) return 1;`
                match self.cur() {
                    None | Some(0) => return Ok(NextOid::Missing),
                    Some(b' ') => {}
                    Some(_) => {
                        return Err(die(format!(
                            "{command} {refname}: expected SP but got: {}",
                            self.remainder_str()
                        )));
                    }
                }
                self.pos += 1; // skip SP
                let arg = self.parse_arg()?;
                if arg.is_empty() {
                    // git: without -z, an empty value means all zeros.
                    Ok(NextOid::Zero)
                } else {
                    Ok(NextOid::Value(String::from_utf8_lossy(&arg).into_owned()))
                }
            }
            Terminator::Nul => {
                // git: `if (**next) die("expected NUL but got"); (*next)++;`
                if self.cur().is_some_and(|c| c != 0) {
                    return Err(die(format!(
                        "{command} {refname}: expected NUL but got: {}",
                        self.remainder_str()
                    )));
                }
                if self.cur().is_none() {
                    // git: `if (*next == end) goto eof;`
                    return Ok(NextOid::Eof);
                }
                self.pos += 1; // skip NUL
                if self.cur().is_none() {
                    return Ok(NextOid::Eof);
                }
                let arg = self.take_record();
                if arg.is_empty() {
                    if allow_empty {
                        // git: warning + treat as zero (caller emits warning).
                        Ok(NextOid::Zero)
                    } else {
                        // git: unspecified — `ret = 1`.
                        Ok(NextOid::Missing)
                    }
                } else {
                    Ok(NextOid::Value(String::from_utf8_lossy(&arg).into_owned()))
                }
            }
        }
    }

    /// git's main loop: after a command's fixed arguments are consumed, the
    /// cursor must sit on the terminator. Otherwise die with `extra input`.
    /// The leftover tail (including its leading separator) is reported verbatim.
    pub(crate) fn finish(&self, command: &str, refname: &str) -> Result<()> {
        match self.cur() {
            None => Ok(()),
            Some(0) if self.term == Terminator::Nul => Ok(()),
            Some(_) => Err(die(format!(
                "{command} {refname}: extra input: {}",
                self.remainder_str()
            ))),
        }
    }

    /// Skip the inter-argument delimiter (SP for `\n`, NUL for `-z`). Returns
    /// `false` (no further argument) when there is nothing to skip. Dies on a
    /// non-SP separator in the `\n` path (git's `expected SP but got`).
    fn skip_delimiter(&mut self) -> Result<bool> {
        match self.term {
            Terminator::Newline => match self.cur() {
                None | Some(0) => Ok(false),
                Some(b' ') => {
                    self.pos += 1;
                    Ok(true)
                }
                Some(_) => Err(die(format!(
                    "expected SP but got: {}",
                    self.remainder_str()
                ))),
            },
            Terminator::Nul => {
                // git: `if (**next) return NULL;` — a non-NUL here means the
                // previous record had not ended, i.e. no further argument.
                match self.cur() {
                    Some(0) => {
                        self.pos += 1;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
        }
    }

    /// Take the bytes up to the next NUL (or end) and advance past them. Used
    /// only in the `-z` path where a record is everything up to a NUL.
    fn take_record(&mut self) -> Vec<u8> {
        let start = self.pos;
        while let Some(c) = self.cur() {
            if c == 0 {
                break;
            }
            self.pos += 1;
        }
        self.buf[start..self.pos].to_vec()
    }
}

/// Result of [`ArgCursor::parse_next_oid`].
pub(crate) enum NextOid {
    /// No further argument at all, or `-z` empty without `allow_empty`
    /// (git's `return 1`).
    Missing,
    /// Present but empty, and git treats it as all-zeros (a *present* value).
    Zero,
    /// `-z` end-of-input while a value was required.
    Eof,
    /// A concrete value to be resolved to an OID.
    Value(String),
}

/// True for the bytes git's `isspace` treats as whitespace in this lexer's
/// C-locale context: space, tab, newline, vertical tab, form feed, carriage
/// return.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Classify a raw `\n`-path line before tokenizing it, reproducing git's first
/// two guards in `update_refs_stdin`:
///   * a line that is *only* the terminator → `empty command in input`
///   * a line beginning with whitespace → `whitespace before command: <line>`
///
/// `line` is the raw line WITHOUT its trailing terminator. Returns the
/// `fatal:` error to raise, or `Ok(())` if the line is well-formed enough to
/// dispatch. Mirrors git checking `*input.buf` (the still-terminated buffer),
/// so an empty `line` slice corresponds to git's `*input.buf == '\n'`.
pub(crate) fn classify_line(line: &[u8]) -> Result<()> {
    match line.first().copied() {
        None => Err(die("empty command in input".to_string())),
        Some(c) if is_space(c) => Err(die(format!(
            "whitespace before command: {}",
            String::from_utf8_lossy(line)
        ))),
        _ => Ok(()),
    }
}

fn die(message: String) -> GitError {
    eprintln!("fatal: {message}");
    GitError::Exit(128)
}

/// Faithful port of git's `unquote_c_style` (quote.c). Decodes a leading
/// `"`-quoted C string from `input`, appending the decoded bytes to `out`.
/// Returns the number of input bytes consumed (up to and including the closing
/// quote) on success, or `None` if the quoting is malformed. A NUL byte
/// terminates the input just as it does in git's C-string view.
pub(crate) fn unquote_c_style(input: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    let mut i = 0usize;
    if input.get(i).copied()? != b'"' {
        return None;
    }
    i += 1;
    loop {
        // Copy the run up to the next '"' or '\\' (NUL ends the C string).
        while let Some(&c) = input.get(i) {
            if c == b'"' || c == b'\\' || c == 0 {
                break;
            }
            out.push(c);
            i += 1;
        }
        match input.get(i).copied() {
            Some(b'"') => {
                i += 1;
                return Some(i);
            }
            Some(b'\\') => {
                i += 1;
            }
            // NUL or end-of-input before a closing quote: malformed.
            _ => return None,
        }
        let esc = input.get(i).copied()?;
        i += 1;
        let decoded = match esc {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'\\' | b'"' => esc,
            b'0'..=b'3' => {
                // Octal: first digit 0..3 (>=4 would overflow a byte), then two
                // more octal digits, all required.
                let mut ac = ((esc - b'0') as u32) << 6;
                let d1 = input.get(i).copied()?;
                if !(b'0'..=b'7').contains(&d1) {
                    return None;
                }
                i += 1;
                ac |= ((d1 - b'0') as u32) << 3;
                let d2 = input.get(i).copied()?;
                if !(b'0'..=b'7').contains(&d2) {
                    return None;
                }
                i += 1;
                ac |= (d2 - b'0') as u32;
                ac as u8
            }
            _ => return None,
        };
        out.push(decoded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unquote(s: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        unquote_c_style(s, &mut out).map(|_| out)
    }

    #[test]
    fn unquote_plain() {
        assert_eq!(unquote(br#""hello""#).as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn unquote_escapes() {
        assert_eq!(unquote(br#""a\tb\n""#).as_deref(), Some(&b"a\tb\n"[..]));
        assert_eq!(unquote(br#""\\""#).as_deref(), Some(&b"\\"[..]));
        assert_eq!(unquote(br#""\"""#).as_deref(), Some(&b"\""[..]));
    }

    #[test]
    fn unquote_octal() {
        // \101 == 'A'
        assert_eq!(unquote(br#""\101""#).as_deref(), Some(&b"A"[..]));
    }

    #[test]
    fn unquote_unbalanced_is_none() {
        assert_eq!(unquote(br#""main"#), None);
    }

    #[test]
    fn unquote_bad_escape_is_none() {
        // \z is not a valid escape (matches t1400 'invalid escape')
        assert_eq!(unquote(br#""ma\zn""#), None);
    }

    #[test]
    fn cursor_plain_args() {
        let line = b"refs/heads/a deadbeef";
        let mut c = ArgCursor::new(line, Terminator::Newline);
        assert_eq!(c.parse_refname().unwrap().as_deref(), Some("refs/heads/a"));
        assert!(matches!(
            c.parse_next_oid("update", "refs/heads/a", true).unwrap(),
            NextOid::Value(v) if v == "deadbeef"
        ));
        assert!(c.finish("update", "refs/heads/a").is_ok());
    }

    #[test]
    fn cursor_extra_input_detected() {
        let line = b"refs/heads/a aaa bbb";
        let mut c = ArgCursor::new(line, Terminator::Newline);
        let _ = c.parse_refname().unwrap();
        let _ = c.parse_next_oid("create", "refs/heads/a", false).unwrap();
        // create takes one oid; the remaining " bbb" is extra input.
        assert!(c.finish("create", "refs/heads/a").is_err());
    }

    #[test]
    fn cursor_quoted_arg_with_space() {
        let line = br#""refs/heads/with space" deadbeef"#;
        let mut c = ArgCursor::new(line, Terminator::Newline);
        assert_eq!(
            c.parse_refname().unwrap().as_deref(),
            Some("refs/heads/with space")
        );
    }

    #[test]
    fn classify_empty_and_whitespace() {
        assert!(classify_line(b"").is_err());
        assert!(classify_line(b" create x").is_err());
        assert!(classify_line(b"create x").is_ok());
    }
}
