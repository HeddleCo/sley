//! Bounds applied to untrusted pack input, kept in one place so the whole
//! budget is reviewable at a glance.
//!
//! Packs reach these parsers straight off the wire (`sley-remote`'s fetch and
//! receive-pack paths hand a remote's bytes to `PackFile::parse`,
//! `PackIndex::write_v2_for_pack`, and the streaming indexer), so every field a
//! peer controls needs an explicit ceiling rather than an implicit one.

use super::*;

/// The smallest number of on-disk bytes a pack entry can possibly occupy.
///
/// An entry is a type/size varint header (at least one byte) followed by a
/// zlib stream, whose two-byte header is itself the minimum any zlib stream
/// can be. The true floor is higher — a real zlib stream also carries at least
/// one deflate block and a four-byte Adler-32, so nine bytes — but the check
/// below is a rejection test on untrusted input, and a deliberately loose
/// lower bound cannot reject a pack that a conforming writer produced.
const MIN_PACK_ENTRY_BYTES: u64 = 3;

/// Upper bound on the speculative `Vec::with_capacity` taken from a declared
/// object count when the total pack length is not known up front (the
/// streaming indexer reads the header before it has seen the body).
///
/// 2^16 entries is roughly the object count of a routine incremental fetch, so
/// ordinary packs still get a single up-front allocation; larger packs simply
/// fall back to the `Vec`'s geometric growth, which costs a handful of
/// reallocations and cannot be steered by the declared count.
pub const PACK_OBJECT_COUNT_PREALLOC_CAP: usize = 64 * 1024;

/// Default maximum delta chain depth accepted when reading a pack (sley#5).
///
/// Deliberately the same value as [`DEFAULT_PACK_DEPTH`], which sley's writer
/// uses and which is also git's `pack.depth` default. This is the value used by
/// [`PackReadLimits::default`]; callers that legitimately consume deeper packs
/// can raise [`PackReadLimits::max_delta_depth`] while retaining a finite bound.
///
/// The bound is what makes whole-pack resolution linear: `resolve_pack_entries`
/// makes repeated passes over the entry list, and each pass advances every
/// chain by at least one link, so the pass count is bounded by the deepest
/// chain. Without a ceiling an adversarial pack that orders one long chain
/// back-to-front costs O(N^2) in passes alone.
pub const MAX_READ_DELTA_CHAIN_DEPTH: usize = DEFAULT_PACK_DEPTH;

/// Validate a pack header's declared object count against the bytes actually
/// available for entries, then return it as a `usize` (sley#4).
///
/// `entry_bytes_available` is the span between the 12-byte header and the
/// trailing checksum. A 32-byte pack claiming four billion objects is rejected
/// here instead of reaching `Vec::with_capacity`.
pub(crate) fn checked_pack_object_count(
    declared: u32,
    entry_bytes_available: u64,
) -> Result<usize> {
    let declared = u64::from(declared);
    let max_possible = entry_bytes_available / MIN_PACK_ENTRY_BYTES;
    if declared > max_possible {
        return Err(GitError::InvalidFormat(format!(
            "pack declares {declared} objects but only has room for {max_possible} \
             in {entry_bytes_available} bytes"
        )));
    }
    usize::try_from(declared)
        .map_err(|_| GitError::InvalidFormat(format!("pack object count {declared} overflows")))
}

/// Capacity to reserve up front for a declared object count (sley#4).
///
/// The count is attacker-controlled, so it selects the reservation but never
/// dictates it; anything above [`PACK_OBJECT_COUNT_PREALLOC_CAP`] grows on
/// demand as entries are actually parsed.
pub(crate) fn pack_entry_prealloc(count: usize) -> usize {
    count.min(PACK_OBJECT_COUNT_PREALLOC_CAP)
}
