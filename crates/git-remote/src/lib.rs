//! `git-remote` — callable fetch / push / clone / ls-remote orchestration.
//!
//! This crate lifts the network-transport orchestration out of the `git-cli`
//! monolith so it can be driven as a library (the way a downstream consumer such
//! as heddle needs). The wire codecs ([`git_protocol`]), the pack encoder
//! ([`git_pack`]), pack building ([`git_odb`]) and ref/commit plumbing already
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

use git_core::Result;
use git_transport::GitCredential;

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
