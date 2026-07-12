//! Read-only projections over index state.
//!
//! These operations own the backend semantics used by `ls-files`: parsing the
//! resolve-undo extension and projecting a historical tree over the current
//! index. Frontends remain responsible for pathspec filtering and rendering.

use crate::{BString, Index, Stage};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};

/// One path's saved pre-merge stages from the `REUC` extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveUndoRecord {
    pub path: BString,
    /// Stages 1, 2, and 3, in that order.
    pub stages: [Option<ResolveUndoStage>; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveUndoStage {
    pub mode: u32,
    pub oid: ObjectId,
}

/// Inputs to Git's `ls-files --with-tree` index projection.
#[derive(Debug, Clone, Copy)]
pub struct TreeOverlayOptions<'a> {
    /// The full (already sparse-expanded) current index.
    pub index: Option<&'a Index>,
    /// Leaf paths flattened from the requested historical tree.
    pub tree_paths: &'a [BString],
}

/// Ordered visible paths after overlaying a historical tree onto the index.
///
/// Paths may repeat when an unmerged current entry and a historical tree entry
/// share a name; that is the same multiplicity `ls-files --with-tree` exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeOverlayOutcome {
    pub paths: Vec<BString>,
}

impl Index {
    /// Parse the `REUC` extension into typed saved-stage records.
    pub fn resolve_undo_records(&self, format: ObjectFormat) -> Result<Vec<ResolveUndoRecord>> {
        parse_resolve_undo_records(self.extension(b"REUC")?, format)
    }
}

/// Project `<tree-ish>` paths over the current index using Git's
/// `overlay_tree_on_index` ordering and shadowing rules.
pub fn project_tree_overlay(options: TreeOverlayOptions<'_>) -> TreeOverlayOutcome {
    let mut entries = Vec::<OverlayEntry>::new();
    if let Some(index) = options.index {
        entries.extend(index.entries.iter().map(|entry| OverlayEntry {
            path: entry.path.clone(),
            // Git hoists every unmerged current entry to stage 3 before adding
            // the tree at stage 1.
            stage: if entry.stage() == Stage::Normal { 0 } else { 3 },
        }));
    }
    entries.extend(
        options
            .tree_paths
            .iter()
            .cloned()
            .map(|path| OverlayEntry { path, stage: 1 }),
    );
    entries.sort_by(|left, right| {
        left.path
            .as_bytes()
            .cmp(right.path.as_bytes())
            .then(left.stage.cmp(&right.stage))
    });

    let mut paths = Vec::with_capacity(entries.len());
    let mut last_stage_zero = None::<BString>;
    for entry in entries {
        if entry.stage == 0 {
            last_stage_zero = Some(entry.path.clone());
        } else if entry.stage == 1
            && last_stage_zero
                .as_ref()
                .is_some_and(|path| path == &entry.path)
        {
            continue;
        }
        paths.push(entry.path);
    }
    TreeOverlayOutcome { paths }
}

#[derive(Debug)]
struct OverlayEntry {
    path: BString,
    stage: u8,
}

fn parse_resolve_undo_records(
    body: Option<&[u8]>,
    format: ObjectFormat,
) -> Result<Vec<ResolveUndoRecord>> {
    let Some(body) = body else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        let path_end = body[offset..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| GitError::InvalidFormat("truncated REUC path".into()))?
            + offset;
        let path = BString::from(body[offset..path_end].to_vec());
        offset = path_end + 1;

        let mut modes = [0u32; 3];
        for mode in &mut modes {
            let mode_end = body[offset..]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| GitError::InvalidFormat("truncated REUC mode".into()))?
                + offset;
            let text = std::str::from_utf8(&body[offset..mode_end])
                .map_err(|_| GitError::InvalidFormat("invalid REUC mode".into()))?;
            *mode = u32::from_str_radix(text, 8)
                .map_err(|_| GitError::InvalidFormat("invalid REUC mode".into()))?;
            offset = mode_end + 1;
        }

        let mut stages = [None, None, None];
        for (idx, mode) in modes.into_iter().enumerate() {
            if mode == 0 {
                continue;
            }
            let end = offset
                .checked_add(format.raw_len())
                .ok_or_else(|| GitError::InvalidFormat("REUC oid length overflow".into()))?;
            if end > body.len() {
                return Err(GitError::InvalidFormat("truncated REUC oid".into()));
            }
            stages[idx] = Some(ResolveUndoStage {
                mode,
                oid: ObjectId::from_raw(format, &body[offset..end])?,
            });
            offset = end;
        }
        records.push(ResolveUndoRecord { path, stages });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{INDEX_FLAG_STAGE_MASK, IndexEntry};

    #[test]
    fn tree_overlay_sorts_hoists_unmerged_and_hides_stage_one_under_stage_zero() {
        let index = Index {
            version: 2,
            entries: vec![entry(b"b", 0), entry(b"a", 2)],
            extensions: Vec::new(),
            checksum: None,
        };
        let tree_paths = vec![
            BString::from(b"c".to_vec()),
            BString::from(b"b".to_vec()),
            BString::from(b"a".to_vec()),
        ];

        let outcome = project_tree_overlay(TreeOverlayOptions {
            index: Some(&index),
            tree_paths: &tree_paths,
        });

        assert_eq!(
            outcome.paths,
            [b"a".as_slice(), b"a", b"b", b"c"]
                .into_iter()
                .map(|path| BString::from(path.to_vec()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolve_undo_parses_sha1_and_sha256_stage_widths() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let oid =
                ObjectId::from_hex(format, &"1".repeat(format.hex_len())).expect("valid test oid");
            let mut body = b"conflicted\0".to_vec();
            body.extend_from_slice(b"100644\0");
            body.extend_from_slice(b"0\0");
            body.extend_from_slice(b"100755\0");
            body.extend_from_slice(oid.as_bytes());
            body.extend_from_slice(oid.as_bytes());

            let records = parse_resolve_undo_records(Some(&body), format)
                .expect("parse resolve-undo records");
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].path.as_bytes(), b"conflicted");
            assert_eq!(records[0].stages[0].expect("stage one").mode, 0o100644);
            assert!(records[0].stages[1].is_none());
            assert_eq!(records[0].stages[2].expect("stage three").mode, 0o100755);
            assert_eq!(records[0].stages[2].expect("stage three").oid, oid);
        }
    }

    #[test]
    fn resolve_undo_rejects_truncated_object_id() {
        let mut body = b"conflicted\0".to_vec();
        body.extend_from_slice(b"100644\0");
        body.extend_from_slice(b"0\0");
        body.extend_from_slice(b"0\0");
        body.extend_from_slice(&[0; 19]);
        let error = parse_resolve_undo_records(Some(&body), ObjectFormat::Sha1)
            .expect_err("truncated oid must fail");
        assert!(
            matches!(error, GitError::InvalidFormat(message) if message == "truncated REUC oid")
        );
    }

    fn entry(path: &[u8], stage: u16) -> IndexEntry {
        let mut flags = path.len() as u16;
        flags &= !INDEX_FLAG_STAGE_MASK;
        flags |= stage << 12;
        IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid: ObjectId::null(ObjectFormat::Sha1),
            flags,
            flags_extended: 0,
            path: BString::from(path.to_vec()),
        }
    }
}
