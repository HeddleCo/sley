//! Byte-level binary-format primitives shared by the on-disk format crates
//! (packfiles, pack indexes, indexes, reftables, commit-graphs, MIDX, ZIP).
//!
//! Two flavors are provided on purpose, because upstream call sites differ in
//! how they handle short input:
//!
//! - `u16_be` / `u32_be` / `u64_be` decode an **exact-width slice** and panic on
//!   short input. Their callers pre-slice (`&data[start..start + 4]`), so the
//!   panic reproduces the previous out-of-bounds index panic bit-for-bit and
//!   keeps the hot decode loops free of extra branches.
//! - `read_*` / `get_*` take `(bytes, offset)` and report truncation through
//!   `Result` / `Option`, matching parsers that walk untrusted buffers.
//!
//! The biased varint here is git's "offset encoding" used by ofs-delta base
//! offsets (gitformat-pack), index v4 path compression, and the untracked
//! cache: big-endian group order, high bit marks continuation, and every
//! continued group carries a `+1` bias before shifting.

use crate::{GitError, Result};

// ---------------------------------------------------------------------------
// Fixed-width big-endian reads over pre-sliced buffers.
//
// Direct indexing (not `try_into`) keeps these identical to the helpers they
// replace: short input panics at the index, exactly as before.
// ---------------------------------------------------------------------------

/// Read a big-endian `u16` from the first two bytes of `bytes`.
///
/// Panics if `bytes` is shorter than 2 bytes (callers pre-slice).
pub fn u16_be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// Read a big-endian `u32` from the first four bytes of `bytes`.
///
/// Panics if `bytes` is shorter than 4 bytes (callers pre-slice).
pub fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Read a big-endian `u64` from the first eight bytes of `bytes`.
///
/// Panics if `bytes` is shorter than 8 bytes (callers pre-slice).
pub fn u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

// ---------------------------------------------------------------------------
// Bounds-checked readers at an offset.
// ---------------------------------------------------------------------------

/// Read a big-endian `u16` at `offset`, or `Err` when fewer than 2 bytes remain.
pub fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint16".into()))?;
    Ok(u16::from_be_bytes([raw[0], raw[1]]))
}

/// Read a big-endian unsigned 24-bit integer at `offset`, or `Err` when fewer
/// than 3 bytes remain.
pub fn read_u24(bytes: &[u8], offset: usize) -> Result<u32> {
    let raw = bytes
        .get(offset..offset + 3)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint24".into()))?;
    Ok((u32::from(raw[0]) << 16) | (u32::from(raw[1]) << 8) | u32::from(raw[2]))
}

/// Read a big-endian `u32` at `offset`, or `Err` when fewer than 4 bytes remain.
pub fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint32".into()))?;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Read a big-endian `u64` at `offset`, or `Err` when fewer than 8 bytes remain.
pub fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint64".into()))?;
    Ok(u64::from_be_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

/// Read a little-endian `u16` at `offset`, or `None` when out of bounds.
pub fn get_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

/// Read a little-endian `u32` at `offset`, or `None` when out of bounds.
pub fn get_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Read a little-endian `u64` at `offset`, or `None` when out of bounds.
pub fn get_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Read a big-endian `u32` at `offset`, or `None` when out of bounds.
pub fn get_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

// ---------------------------------------------------------------------------
// Writers.
// ---------------------------------------------------------------------------

/// Append a 24-bit big-endian value. Values above `0xff_ffff` wrap silently;
/// range policy belongs to the caller (e.g. reftable block-size limits).
pub fn write_u24(out: &mut Vec<u8>, value: u32) {
    out.push((value >> 16) as u8);
    out.push((value >> 8) as u8);
    out.push(value as u8);
}

/// Overwrite three bytes at `offset` with a big-endian 24-bit value, or `Err`
/// when the slice is too short. Range policy belongs to the caller.
pub fn write_u24_at(out: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let target = out
        .get_mut(offset..offset + 3)
        .ok_or_else(|| GitError::InvalidFormat("uint24 write is out of bounds".into()))?;
    target[0] = (value >> 16) as u8;
    target[1] = (value >> 8) as u8;
    target[2] = value as u8;
    Ok(())
}

// ---------------------------------------------------------------------------
// Biased varints ("offset encoding").
// ---------------------------------------------------------------------------

/// Why a biased-varint decode failed.
///
/// Kept distinct because upstream call sites report different errors: pack
/// parsers say "truncated..." when the buffer runs out but "offset overflow"
/// when the accumulator saturates, and byte-for-byte parity requires both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiasedVarintError {
    /// The buffer ended before a terminal byte was read.
    Truncated,
    /// The accumulated value does not fit in `u64`.
    ///
    /// Mirrors upstream git: only additions are overflow-checked; the `<< 7`
    /// silently drops bits shifted past the top (a constant shift can never
    /// fail), so hostile inputs may wrap rather than always erroring.
    Overflow,
}

/// Decode git's biased varint from `bytes` starting at `*cursor`, advancing the
/// cursor past the consumed bytes.
///
/// This is the loop behind ofs-delta base offsets (gitformat-pack); it is kept
/// allocation-free and branch-shaped like its per-crate predecessors so the
/// pack decode path does not regress.
pub fn read_biased_varint(
    bytes: &[u8],
    cursor: &mut usize,
) -> std::result::Result<u64, BiasedVarintError> {
    let Some(mut byte) = bytes.get(*cursor).copied() else {
        return Err(BiasedVarintError::Truncated);
    };
    *cursor += 1;
    let mut value = u64::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        let Some(next) = bytes.get(*cursor).copied() else {
            return Err(BiasedVarintError::Truncated);
        };
        byte = next;
        *cursor += 1;
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add(u64::from(byte & 0x7f)))
            .ok_or(BiasedVarintError::Overflow)?;
    }
    Ok(value)
}

/// Encode `value` as git's biased varint, most-significant group first: every
/// byte except the last has the continuation bit set.
pub fn write_biased_varint(mut value: u64, out: &mut Vec<u8>) {
    // ceil(64 / 7) == 10 groups is enough for any u64.
    let mut groups = [0u8; 10];
    let mut len = 0;
    groups[len] = (value & 0x7f) as u8;
    len += 1;
    value >>= 7;
    while value != 0 {
        value -= 1;
        groups[len] = 0x80 | (value & 0x7f) as u8;
        len += 1;
        value >>= 7;
    }
    out.extend(groups[..len].iter().rev());
}

// ---------------------------------------------------------------------------
// Shared prefixes.
// ---------------------------------------------------------------------------

/// Length of the longest common prefix of two slices, element-wise.
///
/// Generic over the element type so byte slices (reftable key compression,
/// index v4 path compression) and line slices (three-way blob merge) share one
/// implementation; monomorphization emits the same code as the concrete
/// versions it replaces.
pub fn common_prefix_len<T: PartialEq>(left: &[T], right: &[T]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_be_reads_round_trip() {
        assert_eq!(u16_be(&[0x12, 0x34]), 0x1234);
        assert_eq!(u32_be(&[0x12, 0x34, 0x56, 0x78]), 0x1234_5678);
        assert_eq!(
            u64_be(&[0, 0, 0, 0, 0x12, 0x34, 0x56, 0x78]),
            0x1234_5678_u64
        );
    }

    /// Render the `Err` of a primitives call for assertion; panics on `Ok`.
    fn err_string<T>(result: crate::Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected Err"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn offset_readers_report_truncation() {
        assert_eq!(
            err_string(read_u16(&[0x12], 0)),
            "invalid format: truncated uint16"
        );
        assert_eq!(read_u16(&[0x12, 0x34], 0).expect("in bounds"), 0x1234);
        assert_eq!(
            err_string(read_u24(&[1, 2], 0)),
            "invalid format: truncated uint24"
        );
        assert_eq!(
            read_u24(&[0, 0xab, 0xcd, 0xef], 1).expect("in bounds"),
            0xab_cdef
        );
        assert_eq!(
            err_string(read_u32(&[0; 3], 0)),
            "invalid format: truncated uint32"
        );
        assert_eq!(
            err_string(read_u64(&[0; 7], 0)),
            "invalid format: truncated uint64"
        );
    }

    #[test]
    fn offset_getters_return_none_when_short() {
        assert_eq!(get_u16_le(&[0x01, 0x02], 0), Some(0x0201));
        assert_eq!(get_u16_le(&[0x01], 0), None);
        assert_eq!(get_u32_le(&[1, 2, 3, 4], 0), Some(0x0403_0201));
        assert_eq!(get_u32_le(&[1, 2, 3, 4], 1), None);
        assert_eq!(get_u64_le(&[1; 8], 0), Some(0x0101_0101_0101_0101));
        assert_eq!(get_u32_be(&[1, 2, 3, 4], 0), Some(0x0102_0304));
        assert_eq!(get_u32_be(&[], 0), None);
    }

    #[test]
    fn u24_writers_round_trip() {
        let mut buf = Vec::new();
        write_u24(&mut buf, 0x0a_bc_de);
        assert_eq!(buf, vec![0x0a, 0xbc, 0xde]);

        let mut scratch = [0xff_u8; 5];
        write_u24_at(&mut scratch, 1, 0x01_02_03).expect("in bounds");
        assert_eq!(&scratch, &[0xff, 0x01, 0x02, 0x03, 0xff]);
        assert!(write_u24_at(&mut scratch, 4, 0).is_err());
    }

    #[test]
    fn biased_varint_matches_ofs_delta_encoding() {
        // Relative offsets 0..=127 are one byte; 128 encodes as 0x80 0x00
        // (the continued group carries the +1 bias: "(0 + 1) << 7 | 0").
        let mut out = Vec::new();
        write_biased_varint(0, &mut out);
        assert_eq!(out, vec![0x00]);
        out.clear();
        write_biased_varint(127, &mut out);
        assert_eq!(out, vec![0x7f]);
        out.clear();
        write_biased_varint(128, &mut out);
        assert_eq!(out, vec![0x80, 0x00]);
        out.clear();
        write_biased_varint(0xffff_ffff_ffff_ffff, &mut out);

        let mut cursor = 0usize;
        let decoded = read_biased_varint(&out, &mut cursor).expect("test operation should succeed");
        assert_eq!(decoded, 0xffff_ffff_ffff_ffff);
        assert_eq!(cursor, out.len());

        // Truncated input reports Truncated and leaves the cursor at the end
        // of the consumed bytes.
        let mut cursor = 0usize;
        assert_eq!(
            read_biased_varint(&[0x81, 0x81], &mut cursor),
            Err(BiasedVarintError::Truncated)
        );
        assert_eq!(cursor, 2);
    }

    #[test]
    fn biased_varint_rejects_overflow() {
        // `checked_shl` only guards the shift amount (upstream-compatible:
        // lost high bits are not detected), so overflow surfaces through
        // `checked_add`: drive the accumulator to exactly `u64::MAX`, then hit
        // one more continuation group.
        let mut hostile = Vec::new();
        write_biased_varint(u64::MAX, &mut hostile);
        let last = hostile.len() - 1;
        hostile[last] = 0xff; // terminal -> continuation, same payload 0x7f
        hostile.push(0x00); // one more group: (MAX + 1) overflows
        let mut cursor = 0usize;
        assert_eq!(
            read_biased_varint(&hostile, &mut cursor),
            Err(BiasedVarintError::Overflow)
        );
    }

    #[test]
    fn common_prefix_counts_shared_elements() {
        assert_eq!(
            common_prefix_len(b"refs/heads/main", b"refs/heads/next"),
            11
        );
        assert_eq!(common_prefix_len(b"", b""), 0);
        assert_eq!(common_prefix_len(b"abc", b"abd"), 2);
        assert_eq!(common_prefix_len(b"ab", b"abcd"), 2);
        assert_eq!(common_prefix_len(&[1u16, 2, 3], &[1u16, 9]), 1);
    }
}
