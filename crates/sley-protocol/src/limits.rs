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
//!
//! # Configuration
//!
//! The values are the *defaults* of [`TransportLimits`], not the only values
//! available. A caller passes a different [`TransportLimits`] to the
//! `*_with_limits` entry points, and `sley-transport` builds one from git
//! config (`sley.maxRefAdvertisementBytes`, `sley.maxPackfileResponseBytes`,
//! `sley.minTransferBytesPerSec`) so an operator never has to recompile. This
//! is deliberately the same shape as `sley-pack`'s `PackReadLimits`: a `Copy`
//! struct with a `Default` that reproduces today's behaviour, threaded through
//! `*_with_limits` variants of the existing entry points.
//!
//! Configuration moves a ceiling; it cannot remove one. Every field is clamped
//! to a hard maximum ([`MAX_CONFIGURABLE_RESPONSE_BYTES`],
//! [`MAX_BODY_TRANSFER_TIMEOUT`]), zero is read as "unset" rather than as
//! "refuse everything", and there is no sentinel meaning "unlimited". A
//! configured budget is therefore always finite, and the derived wall-clock
//! deadline is finite no matter what the size ceiling is raised to.

use sley_core::{GitError, Result};
use std::io::Read;
use std::time::Duration;

/// Worst-case on-the-wire size of one ref in a v0/v1 reference advertisement.
///
/// A ref line is a pkt-line: 4 bytes of length prefix, the object id in hex
/// (64 for SHA-256, the larger of the two formats), a space, the refname, and a
/// terminating LF — about 198 bytes for a generous 128-byte refname. Rounded up
/// to 256 so the ceiling stays an over-estimate rather than an under-estimate.
///
/// This is also the divisor that turns a byte ceiling back into the unit an
/// operator actually thinks in, which is why the over-limit error reports both.
pub const REF_ADVERTISEMENT_BYTES_PER_REF: u64 = 256;

/// Refs the default reference advertisement ceiling is sized to admit.
///
/// The largest widely-cloned public repositories advertise far less: linux.git
/// advertises on the order of a thousand refs, Chromium a few thousand. Half a
/// million leaves better than two orders of magnitude of headroom for those.
///
/// It does *not* cover every shape a repository can take. A Gerrit-style
/// review system mints a `refs/changes/<n>/<change>/<patchset>` ref per
/// patchset, and large installations (Android's, for one) run past a million
/// refs. Such a repository is exactly the case the protocol-level answer is
/// for: protocol v2 `ls-refs` with a `ref-prefix` asks for the refs the client
/// wants and never materialises the full advertisement, so the ceiling is not
/// reached rather than raised. sley negotiates v2 for `git-upload-pack` by
/// default and sends `ref-prefix HEAD`, `refs/heads/`, `refs/tags/`, so its
/// own fetch and clone paths never see a `refs/changes/*` ref at all.
///
/// The ceiling still has to be right, because three paths do materialise a
/// full v0/v1 advertisement: `git-receive-pack` (push), which upstream
/// `remote-curl.c` never negotiates v2 for; any server without v2 support,
/// which answers a v2 request with a v0 advertisement; and a client with
/// `protocol.version` pinned to 0 or 1.
pub const DEFAULT_MAX_ADVERTISED_REFS: u64 = 512 * 1024;

/// Default ceiling on a buffered reference advertisement / service-discovery
/// response.
///
/// 512Ki refs x 256 bytes = 128 MiB. See [`DEFAULT_MAX_ADVERTISED_REFS`] for
/// what that ref count is chosen to cover and what it deliberately does not.
pub const MAX_REF_ADVERTISEMENT_BYTES: u64 =
    DEFAULT_MAX_ADVERTISED_REFS * REF_ADVERTISEMENT_BYTES_PER_REF;

/// Default ceiling on a packfile-bearing response that is buffered whole in
/// memory.
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
/// halves of the budget cannot drift apart — raising a size ceiling raises the
/// deadline that pays for it, in the same [`TransportLimits`] value.
///
/// 1 MiB/s (8 Mbit/s) is well below ordinary broadband, and it bounds the
/// *whole* transfer rather than any instant, so a slow link is only refused
/// when the total transfer would not have finished in the budget anyway. The
/// tradeoff it encodes: a multi-gigabyte clone over a sub-megabyte link is
/// refused rather than held open indefinitely, and such a clone should use a
/// bundle or a partial clone.
pub const MIN_TRANSFER_BYTES_PER_SEC: u64 = 1024 * 1024;

/// The largest value any configured response ceiling is allowed to take.
///
/// Configuration exists to move a ceiling, not to remove it, so a configured
/// value is clamped rather than trusted. 64 GiB is an order of magnitude past
/// the largest legitimate response either ceiling describes, which makes the
/// clamp unreachable in practice while keeping "the bound is finite" a property
/// of the code instead of a property of the operator's restraint.
pub const MAX_CONFIGURABLE_RESPONSE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// The longest a single body transfer phase may be given, whatever the size
/// ceiling and minimum rate are configured to.
///
/// The body deadline is derived (`size ceiling / minimum rate`), so without
/// this a large enough ceiling or a small enough rate would derive a deadline
/// long enough to be no deadline at all — reintroducing exactly the unbounded
/// wall-clock sley#163 closed. 24 hours is far past any transfer worth holding
/// a socket open for and is unreachable at the defaults (which derive 4096s).
pub const MAX_BODY_TRANSFER_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// One buffered-read ceiling: the value, what it protects, and — the part that
/// makes it usable — what to do about it when it binds.
///
/// A limit whose error says only "too large" leaves the reader to guess whether
/// they hit an attack, a misconfiguration, or a legitimate repository that sley
/// simply cannot read; and a limit whose only remedy is "edit this constant and
/// rebuild" is not a remedy for anyone running a release build. So a ceiling
/// carries its own remedy text to the point of failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadCeiling {
    /// The ceiling in bytes.
    pub limit: u64,
    /// What is being read, for the error message.
    pub what: &'static str,
    /// What the reader should actually do about it.
    pub remedy: &'static str,
}

impl ReadCeiling {
    /// The error raised when this ceiling binds.
    ///
    /// `observed` is what the reader had buffered when it stopped, which is one
    /// byte past the ceiling — the bounded readers stop there deliberately
    /// rather than reading the overage to find out how big the response really
    /// was. The message says so, so the number is not mistaken for the peer's
    /// actual response size.
    fn exceeded(&self, observed: u64) -> GitError {
        GitError::InvalidFormat(format!(
            "{} exceeds the configured ceiling of {} bytes (stopped at {} bytes; \
             the read stops one byte past the ceiling, so the peer may be sending \
             much more). {}",
            self.what, self.limit, observed, self.remedy
        ))
    }
}

const REF_ADVERTISEMENT_REMEDY: &str = concat!(
    "A protocol v0/v1 reference advertisement carries every ref in the ",
    "repository, so a repository with more refs than this admits cannot be read ",
    "this way at all and raising the ceiling only moves where it fails. The fix ",
    "is to not fetch the whole advertisement: protocol v2 `ls-refs` with a ",
    "`ref-prefix` requests only the refs you need and never materialises the ",
    "rest. sley negotiates v2 for git-upload-pack by default, so reaching this ",
    "means one of: the remote has no v2 support, `protocol.version` is pinned ",
    "to 0 or 1 locally, or this is git-receive-pack (push), which is v0 by ",
    "upstream parity. If none of those can change, raise the ceiling with ",
    "`git config sley.maxRefAdvertisementBytes <bytes>` — at ~256 bytes per ref ",
    "the default admits about 512Ki refs, and a review system with a ref per ",
    "patchset can legitimately exceed that."
);

const PACKFILE_RESPONSE_REMEDY: &str = concat!(
    "This entry point buffers the entire response in memory by construction. ",
    "The fetch and clone paths do not use it — they stream the pack into the ",
    "object store — so a response this large is better served by streaming than ",
    "by a larger buffer. If it genuinely has to be buffered, raise the ceiling ",
    "with `git config sley.maxPackfileResponseBytes <bytes>`; the body deadline ",
    "is derived from it and moves with it."
);

const REQUEST_BODY_REMEDY: &str = concat!(
    "This is the buffering fallback of `HttpClient::post_reader`; ",
    "`UreqHttpClient` streams the request body instead and never reaches it. ",
    "Supply a client that overrides `post_reader`, or raise the ceiling with ",
    "`git config sley.maxPackfileResponseBytes <bytes>`."
);

const RECEIVE_PACK_RESPONSE_REMEDY: &str = concat!(
    "This body is drained, not parsed: the push report is one pkt-line per ",
    "pushed ref, so a legitimate one is orders of magnitude below this ceiling ",
    "and a body that reaches it is not a push report. Raise the ceiling with ",
    "`git config sley.maxRefAdvertisementBytes <bytes>` only if the remote is ",
    "known to be answering with something else large and benign."
);

/// Every ceiling a peer's response is measured against, in one configurable
/// value.
///
/// [`Default`] reproduces the documented constants above, so a caller that
/// configures nothing gets exactly the behaviour the constants describe. The
/// same shape as `sley_pack::PackReadLimits`: `Copy`, `Default`, and threaded
/// through `*_with_limits` variants of the existing entry points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportLimits {
    /// Ceiling on a buffered v0/v1 reference advertisement or service-discovery
    /// response. Default [`MAX_REF_ADVERTISEMENT_BYTES`].
    pub max_ref_advertisement_bytes: u64,
    /// Ceiling on a buffered packfile-bearing response or request body.
    /// Default [`MAX_PACKFILE_RESPONSE_BYTES`].
    pub max_packfile_response_bytes: u64,
    /// Slowest average transfer rate served, which turns the size ceilings into
    /// the body deadline. Default [`MIN_TRANSFER_BYTES_PER_SEC`].
    pub min_transfer_bytes_per_sec: u64,
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            max_ref_advertisement_bytes: MAX_REF_ADVERTISEMENT_BYTES,
            max_packfile_response_bytes: MAX_PACKFILE_RESPONSE_BYTES,
            min_transfer_bytes_per_sec: MIN_TRANSFER_BYTES_PER_SEC,
        }
    }
}

impl TransportLimits {
    /// Clamp every field into the range where it is still a bound.
    ///
    /// Zero reads as "unset" and falls back to the default, matching how git
    /// treats a zero `http.postBuffer`; anything past
    /// [`MAX_CONFIGURABLE_RESPONSE_BYTES`] is clamped down. Applied by every
    /// constructor that takes operator input, so no configured
    /// [`TransportLimits`] can describe an unbounded read or an unbounded wait.
    #[must_use]
    pub fn clamped(self) -> Self {
        let default = Self::default();
        let clamp = |value: u64, fallback: u64| {
            if value == 0 {
                fallback
            } else {
                value.min(MAX_CONFIGURABLE_RESPONSE_BYTES)
            }
        };
        Self {
            max_ref_advertisement_bytes: clamp(
                self.max_ref_advertisement_bytes,
                default.max_ref_advertisement_bytes,
            ),
            max_packfile_response_bytes: clamp(
                self.max_packfile_response_bytes,
                default.max_packfile_response_bytes,
            ),
            min_transfer_bytes_per_sec: if self.min_transfer_bytes_per_sec == 0 {
                default.min_transfer_bytes_per_sec
            } else {
                self.min_transfer_bytes_per_sec
            },
        }
    }

    /// Ceiling for a buffered v0/v1 reference advertisement.
    #[must_use]
    pub fn ref_advertisement(&self) -> ReadCeiling {
        ReadCeiling {
            limit: self.max_ref_advertisement_bytes,
            what: "service discovery / reference advertisement response",
            remedy: REF_ADVERTISEMENT_REMEDY,
        }
    }

    /// Ceiling for a `git-receive-pack` response body that is drained unparsed.
    #[must_use]
    pub fn receive_pack_response(&self) -> ReadCeiling {
        ReadCeiling {
            limit: self.max_ref_advertisement_bytes,
            what: "receive-pack response",
            remedy: RECEIVE_PACK_RESPONSE_REMEDY,
        }
    }

    /// Ceiling for a buffered packfile-bearing upload-pack response.
    #[must_use]
    pub fn packfile_response(&self) -> ReadCeiling {
        ReadCeiling {
            limit: self.max_packfile_response_bytes,
            what: "upload-pack packfile response",
            remedy: PACKFILE_RESPONSE_REMEDY,
        }
    }

    /// Ceiling for an HTTP request body buffered by the `post_reader` fallback.
    #[must_use]
    pub fn http_request_body(&self) -> ReadCeiling {
        ReadCeiling {
            limit: self.max_packfile_response_bytes,
            what: "HTTP request body",
            remedy: REQUEST_BODY_REMEDY,
        }
    }

    /// Wall-clock deadline for one body transfer phase, derived from the size
    /// ceiling and the slowest rate served rather than chosen independently.
    ///
    /// Clamped to [`MAX_BODY_TRANSFER_TIMEOUT`] so no combination of configured
    /// ceiling and configured rate can derive a deadline that is not a deadline.
    #[must_use]
    pub fn body_transfer_timeout(&self) -> Duration {
        let rate = self.min_transfer_bytes_per_sec.max(1);
        let derived = Duration::from_secs(self.max_packfile_response_bytes / rate);
        if derived > MAX_BODY_TRANSFER_TIMEOUT {
            MAX_BODY_TRANSFER_TIMEOUT
        } else {
            derived
        }
    }

    /// How many refs [`Self::max_ref_advertisement_bytes`] admits, at the
    /// worst-case bytes-per-ref the ceiling is derived from.
    #[must_use]
    pub fn admitted_refs(&self) -> u64 {
        self.max_ref_advertisement_bytes / REF_ADVERTISEMENT_BYTES_PER_REF
    }
}

/// Read `reader` to end, refusing to buffer more than `ceiling` allows.
///
/// The bounded counterpart of `Read::read_to_end`, which on a socket carrying
/// remote data has no ceiling at all.
pub fn read_to_end_bounded(reader: &mut dyn Read, ceiling: ReadCeiling) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    append_to_end_bounded(reader, &mut buffer, ceiling)?;
    Ok(buffer)
}

/// Append the rest of `reader` to `buffer`, refusing to let `buffer` grow past
/// the ceiling in total.
///
/// The limit covers what `buffer` already holds, so a caller that has read a
/// prefix cannot spend the budget twice.
pub fn append_to_end_bounded(
    reader: &mut dyn Read,
    buffer: &mut Vec<u8>,
    ceiling: ReadCeiling,
) -> Result<()> {
    let buffered = buffer.len() as u64;
    if buffered > ceiling.limit {
        return Err(ceiling.exceeded(buffered));
    }
    // One byte past the limit: enough to tell "exactly at the ceiling" (fine)
    // from "over it" (refused) without reading the overage.
    let headroom = (ceiling.limit - buffered).saturating_add(1);
    (&mut *reader).take(headroom).read_to_end(buffer)?;
    let total = buffer.len() as u64;
    if total > ceiling.limit {
        return Err(ceiling.exceeded(total));
    }
    Ok(())
}
