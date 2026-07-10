//! Revision resolution wrappers (ambiguous-ref warnings, treeish/commitish helpers).

use std::path::Path;

use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectPrefixResolution};
use sley::plumbing::sley_refs::FileRefStore;
use sley::{ObjectFormat, ObjectId, Result};

use crate::sley_rev;

pub(crate) fn rev_parse_symbolic_full_name(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<Option<String>> {
    sley_rev::resolve_revision_symbolic_full_name(git_dir, format, rev)
}

pub(crate) fn resolve_revision(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    let db = crate::repository::open_object_database(git_dir, format)?;
    sley_rev::RevisionResolver::new(git_dir, format, &db).resolve(rev)
}

pub(crate) fn resolve_revision_commitish(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    if is_short_hex_object_prefix(format, rev) {
        return sley_rev::resolve_short_object_id(
            git_dir,
            format,
            rev,
            sley_rev::ObjectDisambiguation::Commitish,
        )?
        .into_result(rev);
    }
    let db = crate::repository::open_object_database(git_dir, format)?;
    sley_rev::RevisionResolver::new(git_dir, format, &db).resolve(rev)
}

pub(crate) fn resolve_revision_treeish(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    warn_ambiguous_refname_for_object_prefix(git_dir, format, rev);
    if is_short_hex_object_prefix(format, rev) {
        return sley_rev::resolve_short_object_id(
            git_dir,
            format,
            rev,
            sley_rev::ObjectDisambiguation::Treeish,
        )?
        .into_result(rev);
    }
    let db = crate::repository::open_object_database(git_dir, format)?;
    sley_rev::RevisionResolver::new(git_dir, format, &db).resolve(rev)
}

fn is_short_hex_object_prefix(format: ObjectFormat, rev: &str) -> bool {
    rev.len() >= 4
        && rev.len() < format.hex_len()
        && rev.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn warn_ambiguous_refname_for_object_prefix(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
) {
    if rev.len() < 4
        || rev.len() > format.hex_len()
        || !rev.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !revision_ref_name_exists(git_dir, format, rev)
    {
        return;
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if matches!(
        db.resolve_prefix(rev),
        Ok(ObjectPrefixResolution::Unique(_) | ObjectPrefixResolution::Ambiguous(_))
    ) {
        eprintln!("warning: refname '{rev}' is ambiguous.");
    }
}

pub(crate) fn revision_ref_name_exists(git_dir: &Path, format: ObjectFormat, rev: &str) -> bool {
    let refs = FileRefStore::new(git_dir, format);
    if rev == "HEAD" {
        return refs.read_ref("HEAD").ok().flatten().is_some();
    }
    if rev.starts_with("refs/") {
        return refs.read_ref(rev).ok().flatten().is_some();
    }
    refs.read_ref(&format!("refs/heads/{rev}"))
        .ok()
        .flatten()
        .is_some()
        || refs
            .read_ref(&format!("refs/tags/{rev}"))
            .ok()
            .flatten()
            .is_some()
}

pub(crate) fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}
