//! Callable fetch orchestration for HTTP(S) and local (`file://`/path) remotes.
//!
//! [`fetch`] sequences the moved transport plumbing ([`crate::http`],
//! [`crate::local`]) and the protocol codecs ([`sley_protocol`]) into the full
//! fetch flow: it advertises refs, plans the ref-map for the requested refspecs,
//! installs the packfile, writes `FETCH_HEAD`, applies the remote-tracking ref
//! updates, and prunes stale tracking refs. Everything is taken as explicit
//! parameters — `git_dir`, the [`ObjectFormat`], the repository [`GitConfig`],
//! the already-resolved remote, and the seam objects ([`CredentialProvider`],
//! [`ProgressSink`]) — so it never reads process-global state, parses arguments,
//! or prints. Human-facing prune notices go through the [`ProgressSink`]; the
//! structured result (applied updates, pruned refs, the remote `HEAD` symref)
//! comes back in [`FetchOutcome`] for the caller to format.
//!
//! Bundle fetch lives in [`crate::bundle`]; SSH uses the dispatch below. The ref-map
//! / `FETCH_HEAD` / prune helpers are shared so there is a single implementation.

use crate::local::LocalDeepenPlan;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sley_config::GitConfig;
use sley_config::remotes::{remote_config_values, remote_exists, rewrite_url_with_config};
use sley_core::{GitError, ObjectFormat, ObjectId, Result, redact_url_for_display};
use sley_odb::{
    FileObjectDatabase, ObjectReader, collect_reachable_object_ids,
    collect_reachable_object_ids_excluding,
};
#[cfg(feature = "http")]
use sley_protocol::ProtocolVersion;
use sley_protocol::{
    FetchHeadRecord, FetchRefUpdate, RefAdvertisement, RefSpec, encode_fetch_head,
    fetch_ref_updates_to_fetch_head, parse_refspec, plan_fetch_ref_updates, refname_matches,
    refspec_map_source,
};
use sley_refs::{FileRefStore, Ref, RefTarget, RefUpdate, ReflogEntry};
use sley_transport::{RemoteTransport, RemoteUrl};

use crate::{CredentialProvider, ProgressSink};

/// How a fetch obtains refs and objects from the remote.
///
/// The caller resolves the remote (URL rewriting, repository discovery — all
/// process-state dependent) and hands `fetch` a concrete transport.
pub enum FetchSource {
    /// A smart-HTTP(S) remote at the given already-resolved URL.
    Http(RemoteUrl),
    /// An SSH remote at the given already-resolved URL. Fetched by spawning `ssh`
    /// (the credential seam is unused — the `ssh` program owns authentication).
    Ssh(RemoteUrl),
    /// A native anonymous `git://` remote at the given already-resolved URL.
    Git {
        remote: RemoteUrl,
        protocol_v2: bool,
    },
    /// A local repository served in-process from `git_dir`.
    Local {
        /// The remote repository's `$GIT_DIR`.
        git_dir: PathBuf,
        /// The remote repository's common `$GIT_DIR` (object format source).
        common_git_dir: PathBuf,
    },
}

/// Controls for a [`fetch`] run, mirroring the `git fetch` flags the CLI parses.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Suppress prune notices (deletions still happen; only the [`ProgressSink`]
    /// output is silenced — the caller wires that).
    pub quiet: bool,
    /// Auto-follow annotated tags pointing at fetched commits.
    pub auto_follow_tags: bool,
    /// Fetch every tag (`--tags`), independent of reachability.
    pub fetch_all_tags: bool,
    /// Prune remote-tracking refs that no longer exist on the remote.
    pub prune: bool,
    /// Prune local tags absent from the remote when pruning is enabled.
    pub prune_tags: bool,
    /// Plan and report the fetch without installing objects or updating refs.
    pub dry_run: bool,
    /// Force ref updates (`git fetch --force`), equivalent to applying `+` to
    /// each effective refspec.
    pub force: bool,
    /// Append to `FETCH_HEAD` instead of truncating it.
    pub append: bool,
    /// Write `FETCH_HEAD` (the CLI's `--write-fetch-head`).
    pub write_fetch_head: bool,
    /// Whether the tag option (`--tags`/`--no-tags`) was set explicitly, so the
    /// configured `remote.<name>.tagopt` must not override it.
    pub tag_option_explicit: bool,
    /// Whether the prune option (`--prune`/`--no-prune`) was set explicitly, so
    /// the configured `remote.<name>.prune`/`fetch.prune` must not override it.
    pub prune_option_explicit: bool,
    /// Whether the prune-tags option (`--prune-tags`/`--no-prune-tags`) was set
    /// explicitly, so configured prune tag options must not override it.
    pub prune_tags_option_explicit: bool,
    /// Explicit `--refmap` mappings for command-line refspec tracking updates.
    /// `None` means use `remote.<name>.fetch`; `Some([])` disables the
    /// opportunistic tracking update.
    pub refmap: Option<Vec<String>>,
    /// Shallow fetch depth (`--depth N`): truncate history to `N` commits per tip.
    /// `None` is a full fetch. Honored by the HTTP and SSH transports and by the
    /// in-process local (`file://`/path) server, which computes the deepen
    /// boundary itself (see [`crate::local::compute_local_deepen`]).
    pub depth: Option<u32>,
    /// When fetching configured remote refspecs, mark updates whose `src`
    /// matches one of these (possibly-abbreviated) `branch.<name>.merge` values
    /// as eligible for merge in `FETCH_HEAD`. More than one entry is an octopus
    /// merge config. Empty falls back to git's default (first ref of the first
    /// non-pattern configured refspec). Used by `fetch` (current-branch merge
    /// config) and `pull`.
    pub merge_srcs: Vec<String>,
    /// Partial-clone object filter (`--filter=blob:none`): omit filtered
    /// objects from the transferred pack. Local-only today: HTTP and SSH do not
    /// send `filter` requests yet, so callers that require network filtering
    /// must gate that before calling [`fetch`]. Directly-wanted tips are always
    /// packed on the local path, mirroring upstream's filter traversal.
    pub filter: Option<sley_odb::PackObjectFilter>,
    /// `--refetch`: ignore local haves so existing reachable commits can be
    /// repacked under a newly requested partial-clone filter.
    pub refetch: bool,
    /// This fetch is a clone (`fetch_pack_args.cloning`): shallow points sent
    /// by a shallow server are accepted into `$GIT_DIR/shallow` unconditionally.
    pub cloning: bool,
    /// Whether an in-process local promisor install should append the wanted ref
    /// names to the `.promisor` sidecar. No-checkout partial clone keeps these
    /// lines; checkout hydration leaves the final sidecar empty like upstream.
    pub record_promisor_refs: bool,
    /// `--update-shallow`: accept new shallow points from a shallow server
    /// (otherwise refs whose history needs them are rejected).
    pub update_shallow: bool,
    /// `--deepen=N`: `depth` is relative to the client's current boundary.
    /// Local-only today; HTTP and SSH treat `depth` as an absolute `--depth N`.
    pub deepen_relative: bool,
    /// Allow updating the currently checked-out branch (`git fetch -u` /
    /// `--update-head-ok`). Porcelain `pull` uses this internally.
    pub update_head_ok: bool,
    /// `--shallow-since=<date>`: deepen to commits newer than the date.
    /// Local-only today; HTTP and SSH do not send `deepen-since` yet.
    pub deepen_since: Option<i64>,
    /// `--shallow-exclude=<ref>`: deepen to commits not reachable from the ref
    /// (resolved on the remote; a non-ref is an error, like upstream).
    /// Local-only today; HTTP and SSH do not send `deepen-not` yet.
    pub deepen_not: Vec<String>,
    /// Command-line SSH process options supplied by a higher-level porcelain
    /// such as clone (`-4`/`-6`). When absent, fetch derives SSH options from
    /// the effective repository config.
    pub ssh_options: Option<crate::ssh::SshTransportOptions>,
    /// `--atomic`: apply every remote-tracking ref update (and prune deletion)
    /// in a single reference transaction so a single rejected update aborts the
    /// whole fetch and leaves `FETCH_HEAD` empty. The default is non-atomic:
    /// each ref is updated independently and a per-ref failure is reported but
    /// does not block the others.
    pub atomic: bool,
}

/// A remote-tracking ref removed by a prune pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedRef {
    /// The short branch name on the remote (e.g. `topic`).
    pub branch: String,
    /// The full local ref name removed (e.g. `refs/remotes/origin/topic`).
    pub refname: String,
}

/// The structured result of a [`fetch`].
#[derive(Debug, Clone, Default)]
pub struct FetchOutcome {
    /// The ref updates that were planned (and applied unless `dry_run`), in the
    /// order they were resolved. Includes auto-followed tags; entries without a
    /// `dst` are fetch-only (e.g. a bare `HEAD` fetch) and update no local ref.
    pub ref_updates: Vec<FetchRefUpdate>,
    /// Remote-tracking refs pruned (empty unless `prune` and the remote is a
    /// configured remote). Empty on `dry_run`.
    pub pruned: Vec<PrunedRef>,
    /// The remote's advertised `HEAD` symref target (e.g. `refs/heads/main`),
    /// when the remote advertised one. Useful for resolving the default branch.
    pub head_symref: Option<String>,
    /// Whether `FETCH_HEAD` was written.
    pub wrote_fetch_head: bool,
}

/// Fully resolved inputs for a [`fetch`] run.
pub struct FetchRequest<'a> {
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Local repository config snapshot.
    pub config: &'a GitConfig,
    /// Remote name or source string used for config lookup and `FETCH_HEAD`.
    pub remote_name: &'a str,
    /// Already-resolved transport source.
    pub source: &'a FetchSource,
    /// Refspecs requested by the caller. Empty means configured fetch refspecs,
    /// falling back to `HEAD`.
    pub refspecs: &'a [String],
    /// Fetch behavior flags.
    pub options: &'a FetchOptions,
}

/// Mutable seams used while fetching.
pub struct FetchServices<'a> {
    /// Credential source for authenticated transports.
    pub credentials: &'a mut dyn CredentialProvider,
    /// Progress sink for prune notices.
    pub progress: &'a mut dyn ProgressSink,
    /// `reference-transaction` hook handler fired when applying remote-tracking
    /// ref updates. `None` skips the hook (the historical behavior). The CLI
    /// supplies a runner so `--atomic` fetches honor a hook that aborts the
    /// transaction, matching git's `store_updated_refs`.
    pub ref_hook: Option<&'a dyn sley_refs::ReferenceTransactionHook>,
}

/// Fetch from a resolved `source` into the repository at `git_dir`.
///
/// Performs the work the CLI's `fetch_http_repository`/`fetch_local_repository`
/// did: applies configured tag/prune options, plans the ref-map for `refspecs`
/// (empty means the remote's configured fetch refspecs, falling back to `HEAD`),
/// installs the pack, writes `FETCH_HEAD`, applies remote-tracking updates, and
/// prunes. `remote_name` is the remote/argument the caller resolved `source`
/// from (used for `FETCH_HEAD` descriptions and to look up `remote.<name>.*`).
///
/// Emits prune notices through `progress` and returns the structured
/// [`FetchOutcome`]; never prints or returns `GitError::Exit`.
pub fn fetch(request: FetchRequest<'_>, services: FetchServices<'_>) -> Result<FetchOutcome> {
    let ref_hook = services.ref_hook;
    let mut options = request.options.clone();
    apply_configured_remote_tag_option(request.config, request.remote_name, &mut options);
    apply_configured_fetch_prune_option(request.config, request.remote_name, &mut options);
    crate::protocol::check_transport_allowed(
        scheme_for_fetch_source(request.source),
        Some(request.config),
        None,
    )
    .map_err(crate::protocol::transport_policy_git_error)?;
    // A pack must be installed as a promisor pack when the remote is already a
    // promisor remote OR this fetch applies an object filter: a filtered fetch
    // omits objects, so its pack is only valid as a `.promisor` pack (git's
    // fetch-pack writes `.promisor` whenever the request carries a filter).
    let promisor_remote = request
        .config
        .get_bool("remote", Some(request.remote_name), "promisor")
        .unwrap_or(false)
        || request.options.filter.is_some();
    let max_input_size = fetch_max_input_size(request.config);
    let configured_refspecs = if request.refspecs.is_empty() {
        remote_config_values(request.config, request.remote_name, "fetch")
    } else {
        Vec::new()
    };
    let configured_refspecs_empty = configured_refspecs.is_empty();
    // git's `get_ref_map`: a default fetch (no command-line refspecs) of the
    // current branch's tracking remote also fetches the branch's
    // `branch.<x>.merge` refs (`add_merge_config`) as source-only refs recorded
    // for-merge in FETCH_HEAD. When the remote has no configured fetch refspec
    // either, those merge refs replace the bare-`HEAD` default fetch entirely.
    let has_merge_config = request.refspecs.is_empty() && !options.merge_srcs.is_empty();
    let default_head_fetch =
        request.refspecs.is_empty() && configured_refspecs_empty && !has_merge_config;
    let configured_remote_fetch = request.refspecs.is_empty() && !configured_refspecs_empty;
    let fetch_head_source = fetch_head_source_description(request.config, request.remote_name);
    let prune_refspecs =
        prune_refspecs_for_source(&configured_refspecs, request.refspecs, options.prune_tags);
    let mut effective_refspecs = fetch_refspecs_for_source(
        configured_refspecs,
        request.refspecs,
        options.fetch_all_tags,
    );
    if options.prune_tags
        && request.refspecs.is_empty()
        && !effective_refspecs
            .iter()
            .any(|refspec| refspec == "refs/tags/*:refs/tags/*")
    {
        effective_refspecs.push("refs/tags/*:refs/tags/*".to_string());
    }
    if has_merge_config {
        // Drop the synthetic bare-`HEAD` refspec the helper inserts when nothing
        // is configured; the merge refs are fetched for-merge instead.
        if configured_refspecs_empty && request.refspecs.is_empty() {
            effective_refspecs.retain(|spec| spec != "HEAD");
        }
        // Parse the configured refspecs so coverage (pattern-aware) can be tested
        // against their sources, mirroring `add_merge_config`'s ref-map lookup.
        let configured_parsed = effective_refspecs
            .iter()
            .map(|refspec| parse_refspec(refspec))
            .collect::<Result<Vec<_>>>()?;
        for merge_src in &options.merge_srcs {
            // git fetches a merge ref only when it is not already reachable
            // through a configured fetch refspec (`add_merge_config`). A glob
            // refspec like `refs/heads/*` already covers `refs/heads/three`.
            let covered = configured_parsed.iter().any(|refspec| {
                refspec
                    .src
                    .as_deref()
                    .is_some_and(|src| refspec_source_covers(refspec, src, merge_src))
            });
            if !covered {
                // Source-only refspec (no `:dst`): fetched and written to
                // FETCH_HEAD but creating no local ref.
                effective_refspecs.push(merge_src.clone());
            }
        }
    }
    let mut parsed_refspecs = effective_refspecs
        .iter()
        .map(|refspec| parse_refspec(refspec))
        .collect::<Result<Vec<_>>>()?;
    if options.force {
        for refspec in &mut parsed_refspecs {
            refspec.force = true;
        }
    }
    if options.refmap.is_some() && request.refspecs.is_empty() {
        return Err(GitError::Command(
            "--refmap option is only meaningful with command-line refspec(s)".into(),
        ));
    }
    let tracking_refspec_strings = if request.refspecs.is_empty() {
        Vec::new()
    } else {
        options.refmap.clone().unwrap_or_else(|| {
            configured_refspecs_for_tracking(request.config, request.remote_name)
        })
    };
    let tracking_refspecs = tracking_refspec_strings
        .iter()
        .map(|refspec| parse_refspec(refspec))
        .collect::<Result<Vec<_>>>()?;
    let parsed_prune_refspecs = prune_refspecs
        .iter()
        .map(|refspec| parse_refspec(refspec))
        .collect::<Result<Vec<_>>>()?;

    let store = FileRefStore::new(request.git_dir, request.format);
    let mut outcome = FetchOutcome::default();

    // Advertise refs, plan the ref-map, install the pack, then update refs/prune.
    // The two transports differ only in how they advertise and how they pull the
    // pack; the ref-map planning and ref bookkeeping are identical.
    let advertisements = match request.source {
        #[cfg(not(feature = "http"))]
        FetchSource::Http(_) => {
            return Err(GitError::Unsupported(
                "HTTP transport is not enabled in this build".into(),
            ));
        }
        #[cfg(feature = "http")]
        FetchSource::Http(remote) => {
            let client = crate::http::new_http_client();
            let discovered = crate::http::http_service_advertisements(
                &client,
                remote,
                request.format,
                sley_protocol::GitService::UploadPack,
                services.credentials,
            )?;
            let advertisements = discovered.set.refs;
            let features = advertisements
                .first()
                .map(|advertisement| {
                    sley_protocol::parse_upload_pack_features(&advertisement.capabilities)
                })
                .transpose()?
                .unwrap_or_default();
            outcome.head_symref = head_symref_from_features(&features.symrefs);
            let (mut updates, opportunistic_dsts) = plan_and_adjust_updates(FetchPlanInput {
                advertisements: &advertisements,
                refspecs: &parsed_refspecs,
                options: &options,
                store: &store,
                reachable: None,
                local_db: None,
                deepen_excluded: None,
                format: request.format,
                configured_remote_fetch,
                has_merge_config,
                tracking_refspecs: &tracking_refspecs,
            })?;
            let wants = updates.iter().map(|update| update.oid).collect();
            // Shallow fetch: replay the current boundary as `shallow` lines and ask
            // the server to deepen to `depth`, then fold the server's shallow-info
            // back into `$GIT_DIR/shallow`. A `None` depth keeps the full-fetch path.
            let existing_shallow =
                shallow_boundary_for_request(request.git_dir, request.format, options.depth)?;
            let pack_request = crate::http::HttpFetchPackRequest {
                client: &client,
                git_dir: request.git_dir,
                format: request.format,
                remote,
                wants,
                shallow: existing_shallow,
                deepen: options.depth,
                promisor: promisor_remote,
                max_input_size,
            };
            let shallow_info = if discovered.set.protocol == ProtocolVersion::V2 {
                let handshake = discovered.handshake.as_ref().ok_or_else(|| {
                    GitError::InvalidFormat(
                        "protocol v2 HTTP fetch requires a v2 handshake from service discovery"
                            .into(),
                    )
                })?;
                crate::http::install_fetch_pack_via_http_protocol_v2_fetch(
                    pack_request,
                    handshake,
                    services.credentials,
                )?
            } else {
                crate::http::install_fetch_pack_via_http_upload_pack(
                    pack_request,
                    services.credentials,
                )?
            };
            if !options.dry_run {
                crate::shallow::apply_shallow_info(request.git_dir, request.format, &shallow_info)?;
            }
            finalize_fetch(
                FetchFinalize {
                    git_dir: request.git_dir,
                    format: request.format,
                    store: &store,
                    options: &options,
                    fetch_head_source: &fetch_head_source,
                    default_head_fetch,
                    log_all_ref_updates: fetch_log_all_ref_updates(request.config),
                    ref_hook,
                    opportunistic_dsts: &opportunistic_dsts,
                },
                &mut updates,
                &mut outcome,
            )?;
            advertisements
        }
        FetchSource::Ssh(remote) => {
            // SSH advertises and pulls the pack by spawning `ssh` (no credential
            // seam — the `ssh` program authenticates), but the ref-map planning
            // and ref bookkeeping are the same shared flow as HTTP.
            let ssh_options = options
                .ssh_options
                .unwrap_or_else(|| crate::ssh::ssh_transport_options_from_config(request.config));
            let (advertisements, features) =
                crate::ssh::ssh_upload_pack_advertisements_with_options(
                    remote,
                    request.format,
                    ssh_options,
                )?;
            outcome.head_symref = head_symref_from_features(&features.symrefs);
            let (mut updates, opportunistic_dsts) = plan_and_adjust_updates(FetchPlanInput {
                advertisements: &advertisements,
                refspecs: &parsed_refspecs,
                options: &options,
                store: &store,
                reachable: None,
                local_db: None,
                deepen_excluded: None,
                format: request.format,
                configured_remote_fetch,
                has_merge_config,
                tracking_refspecs: &tracking_refspecs,
            })?;
            if remote.transport == RemoteTransport::Ext && options.auto_follow_tags {
                append_missing_ext_advertised_tags(
                    &advertisements,
                    &parsed_refspecs,
                    &store,
                    &mut updates,
                )?;
            }
            let wants = updates.iter().map(|update| update.oid).collect();
            // Shallow fetch over SSH mirrors the HTTP path: replay the current
            // boundary, deepen to `depth`, then apply the server's shallow-info.
            let existing_shallow =
                shallow_boundary_for_request(request.git_dir, request.format, options.depth)?;
            let shallow_info = crate::ssh::install_fetch_pack_via_ssh_upload_pack(
                crate::ssh::SshFetchPackRequest {
                    git_dir: request.git_dir,
                    format: request.format,
                    remote,
                    features: &features,
                    wants,
                    shallow: existing_shallow,
                    deepen: options.depth,
                    promisor: promisor_remote,
                    command_options: ssh_options,
                    max_input_size,
                },
            )?;
            if !options.dry_run {
                crate::shallow::apply_shallow_info(request.git_dir, request.format, &shallow_info)?;
            }
            finalize_fetch(
                FetchFinalize {
                    git_dir: request.git_dir,
                    format: request.format,
                    store: &store,
                    options: &options,
                    fetch_head_source: &fetch_head_source,
                    default_head_fetch,
                    log_all_ref_updates: fetch_log_all_ref_updates(request.config),
                    ref_hook,
                    opportunistic_dsts: &opportunistic_dsts,
                },
                &mut updates,
                &mut outcome,
            )?;
            advertisements
        }
        FetchSource::Git {
            remote,
            protocol_v2,
        } => {
            let protocol_v2 =
                *protocol_v2 || request.config.get("protocol", None, "version") == Some("2");
            let discovered = crate::git::git_upload_pack_advertisements_with_protocol(
                remote,
                request.format,
                protocol_v2,
            )?;
            let advertisements = discovered.refs;
            let features = discovered.features;
            outcome.head_symref = head_symref_from_features(&features.symrefs);
            let (mut updates, opportunistic_dsts) = plan_and_adjust_updates(FetchPlanInput {
                advertisements: &advertisements,
                refspecs: &parsed_refspecs,
                options: &options,
                store: &store,
                reachable: None,
                local_db: None,
                deepen_excluded: None,
                format: request.format,
                configured_remote_fetch,
                has_merge_config,
                tracking_refspecs: &tracking_refspecs,
            })?;
            let wants = updates.iter().map(|update| update.oid).collect();
            let existing_shallow =
                shallow_boundary_for_request(request.git_dir, request.format, options.depth)?;
            let shallow_info = crate::git::install_fetch_pack_via_git_upload_pack(
                crate::git::GitFetchPackRequest {
                    git_dir: request.git_dir,
                    format: request.format,
                    remote,
                    features: &features,
                    wants,
                    shallow: existing_shallow,
                    deepen: options.depth,
                    promisor: promisor_remote,
                    protocol_v2: discovered.protocol_v2,
                    max_input_size,
                },
            )?;
            if !options.dry_run {
                crate::shallow::apply_shallow_info(request.git_dir, request.format, &shallow_info)?;
            }
            finalize_fetch(
                FetchFinalize {
                    git_dir: request.git_dir,
                    format: request.format,
                    store: &store,
                    options: &options,
                    fetch_head_source: &fetch_head_source,
                    default_head_fetch,
                    log_all_ref_updates: fetch_log_all_ref_updates(request.config),
                    ref_hook,
                    opportunistic_dsts: &opportunistic_dsts,
                },
                &mut updates,
                &mut outcome,
            )?;
            advertisements
        }
        FetchSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } => {
            let remote_format = crate::object_format_for_git_dir(remote_common_git_dir)?;
            if remote_format != request.format {
                return Err(GitError::InvalidObjectId(format!(
                    "remote repository uses {}, local repository uses {}",
                    remote_format.name(),
                    request.format.name()
                )));
            }
            let advertisements =
                crate::local::local_fetch_advertisements(remote_git_dir, request.format)?;
            // The remote's advertised HEAD symref target (e.g. `refs/heads/main`),
            // used by the CLI to create `refs/remotes/<remote>/HEAD` on a default
            // fetch — parity with the network transports' `head_symref`.
            if advertisements
                .iter()
                .any(|advertisement| advertisement.name == "HEAD")
                && let Some(RefTarget::Symbolic(target)) =
                    FileRefStore::new(remote_git_dir, request.format).read_ref("HEAD")?
            {
                outcome.head_symref = Some(target);
            }
            let remote_db = FileObjectDatabase::from_git_dir(remote_common_git_dir, request.format);
            // Shallow fetch: the in-process upload-pack needs its deepen plan up
            // front. The boundary walk starts from the primary planned tips
            // (upload-pack's `want_obj`) — auto-followed tags are this path's
            // include-tag equivalent and must not deepen the walk, and the tag
            // auto-follow below must not see history past the boundary. The
            // primary plan is recomputed inside `plan_and_adjust_updates`; the
            // planner is a pure function over the same inputs, so both runs
            // agree. A `None` depth keeps the full-fetch path.
            // The remote's own boundary: a shallow server reports its graft
            // points on ANY fetch (upstream `send_shallow_info` runs an
            // implicit INFINITE_DEPTH deepen when no deepen was requested).
            let remote_shallow =
                crate::shallow::read_shallow(remote_common_git_dir, request.format)?;
            let explicit_deepen = options.depth.is_some()
                || options.deepen_since.is_some()
                || !options.deepen_not.is_empty();
            let implicit_deepen = !explicit_deepen && !remote_shallow.is_empty();
            // `--shallow-exclude` values must name refs on the remote
            // (upstream upload-pack `process_deepen_not`).
            let mut deepen_not_oids = Vec::new();
            for name in &options.deepen_not {
                let resolved = advertisements.iter().find(|advertisement| {
                    advertisement.name == *name
                        || advertisement.name == format!("refs/tags/{name}")
                        || advertisement.name == format!("refs/heads/{name}")
                        || advertisement.name == format!("refs/{name}")
                });
                match resolved {
                    Some(advertisement) => deepen_not_oids.push(advertisement.oid),
                    None => {
                        return Err(GitError::Command(format!(
                            "git upload-pack: deepen-not is not a ref: {name}"
                        )));
                    }
                }
            }
            let plan_deepen = |heads: &[ObjectId]| -> Result<Option<LocalDeepenPlan>> {
                if !explicit_deepen && !implicit_deepen {
                    return Ok(None);
                }
                // Replay the current boundary, like the HTTP and SSH paths.
                let client_shallow = crate::shallow::read_shallow(request.git_dir, request.format)?;
                if options.deepen_since.is_some() || !deepen_not_oids.is_empty() {
                    return Ok(Some(crate::local::compute_local_deepen_by_rev_list(
                        &remote_db,
                        request.format,
                        heads,
                        client_shallow,
                        options.deepen_since,
                        &deepen_not_oids,
                    )?));
                }
                let depth = options.depth.unwrap_or(crate::local::INFINITE_DEPTH);
                Ok(Some(crate::local::compute_local_deepen(
                    &remote_db,
                    request.format,
                    heads,
                    client_shallow,
                    depth,
                    options.deepen_relative,
                )?))
            };
            let primary_heads = {
                let primary = plan_fetch_ref_updates(
                    &advertisements,
                    &parsed_refspecs,
                    options.auto_follow_tags,
                )?;
                let mut seen = HashSet::new();
                let mut heads = Vec::new();
                for update in &primary {
                    if seen.insert(update.oid) {
                        heads.push(update.oid);
                    }
                }
                heads
            };
            let mut deepen_plan = plan_deepen(&primary_heads)?;
            let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
            let (mut updates, opportunistic_dsts) = plan_and_adjust_updates(FetchPlanInput {
                advertisements: &advertisements,
                refspecs: &parsed_refspecs,
                options: &options,
                store: &store,
                reachable: Some((&remote_db, &advertisements)),
                local_db: Some(&local_db),
                deepen_excluded: deepen_plan.as_ref().map(|plan| &plan.excluded),
                format: request.format,
                configured_remote_fetch,
                has_merge_config,
                tracking_refspecs: &tracking_refspecs,
            })?;
            // A shallow server's new boundary points are only written on a
            // clone, an explicit deepen, or `--update-shallow`; otherwise the
            // refs whose history would need them are rejected and dropped
            // (upstream fetch-pack `update_shallow` + REF_STATUS_REJECT_SHALLOW).
            if implicit_deepen && !options.cloning && !options.update_shallow {
                let client_shallow: HashSet<ObjectId> =
                    crate::shallow::read_shallow(request.git_dir, request.format)?
                        .into_iter()
                        .collect();
                let new_points: HashSet<ObjectId> = deepen_plan
                    .as_ref()
                    .map(|plan| {
                        plan.shallow_info
                            .iter()
                            .filter_map(|entry| match entry {
                                sley_protocol::ProtocolV2FetchShallowInfo::Shallow(oid)
                                    if !client_shallow.contains(oid) =>
                                {
                                    Some(*oid)
                                }
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !new_points.is_empty() {
                    let mut dirty_cache: HashMap<ObjectId, bool> = HashMap::new();
                    let mut dirty = |tip: &ObjectId| -> Result<bool> {
                        if let Some(&cached) = dirty_cache.get(tip) {
                            return Ok(cached);
                        }
                        let result =
                            tip_reaches_boundary(&remote_db, request.format, tip, &new_points)?;
                        dirty_cache.insert(*tip, result);
                        Ok(result)
                    };
                    let mut kept = Vec::new();
                    for update in updates {
                        if dirty(&update.oid)? {
                            continue;
                        }
                        kept.push(update);
                    }
                    updates = kept;
                    // Re-plan the boundary from the surviving tips so the pack
                    // walk and the shallow-info reflect only what is sent.
                    let mut seen = HashSet::new();
                    let mut heads = Vec::new();
                    for update in &updates {
                        if seen.insert(update.oid) {
                            heads.push(update.oid);
                        }
                    }
                    deepen_plan = if heads.is_empty() {
                        None
                    } else {
                        plan_deepen(&heads)?
                    };
                }
            }
            let starts: Vec<ObjectId> = if options.refetch {
                let mut seen = HashSet::new();
                updates
                    .iter()
                    .map(|update| update.oid)
                    .chain(primary_heads.iter().copied())
                    .filter(|oid| seen.insert(*oid))
                    .collect()
            } else if deepen_plan.is_none() {
                let mut starts = Vec::new();
                for update in &updates {
                    if !local_db.contains(&update.oid)? {
                        starts.push(update.oid);
                    }
                }
                starts
            } else {
                updates.iter().map(|update| update.oid).collect()
            };
            let shallow_info = if starts.is_empty() && deepen_plan.is_none() {
                if !updates.is_empty() {
                    sley_protocol::trace_packet_write_payload(b"0000");
                }
                Vec::new()
            } else {
                crate::local::install_fetch_pack_via_local_upload_pack(
                    request.git_dir,
                    remote_git_dir,
                    request.format,
                    starts,
                    deepen_plan.as_ref(),
                    promisor_remote,
                    options.record_promisor_refs,
                    options.filter.clone(),
                    options.refetch,
                    local_fetch_unpack_limit(request.git_dir, promisor_remote),
                )?
            };
            if !options.dry_run {
                crate::shallow::apply_shallow_info(request.git_dir, request.format, &shallow_info)?;
            }
            finalize_fetch(
                FetchFinalize {
                    git_dir: request.git_dir,
                    format: request.format,
                    store: &store,
                    options: &options,
                    fetch_head_source: &fetch_head_source,
                    default_head_fetch,
                    log_all_ref_updates: fetch_log_all_ref_updates(request.config),
                    ref_hook,
                    opportunistic_dsts: &opportunistic_dsts,
                },
                &mut updates,
                &mut outcome,
            )?;
            advertisements
        }
    };

    if options.prune && !parsed_prune_refspecs.is_empty() {
        outcome.pruned = prune_refs_from_advertisements(
            PruneRefsInput {
                config: request.config,
                store: &store,
                remote: request.remote_name,
                advertisements: &advertisements,
                refspecs: &parsed_prune_refspecs,
                dry_run: options.dry_run,
                quiet: options.quiet,
            },
            services.progress,
        )?;
    }

    Ok(outcome)
}

fn scheme_for_fetch_source(source: &FetchSource) -> &'static str {
    match source {
        FetchSource::Http(remote) => crate::protocol::transport_scheme_for_remote(remote),
        FetchSource::Ssh(remote) => crate::protocol::transport_scheme_for_remote(remote),
        FetchSource::Git { remote, .. } => crate::protocol::transport_scheme_for_remote(remote),
        FetchSource::Local { .. } => "file",
    }
}

fn local_fetch_unpack_limit(git_dir: &Path, promisor_remote: bool) -> Option<usize> {
    if promisor_remote {
        return None;
    }
    git_dir
        .join("objects")
        .join("info")
        .join("alternates")
        .exists()
        .then_some(100)
}

/// Does the (graft-aware) history of `tip` on the remote touch one of the
/// server's new shallow boundary points? Mirrors upstream
/// `assign_shallow_commits_to_refs`'s per-ref reachability test.
fn tip_reaches_boundary<R: sley_odb::ObjectReader>(
    remote_db: &R,
    format: ObjectFormat,
    tip: &ObjectId,
    boundary: &HashSet<ObjectId>,
) -> Result<bool> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut queue: Vec<ObjectId> = vec![*tip];
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = remote_db.read_object(&oid)?;
        let commit = match object.object_type {
            sley_object::ObjectType::Commit => {
                sley_object::Commit::parse_ref(format, &object.body)?
            }
            sley_object::ObjectType::Tag => {
                let tag = sley_object::Tag::parse_ref(format, &object.body)?;
                queue.push(tag.object);
                continue;
            }
            _ => continue,
        };
        if boundary.contains(&oid) {
            return Ok(true);
        }
        queue.extend(sley_odb::grafted_parents(remote_db, &oid, commit.parents));
    }
    Ok(false)
}

/// The shallow boundary to replay in a deepen request: the oids in
/// `$GIT_DIR/shallow` when `depth` is set, otherwise empty (a full fetch sends no
/// `shallow` lines). Reading the file only when deepening keeps the non-shallow
/// path's wire form unchanged.
fn shallow_boundary_for_request(
    git_dir: &Path,
    format: ObjectFormat,
    depth: Option<u32>,
) -> Result<Vec<ObjectId>> {
    if depth.is_none() {
        return Ok(Vec::new());
    }
    crate::shallow::read_shallow(git_dir, format)
}

/// Plan the ref-map and apply the auto-follow-tag / not-for-merge adjustments
/// shared by both transports. `reachable` (local only) enables appending tags
/// reachable from fetched commits via the remote object database;
/// `deepen_excluded` (local shallow fetch only) keeps that reachability walk
/// from crossing the deepen boundary.
struct FetchPlanInput<'a> {
    advertisements: &'a [RefAdvertisement],
    refspecs: &'a [RefSpec],
    options: &'a FetchOptions,
    store: &'a FileRefStore,
    reachable: Option<(&'a FileObjectDatabase, &'a [RefAdvertisement])>,
    /// The local repository's object database, used to follow tags whose target
    /// is already present locally (git's `find_non_local_tags` `odb_has_object`
    /// check). Only the local transport supplies it; auto-follow is local-only.
    local_db: Option<&'a FileObjectDatabase>,
    deepen_excluded: Option<&'a HashSet<ObjectId>>,
    format: ObjectFormat,
    configured_remote_fetch: bool,
    /// Default fetch (no command-line refspecs) of the current branch's tracking
    /// remote with `branch.<x>.merge` configured. The merge refs drive which
    /// FETCH_HEAD entries are for-merge (`add_merge_config`).
    has_merge_config: bool,
    /// Opportunistic tracking mappings used only for command-line refspecs.
    tracking_refspecs: &'a [RefSpec],
}

fn plan_and_adjust_updates(
    input: FetchPlanInput<'_>,
) -> Result<(Vec<FetchRefUpdate>, HashSet<String>)> {
    let FetchPlanInput {
        advertisements,
        refspecs,
        options,
        store,
        reachable,
        local_db,
        deepen_excluded,
        format,
        configured_remote_fetch,
        has_merge_config,
        tracking_refspecs,
    } = input;
    let visible_advertisements = advertisements_without_peeled_refs(advertisements);
    let planning_advertisements = if visible_advertisements.len() == advertisements.len() {
        advertisements
    } else {
        visible_advertisements.as_slice()
    };
    let mut updates =
        plan_fetch_ref_updates(planning_advertisements, refspecs, options.auto_follow_tags)?;
    if options.fetch_all_tags {
        mark_tag_refspec_updates_not_for_merge(&mut updates);
    } else {
        if options.auto_follow_tags
            && let Some((remote_db, advertisements)) = reachable
        {
            let visible_reachable_advertisements =
                advertisements_without_peeled_refs(advertisements);
            let reachable_advertisements =
                if visible_reachable_advertisements.len() == advertisements.len() {
                    advertisements
                } else {
                    visible_reachable_advertisements.as_slice()
                };
            append_reachable_auto_follow_tags(
                reachable_advertisements,
                remote_db,
                local_db,
                format,
                refspecs,
                &mut updates,
                deepen_excluded,
            )?;
        }
        retain_missing_auto_follow_tags(store, &mut updates)?;
    }
    if configured_remote_fetch || has_merge_config {
        for update in &mut updates {
            update.not_for_merge = true;
        }
        if !options.merge_srcs.is_empty() {
            // The current branch's `branch.<name>.merge` ref(s) are what we'll
            // merge, so they are the for-merge entries in FETCH_HEAD. Each entry
            // is matched with git's abbreviation rules (`branch_merge_matches`);
            // more than one is an octopus merge config.
            for update in &mut updates {
                if options
                    .merge_srcs
                    .iter()
                    .any(|src| refname_matches(src, &update.src))
                {
                    update.not_for_merge = false;
                }
            }
        } else if let Some(first) = refspecs.iter().find(|refspec| !refspec.negative)
            && !first.pattern
        {
            // No merge config: mirror git's get_ref_map default, which marks the
            // first matched ref of the first configured (non-pattern) fetch
            // refspec as for-merge. Pattern-led configs (e.g. refs/heads/*) leave
            // every entry not-for-merge.
            if let Some(update) = updates.first_mut() {
                update.not_for_merge = false;
            }
        }
        // git's store_updated_refs writes FETCH_HEAD in two passes: all for-merge
        // entries first (in ref-map order), then all not-for-merge. Reorder
        // stably to reproduce that layout.
        updates.sort_by_key(|update| update.not_for_merge);
    }
    let opportunistic_dsts =
        append_opportunistic_tracking_updates(&mut updates, tracking_refspecs)?;
    ref_remove_duplicate_updates(&mut updates)?;
    Ok((updates, opportunistic_dsts))
}

/// Mirror git's `ref_remove_duplicates` (remote.c): two ref-map entries with the
/// same destination are collapsed when they came from the same source ref (e.g.
/// a remote that lists `+refs/heads/*:refs/remotes/origin/*` twice), and rejected
/// when two *different* sources would map to one destination.
fn ref_remove_duplicate_updates(updates: &mut Vec<FetchRefUpdate>) -> Result<()> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut error = None;
    updates.retain(|update| {
        let Some(dst) = update.dst.as_deref() else {
            return true;
        };
        match seen.get(dst) {
            Some(prev_src) if prev_src == &update.src => false,
            Some(prev_src) => {
                if error.is_none() {
                    error = Some(GitError::Command(format!(
                        "Cannot fetch both {} and {} to {dst}",
                        prev_src, update.src
                    )));
                }
                true
            }
            None => {
                seen.insert(dst.to_string(), update.src.clone());
                true
            }
        }
    });
    match error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn configured_refspecs_for_tracking(config: &GitConfig, remote: &str) -> Vec<String> {
    if remote_exists(config, remote) {
        remote_config_values(config, remote, "fetch")
    } else {
        Vec::new()
    }
}

/// Append the opportunistic remote-tracking updates for a command-line refspec
/// fetch (a fetched ref that also matches a configured tracking refspec). Returns
/// the set of destinations these added — git marks them `FETCH_HEAD_IGNORE`, so
/// the caller excludes them from `FETCH_HEAD` while still applying them as refs.
fn append_opportunistic_tracking_updates(
    updates: &mut Vec<FetchRefUpdate>,
    tracking_refspecs: &[RefSpec],
) -> Result<HashSet<String>> {
    let mut opportunistic_dsts = HashSet::new();
    if tracking_refspecs.is_empty() {
        return Ok(opportunistic_dsts);
    }
    let mut seen_dsts = updates
        .iter()
        .filter_map(|update| update.dst.clone())
        .collect::<HashSet<_>>();
    let mut additions = Vec::new();
    for update in updates.iter() {
        if fetch_refspec_excludes(tracking_refspecs, &update.src)? {
            continue;
        }
        for refspec in tracking_refspecs.iter().filter(|refspec| !refspec.negative) {
            let Some(dst) = refspec_map_source(refspec, &update.src)? else {
                continue;
            };
            if !seen_dsts.insert(dst.clone()) {
                continue;
            }
            opportunistic_dsts.insert(dst.clone());
            additions.push(FetchRefUpdate {
                src: update.src.clone(),
                dst: Some(dst),
                oid: update.oid,
                not_for_merge: true,
                force: refspec.force,
            });
        }
    }
    updates.extend(additions);
    Ok(opportunistic_dsts)
}

fn advertisements_without_peeled_refs(
    advertisements: &[RefAdvertisement],
) -> Vec<RefAdvertisement> {
    advertisements
        .iter()
        .filter(|advertisement| !advertisement.name.ends_with("^{}"))
        .cloned()
        .collect()
}

fn append_missing_ext_advertised_tags(
    advertisements: &[RefAdvertisement],
    refspecs: &[RefSpec],
    store: &FileRefStore,
    updates: &mut Vec<FetchRefUpdate>,
) -> Result<()> {
    let mut seen = updates
        .iter()
        .map(|update| update.src.clone())
        .collect::<HashSet<_>>();
    let mut tags = Vec::new();
    for reference in advertisements {
        if !reference.name.starts_with("refs/tags/")
            || reference.name.ends_with("^{}")
            || !seen.insert(reference.name.clone())
            || fetch_refspec_excludes(refspecs, &reference.name)?
            || store.read_ref(&reference.name)?.is_some()
        {
            continue;
        }
        tags.push(FetchRefUpdate {
            src: reference.name.clone(),
            dst: Some(reference.name.clone()),
            oid: reference.oid,
            not_for_merge: true,
            force: false,
        });
    }
    tags.sort_by(|a, b| a.src.cmp(&b.src));
    updates.extend(tags);
    Ok(())
}

/// Write `FETCH_HEAD`, apply the remote-tracking ref updates, and record the
/// applied updates in `outcome`. A no-op on `dry_run` (the pack is already
/// installed; refs and `FETCH_HEAD` are left untouched), matching the CLI.
struct FetchFinalize<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    store: &'a FileRefStore,
    options: &'a FetchOptions,
    fetch_head_source: &'a str,
    default_head_fetch: bool,
    log_all_ref_updates: bool,
    ref_hook: Option<&'a dyn sley_refs::ReferenceTransactionHook>,
    /// Destinations of opportunistic tracking updates (git's `FETCH_HEAD_IGNORE`):
    /// applied as refs but excluded from `FETCH_HEAD`.
    opportunistic_dsts: &'a HashSet<String>,
}

/// git's `store_updated_refs` (builtin/fetch.c) downgrades any for-merge
/// FETCH_HEAD entry whose object does not peel to a commit to not-for-merge: an
/// explicit `tag <name>` whose tag points at a tree or blob (e.g. `tag-one-tree`)
/// is recorded but never eligible for merge. Runs after the pack is installed so
/// the objects are present locally.
fn downgrade_non_commit_for_merge(
    git_dir: &Path,
    format: ObjectFormat,
    updates: &mut [FetchRefUpdate],
) {
    if updates.iter().all(|update| update.not_for_merge) {
        return;
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    for update in updates.iter_mut() {
        if !update.not_for_merge && sley_rev::peel_to_commit(&db, format, &update.oid).is_err() {
            update.not_for_merge = true;
        }
    }
}

fn finalize_fetch(
    finalize: FetchFinalize<'_>,
    updates: &mut Vec<FetchRefUpdate>,
    outcome: &mut FetchOutcome,
) -> Result<()> {
    let FetchFinalize {
        git_dir,
        format,
        store,
        options,
        fetch_head_source,
        default_head_fetch,
        log_all_ref_updates,
        ref_hook,
        opportunistic_dsts,
    } = finalize;
    if options.dry_run {
        outcome.ref_updates = std::mem::take(updates);
        return Ok(());
    }
    downgrade_non_commit_for_merge(git_dir, format, updates);
    validate_fetch_ref_updates(git_dir, format, store, options.update_head_ok, updates)?;
    if options.atomic {
        // Atomic fetch (`do_fetch`/`store_updated_refs` with a transaction): a
        // single rejected update aborts the whole fetch and leaves `FETCH_HEAD`
        // empty. git truncates `FETCH_HEAD` up front (unless `--append`) and
        // only re-writes the buffered records once the transaction commits, so
        // an abort leaves the truncated (empty) file. Reject non-fast-forward
        // tracking updates first, then apply every update in one transaction
        // (firing the `reference-transaction` hook, which may itself abort).
        if options.write_fetch_head && !options.append {
            fs::write(git_dir.join("FETCH_HEAD"), b"")?;
        }
        if let Some(reason) = atomic_non_fast_forward_rejection(git_dir, format, store, updates)? {
            return Err(GitError::Command(reason));
        }
        apply_fetch_ref_updates(
            store,
            format,
            fetch_head_source,
            log_all_ref_updates,
            updates,
            ref_hook,
        )?;
        if options.write_fetch_head {
            // Already truncated above when not appending, so always append the
            // committed records (mirrors git's buffer-then-`commit_fetch_head`).
            write_finalized_fetch_head(
                git_dir,
                fetch_head_source,
                default_head_fetch,
                updates,
                opportunistic_dsts,
                true,
            )?;
            outcome.wrote_fetch_head = true;
        }
        outcome.ref_updates = std::mem::take(updates);
        return Ok(());
    }
    if options.write_fetch_head {
        write_finalized_fetch_head(
            git_dir,
            fetch_head_source,
            default_head_fetch,
            updates,
            opportunistic_dsts,
            options.append,
        )?;
        outcome.wrote_fetch_head = true;
    }
    apply_fetch_ref_updates(
        store,
        format,
        fetch_head_source,
        log_all_ref_updates,
        updates,
        ref_hook,
    )?;
    outcome.ref_updates = std::mem::take(updates);
    Ok(())
}

/// Write `FETCH_HEAD` for the planned `updates`, using the bare-`HEAD` default
/// record when the fetch was a single default `HEAD` fetch. Opportunistic
/// tracking updates (git's `FETCH_HEAD_IGNORE`) are dropped — they are applied
/// as refs but not recorded in `FETCH_HEAD`.
fn write_finalized_fetch_head(
    git_dir: &Path,
    fetch_head_source: &str,
    default_head_fetch: bool,
    updates: &[FetchRefUpdate],
    opportunistic_dsts: &HashSet<String>,
    append: bool,
) -> Result<()> {
    if default_head_fetch
        && updates.len() == 1
        && updates[0].src == "HEAD"
        && updates[0].dst.is_none()
    {
        return write_default_fetch_head(git_dir, fetch_head_source, updates[0].oid, append);
    }
    let records: Vec<FetchRefUpdate> = updates
        .iter()
        .filter(|update| {
            update
                .dst
                .as_deref()
                .is_none_or(|dst| !opportunistic_dsts.contains(dst))
        })
        .cloned()
        .collect();
    write_fetch_head(git_dir, fetch_head_source, &records, append)
}

/// Reject the first non-fast-forward tracking update an `--atomic` fetch would
/// make (a non-forced refspec whose destination already exists and whose new tip
/// does not descend from the old). Returns the git-shaped `! [rejected]` line so
/// the whole atomic transaction can be aborted before any ref is touched.
fn atomic_non_fast_forward_rejection(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    updates: &[FetchRefUpdate],
) -> Result<Option<String>> {
    let mut db: Option<FileObjectDatabase> = None;
    for update in updates {
        let Some(dst) = update.dst.as_deref() else {
            continue;
        };
        if update.force {
            continue;
        }
        let Some(RefTarget::Direct(old)) = store.read_ref(dst)? else {
            continue;
        };
        if old == update.oid || dst.starts_with("refs/tags/") {
            continue;
        }
        let db = db.get_or_insert_with(|| FileObjectDatabase::from_git_dir(git_dir, format));
        if !crate::push::is_fast_forward(db, format, &old, &update.oid)? {
            return Ok(Some(format!(
                "! [rejected]        {} -> {}  (non-fast-forward)",
                update.src, dst
            )));
        }
    }
    Ok(None)
}

fn apply_fetch_ref_updates(
    store: &FileRefStore,
    format: ObjectFormat,
    fetch_head_source: &str,
    log_all_ref_updates: bool,
    updates: &[FetchRefUpdate],
    ref_hook: Option<&dyn sley_refs::ReferenceTransactionHook>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut tx = store.transaction();
    if let Some(hook) = ref_hook {
        tx = tx.with_hook(hook);
    }
    for update in updates {
        let Some(dst) = update.dst.as_deref() else {
            continue;
        };
        if !seen.insert(dst.to_string()) {
            return Err(GitError::Transaction(format!("duplicate fetch ref {dst}")));
        }
        let old_oid = match store.read_ref(dst)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            Some(RefTarget::Symbolic(target)) => {
                return Err(GitError::Transaction(format!(
                    "fetch ref {dst} would overwrite symbolic ref {target}"
                )));
            }
            None => None,
        };
        let reflog = if log_all_ref_updates && fetch_should_write_reflog(dst) {
            Some(ReflogEntry {
                old_oid: old_oid.unwrap_or_else(|| ObjectId::null(format)),
                new_oid: update.oid,
                committer: fetch_reflog_committer(),
                message: fetch_reflog_message(fetch_head_source, update, old_oid.is_some()),
            })
        } else {
            None
        };
        tx.update(RefUpdate {
            name: dst.to_string(),
            expected: old_oid.map(RefTarget::Direct),
            new: RefTarget::Direct(update.oid),
            reflog,
        });
    }
    tx.commit()
}

/// Effective fetch pack input cap, mirroring git's `fetch.maxInputSize` with
/// `transfer.maxSize` as the fallback when the fetch-specific key is unset.
pub fn fetch_max_input_size(config: &GitConfig) -> Option<u64> {
    config_max_input_size(config, "fetch", "maxInputSize")
        .or_else(|| config_max_input_size(config, "transfer", "maxSize"))
}

fn config_max_input_size(config: &GitConfig, section: &str, key: &str) -> Option<u64> {
    let raw = config.get(section, None, key)?;
    match sley_config::parse_config_int(raw) {
        Some(limit) if limit > 0 => Some(limit as u64),
        _ => None,
    }
}

fn fetch_log_all_ref_updates(config: &GitConfig) -> bool {
    match config.get("core", None, "logallrefupdates") {
        Some(value) => {
            let value = value.to_ascii_lowercase();
            matches!(value.as_str(), "true" | "yes" | "on" | "1" | "always")
        }
        None => false,
    }
}

fn fetch_should_write_reflog(refname: &str) -> bool {
    refname == "HEAD"
        || refname.starts_with("refs/heads/")
        || refname.starts_with("refs/remotes/")
        || refname.starts_with("refs/notes/")
}

fn fetch_reflog_committer() -> Vec<u8> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("Git Rs <sley@example.invalid> {seconds} +0000").into_bytes()
}

fn fetch_reflog_message(source: &str, update: &FetchRefUpdate, old_exists: bool) -> Vec<u8> {
    let src = fetch_reflog_short_ref(&update.src);
    let dst = update
        .dst
        .as_deref()
        .map(fetch_reflog_short_ref)
        .unwrap_or_else(|| update.src.clone());
    let action = if !old_exists {
        if update.src.starts_with("refs/tags/") {
            "storing tag"
        } else if update.src.starts_with("refs/heads/") {
            "storing head"
        } else {
            "storing ref"
        }
    } else if update.force {
        "forced-update"
    } else if update.src.starts_with("refs/tags/") {
        "updating tag"
    } else {
        "fast-forward"
    };
    format!("fetch {source} {src}:{dst}: {action}").into_bytes()
}

fn fetch_reflog_short_ref(refname: &str) -> String {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(short) = refname.strip_prefix(prefix) {
            return short.to_string();
        }
    }
    refname.to_string()
}

fn validate_fetch_ref_updates(
    git_dir: &Path,
    _format: ObjectFormat,
    store: &FileRefStore,
    update_head_ok: bool,
    updates: &[FetchRefUpdate],
) -> Result<()> {
    for update in updates {
        let Some(dst) = update.dst.as_deref() else {
            continue;
        };
        let old = match store.read_ref(dst)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            Some(RefTarget::Symbolic(target)) => {
                return Err(GitError::Transaction(format!(
                    "ref {dst} would overwrite symbolic ref {target}"
                )));
            }
            None => None,
        };
        if old.is_some()
            && !update_head_ok
            && dst.starts_with("refs/heads/")
            && let Some(worktree) = sley_worktree::find_shared_symref(git_dir, "HEAD", dst)?
        {
            return Err(GitError::InvalidFormat(format!(
                "fatal: refusing to fetch into branch '{dst}' checked out at '{}'",
                worktree.path.display()
            )));
        }
        if old.is_some()
            && old != Some(update.oid)
            && dst.starts_with("refs/tags/")
            && !update.force
        {
            return Err(GitError::Command(format!(
                "! [rejected]        {} -> {}  (would clobber existing tag)",
                update.src, dst
            )));
        }
    }
    Ok(())
}

/// The remote's advertised `HEAD` symref target (`HEAD:<target>` capability).
fn head_symref_from_features(symrefs: &[String]) -> Option<String> {
    symrefs
        .iter()
        .find_map(|entry| entry.strip_prefix("HEAD:").map(|target| target.to_string()))
}

/// Apply the configured `remote.<name>.tagopt` unless the tag option was set
/// explicitly on the command line.
pub fn apply_configured_remote_tag_option(
    config: &GitConfig,
    source: &str,
    options: &mut FetchOptions,
) {
    if options.tag_option_explicit || !remote_exists(config, source) {
        return;
    }
    match remote_config_values(config, source, "tagopt")
        .into_iter()
        .last()
        .as_deref()
    {
        Some("--tags") => {
            options.auto_follow_tags = true;
            options.fetch_all_tags = true;
        }
        Some("--no-tags") => {
            options.auto_follow_tags = false;
            options.fetch_all_tags = false;
        }
        _ => {}
    }
}

/// Apply the configured `remote.<name>.prune` (then `fetch.prune`) unless the
/// prune option was set explicitly on the command line.
pub fn apply_configured_fetch_prune_option(
    config: &GitConfig,
    source: &str,
    options: &mut FetchOptions,
) {
    if !options.prune_option_explicit {
        if let Some(prune) = config.get_bool("remote", Some(source), "prune") {
            options.prune = prune;
        } else if let Some(prune) = config.get_bool("fetch", None, "prune") {
            options.prune = prune;
        }
    }
    if !options.prune_tags_option_explicit {
        if let Some(prune_tags) = config.get_bool("remote", Some(source), "prunetags") {
            options.prune_tags = prune_tags;
        } else if let Some(prune_tags) = config.get_bool("fetch", None, "prunetags") {
            options.prune_tags = prune_tags;
        }
    }
}

/// The effective refspec list for a fetch: explicit `refspecs`, else the
/// `configured` remote refspecs, else `HEAD`; with `refs/tags/*` appended when
/// fetching all tags.
pub fn fetch_refspecs_for_source(
    configured: Vec<String>,
    refspecs: &[String],
    fetch_all_tags: bool,
) -> Vec<String> {
    let mut effective = if !refspecs.is_empty() {
        refspecs.to_vec()
    } else if configured.is_empty() {
        vec!["HEAD".to_string()]
    } else {
        configured
    };
    if fetch_all_tags {
        effective.push("refs/tags/*:refs/tags/*".to_string());
    }
    effective
}

fn prune_refspecs_for_source(
    configured: &[String],
    refspecs: &[String],
    prune_tags: bool,
) -> Vec<String> {
    let mut effective = if !refspecs.is_empty() {
        refspecs.to_vec()
    } else {
        configured.to_vec()
    };
    if prune_tags && refspecs.is_empty() {
        effective.push("refs/tags/*:refs/tags/*".to_string());
    }
    effective
}

/// Whether a refspec (with source `src`) already covers `merge_src` — the test
/// `add_merge_config` makes before fetching a `branch.<x>.merge` ref separately.
/// A pattern source (`refs/heads/*`) covers any ref whose name fits the
/// prefix/suffix; a literal source matches by git's abbreviated `refname_match`.
fn refspec_source_covers(refspec: &RefSpec, src: &str, merge_src: &str) -> bool {
    if refspec.pattern {
        let Some((prefix, suffix)) = src.split_once('*') else {
            return false;
        };
        // A `branch.<x>.merge` value may be abbreviated (`two` for
        // `refs/heads/two`); git's `refname_match` resolves it against the
        // ref-map entry the glob produced. Test the merge ref both verbatim and
        // qualified under `refs/heads/`, the namespace branch merges live in.
        let fits = |name: &str| {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        };
        fits(merge_src) || fits(&format!("refs/heads/{merge_src}"))
    } else {
        refname_matches(merge_src, src) || refname_matches(src, merge_src)
    }
}

/// Mark tag refspec updates (`refs/tags/X:refs/tags/X`) as not-for-merge.
pub fn mark_tag_refspec_updates_not_for_merge(updates: &mut [FetchRefUpdate]) {
    for update in updates {
        if update.src.starts_with("refs/tags/") && update.dst.as_deref() == Some(&update.src) {
            update.not_for_merge = true;
        }
    }
}

/// Drop auto-followed tags that already exist locally, keeping only missing ones.
pub fn retain_missing_auto_follow_tags(
    store: &FileRefStore,
    updates: &mut Vec<FetchRefUpdate>,
) -> Result<()> {
    let mut retained = Vec::with_capacity(updates.len());
    for update in updates.drain(..) {
        if update.not_for_merge
            && update.src.starts_with("refs/tags/")
            && update.dst.as_deref() == Some(&update.src)
            && store.read_ref(&update.src)?.is_some()
        {
            continue;
        }
        retained.push(update);
    }
    *updates = retained;
    Ok(())
}

/// Append tags reachable from the fetched (non-tag) commits, using the remote
/// object database to test reachability.
pub fn append_reachable_auto_follow_tags(
    advertisements: &[RefAdvertisement],
    remote_db: &FileObjectDatabase,
    local_db: Option<&FileObjectDatabase>,
    format: ObjectFormat,
    refspecs: &[RefSpec],
    updates: &mut Vec<FetchRefUpdate>,
    deepen_excluded: Option<&HashSet<ObjectId>>,
) -> Result<()> {
    if !updates.iter().any(|update| update.dst.is_some()) {
        return Ok(());
    }
    // Drop any auto-follow tag entries the shared planner added: when we have the
    // remote object database we are the authoritative tag follower (we peel
    // annotated tags) and we re-add the full set sorted by refname, mirroring
    // git's `find_non_local_tags`, which inserts into a sorted string-list.
    updates.retain(|update| {
        !(update.src.starts_with("refs/tags/")
            && update.dst.as_deref() == Some(update.src.as_str())
            && update.not_for_merge)
    });
    // Reachability seeds are every object we're fetching (git's `fetch_oids`):
    // non-tag tips directly, and tag updates by their peeled target so an
    // explicitly-requested `tag <name>` still seeds the auto-follow of its
    // siblings.
    let mut starts = Vec::new();
    for update in updates.iter().filter(|update| update.dst.is_some()) {
        if update.src.starts_with("refs/tags/") {
            if let Some(target) = peel_tag_target(remote_db, format, &update.oid)? {
                starts.push(target);
            } else {
                starts.push(update.oid);
            }
        } else {
            starts.push(update.oid);
        }
    }
    // A deepen fetch must not auto-follow tags past the shallow boundary: only
    // tags whose target lands in the truncated pack are followed (upstream's
    // include-tag packs a tag only when its referenced object is packed).
    let reachable = match deepen_excluded {
        Some(excluded) => {
            collect_reachable_object_ids_excluding(remote_db, format, starts, excluded)?
        }
        None => collect_reachable_object_ids(remote_db, format, starts)?,
    };
    let fetched_srcs = updates
        .iter()
        .map(|update| update.src.clone())
        .collect::<HashSet<_>>();
    let mut followed = Vec::new();
    for reference in advertisements {
        if !reference.name.starts_with("refs/tags/")
            || fetched_srcs.contains(&reference.name)
            || fetch_refspec_excludes(refspecs, &reference.name)?
        {
            continue;
        }
        // A tag is auto-followed when the object it ultimately points at is
        // either among the objects being fetched (reachable from a fetched tip)
        // or already present in the local object database (git's
        // `find_non_local_tags`: `oidset_contains(fetch_oids) || odb_has_object`).
        // For lightweight tags the target is the advertised oid; for annotated
        // tags it is the peeled target (the tag object is never reachable from a
        // commit, so peel through the chain).
        let target = peel_tag_target(remote_db, format, &reference.oid)?.unwrap_or(reference.oid);
        let fetched = reachable.contains(&reference.oid) || reachable.contains(&target);
        let present_locally = local_db
            .map(|db| db.contains(&target))
            .transpose()?
            .unwrap_or(false);
        if !fetched && !present_locally {
            continue;
        }
        followed.push(FetchRefUpdate {
            src: reference.name.clone(),
            dst: Some(reference.name.clone()),
            oid: reference.oid,
            not_for_merge: true,
            force: false,
        });
    }
    followed.sort_by(|a, b| a.src.cmp(&b.src));
    updates.extend(followed);
    Ok(())
}

/// Peel an annotated-tag object to the non-tag object it ultimately references,
/// following nested tag chains. Returns `None` if `oid` is not an annotated tag
/// (a lightweight tag points directly at its target, already the advertised oid)
/// or cannot be read from `db`.
fn peel_tag_target(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let mut current = *oid;
    let mut peeled = None;
    loop {
        let Ok(object) = db.read_object(&current) else {
            return Ok(peeled);
        };
        if object.object_type != sley_object::ObjectType::Tag {
            return Ok(peeled);
        }
        let tag = sley_object::Tag::parse(format, &object.body)?;
        current = tag.object;
        peeled = Some(current);
    }
}

/// Whether any negative refspec excludes `name`.
pub fn fetch_refspec_excludes(refspecs: &[RefSpec], name: &str) -> Result<bool> {
    for refspec in refspecs.iter().filter(|refspec| refspec.negative) {
        if refspec.pattern {
            if refspec_map_source(refspec, name)?.is_some() {
                return Ok(true);
            }
        } else if refspec.src.as_deref() == Some(name) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reorder updates so a bundle `--tags` fetch lists non-tags, then tags pointing
/// at fetched commits, then the remaining tags (matching git's ordering).
pub fn order_bundle_fetch_all_tags_updates(updates: &mut Vec<FetchRefUpdate>) {
    let followed_oids = updates
        .iter()
        .filter(|update| !update.src.starts_with("refs/tags/") && update.dst.is_some())
        .map(|update| update.oid)
        .collect::<HashSet<_>>();
    if followed_oids.is_empty() {
        return;
    }

    let mut non_tags = Vec::new();
    let mut followed_tags = Vec::new();
    let mut other_tags = Vec::new();
    for update in updates.drain(..) {
        if update.src.starts_with("refs/tags/") {
            if followed_oids.contains(&update.oid) {
                followed_tags.push(update);
            } else {
                other_tags.push(update);
            }
        } else {
            non_tags.push(update);
        }
    }
    updates.extend(non_tags);
    updates.extend(followed_tags);
    updates.extend(other_tags);
}

/// Write a single default `FETCH_HEAD` record (a bare `HEAD` fetch).
pub fn write_default_fetch_head(
    git_dir: &Path,
    source: &str,
    oid: ObjectId,
    append: bool,
) -> Result<()> {
    let records = [FetchHeadRecord {
        oid,
        not_for_merge: false,
        description: source.to_string(),
    }];
    write_fetch_head_records(git_dir, &records, append)?;
    Ok(())
}

/// Write `FETCH_HEAD` records, truncating or appending per `append`.
pub fn write_fetch_head_records(
    git_dir: &Path,
    records: &[FetchHeadRecord],
    append: bool,
) -> Result<()> {
    let encoded = encode_fetch_head(records)?;
    if append {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(git_dir.join("FETCH_HEAD"))?;
        file.write_all(&encoded)?;
    } else {
        fs::write(git_dir.join("FETCH_HEAD"), encoded)?;
    }
    Ok(())
}

/// Write `FETCH_HEAD` from fetched ref updates, describing each by `description`.
pub fn write_fetch_head(
    git_dir: &Path,
    description: &str,
    fetched: &[FetchRefUpdate],
    append: bool,
) -> Result<()> {
    let records = fetch_ref_updates_to_fetch_head(fetched, description)?;
    write_fetch_head_records(git_dir, &records, append)?;
    Ok(())
}

/// The `FETCH_HEAD` source description for `source`: its configured URL (rewritten
/// per `url.<base>.insteadOf`) if any, otherwise the rewritten `source`.
pub fn fetch_head_source_description(config: &GitConfig, source: &str) -> String {
    let url = remote_config_values(config, source, "url")
        .into_iter()
        .next()
        .map(|url| rewrite_url_with_config(config, &url, false))
        .unwrap_or_else(|| rewrite_url_with_config(config, source, false));
    redact_url_for_display(&trim_fetch_head_display_url(&url))
}

/// Mirror git's `display_state` URL trimming (builtin/fetch.c): strip trailing
/// slashes and a trailing `.git` so the `FETCH_HEAD` note reads `branch 'x' of
/// ../` rather than `branch 'x' of ../.git/`.
fn trim_fetch_head_display_url(url: &str) -> String {
    let bytes = url.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    // `end` is the length excluding trailing slashes; git's `i` (index of the
    // last non-slash byte) is `end - 1`, and it strips `.git` only when `i > 4`.
    if end > 5 && &bytes[end - 4..end] == b".git" {
        end -= 4;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Prune refs whose destinations are covered by the active fetch refspecs and
/// whose corresponding remote sources are absent from `advertisements`,
/// deleting them and emitting git's notice lines through `progress` (unless
/// `quiet`). Returns the refs that were pruned.
pub struct PruneRefsInput<'a> {
    pub config: &'a GitConfig,
    pub store: &'a FileRefStore,
    pub remote: &'a str,
    pub advertisements: &'a [RefAdvertisement],
    pub refspecs: &'a [RefSpec],
    pub dry_run: bool,
    pub quiet: bool,
}

pub fn prune_refs_from_advertisements(
    input: PruneRefsInput<'_>,
    progress: &mut dyn ProgressSink,
) -> Result<Vec<PrunedRef>> {
    let remote_refs = input
        .advertisements
        .iter()
        .filter(|advertisement| !advertisement.name.ends_with("^{}"))
        .map(|advertisement| advertisement.name.as_str())
        .collect::<BTreeSet<_>>();
    let local_refs = input.store.list_refs()?;
    let stale_refs = stale_refs_for_prune(&local_refs, input.refspecs, &remote_refs)?;
    if stale_refs.is_empty() {
        return Ok(Vec::new());
    }
    let mut emit = |line: &str| {
        if !input.quiet {
            progress.message(line);
        }
    };
    let display_url = redact_url_for_display(
        &remote_config_values(input.config, input.remote, "url")
            .into_iter()
            .next()
            .unwrap_or_else(|| input.remote.into()),
    );
    emit(&format!("Pruning {}", input.remote));
    emit(&format!("URL: {display_url}"));
    let mut pruned = Vec::new();
    for refname in stale_refs {
        if !input.dry_run {
            match input.store.read_ref(&refname)? {
                Some(RefTarget::Symbolic(_)) => {
                    let _ = input.store.delete_symbolic_ref(&refname)?;
                }
                Some(RefTarget::Direct(_)) => {
                    let _ = input.store.delete_ref(&refname)?;
                }
                None => {}
            }
        }
        let display = prettify_pruned_ref(input.remote, &refname);
        let action = if input.dry_run {
            "would prune"
        } else {
            "pruned"
        };
        emit(&format!(" * [{action}] {display}"));
        let branch = display;
        pruned.push(PrunedRef { branch, refname });
    }
    Ok(pruned)
}

fn stale_refs_for_prune(
    local_refs: &[Ref],
    refspecs: &[RefSpec],
    remote_refs: &BTreeSet<&str>,
) -> Result<Vec<String>> {
    let mut stale = Vec::new();
    for reference in local_refs {
        if matches!(reference.target, RefTarget::Symbolic(_)) {
            continue;
        }
        let sources = prune_sources_for_destination(refspecs, &reference.name)?;
        if sources.is_empty() {
            continue;
        }
        if sources
            .iter()
            .all(|source| !remote_refs.contains(source.as_str()))
        {
            stale.push(reference.name.clone());
        }
    }
    stale.sort();
    Ok(stale)
}

fn prune_sources_for_destination(refspecs: &[RefSpec], destination: &str) -> Result<Vec<String>> {
    let mut sources = Vec::new();
    for refspec in refspecs.iter().filter(|refspec| !refspec.negative) {
        let Some(src) = refspec.src.as_deref() else {
            continue;
        };
        let Some(dst) = refspec.dst.as_deref() else {
            continue;
        };
        if refspec.pattern {
            let Some((dst_prefix, dst_suffix)) = dst.split_once('*') else {
                continue;
            };
            let Some(middle) = destination
                .strip_prefix(dst_prefix)
                .and_then(|value| value.strip_suffix(dst_suffix))
            else {
                continue;
            };
            let (src_prefix, src_suffix) = src.split_once('*').ok_or_else(|| {
                GitError::InvalidFormat("pattern refspec source is missing wildcard".into())
            })?;
            sources.push(format!("{src_prefix}{middle}{src_suffix}"));
        } else if dst == destination {
            sources.push(src.to_string());
        }
    }
    sources.sort();
    sources.dedup();
    Ok(sources)
}

fn prettify_pruned_ref(remote: &str, refname: &str) -> String {
    if let Some(branch) = refname.strip_prefix(&format!("refs/remotes/{remote}/")) {
        return format!("{remote}/{branch}");
    }
    if let Some(tag) = refname.strip_prefix("refs/tags/") {
        return tag.to_string();
    }
    refname.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use sley_config::{ConfigEntry, ConfigSection};
    use sley_formats::RepositoryLayout;
    use sley_object::{Commit, EncodedObject, ObjectType, Tree};
    use sley_odb::{FileObjectDatabase, ObjectWriter};
    use sley_refs::{RefTarget, RefUpdate};

    use crate::{NoCredentials, SilentProgress};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sley-remote-fetch-{name}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        RepositoryLayout::init_at(&dir, ObjectFormat::Sha1, false)
            .expect("test repository should initialize");
        dir.join(".git")
    }

    fn commit_on(git_dir: &Path, branch: &str, message: &str) -> ObjectId {
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree { entries: vec![] }.write(),
            ))
            .expect("tree should write");
        let identity = b"Test User <test@example.invalid> 1 +0000".to_vec();
        let oid = db
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                Commit {
                    tree,
                    parents: Vec::new(),
                    author: identity.clone(),
                    committer: identity,
                    encoding: None,
                    message: format!("{message}\n").into_bytes(),
                }
                .write(),
            ))
            .expect("commit should write");
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
        tx.commit().expect("refs should update");
        oid
    }

    fn default_options() -> FetchOptions {
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
            depth: None,
            merge_srcs: Vec::new(),
            filter: None,
            refetch: false,
            cloning: false,
            record_promisor_refs: true,
            update_shallow: false,
            deepen_relative: false,
            update_head_ok: false,
            deepen_since: None,
            deepen_not: Vec::new(),
            ssh_options: None,
            atomic: false,
        }
    }

    #[test]
    fn local_fetch_installs_pack_updates_ref_and_fetch_head() {
        let remote = temp_repo("remote");
        let local = temp_repo("local");
        let tip = commit_on(&remote, "main", "remote tip");
        let source = FetchSource::Local {
            git_dir: remote.clone(),
            common_git_dir: remote.clone(),
        };
        let refspecs = vec!["refs/heads/main:refs/remotes/origin/main".to_string()];
        let options = default_options();
        let mut credentials = NoCredentials;
        let mut progress = SilentProgress;

        let outcome = fetch(
            FetchRequest {
                git_dir: &local,
                format: ObjectFormat::Sha1,
                config: &GitConfig::default(),
                remote_name: "origin",
                source: &source,
                refspecs: &refspecs,
                options: &options,
            },
            FetchServices {
                credentials: &mut credentials,
                progress: &mut progress,
                ref_hook: None,
            },
        )
        .expect("fetch should succeed");

        assert_eq!(outcome.ref_updates.len(), 1);
        assert!(outcome.wrote_fetch_head);
        let local_db = FileObjectDatabase::from_git_dir(&local, ObjectFormat::Sha1);
        assert!(local_db.contains(&tip).expect("contains should read"));
        let local_refs = FileRefStore::new(&local, ObjectFormat::Sha1);
        assert_eq!(
            local_refs
                .read_ref("refs/remotes/origin/main")
                .expect("ref should read"),
            Some(RefTarget::Direct(tip))
        );
        let fetch_head = fs::read_to_string(local.join("FETCH_HEAD")).expect("FETCH_HEAD exists");
        assert!(fetch_head.contains("origin"));
    }

    #[test]
    fn shallow_local_fetch_writes_depth_boundary_metadata() {
        let remote = temp_repo("remote-shallow");
        let local = temp_repo("local-shallow");
        let tip = commit_on(&remote, "main", "tip");
        let source = FetchSource::Local {
            git_dir: remote.clone(),
            common_git_dir: remote.clone(),
        };
        let mut options = default_options();
        options.depth = Some(1);
        let mut credentials = NoCredentials;
        let mut progress = SilentProgress;

        fetch(
            FetchRequest {
                git_dir: &local,
                format: ObjectFormat::Sha1,
                config: &GitConfig::default(),
                remote_name: "origin",
                source: &source,
                refspecs: &["refs/heads/main:refs/remotes/origin/main".to_string()],
                options: &options,
            },
            FetchServices {
                credentials: &mut credentials,
                progress: &mut progress,
                ref_hook: None,
            },
        )
        .expect("shallow fetch should succeed");

        assert_eq!(
            crate::shallow::read_shallow(&local, ObjectFormat::Sha1)
                .expect("shallow file should read"),
            vec![tip]
        );
    }

    fn pack_file_count(git_dir: &Path) -> usize {
        fs::read_dir(git_dir.join("objects/pack"))
            .expect("pack directory should read")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pack"))
            .count()
    }

    #[test]
    fn same_depth_shallow_local_fetch_does_not_install_pack() {
        let remote = temp_repo("remote-shallow-noop");
        let local = temp_repo("local-shallow-noop");
        let tip = commit_on(&remote, "main", "tip");
        let source = FetchSource::Local {
            git_dir: remote.clone(),
            common_git_dir: remote.clone(),
        };
        let mut options = default_options();
        options.depth = Some(1);
        let refspecs = ["refs/heads/main:refs/remotes/origin/main".to_string()];
        let mut credentials = NoCredentials;
        let mut progress = SilentProgress;

        fetch(
            FetchRequest {
                git_dir: &local,
                format: ObjectFormat::Sha1,
                config: &GitConfig::default(),
                remote_name: "origin",
                source: &source,
                refspecs: &refspecs,
                options: &options,
            },
            FetchServices {
                credentials: &mut credentials,
                progress: &mut progress,
                ref_hook: None,
            },
        )
        .expect("initial shallow fetch should succeed");
        let pack_count = pack_file_count(&local);
        let shallow = crate::shallow::read_shallow(&local, ObjectFormat::Sha1)
            .expect("shallow file should read");

        fetch(
            FetchRequest {
                git_dir: &local,
                format: ObjectFormat::Sha1,
                config: &GitConfig::default(),
                remote_name: "origin",
                source: &source,
                refspecs: &refspecs,
                options: &options,
            },
            FetchServices {
                credentials: &mut credentials,
                progress: &mut progress,
                ref_hook: None,
            },
        )
        .expect("same-depth shallow fetch should succeed");

        assert_eq!(pack_file_count(&local), pack_count);
        assert_eq!(
            crate::shallow::read_shallow(&local, ObjectFormat::Sha1)
                .expect("shallow file should read"),
            shallow
        );
        assert_eq!(shallow, vec![tip]);
    }

    #[test]
    fn fetch_head_source_description_redacts_embedded_credentials() {
        let config = GitConfig {
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin".into()),
                vec![ConfigEntry::new(
                    "url",
                    Some("https://user:pass@host/repo.git".into()),
                )],
            )],
            ..GitConfig::default()
        };
        assert_eq!(
            fetch_head_source_description(&config, "origin"),
            "https://<redacted>@host/repo"
        );
    }

    #[test]
    fn failed_local_fetch_does_not_partially_mutate_refs_or_fetch_head() {
        let remote = temp_repo("remote-missing");
        let local = temp_repo("local-missing");
        let old = commit_on(&local, "main", "old local");
        let bogus =
            ObjectId::from_hex(ObjectFormat::Sha1, &"11".repeat(20)).expect("valid bogus oid");
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        let mut tx = remote_refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(bogus),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/main".into()),
            reflog: None,
        });
        tx.commit().expect("remote bogus ref should write");
        let local_refs = FileRefStore::new(&local, ObjectFormat::Sha1);
        let mut tx = local_refs.transaction();
        tx.update(RefUpdate {
            name: "refs/remotes/origin/main".into(),
            expected: None,
            new: RefTarget::Direct(old),
            reflog: None,
        });
        tx.commit().expect("local tracking ref should write");
        let source = FetchSource::Local {
            git_dir: remote.clone(),
            common_git_dir: remote.clone(),
        };
        let options = default_options();
        let mut credentials = NoCredentials;
        let mut progress = SilentProgress;

        let err = fetch(
            FetchRequest {
                git_dir: &local,
                format: ObjectFormat::Sha1,
                config: &GitConfig::default(),
                remote_name: "origin",
                source: &source,
                refspecs: &["refs/heads/main:refs/remotes/origin/main".to_string()],
                options: &options,
            },
            FetchServices {
                credentials: &mut credentials,
                progress: &mut progress,
                ref_hook: None,
            },
        )
        .expect_err("fetch should fail before finalizing refs");

        assert!(err.to_string().contains("missing object"));
        assert_eq!(
            local_refs
                .read_ref("refs/remotes/origin/main")
                .expect("ref should read"),
            Some(RefTarget::Direct(old))
        );
        assert!(!local.join("FETCH_HEAD").exists());
    }

    fn config_from_ini(ini: &str) -> GitConfig {
        GitConfig::parse(ini.as_bytes()).expect("config should parse")
    }

    #[test]
    fn fetch_max_input_size_unset_means_unlimited() {
        assert_eq!(fetch_max_input_size(&GitConfig::default()), None);
    }

    #[test]
    fn fetch_max_input_size_honors_fetch_section() {
        let cfg = config_from_ini("[fetch]\n\tmaxInputSize = 64\n");
        assert_eq!(fetch_max_input_size(&cfg), Some(64));
    }

    #[test]
    fn fetch_max_input_size_falls_back_to_transfer_max_size() {
        let cfg = config_from_ini("[transfer]\n\tmaxSize = 1k\n");
        assert_eq!(fetch_max_input_size(&cfg), Some(1024));
    }

    #[test]
    fn fetch_max_input_size_prefers_fetch_over_transfer() {
        let cfg = config_from_ini(
            "[fetch]\n\tmaxInputSize = 64\n[transfer]\n\tmaxSize = 1k\n",
        );
        assert_eq!(fetch_max_input_size(&cfg), Some(64));
    }

    #[test]
    fn fetch_max_input_size_non_positive_means_unlimited() {
        let cfg = config_from_ini("[fetch]\n\tmaxInputSize = 0\n");
        assert_eq!(fetch_max_input_size(&cfg), None);
    }
}
