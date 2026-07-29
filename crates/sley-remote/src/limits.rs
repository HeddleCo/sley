//! Transport-neutral resolution of the bounded-response budgets.

use sley_config::GitConfig;
use sley_protocol::TransportLimits;

/// Resolve the buffered-response ceilings from git config.
///
/// The keys, all optional, all sizes in git's usual `k`/`m`/`g` notation:
///
/// * `sley.maxRefAdvertisementBytes` -- ceiling on a buffered v0/v1 reference
///   advertisement or an unparsed receive-pack response (default 128 MiB,
///   about 512Ki refs). The remedy of first resort for a repository with more
///   refs than that is protocol v2 `ls-refs` with a `ref-prefix`, which sley
///   already uses by default for `git-upload-pack`; this key exists for the
///   paths that cannot, chiefly `git-receive-pack` and servers without v2.
/// * `sley.maxPackfileResponseBytes` -- ceiling on a packfile-bearing response
///   buffered whole (default 4 GiB). The HTTP body deadline is derived from it,
///   so raising it raises the time budget that pays for it.
/// * `sley.minTransferBytesPerSec` -- the slowest average HTTP transfer rate
///   served (default 1 MiB/s), the other half of that derivation.
///
/// Every value is clamped by [`TransportLimits::clamped`]: an unset, zero or
/// unparseable value falls back to the default, and no value can raise a
/// ceiling past `sley_protocol::MAX_CONFIGURABLE_RESPONSE_BYTES` or derive a
/// deadline past `sley_protocol::MAX_BODY_TRANSFER_TIMEOUT`. Configuration
/// moves these bounds; it cannot remove them.
pub fn transport_limits_from_config(config: Option<&GitConfig>) -> TransportLimits {
    let defaults = TransportLimits::default();
    let size = |key: &str, fallback: u64| -> u64 {
        config
            .and_then(|config| config.get("sley", None, key))
            .and_then(sley_config::parse_config_int)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .filter(|bytes| *bytes > 0)
            .unwrap_or(fallback)
    };
    TransportLimits {
        max_ref_advertisement_bytes: size(
            "maxRefAdvertisementBytes",
            defaults.max_ref_advertisement_bytes,
        ),
        max_packfile_response_bytes: size(
            "maxPackfileResponseBytes",
            defaults.max_packfile_response_bytes,
        ),
        min_transfer_bytes_per_sec: size(
            "minTransferBytesPerSec",
            defaults.min_transfer_bytes_per_sec,
        ),
    }
    .clamped()
}
