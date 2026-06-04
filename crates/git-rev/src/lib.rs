use git_core::{GitError, ObjectId, Result};
use git_formats::{Commit, CommitGraph, ObjectType, Tag};
use git_odb::{FileObjectDatabase, ObjectPrefixResolution, ObjectReader};
use git_refs::{FileRefStore, PackedRef, RefTarget};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSpec {
    pub raw: String,
}

impl RevisionSpec {
    pub fn parse(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(GitError::InvalidFormat("empty revision spec".into()));
        }
        Ok(Self { raw })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub oid: ObjectId,
    pub parents: Vec<ObjectId>,
    pub commit: Commit,
}

pub fn resolve_revision(
    git_dir: impl AsRef<Path>,
    format: git_core::ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    resolve_revision_with_reader(git_dir, format, &db, rev)
}

pub fn resolve_revision_with_reader<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    rev: &str,
) -> Result<ObjectId> {
    if let Some((base, suffix)) = split_revision_suffix(rev)? {
        if base.is_empty() {
            return Err(GitError::InvalidFormat(format!(
                "revision {rev} has empty base"
            )));
        }
        let base_oid = resolve_revision_with_reader(git_dir, format, reader, base)?;
        return apply_revision_suffix(git_dir, reader, format, &base_oid, suffix, rev);
    }
    resolve_revision_name(git_dir, format, rev)
}

fn resolve_revision_name(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    if rev.len() == format.hex_len() && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return ObjectId::from_hex(format, rev);
    }
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    if let Some(oid) = resolve_revision_ref(&refs, rev)? {
        return Ok(oid);
    }
    if rev.len() >= 4
        && rev.len() < format.hex_len()
        && rev.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        match db.resolve_prefix(rev)? {
            ObjectPrefixResolution::Unique(oid) => return Ok(oid),
            ObjectPrefixResolution::Ambiguous(matches) => {
                let mut names = matches
                    .into_iter()
                    .map(|oid| oid.to_string())
                    .collect::<Vec<_>>();
                names.sort();
                return Err(GitError::InvalidObjectId(format!(
                    "short object ID {rev} is ambiguous: {}",
                    names.join(", ")
                )));
            }
            ObjectPrefixResolution::Missing => {}
        }
    }
    Err(GitError::NotFound(format!("revision {rev}")))
}

fn resolve_revision_ref(refs: &FileRefStore, rev: &str) -> Result<Option<ObjectId>> {
    let target = if rev == "HEAD" {
        refs.read_ref("HEAD")?
    } else if rev.starts_with("refs/") {
        refs.read_ref(rev)?
    } else {
        refs.read_ref(&format!("refs/heads/{rev}"))?
            .or(refs.read_ref(&format!("refs/tags/{rev}"))?)
    };
    match target {
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        Some(RefTarget::Symbolic(name)) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionSuffix {
    Parent(usize),
    FirstParent(usize),
    Peel(PeelKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeelKind {
    AnyNonTag,
    Object,
    Commit,
    Tree,
    Tag,
}

fn split_revision_suffix(rev: &str) -> Result<Option<(&str, RevisionSuffix)>> {
    let caret = rev.rfind('^');
    let tilde = rev.rfind('~');
    let Some((op, pos)) = (match (caret, tilde) {
        (Some(caret), Some(tilde)) if caret > tilde => Some(('^', caret)),
        (Some(caret), Some(tilde)) if tilde > caret => Some(('~', tilde)),
        (Some(caret), None) => Some(('^', caret)),
        (None, Some(tilde)) => Some(('~', tilde)),
        (None, None) => None,
        _ => None,
    }) else {
        return Ok(None);
    };
    let (base, suffix) = rev.split_at(pos);
    let suffix = &suffix[1..];
    match op {
        '^' => {
            let parent = if suffix.is_empty() {
                1
            } else if let Some(kind) = parse_peel_suffix(rev, suffix)? {
                return Ok(Some((base, RevisionSuffix::Peel(kind))));
            } else if suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                parse_revision_count(rev, suffix)?
            } else {
                return Ok(None);
            };
            Ok(Some((base, RevisionSuffix::Parent(parent))))
        }
        '~' => {
            let count = if suffix.is_empty() {
                1
            } else if suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                parse_revision_count(rev, suffix)?
            } else {
                return Ok(None);
            };
            Ok(Some((base, RevisionSuffix::FirstParent(count))))
        }
        _ => Ok(None),
    }
}

fn parse_peel_suffix(rev: &str, suffix: &str) -> Result<Option<PeelKind>> {
    if !suffix.starts_with('{') {
        return Ok(None);
    }
    let Some(kind) = suffix
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
    else {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision peel suffix in {rev}"
        )));
    };
    let kind = match kind {
        "" => PeelKind::AnyNonTag,
        "object" => PeelKind::Object,
        "commit" => PeelKind::Commit,
        "tree" => PeelKind::Tree,
        "tag" => PeelKind::Tag,
        other => {
            return Err(GitError::Unsupported(format!(
                "revision peel suffix ^{{{other}}}"
            )));
        }
    };
    Ok(Some(kind))
}

fn parse_revision_count(rev: &str, text: &str) -> Result<usize> {
    text.parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid revision suffix in {rev}")))
}

fn apply_revision_suffix<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: git_core::ObjectFormat,
    base: &ObjectId,
    suffix: RevisionSuffix,
    raw_rev: &str,
) -> Result<ObjectId> {
    match suffix {
        RevisionSuffix::Parent(parent) => {
            if parent == 0 {
                return Err(GitError::InvalidFormat(format!(
                    "invalid zero parent in {raw_rev}"
                )));
            }
            commit_parents_with_graph(git_dir, reader, format, base)?
                .get(parent - 1)
                .cloned()
                .ok_or_else(|| GitError::NotFound(format!("parent {parent} of {base}")))
        }
        RevisionSuffix::FirstParent(count) => {
            let mut current = base.clone();
            for _ in 0..count {
                current = commit_parents_with_graph(git_dir, reader, format, &current)?
                    .first()
                    .cloned()
                    .ok_or_else(|| GitError::NotFound(format!("first parent of {current}")))?;
            }
            Ok(current)
        }
        RevisionSuffix::Peel(kind) => peel_revision(reader, format, base, kind),
    }
}

fn commit_parents_with_graph<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<ObjectId>> {
    if let Some(parents) = commit_graph_parents_for_oid(git_dir, format, oid)? {
        return Ok(parents);
    }
    commit_parents(reader, format, oid)
}

fn commit_graph_parents_for_oid(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<Vec<ObjectId>>> {
    let path = git_dir.join("objects").join("info").join("commit-graph");
    if !path.exists() {
        return Ok(None);
    }
    let graph = CommitGraph::parse(&fs::read(path)?, format)?;
    let Some(entry) = graph.find(oid) else {
        return Ok(None);
    };
    let mut parents = Vec::with_capacity(entry.parents.len());
    for parent in &entry.parents {
        let parent = usize::try_from(*parent)
            .map_err(|_| GitError::InvalidFormat("commit-graph parent index overflow".into()))?;
        let Some(parent_entry) = graph.commits.get(parent) else {
            return Err(GitError::InvalidFormat(
                "commit-graph parent points past commit table".into(),
            ));
        };
        parents.push(parent_entry.oid.clone());
    }
    Ok(Some(parents))
}

fn commit_parents<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<ObjectId>> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse(format, &object.body)?.parents)
}

fn peel_revision<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
    kind: PeelKind,
) -> Result<ObjectId> {
    match kind {
        PeelKind::AnyNonTag => peel_tags(reader, format, oid),
        PeelKind::Object => {
            reader.read_object(oid)?;
            Ok(oid.clone())
        }
        PeelKind::Commit => peel_to_commit(reader, format, oid),
        PeelKind::Tree => peel_to_tree(reader, format, oid),
        PeelKind::Tag => {
            let object = reader.read_object(oid)?;
            if object.object_type == ObjectType::Tag {
                Ok(oid.clone())
            } else {
                Err(GitError::InvalidObject(format!(
                    "expected tag {oid}, found {}",
                    object.object_type.as_str()
                )))
            }
        }
    }
}

pub fn peel_tags<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(oid.clone());
    }
    let tag = Tag::parse(format, &object.body)?;
    peel_tags(reader, format, &tag.object)
}

pub fn peel_to_tree<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    match object.object_type {
        ObjectType::Tree => Ok(oid.clone()),
        ObjectType::Commit => Ok(Commit::parse(format, &object.body)?.tree),
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            peel_to_tree(reader, format, &tag.object)
        }
        other => Err(GitError::InvalidObject(format!(
            "expected tree-ish {oid}, found {}",
            other.as_str()
        ))),
    }
}

pub fn peel_to_commit<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    match object.object_type {
        ObjectType::Commit => Ok(oid.clone()),
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            peel_to_commit(reader, format, &tag.object)
        }
        other => Err(GitError::InvalidObject(format!(
            "expected commit-ish {oid}, found {}",
            other.as_str()
        ))),
    }
}

pub fn pack_refs_with_auto_peel(
    git_dir: impl AsRef<Path>,
    format: git_core::ObjectFormat,
    prune_loose: bool,
) -> Result<Vec<PackedRef>> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    refs.pack_refs_with_peeler(prune_loose, |_, oid| {
        let peeled = peel_tags(&db, format, oid)?;
        if &peeled == oid {
            Ok(None)
        } else {
            Ok(Some(peeled))
        }
    })
}

pub fn parse_commit_parents(format: git_core::ObjectFormat, body: &[u8]) -> Result<Vec<ObjectId>> {
    let text = std::str::from_utf8(body).map_err(|err| GitError::InvalidObject(err.to_string()))?;
    let mut parents = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(hex) = line.strip_prefix("parent ") {
            parents.push(ObjectId::from_hex(format, hex)?);
        }
    }
    Ok(parents)
}

pub fn walk_commits<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<CommitRecord>> {
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.into_iter().collect();
    let mut out = Vec::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let object = reader.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        let parents = commit.parents.clone();
        pending.extend(parents.iter().cloned());
        out.push(CommitRecord {
            oid,
            parents,
            commit,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_core::ObjectFormat;
    use git_formats::EncodedObject;
    use git_odb::{ObjectDatabase, ObjectWriter};
    use git_refs::{RefTarget, RefUpdate};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn resolve_revision_reads_symbolic_head_and_tags() {
        let git_dir = temp_git_dir();
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD").unwrap(),
            oid
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "v1.0").unwrap(),
            oid
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_supports_parent_suffixes() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let base = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let first_parent = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"main\n");
        let second_parent = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"side\n");
        let merge = write_test_commit(
            &mut db,
            tree,
            vec![first_parent.clone(), second_parent.clone()],
            b"merge\n",
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^"))
                .unwrap(),
            first_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^2"))
                .unwrap(),
            second_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}~2"))
                .unwrap(),
            base
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_supports_abbreviated_loose_object_ids() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"abbrev\n".to_vec()))
            .unwrap();

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &oid.to_hex()[..8]).unwrap(),
            oid
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_prefers_ref_over_abbreviated_object_id() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"abbrev conflict\n".to_vec(),
            ))
            .unwrap();
        let target = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: format!("refs/heads/{}", &object.to_hex()[..4]),
            expected: None,
            new: RefTarget::Direct(target.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &object.to_hex()[..4]).unwrap(),
            target
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_uses_commit_graph_for_parent_suffixes() {
        let git_dir = temp_git_dir();
        fs::create_dir_all(git_dir.join("objects").join("info")).unwrap();
        let parent = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let child = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        fs::write(git_dir.join("HEAD"), format!("{child}\n")).unwrap();
        fs::write(
            git_dir.join("objects").join("info").join("commit-graph"),
            test_commit_graph(ObjectFormat::Sha1, &parent, &child),
        )
        .unwrap();

        struct MissingReader;
        impl ObjectReader for MissingReader {
            fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
                Err(GitError::NotFound(format!(
                    "object reader should not be used for {oid}"
                )))
            }
        }

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &MissingReader, "HEAD^",)
                .unwrap(),
            parent
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn peel_to_tree_handles_commits_and_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            peel_to_tree(&db, ObjectFormat::Sha1, &commit).unwrap(),
            tree
        );
        assert_eq!(peel_to_tree(&db, ObjectFormat::Sha1, &tag).unwrap(), tree);
    }

    #[test]
    fn peel_to_commit_handles_annotated_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            peel_to_commit(&db, ObjectFormat::Sha1, &tag).unwrap(),
            commit
        );
    }

    #[test]
    fn resolve_revision_supports_peel_suffixes() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{tag}^{{}}"))
                .unwrap(),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{commit}}")
            )
            .unwrap(),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tree}}")
            )
            .unwrap(),
            tree
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tag}}")
            )
            .unwrap(),
            tag
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn pack_refs_with_auto_peel_writes_peeled_tag() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            committer: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            encoding: None,
            message: b"base\n".to_vec(),
        };
        let commit = db
            .write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap();
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        let packed = pack_refs_with_auto_peel(&git_dir, ObjectFormat::Sha1, true).unwrap();
        let packed_tag = packed
            .iter()
            .find(|packed| packed.reference.name == "refs/tags/v1.0")
            .unwrap();
        assert_eq!(packed_tag.peeled, Some(commit.clone()));
        assert_eq!(
            refs.read_ref("refs/tags/v1.0").unwrap(),
            Some(RefTarget::Direct(tag))
        );
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    fn write_test_commit(
        db: &mut ObjectDatabase,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &[u8],
    ) -> ObjectId {
        let commit = Commit {
            tree,
            parents,
            author: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            committer: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            encoding: None,
            message: message.to_vec(),
        };
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap()
    }

    fn temp_git_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "git-rs-rev-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_commit_graph(format: ObjectFormat, parent: &ObjectId, child: &ObjectId) -> Vec<u8> {
        let tree = ObjectId::from_hex(format, "4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
        let mut oidf = vec![0u8; 256 * 4];
        let parent_first = parent.as_bytes()[0] as usize;
        let child_first = child.as_bytes()[0] as usize;
        for idx in 0..256 {
            let count = u32::from(idx >= parent_first) + u32::from(idx >= child_first);
            oidf[idx * 4..idx * 4 + 4].copy_from_slice(&count.to_be_bytes());
        }
        let mut oidl = Vec::new();
        oidl.extend_from_slice(parent.as_bytes());
        oidl.extend_from_slice(child.as_bytes());
        let mut cdat = Vec::new();
        cdat.extend_from_slice(&commit_graph_cdat_entry(
            &tree,
            0x7000_0000,
            0x7000_0000,
            1,
            1,
        ));
        cdat.extend_from_slice(&commit_graph_cdat_entry(&tree, 0, 0x7000_0000, 2, 2));
        commit_graph_file(
            format,
            &[(*b"OIDF", oidf), (*b"OIDL", oidl), (*b"CDAT", cdat)],
        )
    }

    fn commit_graph_cdat_entry(
        tree: &ObjectId,
        parent_one: u32,
        parent_two: u32,
        generation: u32,
        commit_time: u64,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(tree.as_bytes());
        out.extend_from_slice(&parent_one.to_be_bytes());
        out.extend_from_slice(&parent_two.to_be_bytes());
        let high = (generation << 2) | ((commit_time >> 32) as u32 & 0x3);
        out.extend_from_slice(&high.to_be_bytes());
        out.extend_from_slice(&(commit_time as u32).to_be_bytes());
        out
    }

    fn commit_graph_file(format: ObjectFormat, chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let lookup_len = (chunks.len() + 1) * 12;
        let mut out = Vec::new();
        out.extend_from_slice(b"CGPH");
        out.push(1);
        out.push(match format {
            ObjectFormat::Sha1 => 1,
            ObjectFormat::Sha256 => 2,
        });
        out.push(chunks.len() as u8);
        out.push(0);
        let mut offset = (8 + lookup_len) as u64;
        for (id, data) in chunks {
            out.extend_from_slice(id);
            out.extend_from_slice(&offset.to_be_bytes());
            offset += data.len() as u64;
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&offset.to_be_bytes());
        for (_id, data) in chunks {
            out.extend_from_slice(data);
        }
        let checksum = git_core::digest_bytes(format, &out).unwrap();
        out.extend_from_slice(checksum.as_bytes());
        out
    }
}
