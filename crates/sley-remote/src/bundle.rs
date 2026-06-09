//! Callable fetch orchestration for `.bundle` files.
//!
//! [`fetch_bundle`] installs objects from a parsed [`Bundle`] and applies the
//! requested refspec map, mirroring `git fetch` against a bundle path. Everything
//! is taken as explicit parameters — `git_dir`, the [`ObjectFormat`], the bundle
//! path (for `FETCH_HEAD` descriptions), the parsed bundle, refspecs, and
//! [`FetchOptions`] — so it never reads process-global state, parses arguments, or
//! prints.

use std::path::Path;

use sley_core::{GitError, ObjectFormat, Result};
use sley_formats::{Bundle, BundleReference};
use sley_odb::{FileObjectDatabase, install_bundle_pack, verify_bundle_prerequisites};
use sley_protocol::{
    FetchHeadRecord, FetchRefUpdate, RefAdvertisement, parse_refspec, plan_fetch_ref_updates,
};
use sley_refs::{BundleRefUpdate, FileRefStore};

use crate::fetch::{
    FetchOptions, fetch_refspecs_for_source, mark_tag_refspec_updates_not_for_merge,
    order_bundle_fetch_all_tags_updates, retain_missing_auto_follow_tags, write_fetch_head,
    write_fetch_head_records,
};

/// Fully resolved inputs for a [`fetch_bundle`] run.
pub struct FetchBundleRequest<'a> {
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Bundle path or source string used for `FETCH_HEAD` descriptions.
    pub bundle_path: &'a str,
    /// Parsed bundle contents.
    pub bundle: &'a Bundle,
    /// Refspecs requested by the caller. Empty means fetch the bundle's default
    /// `HEAD` only (no ref updates).
    pub refspecs: &'a [String],
    /// Fetch behavior flags.
    pub options: &'a FetchOptions,
}

/// Fetch from a parsed `bundle` into the repository at `git_dir`.
///
/// Installs the bundle pack (unless `dry_run`), plans the ref-map for `refspecs`
/// (empty means write `FETCH_HEAD` for the bundle's `HEAD` only), writes
/// `FETCH_HEAD` when requested, and applies remote-tracking ref updates. Bundle
/// fetches have no shallow support; callers should warn-and-ignore a `--depth`
/// before calling.
pub fn fetch_bundle(request: FetchBundleRequest<'_>) -> Result<()> {
    let prerequisite_reader = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    let references = if request.options.dry_run {
        verify_bundle_prerequisites(request.bundle, &prerequisite_reader)?;
        request.bundle.references.clone()
    } else {
        let database = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
        install_bundle_pack(request.bundle, &prerequisite_reader, &database)?.references
    };
    if request.refspecs.is_empty() {
        if request.options.dry_run {
            return Ok(());
        }
        if request.options.write_fetch_head {
            let reference = bundle_default_fetch_reference(&references)?;
            write_bundle_default_fetch_head(
                request.git_dir,
                request.bundle_path,
                reference,
                request.options.append,
            )?;
        }
        return Ok(());
    }
    let refspecs = fetch_refspecs_for_source(
        Vec::new(),
        request.refspecs,
        request.options.fetch_all_tags,
    );
    let mut fetched =
        bundle_fetch_refs(&references, &refspecs, request.options.auto_follow_tags)?;
    if request.options.fetch_all_tags {
        mark_tag_refspec_updates_not_for_merge(&mut fetched);
        order_bundle_fetch_all_tags_updates(&mut fetched);
    }
    let store = FileRefStore::new(request.git_dir, request.format);
    if !request.options.fetch_all_tags {
        retain_missing_auto_follow_tags(&store, &mut fetched)?;
    }
    if request.options.dry_run {
        return Ok(());
    }
    if request.options.write_fetch_head {
        write_fetch_head(
            request.git_dir,
            request.bundle_path,
            &fetched,
            request.options.append,
        )?;
    }
    let updates = fetched
        .iter()
        .filter_map(|fetched| {
            fetched.dst.as_ref().map(|dst| BundleRefUpdate {
                name: dst.clone(),
                oid: fetched.oid,
            })
        })
        .collect::<Vec<_>>();
    store.apply_bundle_ref_updates(&updates, None)?;
    Ok(())
}

fn bundle_default_fetch_reference(references: &[BundleReference]) -> Result<&BundleReference> {
    references
        .iter()
        .find(|reference| reference.name == "HEAD")
        .ok_or_else(|| GitError::reference_not_found("remote ref HEAD"))
}

fn write_bundle_default_fetch_head(
    git_dir: &Path,
    bundle_path: &str,
    reference: &BundleReference,
    append: bool,
) -> Result<()> {
    let records = [FetchHeadRecord {
        oid: reference.oid,
        not_for_merge: false,
        description: bundle_path.to_string(),
    }];
    write_fetch_head_records(git_dir, &records, append)?;
    Ok(())
}

fn bundle_fetch_refs(
    references: &[BundleReference],
    refspecs: &[String],
    auto_follow_tags: bool,
) -> Result<Vec<FetchRefUpdate>> {
    let refs = references
        .iter()
        .map(|reference| RefAdvertisement {
            oid: reference.oid,
            name: reference.name.clone(),
            capabilities: Vec::new(),
        })
        .collect::<Vec<_>>();
    let refspecs = refspecs
        .iter()
        .map(|refspec| parse_refspec(refspec))
        .collect::<Result<Vec<_>>>()?;
    plan_fetch_ref_updates(&refs, &refspecs, auto_follow_tags)
}