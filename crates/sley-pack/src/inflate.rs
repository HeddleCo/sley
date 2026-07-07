//! Bounded inflate allocation helpers for untrusted compressed input.

/// Worst-case expansion ratio for zlib deflate streams (git's bound). A
/// `compressed_len`-byte input cannot plausibly decompress to more than this
/// multiple without exceeding what the format allows.
pub const MAX_INFLATE_EXPANSION: usize = 1032;

/// An absolute ceiling on the speculative pre-reservation, independent of the
/// input length, so even a large legitimate-looking compressed input can't be
/// turned into a multi-gigabyte up-front allocation. Inflate still grows the
/// output buffer organically past this when a real stream genuinely produces
/// that much — this only caps the *speculative* reserve.
pub const MAX_INFLATE_RESERVE: usize = 64 * 1024 * 1024;

/// Bound a caller-supplied (possibly attacker-controlled) decompressed-size hint
/// to something safe to reserve up front: no larger than what `compressed_len`
/// input bytes could plausibly inflate to, and never above a fixed ceiling. The
/// returned value is only used to size the initial allocation; the inflate loop
/// grows the buffer as the real stream produces output, so legitimate large
/// objects still decode correctly — they just don't get the whole allocation at
/// once.
pub fn bounded_inflate_reserve(size_hint: usize, compressed_len: usize) -> usize {
    let input_ceiling = compressed_len.saturating_mul(MAX_INFLATE_EXPANSION);
    // 64 (floor) <= MAX_INFLATE_RESERVE (ceiling) always, so `clamp` cannot panic.
    size_hint.min(input_ceiling).clamp(64, MAX_INFLATE_RESERVE)
}

#[cfg(test)]
mod tests {
    use super::{bounded_inflate_reserve, MAX_INFLATE_RESERVE};

    #[test]
    fn bounded_inflate_reserve_caps_attacker_declared_size() {
        // A tiny compressed input can't justify a multi-gigabyte reservation.
        assert_eq!(bounded_inflate_reserve(u64::MAX as usize, 10), 10 * 1032);
        // The absolute ceiling caps even a large input-justified hint.
        assert_eq!(
            bounded_inflate_reserve(usize::MAX, usize::MAX),
            MAX_INFLATE_RESERVE
        );
        // A modest legitimate hint is preserved unchanged (no regression for real
        // objects): 1000 bytes of output from 500 bytes of input is well within
        // both bounds.
        assert_eq!(bounded_inflate_reserve(1000, 500), 1000);
        // Floor of 64 for tiny hints.
        assert_eq!(bounded_inflate_reserve(0, 0), 64);
    }
}