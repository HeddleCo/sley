//! git-object — Git's object model: commits, trees, tags, and the raw encoded
//! object framing they share.
//!
//! This crate carries the in-memory representations of Git's four object types
//! ([`Commit`], [`Tree`], [`Tag`], and the blob payload carried inside
//! [`EncodedObject`]) together with their parse/serialize routines and the
//! [`parse_framed_object`] helper that decodes the `"<type> <len>\0<body>"`
//! loose-object frame.

use git_core::{GitError, ObjectFormat, ObjectId, Result};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectType {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}

impl FromStr for ObjectType {
    type Err = GitError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "blob" => Ok(Self::Blob),
            "tree" => Ok(Self::Tree),
            "commit" => Ok(Self::Commit),
            "tag" => Ok(Self::Tag),
            other => Err(GitError::InvalidObject(format!(
                "unknown object type {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedObject {
    pub object_type: ObjectType,
    pub body: Vec<u8>,
}

impl EncodedObject {
    pub fn new(object_type: ObjectType, body: impl Into<Vec<u8>>) -> Self {
        Self {
            object_type,
            body: body.into(),
        }
    }

    pub fn framed_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 32);
        out.extend_from_slice(self.object_type.as_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.body.len().to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(&self.body);
        out
    }

    pub fn object_id(&self, format: ObjectFormat) -> Result<ObjectId> {
        git_core::object_id_for_bytes(format, self.object_type.as_str(), &self.body)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: Vec<u8>,
    pub oid: ObjectId,
}

impl Tree {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let mut offset = 0usize;
        let mut entries = Vec::new();
        while offset < bytes.len() {
            let mode_start = offset;
            while bytes.get(offset).copied() != Some(b' ') {
                offset += 1;
                if offset >= bytes.len() {
                    return Err(GitError::InvalidFormat("unterminated tree mode".into()));
                }
            }
            let mode_text = std::str::from_utf8(&bytes[mode_start..offset])
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
            let mode = u32::from_str_radix(mode_text, 8)
                .map_err(|_| GitError::InvalidFormat("invalid tree mode".into()))?;
            offset += 1;
            let name_start = offset;
            while bytes.get(offset).copied() != Some(0) {
                offset += 1;
                if offset >= bytes.len() {
                    return Err(GitError::InvalidFormat("unterminated tree path".into()));
                }
            }
            if offset == name_start {
                return Err(GitError::InvalidFormat("empty tree path".into()));
            }
            let name = bytes[name_start..offset].to_vec();
            offset += 1;
            let oid_end = offset
                .checked_add(format.raw_len())
                .ok_or_else(|| GitError::InvalidFormat("tree oid overflow".into()))?;
            if oid_end > bytes.len() {
                return Err(GitError::InvalidFormat("truncated tree object id".into()));
            }
            let oid = ObjectId::from_raw(format, &bytes[offset..oid_end])?;
            offset = oid_end;
            entries.push(TreeEntry { mode, name, oid });
        }
        Ok(Self { entries })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend_from_slice(format!("{:o}", entry.mode).as_bytes());
            out.push(b' ');
            out.extend_from_slice(&entry.name);
            out.push(0);
            out.extend_from_slice(entry.oid.as_bytes());
        }
        out
    }
}

pub fn tree_entry_object_type(mode: u32) -> ObjectType {
    match mode {
        0o040000 => ObjectType::Tree,
        _ => ObjectType::Blob,
    }
}

/// The five entry kinds Git allows inside a tree, each mapping to a fixed mode.
///
/// This is a *closed* domain used when *writing* trees; for reading arbitrary
/// trees, keep the raw [`TreeEntry::mode`] and classify with
/// [`EntryKind::from_mode`] (which returns `None` for non-canonical modes so
/// they round-trip rather than being silently coerced).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A subtree (`040000`).
    Tree,
    /// A non-executable regular file (`100644`).
    Blob,
    /// An executable regular file (`100755`).
    BlobExecutable,
    /// A symbolic link (`120000`); the blob bytes are the link target and must
    /// never be dereferenced.
    Symlink,
    /// A gitlink / submodule commit pointer (`160000`).
    Commit,
}

impl EntryKind {
    /// The octal tree-entry mode for this kind.
    pub const fn mode(self) -> u32 {
        match self {
            Self::Tree => 0o040000,
            Self::Blob => 0o100644,
            Self::BlobExecutable => 0o100755,
            Self::Symlink => 0o120000,
            Self::Commit => 0o160000,
        }
    }

    /// Classify a raw tree-entry mode, returning `None` for anything that is
    /// not one of Git's canonical five.
    pub const fn from_mode(mode: u32) -> Option<Self> {
        match mode {
            0o040000 => Some(Self::Tree),
            0o100644 => Some(Self::Blob),
            0o100755 => Some(Self::BlobExecutable),
            0o120000 => Some(Self::Symlink),
            0o160000 => Some(Self::Commit),
            _ => None,
        }
    }

    /// The object type an entry of this kind points at (a gitlink points at a
    /// commit that lives in another repository).
    pub const fn object_type(self) -> ObjectType {
        match self {
            Self::Tree => ObjectType::Tree,
            Self::Commit => ObjectType::Commit,
            _ => ObjectType::Blob,
        }
    }
}

impl From<EntryKind> for u32 {
    fn from(kind: EntryKind) -> Self {
        kind.mode()
    }
}

impl TreeEntry {
    /// Classify this entry's mode, if it is one of Git's canonical kinds.
    pub fn kind(&self) -> Option<EntryKind> {
        EntryKind::from_mode(self.mode)
    }

    pub fn is_tree(&self) -> bool {
        self.mode == EntryKind::Tree.mode()
    }

    pub fn is_symlink(&self) -> bool {
        self.mode == EntryKind::Symlink.mode()
    }

    pub fn is_gitlink(&self) -> bool {
        self.mode == EntryKind::Commit.mode()
    }

    pub fn is_executable(&self) -> bool {
        self.mode == EntryKind::BlobExecutable.mode()
    }
}

/// Order two tree entries the way Git canonically sorts them: by name bytes,
/// except that a subtree sorts as though its name ended in `/`. Writing a tree
/// whose entries are in any other order produces a different (wrong) OID.
pub fn tree_entry_cmp(
    left_name: &[u8],
    left_mode: u32,
    right_name: &[u8],
    right_mode: u32,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let shared = left_name.len().min(right_name.len());
    let name_order = left_name[..shared].cmp(&right_name[..shared]);
    if name_order != Ordering::Equal {
        return name_order;
    }
    let left_end = left_name.len() == shared;
    let right_end = right_name.len() == shared;
    match (left_end, right_end) {
        (true, true) => Ordering::Equal,
        (true, false) => tree_name_terminator(left_mode).cmp(&right_name[shared]),
        (false, true) => left_name[shared].cmp(&tree_name_terminator(right_mode)),
        (false, false) => Ordering::Equal,
    }
}

fn tree_name_terminator(mode: u32) -> u8 {
    if mode == 0o040000 {
        b'/'
    } else {
        0
    }
}

/// Builds a single tree level: deduplicates entries by name and emits them in
/// Git's canonical order so the written object is byte-identical to Git's.
///
/// Start from [`TreeBuilder::new`] (empty) or [`TreeBuilder::from_tree`] (edit
/// an existing level), [`upsert`](TreeBuilder::upsert) entries, then
/// [`build`](TreeBuilder::build) / [`write`](TreeBuilder::write).
#[derive(Debug, Clone, Default)]
pub struct TreeBuilder {
    entries: std::collections::HashMap<Vec<u8>, TreeEntry>,
}

impl TreeBuilder {
    pub fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }

    /// Seed the builder with an existing tree level's entries.
    pub fn from_tree(tree: Tree) -> Self {
        let entries = tree
            .entries
            .into_iter()
            .map(|entry| (entry.name.clone(), entry))
            .collect();
        Self { entries }
    }

    /// Insert or replace the entry named `name` with one of Git's canonical
    /// kinds.
    pub fn upsert(&mut self, name: impl Into<Vec<u8>>, kind: EntryKind, oid: ObjectId) {
        self.upsert_raw(name, kind.mode(), oid);
    }

    /// Insert or replace using a raw mode (for round-tripping non-canonical
    /// modes); prefer [`upsert`](TreeBuilder::upsert) for normal entries.
    pub fn upsert_raw(&mut self, name: impl Into<Vec<u8>>, mode: u32, oid: ObjectId) {
        let name = name.into();
        self.entries
            .insert(name.clone(), TreeEntry { mode, name, oid });
    }

    /// Remove the entry named `name`, returning whether one was present.
    pub fn remove(&mut self, name: &[u8]) -> bool {
        self.entries.remove(name).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Collect into a [`Tree`] with entries in Git's canonical order.
    pub fn build(self) -> Tree {
        let mut entries: Vec<TreeEntry> = self.entries.into_values().collect();
        entries
            .sort_by(|left, right| tree_entry_cmp(&left.name, left.mode, &right.name, right.mode));
        Tree { entries }
    }

    /// The canonical serialized tree body.
    pub fn write(self) -> Vec<u8> {
        self.build().write()
    }

    /// The OID this tree will have once written.
    pub fn object_id(self, format: ObjectFormat) -> Result<ObjectId> {
        EncodedObject::new(ObjectType::Tree, self.write()).object_id(format)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub encoding: Option<Vec<u8>>,
    pub message: Vec<u8>,
}

impl Commit {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let split = bytes
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| GitError::InvalidObject("commit missing message separator".into()))?;
        let headers = std::str::from_utf8(&bytes[..split])
            .map_err(|err| GitError::InvalidObject(err.to_string()))?;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        let mut encoding = None;
        for line in headers.lines() {
            if let Some(value) = line.strip_prefix("tree ") {
                tree = Some(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("parent ") {
                parents.push(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("author ") {
                author = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("committer ") {
                committer = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("encoding ") {
                encoding = Some(value.as_bytes().to_vec());
            }
        }
        Ok(Self {
            tree: tree.ok_or_else(|| GitError::InvalidObject("commit missing tree".into()))?,
            parents,
            author: author
                .ok_or_else(|| GitError::InvalidObject("commit missing author".into()))?,
            committer: committer
                .ok_or_else(|| GitError::InvalidObject("commit missing committer".into()))?,
            encoding,
            message: bytes[split + 2..].to_vec(),
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("tree {}\n", self.tree).as_bytes());
        for parent in &self.parents {
            out.extend_from_slice(format!("parent {parent}\n").as_bytes());
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(&self.author);
        out.push(b'\n');
        out.extend_from_slice(b"committer ");
        out.extend_from_slice(&self.committer);
        if let Some(encoding) = &self.encoding {
            out.extend_from_slice(b"\nencoding ");
            out.extend_from_slice(encoding);
        }
        out.extend_from_slice(b"\n\n");
        out.extend_from_slice(&self.message);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub object: ObjectId,
    pub object_type: ObjectType,
    pub name: Vec<u8>,
    pub tagger: Option<Vec<u8>>,
    pub message: Vec<u8>,
}

impl Tag {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let split = bytes
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| GitError::InvalidObject("tag missing message separator".into()))?;
        let headers = std::str::from_utf8(&bytes[..split])
            .map_err(|err| GitError::InvalidObject(err.to_string()))?;
        let mut object = None;
        let mut object_type = None;
        let mut name = None;
        let mut tagger = None;
        for line in headers.lines() {
            if let Some(value) = line.strip_prefix("object ") {
                object = Some(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("type ") {
                object_type = Some(value.parse()?);
            } else if let Some(value) = line.strip_prefix("tag ") {
                name = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("tagger ") {
                tagger = Some(value.as_bytes().to_vec());
            }
        }
        Ok(Self {
            object: object.ok_or_else(|| GitError::InvalidObject("tag missing object".into()))?,
            object_type: object_type
                .ok_or_else(|| GitError::InvalidObject("tag missing type".into()))?,
            name: name.ok_or_else(|| GitError::InvalidObject("tag missing name".into()))?,
            tagger,
            message: bytes[split + 2..].to_vec(),
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("object {}\n", self.object).as_bytes());
        out.extend_from_slice(format!("type {}\n", self.object_type.as_str()).as_bytes());
        out.extend_from_slice(b"tag ");
        out.extend_from_slice(&self.name);
        out.push(b'\n');
        if let Some(tagger) = &self.tagger {
            out.extend_from_slice(b"tagger ");
            out.extend_from_slice(tagger);
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }
}

pub fn parse_framed_object(bytes: &[u8]) -> Result<EncodedObject> {
    let nul = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| GitError::InvalidObject("missing object header terminator".into()))?;
    let header = std::str::from_utf8(&bytes[..nul])
        .map_err(|err| GitError::InvalidObject(err.to_string()))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidObject("missing object size".into()))?;
    let size: usize = size
        .parse()
        .map_err(|_| GitError::InvalidObject("invalid object size".into()))?;
    let body = &bytes[nul + 1..];
    if body.len() != size {
        return Err(GitError::InvalidObject(format!(
            "object declared {size} bytes, found {}",
            body.len()
        )));
    }
    Ok(EncodedObject::new(kind.parse()?, body.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_builder_sorts_canonically_and_dedups() {
        let format = ObjectFormat::Sha1;
        let blob = ObjectId::empty_blob(format);
        let subtree = ObjectId::empty_tree(format);
        // Validate the infallible well-known constants while we're here.
        assert_eq!(subtree.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(blob.to_hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");

        let mut builder = TreeBuilder::new();
        // Inserted out of order. The directory-suffix rule means "foo.txt"
        // (blob) sorts before the "foo" subtree, because '.' (0x2e) < '/' (0x2f)
        // — a plain byte sort of the names would (wrongly) put "foo" first.
        builder.upsert("foo", EntryKind::Tree, subtree);
        builder.upsert("a.txt", EntryKind::Blob, blob.clone());
        builder.upsert("foo.txt", EntryKind::Blob, blob.clone());
        // Last upsert for a name wins.
        builder.upsert("a.txt", EntryKind::BlobExecutable, blob);

        let tree = builder.build();
        let names: Vec<&[u8]> = tree.entries.iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(names, vec![&b"a.txt"[..], &b"foo.txt"[..], &b"foo"[..]]);
        assert_eq!(tree.entries[0].mode, EntryKind::BlobExecutable.mode());
        assert!(tree.entries[2].is_tree());
    }

    #[test]
    fn entry_kind_round_trips_modes() {
        for kind in [
            EntryKind::Tree,
            EntryKind::Blob,
            EntryKind::BlobExecutable,
            EntryKind::Symlink,
            EntryKind::Commit,
        ] {
            assert_eq!(EntryKind::from_mode(kind.mode()), Some(kind));
        }
        assert_eq!(EntryKind::from_mode(0o100600), None);
    }

    #[test]
    fn framed_object_round_trips() {
        let object = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec());
        assert_eq!(parse_framed_object(&object.framed_bytes()).unwrap(), object);
    }

    #[test]
    fn tree_round_trips_entries() {
        let blob = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let tree = Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: b"hello.txt".to_vec(),
                oid: blob,
            }],
        };
        assert_eq!(
            Tree::parse(ObjectFormat::Sha1, &tree.write()).unwrap(),
            tree
        );
    }

    #[test]
    fn commit_round_trips_headers_and_message() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"A U Thor <a@example.invalid> 0 +0000".to_vec(),
            committer: b"C O Mitter <c@example.invalid> 0 +0000".to_vec(),
            encoding: Some(b"ISO-8859-1".to_vec()),
            message: b"subject\n\nbody\n".to_vec(),
        };
        assert_eq!(
            Commit::parse(ObjectFormat::Sha1, &commit.write()).unwrap(),
            commit
        );
    }

    #[test]
    fn tag_round_trips_headers_and_message() {
        let object = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15",
        )
        .unwrap();
        let tag = Tag {
            object,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        assert_eq!(Tag::parse(ObjectFormat::Sha1, &tag.write()).unwrap(), tag);
    }
}
