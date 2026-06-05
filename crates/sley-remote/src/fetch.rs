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
//! SSH and bundle fetch still live in the CLI; only HTTP and local move here. The
//! ref-map / `FETCH_HEAD` / prune helpers are shared (the CLI's SSH and bundle
//! paths call the same `pub` functions) so there is a single implementation.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::{collect_reachable_object_ids, FileObjectDatabase};
use sley_protocol::{
    encode_fetch_head, fetch_ref_updates_to_fetch_head, parse_refspec, plan_fetch_ref_updates,
    refspec_map_source, FetchHeadRecord, FetchRefUpdate, RefAdvertisement, RefSpec,
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
///
/// Shallow/depth is intentionally absent; it is wired in a later stage.
#[derive(Debug, Clone, Copy)]
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
#[allow(clippy::too_many_arguments)]
pub fn fetch(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    remote_name: &str,
    source: &FetchSource,
    refspecs: &[String],
    options: &FetchOptions,
    credentials: &mut dyn CredentialProvider,
    progress: &mut dyn ProgressSink,
) -> Result<FetchOutcome> {
    let mut options = *options;
    apply_configured_remote_tag_option(config, remote_name, &mut options);
    apply_configured_fetch_prune_option(config, remote_name, &mut options);
    let promisor_remote = config
        .get_bool("remote", Some(remote_name), "promisor")
        .unwrap_or(false);
    let configured_refspecs = if refspecs.is_empty() {
        remote_config_values(config, remote_name, "fetch")
    } else {
        Vec::new()
    };
    let default_head_fetch = refspecs.is_empty() && configured_refspecs.is_empty();
    let configured_remote_fetch = refspecs.is_empty() && !configured_refspecs.is_empty();
    let fetch_head_source = fetch_head_source_description(config, remote_name);
    let effective_refspecs =
        fetch_refspecs_for_source(configured_refspecs, refspecs, options.fetch_all_tags);
    let parsed_refspecs = effective_refspecs
        .iter()
        .map(|refspec| parse_refspec(refspec))
        .collect::<Result<Vec<_>>>()?;

    let store = FileRefStore::new(git_dir, format);
    let mut outcome = FetchOutcome::default();

    // Advertise refs, plan the ref-map, install the pack, then update refs/prune.
    // The two transports differ only in how they advertise and how they pull the
    // pack; the ref-map planning and ref bookkeeping are identical.
    let advertisements = match source {
        FetchSource::Http(remote) => {
            let client = crate::http::new_http_client();
            let (advertisements, features) =
                crate::http::http_upload_pack_advertisements(&client, remote, format, credentials)?;
            outcome.head_symref = head_symref_from_features(&features.symrefs);
            let mut updates = plan_and_adjust_updates(
                &advertisements,
                &parsed_refspecs,
                &options,
                &store,
                None,
                format,
                configured_remote_fetch,
            )?;
            let wants = updates.iter().map(|update| update.oid.clone()).collect();
            crate::http::install_fetch_pack_via_http_upload_pack(
                &client,
                git_dir,
                format,
                remote,
                wants,
                promisor_remote,
                credentials,
            )?;
            finalize_fetch(
                git_dir,
                &store,
                &mut updates,
                &options,
                remote_name,
                &fetch_head_source,
                default_head_fetch,
                &mut outcome,
            )?;
            advertisements
        }
        FetchSource::Ssh(remote) => {
            // SSH advertises and pulls the pack by spawning `ssh` (no credential
            // seam — the `ssh` program authenticates), but the ref-map planning
            // and ref bookkeeping are the same shared flow as HTTP.
            let (advertisements, features) =
                crate::ssh::ssh_upload_pack_advertisements(remote, format)?;
            outcome.head_symref = head_symref_from_features(&features.symrefs);
            let mut updates = plan_and_adjust_updates(
                &advertisements,
                &parsed_refspecs,
                &options,
                &store,
                None,
                format,
                configured_remote_fetch,
            )?;
            let wants = updates.iter().map(|update| update.oid.clone()).collect();
            crate::ssh::install_fetch_pack_via_ssh_upload_pack(
                git_dir,
                format,
                remote,
                &features,
                wants,
                promisor_remote,
            )?;
            finalize_fetch(
                git_dir,
                &store,
                &mut updates,
                &options,
                remote_name,
                &fetch_head_source,
                default_head_fetch,
                &mut outcome,
            )?;
            advertisements
        }
        FetchSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } => {
            let remote_format = crate::object_format_for_git_dir(remote_common_git_dir)?;
            if remote_format != format {
                return Err(GitError::InvalidObjectId(format!(
                    "remote repository uses {}, local repository uses {}",
                    remote_format.name(),
                    format.name()
                )));
            }
            let advertisements = crate::local::local_fetch_advertisements(remote_git_dir, format)?;
            let remote_db = FileObjectDatabase::from_git_dir(remote_common_git_dir, format);
            let mut updates = plan_and_adjust_updates(
                &advertisements,
                &parsed_refspecs,
                &options,
                &store,
                Some((&remote_db, &advertisements)),
                format,
                configured_remote_fetch,
            )?;
            let starts = updates.iter().map(|update| update.oid.clone()).collect();
            crate::local::install_fetch_pack_via_local_upload_pack(
                git_dir,
                remote_git_dir,
                format,
                starts,
                promisor_remote,
            )?;
            finalize_fetch(
                git_dir,
                &store,
                &mut updates,
                &options,
                remote_name,
                &fetch_head_source,
                default_head_fetch,
                &mut outcome,
            )?;
            advertisements
        }
    };

    if !options.dry_run && options.prune && remote_exists(config, remote_name) {
        outcome.pruned = prune_remote_tracking_refs_from_advertisements(
            config,
            &store,
            remote_name,
            &advertisements,
            options.quiet,
            progress,
        )?;
    }

    Ok(outcome)
}

/// Plan the ref-map and apply the auto-follow-tag / not-for-merge adjustments
/// shared by both transports. `reachable` (local only) enables appending tags
/// reachable from fetched commits via the remote object database.
#[allow(clippy::too_many_arguments)]
fn plan_and_adjust_updates(
    advertisements: &[RefAdvertisement],
    refspecs: &[RefSpec],
    options: &FetchOptions,
    store: &FileRefStore,
    reachable: Option<(&FileObjectDatabase, &[RefAdvertisement])>,
    format: ObjectFormat,
    configured_remote_fetch: bool,
) -> Result<Vec<FetchRefUpdate>> {
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
            )?;
        }
        retain_missing_auto_follow_tags(store, &mut updates)?;
    }
    if configured_remote_fetch {
        for update in &mut updates {
            update.not_for_merge = true;
        }
    }
    Ok(updates)
}

/// Write `FETCH_HEAD`, apply the remote-tracking ref updates, and record the
/// applied updates in `outcome`. A no-op on `dry_run` (the pack is already
/// installed; refs and `FETCH_HEAD` are left untouched), matching the CLI.
#[allow(clippy::too_many_arguments)]
fn finalize_fetch(
    git_dir: &Path,
    store: &FileRefStore,
    updates: &mut Vec<FetchRefUpdate>,
    options: &FetchOptions,
    remote_name: &str,
    fetch_head_source: &str,
    default_head_fetch: bool,
    outcome: &mut FetchOutcome,
) -> Result<()> {
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
            write_default_fetch_head(git_dir, remote_name, updates[0].oid.clone(), options.append)?;
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
                oid: update.oid.clone(),
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
) -> Result<()> {
    if !updates.iter().any(|update| update.dst.is_some()) {
        return Ok(());
    }
    let starts = updates
        .iter()
        .filter(|update| update.dst.is_some() && !update.src.starts_with("refs/tags/"))
        .map(|update| update.oid.clone());
    let reachable = collect_reachable_object_ids(remote_db, format, starts)?;
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
            oid: reference.oid.clone(),
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
        .map(|update| update.oid.clone())
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

/// Whether `name` is a configured remote.
fn remote_exists(config: &GitConfig, name: &str) -> bool {
    config
        .sections
        .iter()
        .any(|section| section.name == "remote" && section.subsection.as_deref() == Some(name))
}

/// All `remote.<name>.<key>` values, in config order.
fn remote_config_values(config: &GitConfig, name: &str, key: &str) -> Vec<String> {
    config
        .sections
        .iter()
        .filter(|section| section.name == "remote" && section.subsection.as_deref() == Some(name))
        .flat_map(|section| {
            section
                .entries
                .iter()
                .filter(move |entry| entry.key.eq_ignore_ascii_case(key))
                .filter_map(|entry| entry.value.clone())
        })
        .collect()
}

/// Rewrite `url` per the longest matching `url.<base>.insteadOf` (or
/// `pushInsteadOf` when `push`) prefix, mirroring git's `insteadOf` resolution.
fn rewrite_url_with_config(config: &GitConfig, url: &str, push: bool) -> String {
    let mut best: Option<(&str, &str, u8)> = None;
    for section in &config.sections {
        if section.name != "url" {
            continue;
        }
        let Some(base) = section.subsection.as_deref() else {
            continue;
        };
        for entry in &section.entries {
            let priority = if push && entry.key.eq_ignore_ascii_case("pushInsteadOf") {
                2
            } else if entry.key.eq_ignore_ascii_case("insteadOf") {
                1
            } else {
                continue;
            };
            let Some(prefix) = entry.value.as_deref() else {
                continue;
            };
            if !url.starts_with(prefix) {
                continue;
            }
            let replace = match best {
                None => true,
                Some((_, best_prefix, best_priority)) => {
                    priority > best_priority
                        || (priority == best_priority && prefix.len() > best_prefix.len())
                }
            };
            if replace {
                best = Some((base, prefix, priority));
            }
        }
    }
    if let Some((base, prefix, _)) = best {
        format!("{base}{}", &url[prefix.len()..])
    } else {
        url.to_string()
    }
}
