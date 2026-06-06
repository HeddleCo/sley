use sley_core::{GitError, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, Tag};
use sley_odb::FileObjectDatabase;
use sley_odb::ObjectReader;
use sley_odb::ObjectWriter;
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry};
use std::path::Path;

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
pub struct CommitCreate {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIndexOptions {
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
    pub reflog_message: Vec<u8>,
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

pub fn create_commit(writer: &mut impl ObjectWriter, commit: CommitCreate) -> Result<ObjectId> {
    let format = commit.tree.format();
    for parent in &commit.parents {
        if parent.format() != format {
            return Err(GitError::InvalidObjectId(format!(
                "parent {parent} uses {}, tree uses {}",
                parent.format().name(),
                format.name()
            )));
        }
    }
    let commit = Commit {
        tree: commit.tree,
        parents: commit.parents,
        author: commit.author,
        committer: commit.committer,
        encoding: None,
        message: commit.message,
    };
    writer.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
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

pub fn commit_tree_at_head(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    tree: ObjectId,
    options: CommitIndexOptions,
) -> Result<CommitIndexResult> {
    commit_tree_with_amend(git_dir, format, tree, options, false)
}

fn commit_tree_with_amend(
    git_dir: impl AsRef<Path>,
    format: sley_core::ObjectFormat,
    tree: ObjectId,
    options: CommitIndexOptions,
    amend: bool,
) -> Result<CommitIndexResult> {
    let git_dir = git_dir.as_ref();
    let refs = FileRefStore::new(git_dir, format);
    let (updated_ref, parent) = head_update_target(&refs)?;
    let commit_parents = if amend {
        let Some(parent) = &parent else {
            return Err(GitError::NotFound("commit to amend".into()));
        };
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
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
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let oid = create_commit(
        &mut db,
        CommitCreate {
            tree: tree.clone(),
            parents: commit_parents,
            author: options.author,
            committer: options.committer.clone(),
            message: options.message,
        },
    )?;
    let expected = parent.clone().map(RefTarget::Direct);
    let old_oid = parent.clone().unwrap_or(zero_oid(format)?);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: updated_ref.clone(),
        expected,
        new: RefTarget::Direct(oid.clone()),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: oid.clone(),
            committer: options.committer,
            message: options.reflog_message,
        }),
    });
    tx.commit()?;
    Ok(CommitIndexResult {
        oid,
        tree,
        updated_ref,
        parent,
    })
}

pub fn format_commit_identity(name: &str, email: &str, date: &str) -> Result<Vec<u8>> {
    validate_identity_component("name", name)?;
    validate_identity_component("email", email)?;
    let (seconds, timezone) = parse_raw_git_date(date)?;
    Ok(format!("{name} <{email}> {seconds} {timezone}").into_bytes())
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

fn zero_oid(format: sley_core::ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

fn validate_identity_component(name: &str, value: &str) -> Result<()> {
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "commit identity {name} contains a control byte"
        )));
    }
    Ok(())
}

fn parse_raw_git_date(date: &str) -> Result<(i64, String)> {
    let mut parts = date.split_whitespace();
    let seconds = parts
        .next()
        .ok_or_else(|| GitError::InvalidFormat("missing commit date seconds".into()))?;
    let timezone = parts
        .next()
        .ok_or_else(|| GitError::InvalidFormat("missing commit date timezone".into()))?;
    if parts.next().is_some() {
        return Err(GitError::InvalidFormat(
            "commit date has trailing fields".into(),
        ));
    }
    let seconds = seconds.strip_prefix('@').unwrap_or(seconds);
    let seconds = seconds
        .parse::<i64>()
        .map_err(|_| GitError::InvalidFormat("invalid commit date seconds".into()))?;
    validate_timezone(timezone)?;
    Ok((seconds, timezone.to_string()))
}

fn validate_timezone(timezone: &str) -> Result<()> {
    let bytes = timezone.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes[0], b'+' | b'-')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(GitError::InvalidFormat(format!(
            "invalid commit timezone {timezone}"
        )));
    }
    Ok(())
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
