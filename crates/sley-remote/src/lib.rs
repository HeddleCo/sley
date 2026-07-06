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

mod install;
pub use install::{
    install_protocol_v2_fetch_promisor_response_from_reader,
    install_protocol_v2_fetch_response_from_reader,
    install_upload_pack_packfile_promisor_response_from_reader,
    install_upload_pack_packfile_response_from_reader,
    install_upload_pack_raw_promisor_response_from_reader,
    install_upload_pack_raw_response_from_reader,
    install_upload_pack_shallow_packfile_promisor_response_from_reader,
    install_upload_pack_shallow_packfile_response_from_reader,
    install_upload_pack_shallow_raw_promisor_response_from_reader,
    install_upload_pack_shallow_raw_response_from_reader,
    shallow_info_from_protocol_v2_fetch_header,
};

mod credentials;
pub use credentials::{
    CredentialHelperProvider, credential_fill, credential_request_for_url, credential_store,
    http_credential_host, http_protocol_name, http_url_credential,
};

#[cfg(feature = "http")]
mod http;
#[cfg(feature = "http")]
pub use http::{
    HttpFetchPackRequest, HttpServiceAdvertisements, http_advertised_refs,
    http_authorization_headers, http_check_status, http_protocol_v2_fetch_response,
    http_send_with_auth, http_service_advertisements, http_upload_pack_advertisements,
    http_upload_pack_features, http_validate_content_type,
    install_fetch_pack_via_http_protocol_v2_fetch, install_fetch_pack_via_http_upload_pack,
    new_http_client, HttpOperationBatch,
    remote_url_is_http,
};

mod ssh;
pub use ssh::{
    SshFetchPackRequest, SshTransportOptions, install_fetch_pack_via_ssh_upload_pack, ssh_program,
    ssh_transport_options_from_config, ssh_upload_pack_advertisements,
    ssh_upload_pack_advertisements_with_options,
};

mod git;
mod git_proxy;
pub use git::{
    GitFetchPackRequest, git_upload_pack_advertisements,
    git_upload_pack_advertisements_with_protocol, install_fetch_pack_via_git_upload_pack,
};

mod local;
pub use local::{
    INFINITE_DEPTH, LocalDeepenPlan, attach_receive_pack_capabilities,
    attach_upload_pack_capabilities, compute_local_deepen, compute_local_deepen_by_rev_list,
    install_fetch_pack_via_local_upload_pack, local_fetch_advertisements, local_have_oids,
    receive_pack_features, receive_pack_into_local_repository,
    receive_pack_request_uses_push_options, receive_pack_stream_into_local_repository,
    serve_upload_pack_v2, serve_upload_pack_v2_with_config, upload_pack_features,
    upload_pack_from_local_repository, upload_pack_request_uses_sideband,
    upload_pack_sideband_response,
};

mod filter;
pub use filter::{pack_filter_from_spec, pack_filter_from_spec_for_clone};

mod fetch;
pub use fetch::{
    FetchOptions, FetchOutcome, FetchRequest, FetchServices, FetchSource, PruneRefsInput,
    PrunedRef, append_reachable_auto_follow_tags, apply_configured_fetch_prune_option,
    apply_configured_partial_clone_filter, apply_configured_remote_tag_option, fetch,
    fetch_head_source_description,
    fetch_refspec_excludes, fetch_refspecs_for_source, mark_tag_refspec_updates_not_for_merge,
    order_bundle_fetch_all_tags_updates, prune_refs_from_advertisements,
    retain_missing_auto_follow_tags, write_default_fetch_head, write_fetch_head,
    write_fetch_head_records,
};

mod pack;
pub use pack::{
    PushPackRequest, build_push_packfile, build_receive_pack_body,
    remote_advertisement_tips_known_to_local,
};

mod push;
pub use push::{
    PushAction, PushActionPlan, PushActionRequest, PushCommand, PushDestination, PushOptions,
    PushOutcome, PushPlan, PushQuarantine, PushRefStatus, PushReportRef, PushReportRequest,
    PushRequest, PushServices, PushStatusReport, PushThinMode, execute_push_action_plan,
    execute_push_plan, local_push_source_refs, normalize_push_refname, normalize_push_refspec,
    plan_push, plan_push_actions, push, push_actions, push_local_with_report,
    push_url_for_display, reject_non_fast_forward_pushes, stage_local_push_quarantine,
    validate_receive_pack_report,
};

mod ls_remote;
pub use ls_remote::{LsRemoteFilter, LsRemoteRecord, LsRemoteSource, ls_remote};

mod clone;
pub use clone::{CloneOptions, CloneOutcome, CloneRequest, CloneServices, CloneSource, clone};

mod bundle;
pub use bundle::{FetchBundleRequest, fetch_bundle};
mod bundle_uri;
pub use bundle_uri::{
    BundleUriEntry, BundleUriList, bundle_uri_fetch_order, handshake_advertises_bundle_uri,
    http_remote_bundle_uri_list, parse_bundle_uri_line, prefetch_advertised_bundle_uris,
    transfer_bundle_uri_enabled,
};

mod shallow;
pub use shallow::{apply_shallow_info, read_shallow, write_shallow};

mod capabilities;
pub use capabilities::{RemoteTransportKind, TransportCapabilities};

mod protocol;
pub use protocol::{
    TransportPolicyError, check_transport_allowed, is_transport_allowed,
    transport_scheme_for_remote, transport_scheme_for_url,
};

mod resolve;
pub use resolve::{
    fetch_source_for_url, fetch_url, push_destination_for_url, push_url, resolve_fetch_source,
    resolve_push_destination, transport_kind_for_url,
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use sley_config::{ConfigEntry, ConfigSection};
    use sley_formats::RepositoryLayout;
    use sley_object::{Commit, EncodedObject, ObjectType, Tree};
    use sley_odb::{FileObjectDatabase, ObjectWriter};
    use sley_refs::{FileRefStore, RefTarget, RefUpdate};
    use sley_transport::{RemoteUrl, parse_remote_url};

    #[test]
    fn no_credentials_never_fills() {
        let mut provider = NoCredentials;
        let request = GitCredential::default();
        assert!(
            provider
                .fill(request)
                .expect("test operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn silent_progress_accepts_messages() {
        let mut progress = SilentProgress;
        progress.message("Cloning into 'x'...");
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn live_env(name: &str) -> Option<String> {
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => Some(value),
            _ => None,
        }
    }

    fn live_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sley-remote-live-{name}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        RepositoryLayout::init_at(&dir, ObjectFormat::Sha1, false)
            .expect("live test repository should initialize");
        dir.join(".git")
    }

    fn remote_config(url: &str) -> GitConfig {
        GitConfig {
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin".into()),
                vec![
                    ConfigEntry::new("url", Some(url.into())),
                    ConfigEntry::new("fetch", Some("+refs/heads/*:refs/remotes/origin/*".into())),
                ],
            )],
            ..GitConfig::default()
        }
    }

    fn fetch_options(depth: Option<u32>) -> FetchOptions {
        FetchOptions {
            quiet: true,
            auto_follow_tags: false,
            fetch_all_tags: false,
            prune: false,
            prune_tags: false,
            dry_run: false,
            force: false,
            append: false,
            write_fetch_head: true,
            tag_option_explicit: true,
            prune_option_explicit: true,
            prune_tags_option_explicit: true,
            refmap: None,
            depth,
            merge_srcs: Vec::new(),
            filter: None,
            refetch: false,
            cloning: false,
            record_promisor_refs: true,
            update_shallow: false,
            reject_shallow: false,
            deepen_relative: false,
            update_head_ok: false,
            deepen_since: None,
            deepen_not: Vec::new(),
            ssh_options: None,
            atomic: false,
        }
    }

    fn write_live_commit(git_dir: &Path, branch: &str) {
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree { entries: vec![] }.write(),
            ))
            .expect("live commit tree should write");
        let timestamp = 1 + TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let identity =
            format!("Sley Remote Live <sley@example.invalid> {timestamp} +0000").into_bytes();
        let oid = db
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree,
                    parents: Vec::new(),
                    author: identity.clone(),
                    committer: identity,
                    encoding: None,
                    message: format!("sley remote live {branch}\n").into_bytes(),
                }
                .write(),
            ))
            .expect("live commit should write");
        let store = FileRefStore::new(git_dir, format);
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: format!("refs/heads/{branch}"),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic(format!("refs/heads/{branch}")),
            reflog: None,
        });
        tx.commit().expect("live refs should update");
    }

    struct EnvCredentials {
        username: String,
        password: String,
    }

    impl CredentialProvider for EnvCredentials {
        fn fill(&mut self, mut request: GitCredential) -> Result<Option<GitCredential>> {
            request.username = Some(self.username.clone());
            request.password = Some(self.password.clone());
            Ok(Some(request))
        }
    }

    fn live_fetch(
        url_var: &str,
        branch_var: &str,
        source: FetchSource,
        credentials: &mut dyn CredentialProvider,
        depth: Option<u32>,
    ) {
        let Some(url) = live_env(url_var) else {
            return;
        };
        let branch = live_env(branch_var).unwrap_or_else(|| "main".into());
        let local = live_repo(url_var);
        let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
        let config = remote_config(&url);
        let options = fetch_options(depth);
        let mut progress = SilentProgress;

        let outcome = fetch(
            FetchRequest {
                git_dir: &local,
                format: ObjectFormat::Sha1,
                config: &config,
                remote_name: "origin",
                source: &source,
                refspecs: &[refspec],
                options: &options,
            },
            FetchServices {
                credentials,
                progress: &mut progress,
                ref_hook: None,
            },
        )
        .expect("live fetch should succeed");

        assert!(!outcome.ref_updates.is_empty());
        if depth.is_some() {
            assert!(
                local.join("shallow").exists(),
                "shallow fetch should write .git/shallow"
            );
        }
    }

    fn live_push(
        url_var: &str,
        branch_prefix_var: &str,
        destination: PushDestination,
        credentials: &mut dyn CredentialProvider,
    ) {
        let Some(_) = live_env(url_var) else {
            return;
        };
        let branch_prefix =
            live_env(branch_prefix_var).unwrap_or_else(|| "sley-remote-live".into());
        let branch = format!(
            "{branch_prefix}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let local = live_repo(url_var);
        write_live_commit(&local, &branch);
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let options = PushOptions {
            quiet: true,
            force: false,
            thin: PushThinMode::Auto,
        };
        let mut progress = SilentProgress;

        let outcome = push(
            PushRequest {
                git_dir: &local,
                common_git_dir: &local,
                format: ObjectFormat::Sha1,
                config: &GitConfig::default(),
                remote: "origin",
                destination: &destination,
                refspecs: &[refspec],
                options: &options,
            },
            PushServices {
                credentials,
                progress: &mut progress,
            },
        )
        .expect("live push should succeed");

        assert_eq!(outcome.commands.len(), 1);
    }

    #[test]
    fn live_github_https_public_fetch() {
        let Some(url) = live_env("SLEY_REMOTE_LIVE_GITHUB_HTTPS_PUBLIC_URL") else {
            return;
        };
        let remote = parse_remote_url(&url).expect("live HTTPS URL should parse");
        let mut credentials = NoCredentials;
        live_fetch(
            "SLEY_REMOTE_LIVE_GITHUB_HTTPS_PUBLIC_URL",
            "SLEY_REMOTE_LIVE_GITHUB_HTTPS_PUBLIC_BRANCH",
            FetchSource::Http(remote),
            &mut credentials,
            None,
        );
    }

    #[test]
    fn live_private_https_auth_fetch_uses_credential_provider() {
        let (Some(url), Some(username), Some(password)) = (
            live_env("SLEY_REMOTE_LIVE_PRIVATE_HTTPS_URL"),
            live_env("SLEY_REMOTE_LIVE_PRIVATE_HTTPS_USERNAME"),
            live_env("SLEY_REMOTE_LIVE_PRIVATE_HTTPS_PASSWORD"),
        ) else {
            return;
        };
        let remote = parse_remote_url(&url).expect("live private HTTPS URL should parse");
        let mut credentials = EnvCredentials { username, password };
        live_fetch(
            "SLEY_REMOTE_LIVE_PRIVATE_HTTPS_URL",
            "SLEY_REMOTE_LIVE_PRIVATE_HTTPS_BRANCH",
            FetchSource::Http(remote),
            &mut credentials,
            None,
        );
    }

    #[test]
    fn live_https_push() {
        let Some(url) = live_env("SLEY_REMOTE_LIVE_HTTPS_PUSH_URL") else {
            return;
        };
        let remote = parse_remote_url(&url).expect("live HTTPS push URL should parse");
        let mut no_credentials;
        let mut env_credentials;
        let credentials: &mut dyn CredentialProvider = match (
            live_env("SLEY_REMOTE_LIVE_HTTPS_PUSH_USERNAME"),
            live_env("SLEY_REMOTE_LIVE_HTTPS_PUSH_PASSWORD"),
        ) {
            (Some(username), Some(password)) => {
                env_credentials = EnvCredentials { username, password };
                &mut env_credentials
            }
            _ => {
                no_credentials = NoCredentials;
                &mut no_credentials
            }
        };
        live_push(
            "SLEY_REMOTE_LIVE_HTTPS_PUSH_URL",
            "SLEY_REMOTE_LIVE_HTTPS_PUSH_BRANCH_PREFIX",
            PushDestination::Http(remote),
            credentials,
        );
    }

    #[test]
    fn live_ssh_fetch() {
        let Some(url) = live_env("SLEY_REMOTE_LIVE_SSH_FETCH_URL") else {
            return;
        };
        let remote = parse_remote_url(&url).expect("live SSH fetch URL should parse");
        let mut credentials = NoCredentials;
        live_fetch(
            "SLEY_REMOTE_LIVE_SSH_FETCH_URL",
            "SLEY_REMOTE_LIVE_SSH_FETCH_BRANCH",
            FetchSource::Ssh(remote),
            &mut credentials,
            None,
        );
    }

    #[test]
    fn live_ssh_push() {
        let Some(url) = live_env("SLEY_REMOTE_LIVE_SSH_PUSH_URL") else {
            return;
        };
        let remote = parse_remote_url(&url).expect("live SSH push URL should parse");
        let mut credentials = NoCredentials;
        live_push(
            "SLEY_REMOTE_LIVE_SSH_PUSH_URL",
            "SLEY_REMOTE_LIVE_SSH_PUSH_BRANCH_PREFIX",
            PushDestination::Ssh(remote),
            &mut credentials,
        );
    }

    #[test]
    fn live_shallow_https_fetch_and_clone() {
        let Some(url) = live_env("SLEY_REMOTE_LIVE_SHALLOW_HTTPS_URL") else {
            return;
        };
        let branch =
            live_env("SLEY_REMOTE_LIVE_SHALLOW_HTTPS_BRANCH").unwrap_or_else(|| "main".into());
        let remote = parse_remote_url(&url).expect("live shallow HTTPS URL should parse");
        let mut credentials = NoCredentials;
        live_fetch(
            "SLEY_REMOTE_LIVE_SHALLOW_HTTPS_URL",
            "SLEY_REMOTE_LIVE_SHALLOW_HTTPS_BRANCH",
            FetchSource::Http(remote.clone()),
            &mut credentials,
            Some(1),
        );

        let destination = std::env::temp_dir().join(format!(
            "sley-remote-live-clone-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&destination);
        let config = remote_config(&url);
        let mut configure = |_git_dir: &Path| Ok(config.clone());
        let mut configure_branch = |_git_dir: &Path, _branch: &str| Ok(config.clone());
        let options = CloneOptions {
            origin: "origin",
            checkout_branch: &branch,
            remote_head_branch: &branch,
            single_branch: true,
            depth: Some(1),
            deepen_since: None,
            deepen_not: Vec::new(),
            committer: b"Sley Remote Live <sley@example.invalid> 1 +0000".to_vec(),
            detached_head: None,
            checkout: true,
            filter: None,
            // The live test clones a specific branch via --single-branch, so the
            // branch was explicitly requested (a missing remote tip is a hard error).
            branch_explicit: true,
            ref_storage: sley_formats::RefStorageFormat::Files,
            ssh_options: None,
            reject_shallow: false,
        };
        let mut clone_credentials = NoCredentials;
        let mut progress = SilentProgress;

        let outcome = clone(
            CloneRequest {
                destination: &destination,
                git_dir_override: None,
                core_worktree: None,
                format: ObjectFormat::Sha1,
                source: &CloneSource::Http(RemoteUrl { ..remote }),
                options: &options,
            },
            CloneServices {
                configure: &mut configure,
                configure_branch: &mut configure_branch,
                credentials: &mut clone_credentials,
                progress: &mut progress,
            },
        )
        .expect("live shallow HTTPS clone should succeed");

        assert!(outcome.git_dir.join("shallow").exists());
    }
}
