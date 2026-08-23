//! C-style porcelain path quoting (`quote.c:quote_c_style`).
//!
//! Single workspace implementation of the double-quote dialect shared by
//! `status --porcelain`, the diff raw/name-status families, and `ls-tree`: a
//! path is wrapped in double quotes when any byte forces quoting, with each
//! special byte emitted as a short C escape or a three-digit octal escape.

use std::io::{self, Write};

/// Whether `path` needs C-quoting under git's porcelain rules.
///
/// Control bytes (`< 0x20`), `DEL`, `"`, `\`, newline, and tab always force
/// quotes; bytes `>= 0x80` do unless `quote_path_fully` is false
/// (`core.quotePath=false`); a literal space forces quotes only when
/// `quote_space` is set (the status-dialect flag; the diff/ls-tree dialects
/// leave it unset).
fn path_needs_quotes(path: &[u8], quote_space: bool, quote_path_fully: bool) -> bool {
    path.iter().any(|&byte| {
        byte == b'"'
            || byte == b'\\'
            || byte == b'\n'
            || byte == b'\t'
            || byte < 0x20
            || byte == 0x7f
            || (quote_path_fully && byte >= 0x80)
            || (quote_space && byte == b' ')
    })
}

/// Write `path` to `writer` with C-style quoting.
///
/// Unquoted paths pass through verbatim without UTF-8 validation, matching
/// upstream's byte-oriented output; quoted paths escape `"`, `\`, newline,
/// and tab as C escapes and every other non-printable byte as three-digit
/// octal. Bytes `>= 0x80` are octal-escaped only when `quote_path_fully` is
/// true (`core.quotePath`, default on).
pub fn write_quoted_path(
    writer: &mut dyn Write,
    path: &[u8],
    quote_space: bool,
    quote_path_fully: bool,
) -> io::Result<()> {
    if !path_needs_quotes(path, quote_space, quote_path_fully) {
        writer.write_all(path)?;
        return Ok(());
    }
    writer.write_all(b"\"")?;
    for &byte in path {
        match byte {
            b'"' => writer.write_all(b"\\\"")?,
            b'\\' => writer.write_all(b"\\\\")?,
            b'\n' => writer.write_all(b"\\n")?,
            b'\t' => writer.write_all(b"\\t")?,
            0x20..=0x7e => writer.write_all(&[byte])?,
            0x80..=0xff if !quote_path_fully => writer.write_all(&[byte])?,
            _ => write!(writer, "\\{byte:03o}")?,
        }
    }
    writer.write_all(b"\"")?;
    Ok(())
}

/// Render `path` as an owned string with C-style quoting.
///
/// Non-UTF-8 bytes are lossily converted exactly like upstream renderers that
/// build `strbuf`s and hand them to string-based output paths.
pub fn quoted_path(path: &[u8], quote_space: bool, quote_path_fully: bool) -> String {
    let mut out = Vec::with_capacity(path.len() + 2);
    // `Vec<u8>`'s `io::Write` impl cannot fail, so the result carries no error.
    let _ = write_quoted_path(&mut out, path, quote_space, quote_path_fully);
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_stay_bare() {
        assert_eq!(quoted_path(b"src/main.c", false, true), "src/main.c");
        assert_eq!(quoted_path("café".as_bytes(), false, false), "café");
    }

    #[test]
    fn quote_forcing_bytes_are_escaped() {
        assert_eq!(quoted_path(b"tab\there", false, true), "\"tab\\there\"");
        assert_eq!(quoted_path(b"line\nbreak", false, true), "\"line\\nbreak\"");
        assert_eq!(quoted_path(b"a\"b", false, true), "\"a\\\"b\"");
        assert_eq!(quoted_path(b"a\\b", false, true), "\"a\\\\b\"");
        assert_eq!(quoted_path(b"\x01\x7f", false, true), "\"\\001\\177\"");
    }

    #[test]
    fn high_bytes_follow_core_quote_path() {
        // core.quotePath=true (default): UTF-8 bytes are octal-escaped.
        assert_eq!(
            quoted_path("café".as_bytes(), false, true),
            "\"caf\\303\\251\""
        );
        // core.quotePath=false: high bytes stay verbatim...
        assert_eq!(quoted_path("café".as_bytes(), false, false), "café");
        // ...but a quote-forcing byte still triggers quoting, with high bytes
        // passed through inside the quotes.
        assert_eq!(
            quoted_path("caf\x01é".as_bytes(), false, false),
            "\"caf\\001é\""
        );
    }

    #[test]
    fn space_quoting_is_dialect_specific() {
        assert_eq!(quoted_path(b"a b", false, true), "a b");
        assert_eq!(quoted_path(b"a b", true, true), "\"a b\"");
        // Inside the quotes a space stays verbatim.
        assert_eq!(quoted_path(b"\t ", true, true), "\"\\t \"");
    }

    #[test]
    fn unquoted_non_utf8_passes_through_verbatim_to_the_writer() {
        // With core.quotePath=false the high bytes do not force quoting and
        // are written verbatim, without UTF-8 validation.
        let mut out: Vec<u8> = Vec::new();
        let written = write_quoted_path(&mut out, b"raw\xff\xfe", false, false);
        assert!(written.is_ok());
        assert_eq!(out, b"raw\xff\xfe");
    }
}
