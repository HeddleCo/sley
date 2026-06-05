//! `git-remote` — callable fetch / push / clone / ls-remote orchestration.
//!
//! This crate lifts the network-transport orchestration out of the `git-cli`
//! monolith so it can be driven as a library (the way a downstream consumer such
//! as heddle needs). The wire codecs ([`sley_protocol`]), the pack encoder
//! ([`sley_pack`]), pack building ([`sley_odb`]) and ref/commit plumbing already
//! live in their own crates; `git-remote` is the glue that sequences them into
//! `fetch`/`push`/`clone`/`ls-remote`, with the CLI-specific concerns (argument
//! parsing, stdout/stderr formatting, exit codes, repository discovery from
//! process-global state) kept out via the seams below:
//!
//! * [`CredentialProvider`] — how authenticated remotes obtain credentials. The
//!   caller injects one (e.g. a credential-helper-backed impl, an interactive
//!   prompt, or [`NoCredentials`] for unauthenticated/public access).
//! * [`ProgressSink`] — where human-facing progress/summary lines go. The
//!   orchestration returns structured outcomes and emits progress through this
//!   sink instead of printing, so the caller controls presentation.
//!
//! The lift proceeds in stages (see `docs/git-remote-extraction.md`); this is
//! the scaffold (stage A).

use std::path::Path;

use sley_config::GitConfig;
use sley_core::{ObjectFormat, Result};
use sley_transport::GitCredential;

mod credentials;
pub use credentials::{
    credential_fill, credential_request_for_url, credential_store, http_credential_host,
    http_protocol_name, http_url_credential, CredentialHelperProvider,
};

mod http;
pub use http::{
    http_advertised_refs, http_authorization_headers, http_check_status, http_send_with_auth,
    http_service_advertisements, http_upload_pack_advertisements, http_upload_pack_fetch_response,
    http_validate_content_type, install_fetch_pack_via_http_upload_pack, new_http_client,
    remote_url_is_http,
};

mod local;
pub use local::{
    attach_receive_pack_capabilities, attach_upload_pack_capabilities,
    install_fetch_pack_via_local_upload_pack, local_fetch_advertisements, local_have_oids,
    receive_pack_features, receive_pack_into_local_repository,
    receive_pack_request_uses_push_options, upload_pack_features,
    upload_pack_from_local_repository, upload_pack_request_uses_sideband,
    upload_pack_sideband_response,
};

mod fetch;
pub use fetch::{
    append_reachable_auto_follow_tags, apply_configured_fetch_prune_option,
    apply_configured_remote_tag_option, fetch, fetch_head_source_description,
    fetch_refspec_excludes, fetch_refspecs_for_source, mark_tag_refspec_updates_not_for_merge,
    order_bundle_fetch_all_tags_updates, prune_remote_tracking_refs_from_advertisements,
    retain_missing_auto_follow_tags, write_default_fetch_head, write_fetch_head,
    write_fetch_head_records, FetchOptions, FetchOutcome, FetchSource, PrunedRef,
};

mod push;
pub use push::{
    local_push_source_refs, normalize_push_refname, normalize_push_refspec, push,
    reject_non_fast_forward_pushes, remote_advertisement_tips_known_to_local,
    validate_receive_pack_report, PushDestination, PushOptions, PushOutcome,
};

/// The object format of the repository whose common `$GIT_DIR` is `common_git_dir`.
///
/// Reads `common_git_dir/config`'s `extensions.objectFormat`, defaulting to
/// SHA-1 when the config is absent or unreadable (matching git). `common_git_dir`
/// must already be the common git dir; this does no worktree resolution.
pub fn object_format_for_git_dir(common_git_dir: &Path) -> Result<ObjectFormat> {
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(ObjectFormat::Sha1);
    };
    config.repository_object_format()
}

/// Supplies credentials for an authenticated remote, mirroring git's credential
/// protocol: [`fill`](CredentialProvider::fill) is handed a partial
/// [`GitCredential`] describing the request (protocol/host/path) and returns a
/// completed credential, or `None` to proceed unauthenticated.
///
/// [`approve`](CredentialProvider::approve) / [`reject`](CredentialProvider::reject)
/// let a backing store remember or forget a credential after the request
/// succeeds or fails; the default no-ops suit providers without a store.
pub trait CredentialProvider {
    /// Complete `request` into a usable credential, or return `None` to attempt
    /// the request without authentication.
    fn fill(&mut self, request: GitCredential) -> Result<Option<GitCredential>>;

    /// Record `credential` as having worked (e.g. store it). Default: no-op.
    fn approve(&mut self, _credential: &GitCredential) -> Result<()> {
        Ok(())
    }

    /// Record `credential` as having failed (e.g. erase it). Default: no-op.
    fn reject(&mut self, _credential: &GitCredential) -> Result<()> {
        Ok(())
    }
}

/// A [`CredentialProvider`] that never supplies credentials, so every request is
/// attempted unauthenticated. This is what an embedder targeting public remotes
/// (e.g. heddle) uses to suppress prompts.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCredentials;

impl CredentialProvider for NoCredentials {
    fn fill(&mut self, _request: GitCredential) -> Result<Option<GitCredential>> {
        Ok(None)
    }
}

/// Receives human-facing progress and summary events from an operation (the
/// "To <remote>" push summary, prune notices, "Cloning into…", etc.). The
/// orchestration returns structured outcomes regardless; this is purely for
/// presentation, so the default implementations discard everything.
pub trait ProgressSink {
    /// A free-form progress or summary line.
    fn message(&mut self, _message: &str) {}
}

/// A [`ProgressSink`] that discards every event.
#[derive(Debug, Default, Clone, Copy)]
pub struct SilentProgress;

impl ProgressSink for SilentProgress {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_credentials_never_fills() {
        let mut provider = NoCredentials;
        let request = GitCredential::default();
        assert!(provider.fill(request).unwrap().is_none());
    }

    #[test]
    fn silent_progress_accepts_messages() {
        let mut progress = SilentProgress;
        progress.message("Cloning into 'x'...");
    }
}
