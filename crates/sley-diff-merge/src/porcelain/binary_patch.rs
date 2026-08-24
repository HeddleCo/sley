//! Binary patch bodies (`--binary` GIT binary patch blocks and the
//! `Binary files … differ` fallback).

use super::options::DiffRenderOptions;
use super::patch_entry::{
    diff_patch_mode_suffix, diff_patch_oid, diff_patch_prefixed_path, write_diff_similarity_headers,
};
use crate::{NameStatus, NameStatusEntry};
use sley_core::Result;
use std::io::Write;

pub(super) fn write_diff_binary_patch_entry(
    stdout: &mut dyn Write,
    entry: &NameStatusEntry,
    old_content: Option<Vec<u8>>,
    new_content: Option<Vec<u8>>,
    options: DiffRenderOptions<'_>,
) -> Result<()> {
    let old_path = entry.old_path.as_deref().unwrap_or(&entry.path);
    let diff_old_path = diff_patch_prefixed_path(options.src_prefix, old_path);
    let diff_path = diff_patch_prefixed_path(options.dst_prefix, &entry.path);
    let quote = |path: &[u8]| sley_formats::quoted_path(path, false, true);
    let old_similarity_path = quote(old_path);
    let similarity_path = quote(&entry.path);
    writeln!(stdout, "diff --git {diff_old_path} {diff_path}",)?;
    match entry.status {
        NameStatus::Added => {
            if let Some(mode) = entry.new_mode {
                writeln!(stdout, "new file mode {mode:06o}")?;
            }
        }
        NameStatus::Deleted => {
            if let Some(mode) = entry.old_mode {
                writeln!(stdout, "deleted file mode {mode:06o}")?;
            }
        }
        NameStatus::Modified
        | NameStatus::TypeChanged
        | NameStatus::Renamed(_)
        | NameStatus::Copied(_) => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                writeln!(stdout, "old mode {old_mode:06o}")?;
                writeln!(stdout, "new mode {new_mode:06o}")?;
            }
        }
        // Unmerged paths carry no patch meta header.
        NameStatus::Unmerged => {}
    }
    write_diff_similarity_headers(&mut *stdout, entry, &old_similarity_path, &similarity_path)?;
    if old_content.as_deref() == new_content.as_deref() {
        return Ok(());
    }
    // `--binary` implies `--full-index`: the binary apply requires full hex OIDs.
    let index_abbrev = if options.binary {
        options.format.hex_len()
    } else {
        options.abbrev
    };
    writeln!(
        stdout,
        "index {}..{}{}",
        diff_patch_oid(
            options.db,
            entry.old_oid.as_ref(),
            old_content.as_deref(),
            options.format,
            index_abbrev,
        ),
        diff_patch_oid(
            options.db,
            entry.new_oid.as_ref(),
            new_content.as_deref(),
            options.format,
            index_abbrev,
        ),
        diff_patch_mode_suffix(entry)
    )?;
    if options.binary {
        // Emit an applicable `GIT binary patch` block (forward then reverse hunk,
        // each literal-encoded). Round-trips through the apply binary codec.
        writeln!(stdout, "GIT binary patch")?;
        write_git_binary_hunk(stdout, new_content.as_deref().unwrap_or(b""))?;
        write_git_binary_hunk(stdout, old_content.as_deref().unwrap_or(b""))?;
        return Ok(());
    }
    let old = match old_content {
        Some(_) => diff_patch_prefixed_path(options.src_prefix, old_path),
        None => "/dev/null".to_string(),
    };
    let new = match new_content {
        Some(_) => diff_patch_prefixed_path(options.dst_prefix, &entry.path),
        None => "/dev/null".to_string(),
    };
    writeln!(stdout, "Binary files {old} and {new} differ")?;
    Ok(())
}

/// Emit one `literal <N>` binary hunk: the zlib-deflated content base85-encoded
/// in git's `emit_binary_diff_body` line layout (a length-byte + up to 52 bytes
/// per line), terminated by a blank line.
fn write_git_binary_hunk(stdout: &mut dyn Write, content: &[u8]) -> Result<()> {
    let deflated = deflate_zlib(content);
    writeln!(stdout, "literal {}", content.len())?;
    for chunk in deflated.chunks(52) {
        let mut line = Vec::with_capacity(1 + chunk.len() / 4 * 5 + 5);
        // Length byte: 'A'-'Z' for 1-26 bytes, 'a'-'z' for 27-52.
        let len = chunk.len();
        line.push(if len <= 26 {
            (len as u8) + b'A' - 1
        } else {
            (len as u8) - 26 + b'a' - 1
        });
        encode_base85_group(&mut line, chunk);
        stdout.write_all(&line)?;
        stdout.write_all(b"\n")?;
    }
    writeln!(stdout)?;
    Ok(())
}

/// base85-encode `data` (4 bytes → 5 chars, big-endian), git's `encode_85`.
fn encode_base85_group(out: &mut Vec<u8>, data: &[u8]) {
    const EN85: &[u8; 85] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
    let mut i = 0;
    while i < data.len() {
        let mut acc: u32 = 0;
        for shift in [24u32, 16, 8, 0] {
            if i < data.len() {
                acc |= (data[i] as u32) << shift;
                i += 1;
            } else {
                break;
            }
        }
        let mut group = [0u8; 5];
        let mut value = acc;
        for slot in group.iter_mut().rev() {
            *slot = EN85[(value % 85) as usize];
            value /= 85;
        }
        out.extend_from_slice(&group);
    }
}

/// Deflate `content` with a zlib header/trailer.
fn deflate_zlib(content: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write as _;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(9));
    let _ = encoder.write_all(content);
    encoder.finish().unwrap_or_default()
}
