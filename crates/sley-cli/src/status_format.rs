//! Status output path quoting (`core.quotePath` semantics).

use std::io::Write;

use sley::Result;

pub(crate) fn status_quote_path(path: &[u8], quote_space: bool) -> String {
    status_quote_path_full(path, quote_space, true)
}

/// Like [`status_quote_path`] but parameterized by git's `quote_path_fully`
/// (`core.quotePath`): when `quote_path_fully` is false, bytes `>= 0x80` are
/// emitted verbatim instead of octal-escaped, so a UTF-8 path with no other
/// quote-forcing byte comes through raw (matching `quote_c_style` with
/// `core.quotePath=false`). Control bytes, `0x7f`, `"` and `\` are still quoted.
pub(crate) fn status_quote_path_full(
    path: &[u8],
    quote_space: bool,
    quote_path_fully: bool,
) -> String {
    if !status_path_needs_quotes_full(path, quote_space, quote_path_fully) {
        return String::from_utf8_lossy(path).into_owned();
    }
    let mut out: Vec<u8> = vec![b'"'];
    for &byte in path {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x20..=0x7e => out.push(byte),
            0x80..=0xff if !quote_path_fully => out.push(byte),
            _ => out.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    out.push(b'"');
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn write_status_quoted_path(
    writer: &mut impl Write,
    path: &[u8],
    quote_space: bool,
) -> Result<()> {
    if !status_path_needs_quotes(path, quote_space) {
        writer.write_all(path)?;
        return Ok(());
    }
    writer.write_all(b"\"")?;
    for &byte in path {
        match byte {
            b'"' => writer.write_all(br#"\""#)?,
            b'\\' => writer.write_all(br#"\\"#)?,
            b'\n' => writer.write_all(br#"\n"#)?,
            b'\t' => writer.write_all(br#"\t"#)?,
            0x20..=0x7e => writer.write_all(&[byte])?,
            _ => write!(writer, "\\{byte:03o}")?,
        }
    }
    writer.write_all(b"\"")?;
    Ok(())
}

fn status_path_needs_quotes(path: &[u8], quote_space: bool) -> bool {
    status_path_needs_quotes_full(path, quote_space, true)
}

fn status_path_needs_quotes_full(path: &[u8], quote_space: bool, quote_path_fully: bool) -> bool {
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