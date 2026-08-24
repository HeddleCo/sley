//! Status output path quoting (`core.quotePath` semantics).
//!
//! The single C-quoting implementation lives in
//! `sley-formats::path_quote`; these wrappers preserve the historical CLI
//! signatures so call sites stay untouched.

use std::io::Write;

use sley::Result;
use sley::plumbing::sley_formats::{quoted_path, write_quoted_path};

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
    quoted_path(path, quote_space, quote_path_fully)
}

pub(crate) fn write_status_quoted_path(
    writer: &mut impl Write,
    path: &[u8],
    quote_space: bool,
) -> Result<()> {
    Ok(write_quoted_path(writer, path, quote_space, true)?)
}
