//! Ceilings on untrusted wire responses that a caller buffers whole, kept in
//! one place so the whole budget is reviewable at a glance.
//!
//! These bound the *volume* a peer can make sley hold in memory. They are the
//! companion to the wall-clock deadlines in `sley-transport`, which bound the
//! *time* a peer can hold a request open: a size cap alone does nothing against
//! a peer trickling one byte per interval, and a deadline alone does nothing
//! against a peer that can saturate the link (sley#163).
//!
//! Every ceiling below is derived from what a legitimately large repository
//! actually needs, and states its design point, so a future change is a change
//! to a stated assumption rather than to an unexplained number.

use sley_core::{GitError, Result};
use std::io::Read;

/// Worst-case on-the-wire size of one ref in a v0/v1 reference advertisement.
///
/// A ref line is a pkt-line: 4 bytes of length prefix, the object id in hex
/// (64 for SHA-256, the larger of the two formats), a space, the refname, and a
/// terminating LF — about 198 bytes for a generous 128-byte refname. Rounded up
/// to 256 so the ceiling stays an over-estimate rather than an under-estimate.
const REF_ADVERTISEMENT_BYTES_PER_REF: u64 = 256;

/// Refs the reference advertisement is sized to admit.
///
/// The largest widely-cloned public repositories advertise well under 100k
/// refs. Half a million leaves better than 5x headroom over that, and covers
/// review-system repositories that publish a `refs/changes/*` ref per patchset.
/// Beyond this a client should be using protocol v2 `ls-refs` with a
/// `ref-prefix`, which never materialises the full advertisement in the first
/// place.
const MAX_ADVERTISED_REFS: u64 = 512 * 1024;

/// Ceiling on a buffered reference advertisement / service-discovery response.
///
/// 512Ki refs x 256 bytes = 128 MiB.
pub const MAX_REF_ADVERTISEMENT_BYTES: u64 = MAX_ADVERTISED_REFS * REF_ADVERTISEMENT_BYTES_PER_REF;

/// Ceiling on a packfile-bearing response that is buffered whole in memory.
///
/// The design point is a full clone of the largest repository sley is expected
/// to handle: linux.git transfers roughly 2.5 GiB of pack on a full clone
/// today, so 4 GiB leaves about 60% headroom for growth.
///
/// This is deliberately a ceiling on a pathological case and not a target. A
/// response anywhere near it is already a multi-gigabyte resident allocation,
/// which is a property of these buffered entry points, not something the bound
/// introduces — the fetch and clone paths stream a pack into the object store
/// instead of calling them, and a caller that needs more than this should do
/// the same.
pub const MAX_PACKFILE_RESPONSE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The slowest average transfer rate a peer may sustain and still be served.
///
/// Used to turn the size ceilings above into the wall-clock deadlines in
/// `sley-transport`: a body deadline is `size ceiling / this rate`, so the two
/// halves of the budget cannot drift apart.
///
/// 1 MiB/s (8 Mbit/s) is well below ordinary broadband, and it bounds the
/// *whole* transfer rather than any instant, so a slow link is only refused
/// when the total transfer would not have finished in the budget anyway. The
/// tradeoff it encodes: a multi-gigabyte clone over a sub-megabyte link is
/// refused rather than held open indefinitely, and such a clone should use a
/// bundle or a partial clone.
pub const MIN_TRANSFER_BYTES_PER_SEC: u64 = 1024 * 1024;

fn size_limit_exceeded(what: &str, limit: u64) -> GitError {
    GitError::InvalidFormat(format!(
        "{what} exceeds the maximum accepted size of {limit} bytes"
    ))
}

/// Read `reader` to end, refusing to buffer more than `limit` bytes.
///
/// The bounded counterpart of `Read::read_to_end`, which on a socket carrying
/// remote data has no ceiling at all. `what` names the thing being read, for
/// the error message.
pub fn read_to_end_bounded(reader: &mut dyn Read, limit: u64, what: &str) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    append_to_end_bounded(reader, &mut buffer, limit, what)?;
    Ok(buffer)
}

/// Append the rest of `reader` to `buffer`, refusing to let `buffer` grow past
/// `limit` bytes in total.
///
/// The limit covers what `buffer` already holds, so a caller that has read a
/// prefix cannot spend the budget twice.
pub fn append_to_end_bounded(
    reader: &mut dyn Read,
    buffer: &mut Vec<u8>,
    limit: u64,
    what: &str,
) -> Result<()> {
    let buffered = buffer.len() as u64;
    if buffered > limit {
        return Err(size_limit_exceeded(what, limit));
    }
    // One byte past the limit: enough to tell "exactly at the ceiling" (fine)
    // from "over it" (refused) without reading the overage.
    let headroom = (limit - buffered).saturating_add(1);
    (&mut *reader).take(headroom).read_to_end(buffer)?;
    if buffer.len() as u64 > limit {
        return Err(size_limit_exceeded(what, limit));
    }
    Ok(())
}
