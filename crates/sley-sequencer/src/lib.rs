pub mod commit_message;
pub mod history;
pub mod rebase;
pub mod replay;
pub mod stash;

use sley_core::{GitError, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, Tag};
use sley_odb::FileObjectDatabase;
use sley_odb::ObjectReader;
use sley_odb::ObjectWriter;
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry};
use std::path::Path;

// Commit authoring primitives (`CommitCreate`, identity formatting, body
// serialization) are canonical in `sley_object` next to `Commit`; these
// re-exports keep every historical `sley_sequencer::*` path working.
pub use sley_object::{CommitCreate, encode_commit_object, format_commit_identity, format_commit_identity_bytes};

// Stage-B prerequisites: the patch/conflict primitives the inbound
// rebase/am/cherry-pick drive loops consume (21+ refs live in the CLI's
// rebase/am today), plus the rerere engine home. These are re-exports of the
// sley-diff-merge surface so sequencer callers resolve every historical
// `commands::*` symbol through this crate once the engines land.
pub use sley_diff_merge::{
    ConflictStyle, FilePatch, HunkLine, apply_file_patch,
    rerere::{
        MergeRrEntry, RerereConflict, RerereHooks, RerereReporter, RerereStageHook,
        StderrRerereReporter, is_rerere_enabled_with_config, repo_rerere as sequencer_repo_rerere,
        rerere_autoupdate_with_config,
    },
};

/// Sequencer-side `rerere.autoupdate` resolution seam.
///
/// The drive loops call this once per conflicted step to decide whether a clean
/// replay should be staged automatically (git's `rerere.autoupdate`, including
/// the `--rerere-autoupdate` command-line override).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowRerereAuto {
    /// Explicit `--[no-]rerere-autoupdate` override from the invoking command.
    pub override_value: Option<bool>,
}

impl AllowRerereAuto {
    /// Resolve against the effective configuration view (`rerere.autoupdate`;
    /// default off when unset).
    pub fn resolve(&self, config: &sley_config::GitConfig) -> bool {
        rerere_autoupdate_with_config(config, self.override_value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequencerCommand {
    Pick(ObjectId),
    Revert(ObjectId),
    Edit(ObjectId),
    Squash(ObjectId),
    Fixup(ObjectId),
    Exec(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencerTodo {
    pub commands: Vec<SequencerCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryOperation {
    Commit,
    CherryPick,
    Revert,
    Rebase,
    Bisect,
    Stash,
    Notes,
    History,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIndexOptions {
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
    pub reflog_message: Vec<u8>,
    /// `encoding` header value (`i18n.commitEncoding`); `None`/UTF-8 omits it.
    pub encoding: Option<Vec<u8>>,
    pub signature: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIndexResult {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub updated_ref: String,
    pub parent: Option<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCreate {
    pub object: ObjectId,
    pub object_type: ObjectType,
    pub name: Vec<u8>,
    pub tagger: Vec<u8>,
    pub message: Vec<u8>,
}

/// Write a commit built from `commit`'s validated fields.
///
/// Validation, serialization, and signature folding live in
/// [`sley_object::encode_commit_object`] so identity/body building has one home;
/// this seam remains here because [`ObjectWriter`] is an sley-odb trait and
/// sley-odb sits above sley-object.
pub fn create_commit(writer: &mut impl ObjectWriter, commit: CommitCreate) -> Result<ObjectId> {
    let object = sley_object::encode_commit_object(commit)?;
    writer.write_object(object)
}

pub fn create_annotated_tag(writer: &mut impl ObjectWriter, tag: TagCreate) -> Result<ObjectId> {
    if tag
        .name
        .iter()
        .chain(tag.tagger.iter())
        .any(|byte| matches!(*byte, b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(
            "tag name and tagger must not contain control bytes".into(),
        ));
    }
    let tag = Tag {
        object: tag.object,
        object_type: tag.object_type,
        name: tag.name,
        tagger: Some(tag.tagger),
        message: tag.message,
        raw_body: None,
    };
    writer.write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
}

pub fn commit_index(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    options: CommitIndexOptions,
) -> Result<CommitIndexResult> {
    let git_dir = git_dir.as_ref();
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    commit_tree_with_amend(git_dir, format, tree, options, false)
}

pub fn amend_index(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    options: CommitIndexOptions,
) -> Result<CommitIndexResult> {
    let git_dir = git_dir.as_ref();
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    commit_tree_with_amend(git_dir, format, tree, options, true)
}

/// Amend HEAD with an explicit tree (e.g. `commit --amend --only` without
/// pathspecs, which reuses HEAD's tree and ignores the index).
pub fn amend_tree(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    tree: ObjectId,
    options: CommitIndexOptions,
) -> Result<CommitIndexResult> {
    commit_tree_with_amend(git_dir, format, tree, options, true)
}

pub fn commit_tree_at_head(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    tree: ObjectId,
    options: CommitIndexOptions,
) -> Result<CommitIndexResult> {
    commit_tree_with_amend(git_dir, format, tree, options, false)
}

pub fn commit_tree_at_head_with_odb(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    tree: ObjectId,
    options: CommitIndexOptions,
    db: &FileObjectDatabase,
) -> Result<CommitIndexResult> {
    commit_tree_with_amend_with_odb(git_dir, format, tree, options, false, db)
}

fn commit_tree_with_amend(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    tree: ObjectId,
    options: CommitIndexOptions,
    amend: bool,
) -> Result<CommitIndexResult> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    commit_tree_with_amend_with_odb(git_dir, format, tree, options, amend, &db)
}

fn commit_tree_with_amend_with_odb(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    tree: ObjectId,
    options: CommitIndexOptions,
    amend: bool,
    db: &FileObjectDatabase,
) -> Result<CommitIndexResult> {
    let git_dir = git_dir.as_ref();
    let refs = FileRefStore::new(git_dir, format);
    let (updated_ref, parent) = head_update_target(&refs)?;
    let commit_parents = if amend {
        let Some(parent) = &parent else {
            return Err(GitError::not_found("commit to amend"));
        };
        let object = db.read_object(parent)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {}, found {}",
                parent,
                object.object_type.as_str()
            )));
        }
        Commit::parse_ref(format, &object.body)?.parents
    } else {
        parent.iter().cloned().collect()
    };
    let mut writer = db.clone();
    let oid = create_commit(
        &mut writer,
        CommitCreate {
            tree: tree.clone(),
            parents: commit_parents,
            author: options.author,
            committer: options.committer.clone(),
            message: options.message,
            encoding: options.encoding,
            signature: options.signature,
        },
    )?;
    let expected = parent.map(RefTarget::Direct);
    let old_oid = parent.unwrap_or(ObjectId::null(format));
    let reflog = refs
        .should_write_reflog_for_update(&updated_ref, false)?
        .then_some(ReflogEntry {
            old_oid,
            new_oid: oid,
            committer: options.committer,
            message: options.reflog_message,
        });
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: updated_ref.clone(),
        expected,
        new: RefTarget::Direct(oid),
        reflog,
    });
    tx.commit()?;
    Ok(CommitIndexResult {
        oid,
        tree,
        updated_ref,
        parent,
    })
}

pub fn commit_message_from_chunks(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        if idx != 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(chunk);
        out.push(b'\n');
    }
    out
}

fn head_update_target(refs: &FileRefStore) -> Result<(String, Option<ObjectId>)> {
    match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Ok((name, Some(oid))),
            Some(RefTarget::Symbolic(_)) => Err(GitError::InvalidFormat(
                "nested symbolic HEAD target is unsupported".into(),
            )),
            None => Ok((name, None)),
        },
        Some(RefTarget::Direct(oid)) => Ok(("HEAD".into(), Some(oid))),
        None => Ok(("HEAD".into(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;
    use sley_odb::ObjectDatabase;

    #[test]
    fn commit_identity_formats_raw_git_date() {
        let identity =
            format_commit_identity("Example User", "example@example.invalid", "@0 +0000")
                .expect("test operation should succeed");
        assert_eq!(identity, b"Example User <example@example.invalid> 0 +0000");
    }

    #[test]
    fn create_commit_writes_commit_object() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        let identity =
            format_commit_identity("Example User", "example@example.invalid", "@0 +0000")
                .expect("test operation should succeed");
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = create_commit(
            &mut db,
            CommitCreate {
                tree,
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                encoding: None,
                signature: None,
            },
        )
        .expect("test operation should succeed");
        assert_eq!(oid.to_hex(), "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15");
    }

    #[test]
    fn create_annotated_tag_writes_tag_object() {
        let target = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15",
        )
        .expect("test operation should succeed");
        let tagger = format_commit_identity("Example User", "example@example.invalid", "@0 +0000")
            .expect("test operation should succeed");
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = create_annotated_tag(
            &mut db,
            TagCreate {
                object: target,
                object_type: ObjectType::Commit,
                name: b"v1.0".to_vec(),
                tagger,
                message: b"release\n".to_vec(),
            },
        )
        .expect("test operation should succeed");
        assert_eq!(oid.to_hex(), "b9c6a18e58a4efa0a5c023bcf0d8f2a320ae4098");
    }
}
