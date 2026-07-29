//! Remote transport orchestration for embedders (e.g. heddle).
//!
//! This module re-exports [`sley_remote`] and adds [`RemoteContext`], which binds a
//! [`Repository`] to a remote name and resolves fetch/push URLs and transport
//! sources using the repository's effective configuration.
//!
//! Prefer the thin facade entry points:
//!
//! * [`clone_repository`] — destination clone (re-export of [`sley_remote::clone`])
//! * [`OperationContext`] — progress + cancel bundle for transfer helpers
//! * [`Repository::fetch_with_cancel`] / [`Repository::push_with_cancel`] /
//!   [`Repository::push_actions_with_cancel`] — cooperative cancel of pack work
//!
//! Cancel primitives ([`AtomicCancel`], [`CancelFlag`], [`CancellableRead`],
//! [`DynCancelFlag`], `Never`) are re-exported from `sley_remote` / `sley_core`
//! and also at the crate root of `sley`.

use std::path::Path;

pub use sley_remote::*;

use sley_config::GitConfig;

use crate::{Repository, Result};

/// Stable semver for the integration-facing API surface (crate version tracks this).
pub const INTEGRATION_API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Clone a resolved remote into a fresh repository.
///
/// Thin free-function entry point for embedders: identical to
/// [`sley_remote::clone`]. Callers fully resolve [`CloneRequest`] (destination,
/// format, [`CloneSource`]) and inject configure/credentials/progress/cancel
/// via [`CloneServices`].
///
/// For repository-bound fetch/push after open, use [`Repository::fetch`] /
/// [`Repository::push`] (and their `*_with_cancel` variants).
pub fn clone_repository(
    request: CloneRequest<'_>,
    services: CloneServices<'_>,
) -> Result<CloneOutcome> {
    clone(request, services)
}

/// A repository-bound remote: resolves URLs and transport sources from config.
#[derive(Debug, Clone)]
pub struct RemoteContext {
    remote: String,
    config: GitConfig,
}

impl RemoteContext {
    /// Resolve `remote` against `repo`'s effective configuration.
    ///
    /// `remote` may be a configured remote name (`origin`) or a literal URL/path.
    pub fn for_remote(repo: &Repository, remote: impl Into<String>) -> Result<Self> {
        Ok(Self {
            remote: remote.into(),
            config: repo.config_snapshot()?,
        })
    }

    /// The remote name or source string this context was built from.
    pub fn name(&self) -> &str {
        &self.remote
    }

    /// Effective configuration used for URL rewriting and fetch/push options.
    pub fn config(&self) -> &GitConfig {
        &self.config
    }

    /// Transport capabilities of the linked `sley-remote` build.
    pub fn transport_capabilities(&self) -> sley_remote::TransportCapabilities {
        sley_remote::TransportCapabilities::current()
    }

    /// Rewritten fetch URL (`remote.<name>.url` + `url.*.insteadOf`).
    pub fn fetch_url(&self) -> String {
        sley_remote::fetch_url(&self.config, &self.remote)
    }

    /// Rewritten push URL (`pushurl` preferred, then `pushInsteadOf`).
    pub fn push_url(&self) -> String {
        sley_remote::push_url(&self.config, &self.remote)
    }

    /// Coarse transport class for the fetch URL.
    pub fn fetch_transport_kind(&self) -> Result<Option<sley_remote::RemoteTransportKind>> {
        sley_remote::transport_kind_for_url(&self.fetch_url())
    }

    /// Coarse transport class for the push URL.
    pub fn push_transport_kind(&self) -> Result<Option<sley_remote::RemoteTransportKind>> {
        sley_remote::transport_kind_for_url(&self.push_url())
    }

    /// [`FetchSource`] for [`sley_remote::fetch`], using this repository as the
    /// relative base for local paths.
    pub fn fetch_source(&self, repo: &Repository) -> Result<sley_remote::FetchSource> {
        let (_, source) = sley_remote::resolve_fetch_source(
            &self.config,
            &self.remote,
            &repo.remote_relative_base(),
        )?;
        Ok(source)
    }

    /// [`PushDestination`] for [`sley_remote::push`].
    pub fn push_destination(&self, repo: &Repository) -> Result<sley_remote::PushDestination> {
        let (_, destination) = sley_remote::resolve_push_destination(
            &self.config,
            &self.remote,
            &repo.remote_relative_base(),
        )?;
        Ok(destination)
    }
}

impl Repository {
    /// Open a [`RemoteContext`] for `remote` using this repository's config.
    pub fn remote(&self, remote: impl Into<String>) -> Result<RemoteContext> {
        RemoteContext::for_remote(self, remote)
    }

    /// Fetch from `remote` (name or URL), installing objects and updating refs.
    pub fn fetch(
        &self,
        remote: impl Into<String>,
        refspecs: &[String],
        options: FetchOptions,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
    ) -> Result<FetchOutcome> {
        self.fetch_with_cancel(
            remote,
            refspecs,
            options,
            credentials,
            progress,
            sley_core::CancelFlag::never(),
        )
    }

    /// [`Self::fetch`] with cooperative cancellation of pack receive/index.
    ///
    /// Pass `CancelFlag::new(&atomic)` (or any `DynCancelFlag`) so a UI stop or
    /// SIGINT handler can trip [`sley_core::GitError::Cancelled`] mid-transfer without
    /// corrupting the object database.
    pub fn fetch_with_cancel(
        &self,
        remote: impl Into<String>,
        refspecs: &[String],
        options: FetchOptions,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
        cancel: sley_core::DynCancelFlag<'_>,
    ) -> Result<FetchOutcome> {
        self.fetch_with_http_client_and_cancel(
            remote,
            refspecs,
            options,
            credentials,
            progress,
            None,
            cancel,
        )
    }

    /// Like [`Repository::fetch`], with a caller-provided smart-HTTP client.
    pub fn fetch_with_http_client(
        &self,
        remote: impl Into<String>,
        refspecs: &[String],
        options: FetchOptions,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
        http_client: Option<&dyn HttpClient>,
    ) -> Result<FetchOutcome> {
        self.fetch_with_http_client_and_cancel(
            remote,
            refspecs,
            options,
            credentials,
            progress,
            http_client,
            sley_core::CancelFlag::never(),
        )
    }

    /// Like [`Repository::fetch_with_http_client`], with cooperative cancellation.
    #[allow(clippy::too_many_arguments)]
    pub fn fetch_with_http_client_and_cancel(
        &self,
        remote: impl Into<String>,
        refspecs: &[String],
        options: FetchOptions,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
        http_client: Option<&dyn HttpClient>,
        cancel: sley_core::DynCancelFlag<'_>,
    ) -> Result<FetchOutcome> {
        let ctx = self.remote(remote)?;
        let source = ctx.fetch_source(self)?;
        let outcome = fetch_with_http_client(
            FetchRequest {
                git_dir: self.git_dir(),
                format: self.object_format(),
                config: ctx.config(),
                remote_name: ctx.name(),
                source: &source,
                refspecs,
                options: &options,
                validation: None,
            },
            FetchServices {
                credentials,
                progress,
                ref_hook: None,
                cancel,
            },
            http_client,
        )?;
        self.refresh_objects();
        Ok(outcome)
    }

    /// Push `refspecs` to `remote` (name or URL).
    pub fn push(
        &self,
        remote: impl Into<String>,
        refspecs: &[String],
        options: PushOptions,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
    ) -> Result<PushOutcome> {
        self.push_with_cancel(
            remote,
            refspecs,
            options,
            credentials,
            progress,
            sley_core::CancelFlag::never(),
        )
    }

    /// [`Self::push`] with cooperative cancellation of pack generation.
    ///
    /// Pass `CancelFlag::new(&atomic)` (or any `DynCancelFlag`) so a UI stop or
    /// SIGINT handler can trip [`sley_core::GitError::Cancelled`] while building
    /// the push packfile.
    pub fn push_with_cancel(
        &self,
        remote: impl Into<String>,
        refspecs: &[String],
        options: PushOptions,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
        cancel: sley_core::DynCancelFlag<'_>,
    ) -> Result<PushOutcome> {
        let ctx = self.remote(remote)?;
        let destination = ctx.push_destination(self)?;
        push(
            PushRequest {
                git_dir: self.git_dir(),
                common_git_dir: self.common_dir(),
                format: self.object_format(),
                config: ctx.config(),
                remote: ctx.name(),
                destination: &destination,
                refspecs,
                options: &options,
            },
            PushServices {
                credentials,
                progress,
                cancel,
            },
        )
    }

    /// Push a caller-authored exact old/new/delete plan to `remote`.
    pub fn push_actions(
        &self,
        remote: impl Into<String>,
        plan: PushActionPlan,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
    ) -> Result<PushOutcome> {
        self.push_actions_with_cancel(
            remote,
            plan,
            credentials,
            progress,
            sley_core::CancelFlag::never(),
        )
    }

    /// [`Self::push_actions`] with cooperative cancellation of pack generation.
    pub fn push_actions_with_cancel(
        &self,
        remote: impl Into<String>,
        plan: PushActionPlan,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
        cancel: sley_core::DynCancelFlag<'_>,
    ) -> Result<PushOutcome> {
        self.push_actions_with_http_client_and_cancel(
            remote,
            plan,
            credentials,
            progress,
            None,
            cancel,
        )
    }

    /// Like [`Repository::push_actions`], with a caller-provided smart-HTTP client.
    pub fn push_actions_with_http_client(
        &self,
        remote: impl Into<String>,
        plan: PushActionPlan,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
        http_client: Option<&dyn HttpClient>,
    ) -> Result<PushOutcome> {
        self.push_actions_with_http_client_and_cancel(
            remote,
            plan,
            credentials,
            progress,
            http_client,
            sley_core::CancelFlag::never(),
        )
    }

    /// Like [`Repository::push_actions_with_http_client`], with cooperative cancellation.
    #[allow(clippy::too_many_arguments)]
    pub fn push_actions_with_http_client_and_cancel(
        &self,
        remote: impl Into<String>,
        plan: PushActionPlan,
        credentials: &mut dyn CredentialProvider,
        progress: &mut dyn ProgressSink,
        http_client: Option<&dyn HttpClient>,
        cancel: sley_core::DynCancelFlag<'_>,
    ) -> Result<PushOutcome> {
        let ctx = self.remote(remote)?;
        let destination = ctx.push_destination(self)?;
        push_actions_with_http_client(
            PushActionRequest {
                git_dir: self.git_dir(),
                common_git_dir: self.common_dir(),
                format: self.object_format(),
                config: ctx.config(),
                remote: ctx.name(),
                destination: &destination,
                plan: &plan,
            },
            PushServices {
                credentials,
                progress,
                cancel,
            },
            http_client,
        )
    }

    /// List refs advertised by `remote` (name or URL).
    pub fn ls_remote(
        &self,
        remote: impl Into<String>,
        filter: LsRemoteFilter,
        matches: &dyn Fn(&str) -> bool,
        credentials: &mut dyn CredentialProvider,
    ) -> Result<Vec<LsRemoteRecord>> {
        self.ls_remote_with_http_client(remote, filter, matches, credentials, None)
    }

    /// Like [`Repository::ls_remote`], with a caller-provided smart-HTTP client.
    pub fn ls_remote_with_http_client(
        &self,
        remote: impl Into<String>,
        filter: LsRemoteFilter,
        matches: &dyn Fn(&str) -> bool,
        credentials: &mut dyn CredentialProvider,
        http_client: Option<&dyn HttpClient>,
    ) -> Result<Vec<LsRemoteRecord>> {
        let ctx = self.remote(remote)?;
        let source = ctx.fetch_source(self)?;
        let ls_source = match source {
            FetchSource::Http(url) => LsRemoteSource::Http(url),
            FetchSource::Ssh(url) => LsRemoteSource::Ssh(url),
            FetchSource::Git { remote, .. } => LsRemoteSource::Git(remote),
            FetchSource::Local { git_dir, .. } => LsRemoteSource::Local { git_dir },
        };
        Ok(ls_remote_with_http_client(
            LsRemoteRequest {
                source: &ls_source,
                format: self.object_format(),
                filter: &filter,
                config: Some(ctx.config()),
            },
            matches,
            credentials,
            http_client,
        )?
        .records)
    }

    /// Directory relative local remote paths resolve against: working tree root,
    /// or the parent of `.git` for a bare repository, or `git_dir` as fallback.
    pub(crate) fn remote_relative_base(&self) -> std::path::PathBuf {
        self.workdir().unwrap_or_else(|| {
            if self
                .git_dir()
                .file_name()
                .is_some_and(|name| name == ".git")
            {
                self.git_dir()
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| self.git_dir().to_path_buf())
            } else {
                self.git_dir().to_path_buf()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Repository;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sley-remote-ctx-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn remote_context_resolves_configured_origin() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.0).expect("init");
        repo.add_remote("origin", "https://example.invalid/o.git")
            .expect("add remote");
        let ctx = repo.remote("origin").expect("context");
        assert_eq!(ctx.fetch_url(), "https://example.invalid/o.git");
        assert_eq!(
            ctx.fetch_transport_kind().expect("kind"),
            Some(sley_remote::RemoteTransportKind::Http)
        );
        assert!(ctx.transport_capabilities().http_protocol_v2_fetch);
        assert!(ctx.transport_capabilities().ssh_fetch);
        assert!(ctx.transport_capabilities().thin_pack_push);
    }
}
