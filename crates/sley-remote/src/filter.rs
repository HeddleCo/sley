//! User-facing `--filter` spec parsing for partial clones.

use std::path::Path;

use sley_core::{GitError, ObjectFormat, Result};
use sley_object::ObjectType;
use sley_odb::{FileObjectDatabase, ObjectReader, PackObjectFilter};
use sley_rev::revlist::{git_parse_blob_limit, parse_rev_list_tree_depth};

/// Parse a `--filter` spec (`blob:none`, `tree:N`, `blob:limit=…`, `combine:…`).
pub fn pack_filter_from_spec(spec: &str) -> Option<PackObjectFilter> {
    if let Some(parts) = spec.strip_prefix("combine:") {
        return parts
            .split('+')
            .filter_map(pack_filter_from_spec)
            .reduce(combine_pack_filters);
    }
    if spec == "blob:none" {
        return Some(PackObjectFilter::BlobNone);
    }
    if let Some(depth) = spec.strip_prefix("tree:") {
        return parse_rev_list_tree_depth(depth)
            .ok()
            .map(|depth| PackObjectFilter::TreeDepth(depth.min(u32::MAX as usize) as u32));
    }
    spec.strip_prefix("blob:limit=")
        .and_then(git_parse_blob_limit)
        .map(PackObjectFilter::BlobLimit)
}

/// Like [`pack_filter_from_spec`], but also resolves `sparse:oid=…` against a
/// local repository (used during clone when the source is on disk).
pub fn pack_filter_from_spec_for_clone(
    spec: &str,
    remote_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<PackObjectFilter>> {
    if let Some(body) = spec.strip_prefix("sparse:oid=") {
        return sparse_filter_from_remote(body, remote_git_dir, format).map(Some);
    }
    Ok(pack_filter_from_spec(spec))
}

fn sparse_filter_from_remote(
    body: &str,
    remote_git_dir: &Path,
    format: ObjectFormat,
) -> Result<PackObjectFilter> {
    let Some((rev, path)) = body.split_once(':') else {
        return Err(GitError::InvalidFormat(format!(
            "fatal: unable to parse sparse filter data in .{body}"
        )));
    };
    let db = FileObjectDatabase::from_git_dir(remote_git_dir, format);
    let oid = sley_rev::resolve_rev_path(remote_git_dir, format, &db, rev, path).map_err(|_| {
        GitError::InvalidFormat(format!("fatal: unable to access sparse blob in .{body}"))
    })?;
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidFormat(format!(
            "fatal: unable to parse sparse filter data in .{body}"
        )));
    }
    let contents = String::from_utf8_lossy(&object.body);
    let paths = contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix('/').unwrap_or(line);
            (!line.is_empty()).then(|| line.to_string())
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(GitError::InvalidFormat(format!(
            "fatal: unable to parse sparse filter data in .{body}"
        )));
    }
    Ok(PackObjectFilter::SparsePathSet(paths))
}

pub(crate) fn combine_pack_filters(
    left: PackObjectFilter,
    right: PackObjectFilter,
) -> PackObjectFilter {
    match (left, right) {
        (PackObjectFilter::TreeDepth(a), PackObjectFilter::TreeDepth(b)) => {
            PackObjectFilter::TreeDepth(a.min(b))
        }
        (PackObjectFilter::TreeDepth(depth), _) | (_, PackObjectFilter::TreeDepth(depth)) => {
            PackObjectFilter::TreeDepth(depth)
        }
        (PackObjectFilter::SparsePathSet(paths), _)
        | (_, PackObjectFilter::SparsePathSet(paths)) => PackObjectFilter::SparsePathSet(paths),
        (PackObjectFilter::BlobLimit(a), PackObjectFilter::BlobLimit(b)) => {
            PackObjectFilter::BlobLimit(a.min(b))
        }
        (PackObjectFilter::BlobNone, _) | (_, PackObjectFilter::BlobNone) => {
            PackObjectFilter::BlobNone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_filter_blob_none() {
        assert_eq!(
            pack_filter_from_spec("blob:none"),
            Some(PackObjectFilter::BlobNone)
        );
    }

    #[test]
    fn pack_filter_tree_depth() {
        assert_eq!(
            pack_filter_from_spec("tree:1"),
            Some(PackObjectFilter::TreeDepth(1))
        );
    }

    #[test]
    fn pack_filter_combine() {
        assert_eq!(
            pack_filter_from_spec("combine:blob:none+tree:2"),
            Some(PackObjectFilter::TreeDepth(2))
        );
    }
}
