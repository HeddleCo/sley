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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_config::remotes::{remote_config_values, remote_exists, rewrite_url_with_config};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::{
    FileObjectDatabase, collect_reachable_object_ids, collect_reachable_object_ids_excluding,
};
#[cfg(feature = "http")]
use sley_protocol::ProtocolVersion;
use sley_protocol::{
    FetchHeadRecord, FetchRefUpdate, RefAdvertisement, RefSpec, encode_fetch_head,
    fetch_ref_updates_to_fetch_head, parse_refspec, plan_fetch_ref_updates, refspec_map_source,
};
use sley_refs::{BundleRefUpdate, FileRefStore, Ref, RefTarget};
use sley_transport::RemoteUrl;

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
    /// Plan and report the fetch without installing objects or updating refs.
    pub dry_run: bool,
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
    /// Shallow fetch depth (`--depth N`): truncate history to `N` commits per tip.
    /// `None` is a full fetch. Honored by the HTTP and SSH transports and by the
    /// in-process local (`file://`/path) server, which computes the deepen
    /// boundary itself (see [`crate::local::compute_local_deepen`]).
    pub depth: Option<u32>,
    /// When fetching configured remote refspecs, mark the update whose `src`
    /// matches this value as eligible for merge in `FETCH_HEAD` (used by `pull`).
    pub merge_src: Option<String>,
    /// Partial-clone object filter (`--filter=blob:none`): omit filtered
    /// objects from the transferred pack. Local-only today: HTTP and SSH do not
    /// send `filter` requests yet, so callers that require network filtering
    /// must gate that before calling [`fetch`]. Directly-wanted tips are always
    /// packed on the local path, mirroring upstream's filter traversal.
    pub filter: Option<sley_odb::PackObjectFilter>,
    /// This fetch is a clone (`fetch_pack_args.cloning`): shallow points sent
    /// by a shallow server are accepted into `$GIT_DIR/shallow` unconditionally.
    pub cloning: bool,
    /// `--update-shallow`: accept new shallow points from a shallow server
    /// (otherwise refs whose history needs them are rejected).
    pub update_shallow: bool,
    /// `--deepen=N`: `depth` is relative to the client's current boundary.
    /// Local-only today; HTTP and SSH treat `depth` as an absolute `--depth N`.
    pub deepen_relative: bool,
    /// `--shallow-since=<date>`: deepen to commits newer than the date.
    /// Local-only today; HTTP and SSH do not send `deepen-since` yet.
    pub deepen_since: Option<i64>,
    /// `--shallow-exclude=<ref>`: deepen to commits not reachable from the ref
    /// (resolved on the remote; a non-ref is an error, like upstream).
    /// Local-only today; HTTP and SSH do not send `deepen-not` yet.
    pub deepen_not: Vec<String>,
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
    let mut options = request.options.clone();
    apply_configured_remote_tag_option(request.config, request.remote_name, &mut options);
    apply_configured_fetch_prune_option(request.config, request.remote_name, &mut options);
    let promisor_remote = request
        .config
        .get_bool("remote", Some(request.remote_name), "promisor")
        .unwrap_or(false);
    let configured_refspecs = if request.refspecs.is_empty() {
        remote_config_values(request.config, request.remote_name, "fetch")
    } else {
        Vec::new()
    };
    let default_head_fetch = request.refspecs.is_empty() && configured_refspecs.is_empty();
    let configured_remote_fetch = request.refspecs.is_empty() && !configured_refspecs.is_empty();
    let fetch_head_source = fetch_head_source_description(request.config, request.remote_name);
    let effective_refspecs = fetch_refspecs_for_source(
        configured_refspecs,
        request.refspecs,
        options.fetch_all_tags,
    );
    let parsed_refspecs = effective_refspecs
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
            let mut updates = plan_and_adjust_updates(FetchPlanInput {
                advertisements: &advertisements,
                refspecs: &parsed_refspecs,
                options: &options,
                store: &store,
                reachable: None,
                deepen_excluded: None,
                format: request.format,
                configured_remote_fetch,
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
                    store: &store,
                    options: &options,
                    remote_name: request.remote_name,
                    fetch_head_source: &fetch_head_source,
                    default_head_fetch,
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
            let (advertisements, features) =
                crate::ssh::ssh_upload_pack_advertisements(remote, request.format)?;
            outcome.head_symref = head_symref_from_features(&features.symrefs);
            let mut updates = plan_and_adjust_updates(FetchPlanInput {
                advertisements: &advertisements,
                refspecs: &parsed_refspecs,
                options: &options,
                store: &store,
                reachable: None,
                deepen_excluded: None,
                format: request.format,
                configured_remote_fetch,
            })?;
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
                },
            )?;
            if !options.dry_run {
                crate::shallow::apply_shallow_info(request.git_dir, request.format, &shallow_info)?;
            }
            finalize_fetch(
                FetchFinalize {
                    git_dir: request.git_dir,
                    store: &store,
                    options: &options,
                    remote_name: request.remote_name,
                    fetch_head_source: &fetch_head_source,
                    default_head_fetch,
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
            let mut updates = plan_and_adjust_updates(FetchPlanInput {
                advertisements: &advertisements,
                refspecs: &parsed_refspecs,
                options: &options,
                store: &store,
                reachable: Some((&remote_db, &advertisements)),
                deepen_excluded: deepen_plan.as_ref().map(|plan| &plan.excluded),
                format: request.format,
                configured_remote_fetch,
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
            let starts: Vec<ObjectId> = updates.iter().map(|update| update.oid).collect();
            let shallow_info = if starts.is_empty() && deepen_plan.is_none() {
                Vec::new()
            } else {
                crate::local::install_fetch_pack_via_local_upload_pack(
                    request.git_dir,
                    remote_git_dir,
                    request.format,
                    starts,
                    deepen_plan.as_ref(),
                    promisor_remote,
                    options.filter,
                    None,
                )?
            };
            if !options.dry_run {
                crate::shallow::apply_shallow_info(request.git_dir, request.format, &shallow_info)?;
            }
            finalize_fetch(
                FetchFinalize {
                    git_dir: request.git_dir,
                    store: &store,
                    options: &options,
                    remote_name: request.remote_name,
                    fetch_head_source: &fetch_head_source,
                    default_head_fetch,
                },
                &mut updates,
                &mut outcome,
            )?;
            advertisements
        }
    };

    if !options.dry_run && options.prune && remote_exists(request.config, request.remote_name) {
        outcome.pruned = prune_remote_tracking_refs_from_advertisements(
            request.config,
            &store,
            request.remote_name,
            &advertisements,
            options.quiet,
            services.progress,
        )?;
    }

    Ok(outcome)
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
    deepen_excluded: Option<&'a HashSet<ObjectId>>,
    format: ObjectFormat,
    configured_remote_fetch: bool,
}

fn plan_and_adjust_updates(input: FetchPlanInput<'_>) -> Result<Vec<FetchRefUpdate>> {
    let FetchPlanInput {
        advertisements,
        refspecs,
        options,
        store,
        reachable,
        deepen_excluded,
        format,
        configured_remote_fetch,
    } = input;
    let mut updates = plan_fetch_ref_updates(advertisements, refspecs, options.auto_follow_tags)?;
    if options.fetch_all_tags {
        mark_tag_refspec_updates_not_for_merge(&mut updates);
    } else {
        if options.auto_follow_tags
            && let Some((remote_db, advertisements)) = reachable
        {
            append_reachable_auto_follow_tags(
                advertisements,
                remote_db,
                format,
                refspecs,
                &mut updates,
                deepen_excluded,
            )?;
        }
        retain_missing_auto_follow_tags(store, &mut updates)?;
    }
    if configured_remote_fetch {
        for update in &mut updates {
            update.not_for_merge = true;
        }
        if let Some(merge_src) = &options.merge_src {
            for update in &mut updates {
                if update.src == *merge_src {
                    update.not_for_merge = false;
                }
            }
        }
    }
    Ok(updates)
}

/// Write `FETCH_HEAD`, apply the remote-tracking ref updates, and record the
/// applied updates in `outcome`. A no-op on `dry_run` (the pack is already
/// installed; refs and `FETCH_HEAD` are left untouched), matching the CLI.
struct FetchFinalize<'a> {
    git_dir: &'a Path,
    store: &'a FileRefStore,
    options: &'a FetchOptions,
    remote_name: &'a str,
    fetch_head_source: &'a str,
    default_head_fetch: bool,
}

fn finalize_fetch(
    finalize: FetchFinalize<'_>,
    updates: &mut Vec<FetchRefUpdate>,
    outcome: &mut FetchOutcome,
) -> Result<()> {
    let FetchFinalize {
        git_dir,
        store,
        options,
        remote_name,
        fetch_head_source,
        default_head_fetch,
    } = finalize;
    if options.dry_run {
        outcome.ref_updates = std::mem::take(updates);
        return Ok(());
    }
    if options.write_fetch_head {
        if default_head_fetch
            && updates.len() == 1
            && updates[0].src == "HEAD"
            && updates[0].dst.is_none()
        {
            write_default_fetch_head(git_dir, remote_name, updates[0].oid, options.append)?;
        } else {
            write_fetch_head(git_dir, fetch_head_source, updates, options.append)?;
        }
        outcome.wrote_fetch_head = true;
    }
    let ref_updates = updates
        .iter()
        .filter_map(|update| {
            update.dst.as_ref().map(|dst| BundleRefUpdate {
                name: dst.clone(),
                oid: update.oid,
            })
        })
        .collect::<Vec<_>>();
    store.apply_bundle_ref_updates(&ref_updates, None)?;
    outcome.ref_updates = std::mem::take(updates);
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
    if options.prune_option_explicit || !remote_exists(config, source) {
        return;
    }
    if let Some(prune) = config.get_bool("remote", Some(source), "prune") {
        options.prune = prune;
    } else if let Some(prune) = config.get_bool("fetch", None, "prune") {
        options.prune = prune;
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
    format: ObjectFormat,
    refspecs: &[RefSpec],
    updates: &mut Vec<FetchRefUpdate>,
    deepen_excluded: Option<&HashSet<ObjectId>>,
) -> Result<()> {
    if !updates.iter().any(|update| update.dst.is_some()) {
        return Ok(());
    }
    let starts = updates
        .iter()
        .filter(|update| update.dst.is_some() && !update.src.starts_with("refs/tags/"))
        .map(|update| update.oid);
    // A deepen fetch must not auto-follow tags past the shallow boundary: only
    // tags whose target lands in the truncated pack are followed (upstream's
    // include-tag packs a tag only when its referenced object is packed).
    let reachable = match deepen_excluded {
        Some(excluded) => {
            collect_reachable_object_ids_excluding(remote_db, format, starts, excluded)?
        }
        None => collect_reachable_object_ids(remote_db, format, starts)?,
    };
    let mut fetched_srcs = updates
        .iter()
        .map(|update| update.src.clone())
        .collect::<HashSet<_>>();
    for reference in advertisements {
        if !reference.name.starts_with("refs/tags/")
            || fetched_srcs.contains(&reference.name)
            || !reachable.contains(&reference.oid)
            || fetch_refspec_excludes(refspecs, &reference.name)?
        {
            continue;
        }
        fetched_srcs.insert(reference.name.clone());
        updates.push(FetchRefUpdate {
            src: reference.name.clone(),
            dst: Some(reference.name.clone()),
            oid: reference.oid,
            not_for_merge: true,
        });
    }
    Ok(())
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
    remote_config_values(config, source, "url")
        .into_iter()
        .next()
        .map(|url| rewrite_url_with_config(config, &url, false))
        .unwrap_or_else(|| rewrite_url_with_config(config, source, false))
}

/// Prune remote-tracking refs for `remote` that are absent from `advertisements`,
/// deleting them and emitting git's notice lines through `progress` (unless
/// `quiet`). Returns the refs that were pruned.
pub fn prune_remote_tracking_refs_from_advertisements(
    config: &GitConfig,
    store: &FileRefStore,
    remote: &str,
    advertisements: &[RefAdvertisement],
    quiet: bool,
    progress: &mut dyn ProgressSink,
) -> Result<Vec<PrunedRef>> {
    let remote_branches = advertisements
        .iter()
        .filter_map(|advertisement| advertisement.name.strip_prefix("refs/heads/"))
        .collect::<BTreeSet<_>>();
    let local_refs = store.list_refs()?;
    let stale_branches = remote_tracking_branch_names(&local_refs, remote)
        .into_iter()
        .filter(|branch| !remote_branches.contains(branch.as_str()))
        .collect::<Vec<_>>();
    if stale_branches.is_empty() {
        return Ok(Vec::new());
    }
    let mut emit = |line: &str| {
        if !quiet {
            progress.message(line);
        }
    };
    let display_url = remote_config_values(config, remote, "url")
        .into_iter()
        .next()
        .unwrap_or_else(|| remote.into());
    emit(&format!("Pruning {remote}"));
    emit(&format!("URL: {display_url}"));
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    let remote_prefix = format!("refs/remotes/{remote}/");
    let head_target = match store.read_ref(&remote_head)? {
        Some(RefTarget::Symbolic(target)) => Some(target),
        Some(RefTarget::Direct(_)) | None => None,
    };
    let mut pruned = Vec::new();
    for branch in stale_branches {
        let refname = format!("{remote_prefix}{branch}");
        match store.read_ref(&refname)? {
            Some(RefTarget::Symbolic(_)) => {
                let _ = store.delete_symbolic_ref(&refname)?;
            }
            Some(RefTarget::Direct(_)) => {
                let _ = store.delete_ref(&refname)?;
            }
            None => {}
        }
        emit(&format!(" * [pruned] {remote}/{branch}"));
        if head_target.as_deref() == Some(refname.as_str()) {
            let _ = store.delete_symbolic_ref(&remote_head)?;
            emit(&format!(
                " refs/remotes/{remote}/HEAD has become dangling after {refname} was deleted"
            ));
        }
        pruned.push(PrunedRef { branch, refname });
    }
    Ok(pruned)
}

/// Remote-tracking branch names under `refs/remotes/<name>/` (excluding `HEAD`).
fn remote_tracking_branch_names(refs: &[Ref], name: &str) -> Vec<String> {
    let prefix = format!("refs/remotes/{name}/");
    refs.iter()
        .filter_map(|reference| reference.name.strip_prefix(&prefix))
        .filter(|branch| *branch != "HEAD")
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

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
            dry_run: false,
            append: false,
            write_fetch_head: true,
            tag_option_explicit: true,
            prune_option_explicit: true,
            depth: None,
            merge_src: None,
            filter: None,
            cloning: false,
            update_shallow: false,
            deepen_relative: false,
            deepen_since: None,
            deepen_not: Vec::new(),
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
            },
        )
        .expect("shallow fetch should succeed");

        assert_eq!(
            crate::shallow::read_shallow(&local, ObjectFormat::Sha1)
                .expect("shallow file should read"),
            vec![tip]
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
}
