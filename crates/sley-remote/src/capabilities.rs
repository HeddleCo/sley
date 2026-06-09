//! Capability probes for downstream consumers (e.g. heddle).
//!
//! These flags describe what the *library* can do today so embedders can fail
//! loudly instead of silently falling back to another implementation.

/// Transport and remote-operation capabilities exposed by `sley-remote`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportCapabilities {
    /// `fetch` / `clone` over HTTP(S) smart transport.
    pub http_fetch: bool,
    /// `push` over HTTP(S) receive-pack.
    pub http_push: bool,
    /// Protocol v2 service discovery + `ls-refs` ref advertisements.
    pub http_protocol_v2_discovery: bool,
    /// Protocol v2 `fetch` command (pack negotiation over v2 RPC).
    pub http_protocol_v2_fetch: bool,
    /// Shallow clone/fetch via `--depth` / `deepen`.
    pub shallow_fetch: bool,
    /// `fetch` / `push` / `ls-remote` over SSH upload-pack/receive-pack.
    pub ssh_fetch: bool,
    pub ssh_push: bool,
    /// `clone` over SSH (bare mirror / checkout destination).
    pub ssh_clone: bool,
    /// `fetch` from `.bundle` files (git bundle protocol).
    pub bundle_fetch: bool,
    /// In-process `file://` upload-pack / receive-pack.
    pub local_fetch: bool,
    pub local_push: bool,
    /// Credential-helper subprocess provider ([`CredentialHelperProvider`]).
    pub credential_helper: bool,
    /// Thin-pack generation on push (delta bases omitted from pack body).
    pub thin_pack_push: bool,
    /// SHA-256 object format over smart HTTP (negotiated via `object-format`).
    pub sha256_http: bool,
    /// SHA-256 object format over SSH.
    pub sha256_ssh: bool,
}

/// Flip to `true` once HTTP v2 `fetch` RPC is wired (see `http.rs`).
pub const HTTP_PROTOCOL_V2_FETCH: bool = true;
/// Flip to `true` once `CloneSource::Ssh` lands.
pub const SSH_CLONE_SUPPORTED: bool = true;
/// Flip to `true` once bundle fetch is lifted from the CLI.
pub const BUNDLE_FETCH_SUPPORTED: bool = true;
/// Thin-pack push via [`crate::pack::build_push_packfile`] (`PushPackRequest::thin`).
pub const THIN_PACK_PUSH_SUPPORTED: bool = true;

impl TransportCapabilities {
    /// Capabilities of the currently linked `sley-remote` build.
    pub const fn current() -> Self {
        Self {
            http_fetch: cfg!(feature = "http"),
            http_push: cfg!(feature = "http"),
            http_protocol_v2_discovery: cfg!(feature = "http"),
            http_protocol_v2_fetch: cfg!(feature = "http") && HTTP_PROTOCOL_V2_FETCH,
            shallow_fetch: true,
            ssh_fetch: true,
            ssh_push: true,
            ssh_clone: SSH_CLONE_SUPPORTED,
            bundle_fetch: BUNDLE_FETCH_SUPPORTED,
            local_fetch: true,
            local_push: true,
            credential_helper: true,
            thin_pack_push: THIN_PACK_PUSH_SUPPORTED,
            sha256_http: cfg!(feature = "http"),
            sha256_ssh: true,
        }
    }

    /// Whether native push is available for the given transport class.
    pub fn supports_native_push(&self, transport: RemoteTransportKind) -> bool {
        match transport {
            RemoteTransportKind::Http => self.http_push,
            RemoteTransportKind::Ssh => self.ssh_push,
            RemoteTransportKind::Local => self.local_push,
            RemoteTransportKind::Bundle => false,
        }
    }

    /// Whether shallow fetch/clone is available for the given transport class.
    pub fn supports_shallow(&self, transport: RemoteTransportKind) -> bool {
        match transport {
            RemoteTransportKind::Http | RemoteTransportKind::Ssh => self.shallow_fetch,
            RemoteTransportKind::Local | RemoteTransportKind::Bundle => false,
        }
    }
}

/// Coarse transport classification for capability lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTransportKind {
    Http,
    Ssh,
    Local,
    Bundle,
}

#[cfg(all(test, not(feature = "http")))]
mod tests {
    use super::TransportCapabilities;

    #[test]
    fn ssh_only_build_disables_http_fetch_capability() {
        let caps = TransportCapabilities::current();
        assert!(!caps.http_fetch);
        assert!(!caps.http_push);
        assert!(!caps.http_protocol_v2_discovery);
        assert!(!caps.http_protocol_v2_fetch);
        assert!(!caps.sha256_http);
        assert!(caps.ssh_fetch);
    }
}

impl RemoteTransportKind {
    pub fn from_url_scheme(url: &str) -> Option<Self> {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(Self::Http);
        }
        if url.starts_with("ssh://")
            || url.starts_with("git@")
            || url.contains(':') && !url.contains("://")
        {
            return Some(Self::Ssh);
        }
        if url.starts_with("file://") || url.starts_with('/') || url.starts_with("./") {
            return Some(Self::Local);
        }
        if url.ends_with(".bundle") {
            return Some(Self::Bundle);
        }
        None
    }
}