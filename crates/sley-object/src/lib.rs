//! git-object — Git's object model: commits, trees, tags, and the raw encoded
//! object framing they share.
//!
//! This crate carries the in-memory representations of Git's four object types
//! ([`Commit`], [`Tree`], [`Tag`], and the blob payload carried inside
//! [`EncodedObject`]) together with their parse/serialize routines and the
//! [`parse_framed_object`] helper that decodes the `"<type> <len>\0<body>"`
//! loose-object frame.

use sley_core::{GitError, ObjectFormat, ObjectId, Result, Signature};
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
        sley_core::object_id_for_bytes(format, self.object_type.as_str(), &self.body)
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

/// A borrowed parse-view of a single entry in a raw tree object.
///
/// The `name` slice points into the original tree body. The object id is a
/// fixed-size value parsed from the raw bytes, so iterating does not allocate
/// entry names or build an intermediate entry list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntryRef<'a> {
    pub mode: u32,
    pub name: &'a [u8],
    pub oid: ObjectId,
}

/// Fallibly iterates raw tree-object bytes without allocating entry names.
#[derive(Debug, Clone)]
pub struct TreeEntries<'a> {
    format: ObjectFormat,
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> TreeEntries<'a> {
    pub const fn new(format: ObjectFormat, bytes: &'a [u8]) -> Self {
        Self {
            format,
            bytes,
            offset: 0,
        }
    }
}

impl<'a> Iterator for TreeEntries<'a> {
    type Item = Result<TreeEntryRef<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.bytes.len() {
            return None;
        }
        match parse_tree_entry_ref(self.format, self.bytes, self.offset) {
            Ok((entry, next_offset)) => {
                self.offset = next_offset;
                Some(Ok(entry))
            }
            Err(err) => {
                self.offset = self.bytes.len();
                Some(Err(err))
            }
        }
    }
}

impl<'a> From<TreeEntryRef<'a>> for TreeEntry {
    fn from(entry: TreeEntryRef<'a>) -> Self {
        Self {
            mode: entry.mode,
            name: entry.name.to_vec(),
            oid: entry.oid,
        }
    }
}

impl Tree {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let entries = TreeEntries::new(format, bytes)
            .map(|entry| entry.map(TreeEntry::from))
            .collect::<Result<Vec<_>>>()?;
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

fn parse_tree_entry_ref<'a>(
    format: ObjectFormat,
    bytes: &'a [u8],
    offset: usize,
) -> Result<(TreeEntryRef<'a>, usize)> {
    let mode_end = bytes[offset..]
        .iter()
        .position(|byte| *byte == b' ')
        .map(|relative| offset + relative)
        .ok_or_else(|| GitError::InvalidFormat("unterminated tree mode".into()))?;
    let mode_text = std::str::from_utf8(&bytes[offset..mode_end])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mode = u32::from_str_radix(mode_text, 8)
        .map_err(|_| GitError::InvalidFormat("invalid tree mode".into()))?;

    let name_start = mode_end + 1;
    let name_end = bytes[name_start..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|relative| name_start + relative)
        .ok_or_else(|| GitError::InvalidFormat("unterminated tree path".into()))?;
    if name_end == name_start {
        return Err(GitError::InvalidFormat("empty tree path".into()));
    }

    let oid_start = name_end + 1;
    let oid_end = oid_start
        .checked_add(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("tree oid overflow".into()))?;
    if oid_end > bytes.len() {
        return Err(GitError::InvalidFormat("truncated tree object id".into()));
    }

    Ok((
        TreeEntryRef {
            mode,
            name: &bytes[name_start..name_end],
            oid: ObjectId::from_raw(format, &bytes[oid_start..oid_end])?,
        },
        oid_end,
    ))
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

impl TreeEntryRef<'_> {
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

    pub fn to_owned(&self) -> TreeEntry {
        TreeEntry {
            mode: self.mode,
            name: self.name.to_vec(),
            oid: self.oid.clone(),
        }
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
    if mode == 0o040000 { b'/' } else { 0 }
}

/// Builds a single tree level: deduplicates entries by name and emits them in
/// Git's canonical order so the written object is byte-identical to Git's.
///
/// Start from [`TreeBuilder::new`] (empty) or [`TreeBuilder::from_tree`] (edit
/// an existing level), [`upsert`](TreeBuilder::upsert) entries, then
/// [`build`](TreeBuilder::build) / [`write`](TreeBuilder::write).
#[derive(Debug, Clone, Default)]
pub struct TreeBuilder {
    entries: Vec<TreeEntry>,
}

impl TreeBuilder {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Seed the builder with an existing tree level's entries.
    pub fn from_tree(tree: Tree) -> Self {
        Self {
            entries: tree.entries,
        }
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
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.name == name) {
            entry.mode = mode;
            entry.oid = oid;
        } else {
            self.entries.push(TreeEntry { mode, name, oid });
        }
    }

    /// Remove the entry named `name`, returning whether one was present.
    pub fn remove(&mut self, name: &[u8]) -> bool {
        if let Some(position) = self.entries.iter().position(|entry| entry.name == name) {
            self.entries.swap_remove(position);
            true
        } else {
            false
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Collect into a [`Tree`] with entries in Git's canonical order.
    pub fn build(self) -> Tree {
        let mut entries = self.entries;
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

/// A borrowed parse-view of a raw commit object.
///
/// The identity, encoding, and message slices point into the original commit
/// body. Object ids are parsed into fixed-size values while preserving the same
/// validation behavior as [`Commit::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef<'a> {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: &'a [u8],
    pub committer: &'a [u8],
    pub encoding: Option<&'a [u8]>,
    pub message: &'a [u8],
}

impl Commit {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        Ok(Self::parse_ref(format, bytes)?.into())
    }

    pub fn parse_ref<'a>(format: ObjectFormat, bytes: &'a [u8]) -> Result<CommitRef<'a>> {
        CommitRef::parse(format, bytes)
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

    /// Parse the raw [`author`](Commit::author) line into a typed
    /// [`Signature`] parse-view, or `None` if the stored bytes are not a
    /// well-formed git identity.
    ///
    /// This is a read-only lens: it does not touch the raw `author` bytes, which
    /// remain the source of truth for [`Commit::write`]. The returned signature
    /// re-serializes byte-identically to `author` (see
    /// [`Signature::to_ident_bytes`]).
    pub fn author_signature(&self) -> Option<Signature> {
        Signature::from_ident_line(&self.author)
    }

    /// Parse the raw [`committer`](Commit::committer) line into a typed
    /// [`Signature`] parse-view, or `None` if the stored bytes are not a
    /// well-formed git identity. Read-only over the raw bytes, exactly like
    /// [`Commit::author_signature`].
    pub fn committer_signature(&self) -> Option<Signature> {
        Signature::from_ident_line(&self.committer)
    }
}

impl<'a> CommitRef<'a> {
    pub fn parse(format: ObjectFormat, bytes: &'a [u8]) -> Result<Self> {
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
                author = Some(value.as_bytes());
            } else if let Some(value) = line.strip_prefix("committer ") {
                committer = Some(value.as_bytes());
            } else if let Some(value) = line.strip_prefix("encoding ") {
                encoding = Some(value.as_bytes());
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
            message: &bytes[split + 2..],
        })
    }

    pub fn to_owned(&self) -> Commit {
        Commit {
            tree: self.tree.clone(),
            parents: self.parents.clone(),
            author: self.author.to_vec(),
            committer: self.committer.to_vec(),
            encoding: self.encoding.map(<[u8]>::to_vec),
            message: self.message.to_vec(),
        }
    }

    /// Parse the raw [`author`](Commit::author) line into a typed
    /// [`Signature`] parse-view, or `None` if the stored bytes are not a
    /// well-formed git identity.
    ///
    /// This is a read-only lens: it does not touch the raw `author` bytes, which
    /// remain the source of truth for [`Commit::write`]. The returned signature
    /// re-serializes byte-identically to `author` (see
    /// [`Signature::to_ident_bytes`]).
    pub fn author_signature(&self) -> Option<Signature> {
        Signature::from_ident_line(self.author)
    }

    /// Parse the raw [`committer`](Commit::committer) line into a typed
    /// [`Signature`] parse-view, or `None` if the stored bytes are not a
    /// well-formed git identity. Read-only over the raw bytes, exactly like
    /// [`Commit::author_signature`].
    pub fn committer_signature(&self) -> Option<Signature> {
        Signature::from_ident_line(self.committer)
    }
}

impl<'a> From<CommitRef<'a>> for Commit {
    fn from(commit: CommitRef<'a>) -> Self {
        Self {
            tree: commit.tree,
            parents: commit.parents,
            author: commit.author.to_vec(),
            committer: commit.committer.to_vec(),
            encoding: commit.encoding.map(<[u8]>::to_vec),
            message: commit.message.to_vec(),
        }
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

/// A borrowed parse-view of a raw annotated tag object.
///
/// The tag name, tagger identity, and message slices point into the original
/// tag body. The object id and object type are parsed into owned values while
/// preserving the same validation behavior as [`Tag::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRef<'a> {
    pub object: ObjectId,
    pub object_type: ObjectType,
    pub name: &'a [u8],
    pub tagger: Option<&'a [u8]>,
    pub message: &'a [u8],
}

impl Tag {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        Ok(Self::parse_ref(format, bytes)?.into())
    }

    pub fn parse_ref<'a>(format: ObjectFormat, bytes: &'a [u8]) -> Result<TagRef<'a>> {
        TagRef::parse(format, bytes)
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

    /// Parse the raw [`tagger`](Tag::tagger) line into a typed [`Signature`]
    /// parse-view.
    ///
    /// Returns `None` when the tag has no tagger header *or* when the stored
    /// bytes are not a well-formed git identity — callers that need to tell
    /// those apart should inspect [`Tag::tagger`] directly. This is a read-only
    /// lens over the raw bytes, which stay the source of truth for
    /// [`Tag::write`]; the returned signature re-serializes byte-identically to
    /// the stored `tagger` line.
    pub fn tagger_signature(&self) -> Option<Signature> {
        Signature::from_ident_line(self.tagger.as_deref()?)
    }
}

impl<'a> TagRef<'a> {
    pub fn parse(format: ObjectFormat, bytes: &'a [u8]) -> Result<Self> {
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
                name = Some(value.as_bytes());
            } else if let Some(value) = line.strip_prefix("tagger ") {
                tagger = Some(value.as_bytes());
            }
        }
        Ok(Self {
            object: object.ok_or_else(|| GitError::InvalidObject("tag missing object".into()))?,
            object_type: object_type
                .ok_or_else(|| GitError::InvalidObject("tag missing type".into()))?,
            name: name.ok_or_else(|| GitError::InvalidObject("tag missing name".into()))?,
            tagger,
            message: &bytes[split + 2..],
        })
    }

    pub fn to_owned(&self) -> Tag {
        Tag {
            object: self.object.clone(),
            object_type: self.object_type,
            name: self.name.to_vec(),
            tagger: self.tagger.map(<[u8]>::to_vec),
            message: self.message.to_vec(),
        }
    }

    /// Parse the raw [`tagger`](Tag::tagger) line into a typed [`Signature`]
    /// parse-view.
    ///
    /// Returns `None` when the tag has no tagger header *or* when the stored
    /// bytes are not a well-formed git identity — callers that need to tell
    /// those apart should inspect [`Tag::tagger`] directly. This is a read-only
    /// lens over the raw bytes, which stay the source of truth for
    /// [`Tag::write`]; the returned signature re-serializes byte-identically to
    /// the stored `tagger` line.
    pub fn tagger_signature(&self) -> Option<Signature> {
        Signature::from_ident_line(self.tagger?)
    }
}

impl<'a> From<TagRef<'a>> for Tag {
    fn from(tag: TagRef<'a>) -> Self {
        Self {
            object: tag.object,
            object_type: tag.object_type,
            name: tag.name.to_vec(),
            tagger: tag.tagger.map(<[u8]>::to_vec),
            message: tag.message.to_vec(),
        }
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
        assert_eq!(
            parse_framed_object(&object.framed_bytes()).expect("test operation should succeed"),
            object
        );
    }

    #[test]
    fn tree_round_trips_entries() {
        let blob = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .expect("test operation should succeed");
        let tree = Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: b"hello.txt".to_vec(),
                oid: blob,
            }],
        };
        assert_eq!(
            Tree::parse(ObjectFormat::Sha1, &tree.write()).expect("test operation should succeed"),
            tree
        );
    }

    #[test]
    fn tree_entries_iterates_without_name_allocations() {
        let format = ObjectFormat::Sha1;
        let blob = ObjectId::from_hex(format, "ce013625030ba8dba906f756967f9e9ca394464a")
            .expect("test operation should succeed");
        let subtree = ObjectId::empty_tree(format);
        let mut bytes = Vec::new();

        let first_name_start = b"100644 ".len();
        write_tree_entry(&mut bytes, EntryKind::Blob.mode(), b"hello.txt", &blob);
        let second_name_start = bytes.len() + b"40000 ".len();
        write_tree_entry(&mut bytes, EntryKind::Tree.mode(), b"src", &subtree);

        let mut entries = TreeEntries::new(format, &bytes);
        let first = entries
            .next()
            .expect("first entry")
            .expect("test operation should succeed");
        assert_eq!(first.mode, EntryKind::Blob.mode());
        assert_eq!(first.name, b"hello.txt");
        assert_eq!(first.oid, blob);
        assert_eq!(first.kind(), Some(EntryKind::Blob));
        assert!(std::ptr::eq(
            first.name.as_ptr(),
            bytes[first_name_start..].as_ptr()
        ));

        let second = entries
            .next()
            .expect("second entry")
            .expect("test operation should succeed");
        assert_eq!(second.mode, EntryKind::Tree.mode());
        assert_eq!(second.name, b"src");
        assert_eq!(second.oid, subtree);
        assert!(second.is_tree());
        assert!(std::ptr::eq(
            second.name.as_ptr(),
            bytes[second_name_start..].as_ptr()
        ));
        assert!(entries.next().is_none());

        let owned = Tree::parse(format, &bytes).expect("test operation should succeed");
        assert_eq!(owned.entries, vec![first.to_owned(), second.to_owned()]);
    }

    #[test]
    fn tree_entries_reports_invalid_mode_path_and_truncated_oid() {
        let format = ObjectFormat::Sha1;
        let oid = ObjectId::empty_blob(format);

        let mut invalid_mode = b"10088 bad\0".to_vec();
        invalid_mode.extend_from_slice(oid.as_bytes());
        assert_invalid_tree_entry(
            TreeEntries::new(format, &invalid_mode)
                .next()
                .expect("invalid mode result"),
            "invalid tree mode",
        );

        let mut empty_path = b"100644 \0".to_vec();
        empty_path.extend_from_slice(oid.as_bytes());
        assert_invalid_tree_entry(
            TreeEntries::new(format, &empty_path)
                .next()
                .expect("empty path result"),
            "empty tree path",
        );

        let mut truncated_oid = b"100644 bad\0".to_vec();
        truncated_oid.extend_from_slice(&oid.as_bytes()[..format.raw_len() - 1]);
        assert_invalid_tree_entry(
            TreeEntries::new(format, &truncated_oid)
                .next()
                .expect("truncated oid result"),
            "truncated tree object id",
        );
    }

    #[test]
    fn tree_entry_ref_kind_helpers_match_entry_kinds() {
        let oid = ObjectId::null(ObjectFormat::Sha1);

        let tree = TreeEntryRef {
            mode: EntryKind::Tree.mode(),
            name: b"dir",
            oid: oid.clone(),
        };
        assert_eq!(tree.kind(), Some(EntryKind::Tree));
        assert!(tree.is_tree());
        assert!(!tree.is_symlink());
        assert!(!tree.is_gitlink());
        assert!(!tree.is_executable());

        let symlink = TreeEntryRef {
            mode: EntryKind::Symlink.mode(),
            name: b"link",
            oid: oid.clone(),
        };
        assert_eq!(symlink.kind(), Some(EntryKind::Symlink));
        assert!(symlink.is_symlink());
        assert!(!symlink.is_tree());
        assert!(!symlink.is_gitlink());
        assert!(!symlink.is_executable());

        let executable = TreeEntryRef {
            mode: EntryKind::BlobExecutable.mode(),
            name: b"run",
            oid: oid.clone(),
        };
        assert_eq!(executable.kind(), Some(EntryKind::BlobExecutable));
        assert!(executable.is_executable());
        assert!(!executable.is_tree());
        assert!(!executable.is_symlink());
        assert!(!executable.is_gitlink());

        let gitlink = TreeEntryRef {
            mode: EntryKind::Commit.mode(),
            name: b"submodule",
            oid,
        };
        assert_eq!(gitlink.kind(), Some(EntryKind::Commit));
        assert!(gitlink.is_gitlink());
        assert!(!gitlink.is_tree());
        assert!(!gitlink.is_symlink());
        assert!(!gitlink.is_executable());
    }

    #[test]
    fn commit_round_trips_headers_and_message() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"A U Thor <a@example.invalid> 0 +0000".to_vec(),
            committer: b"C O Mitter <c@example.invalid> 0 +0000".to_vec(),
            encoding: Some(b"ISO-8859-1".to_vec()),
            message: b"subject\n\nbody\n".to_vec(),
        };
        assert_eq!(
            Commit::parse(ObjectFormat::Sha1, &commit.write())
                .expect("test operation should succeed"),
            commit
        );
    }

    #[test]
    fn commit_ref_borrows_headers_and_message() {
        let format = ObjectFormat::Sha1;
        let tree_hex = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let parent_hex = "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15";
        let body = format!(
            "tree {tree_hex}\n\
             parent {parent_hex}\n\
             author A U Thor <a@example.invalid> 0 +0000\n\
             committer C O Mitter <c@example.invalid> 1 -0000\n\
             encoding UTF-8\n\
             \n\
             subject\n\nbody\n"
        )
        .into_bytes();

        let commit = CommitRef::parse(format, &body).expect("test operation should succeed");
        assert_eq!(
            commit.tree,
            ObjectId::from_hex(format, tree_hex).expect("test operation should succeed")
        );
        assert_eq!(
            commit.parents,
            vec![ObjectId::from_hex(format, parent_hex).expect("test operation should succeed")]
        );
        assert_borrows_from(
            &body,
            commit.author,
            b"A U Thor <a@example.invalid> 0 +0000",
        );
        assert_borrows_from(
            &body,
            commit.committer,
            b"C O Mitter <c@example.invalid> 1 -0000",
        );
        assert_borrows_from(
            &body,
            commit.encoding.expect("test operation should succeed"),
            b"UTF-8",
        );
        assert_borrows_from(&body, commit.message, b"subject\n\nbody\n");

        assert_eq!(
            Commit::parse_ref(format, &body).expect("test operation should succeed"),
            commit
        );
        assert_eq!(
            commit.to_owned(),
            Commit::parse(format, &body).expect("test operation should succeed")
        );
    }

    #[test]
    fn commit_ref_rejects_missing_or_malformed_required_headers() {
        let format = ObjectFormat::Sha1;
        let valid_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let valid_idents =
            b"author A U Thor <a@example.invalid> 0 +0000\ncommitter C O Mitter <c@example.invalid> 0 +0000\n\nmessage\n";
        let mut missing_tree = Vec::new();
        missing_tree.extend_from_slice(valid_idents);
        assert_invalid_object(
            CommitRef::parse(format, &missing_tree),
            "commit missing tree",
        );

        let malformed_tree = b"tree not-an-object-id\nauthor A U Thor <a@example.invalid> 0 +0000\ncommitter C O Mitter <c@example.invalid> 0 +0000\n\nmessage\n";
        assert!(matches!(
            CommitRef::parse(format, malformed_tree),
            Err(GitError::InvalidObjectId(_))
        ));

        let missing_committer =
            format!("tree {valid_tree}\nauthor A U Thor <a@example.invalid> 0 +0000\n\nmessage\n")
                .into_bytes();
        assert_invalid_object(
            CommitRef::parse(format, &missing_committer),
            "commit missing committer",
        );
    }

    #[test]
    fn tag_round_trips_headers_and_message() {
        let object = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15",
        )
        .expect("test operation should succeed");
        let tag = Tag {
            object,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        assert_eq!(
            Tag::parse(ObjectFormat::Sha1, &tag.write()).expect("test operation should succeed"),
            tag
        );
    }

    #[test]
    fn tag_ref_borrows_name_tagger_and_message() {
        let format = ObjectFormat::Sha1;
        let object_hex = "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15";
        let body = format!(
            "object {object_hex}\n\
             type commit\n\
             tag v1.0-borrowed\n\
             tagger Example User <example@example.invalid> 0 +0000\n\
             \n\
             release notes\n"
        )
        .into_bytes();

        let tag = TagRef::parse(format, &body).expect("test operation should succeed");
        assert_eq!(
            tag.object,
            ObjectId::from_hex(format, object_hex).expect("test operation should succeed")
        );
        assert_eq!(tag.object_type, ObjectType::Commit);
        assert_borrows_from(&body, tag.name, b"v1.0-borrowed");
        assert_borrows_from(
            &body,
            tag.tagger.expect("test operation should succeed"),
            b"Example User <example@example.invalid> 0 +0000",
        );
        assert_borrows_from(&body, tag.message, b"release notes\n");

        assert_eq!(
            Tag::parse_ref(format, &body).expect("test operation should succeed"),
            tag
        );
        assert_eq!(
            tag.to_owned(),
            Tag::parse(format, &body).expect("test operation should succeed")
        );
    }

    #[test]
    fn tag_ref_rejects_missing_or_malformed_required_headers() {
        let format = ObjectFormat::Sha1;
        let object_hex = "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15";

        let missing_name = format!("object {object_hex}\ntype commit\n\nmessage\n").into_bytes();
        assert_invalid_object(TagRef::parse(format, &missing_name), "tag missing name");

        let malformed_object = b"object not-an-object-id\ntype commit\ntag v1.0\n\nmessage\n";
        assert!(matches!(
            TagRef::parse(format, malformed_object),
            Err(GitError::InvalidObjectId(_))
        ));

        let malformed_type =
            format!("object {object_hex}\ntype mystery\ntag v1.0\n\nmessage\n").into_bytes();
        assert_invalid_object(
            TagRef::parse(format, &malformed_type),
            "unknown object type mystery",
        );
    }

    #[test]
    fn commit_signature_accessors_parse_raw_idents_without_changing_storage() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        let author_raw = b"A U Thor <a@example.invalid> 1700000000 +0530".to_vec();
        let committer_raw = b"C O Mitter <c@example.invalid> 1700000001 -0000".to_vec();
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: author_raw.clone(),
            committer: committer_raw.clone(),
            encoding: None,
            message: b"subject\n".to_vec(),
        };

        let author = commit.author_signature().expect("author parses");
        assert_eq!(author.name.as_bytes(), b"A U Thor");
        assert_eq!(author.email.as_bytes(), b"a@example.invalid");
        assert_eq!(author.time.seconds, 1_700_000_000);
        assert_eq!(author.time.timezone_offset_minutes, 330);
        assert!(!author.time.negative_utc);
        // The parse-view re-serializes to exactly the stored bytes.
        assert_eq!(author.to_ident_bytes(), author_raw);

        let committer = commit.committer_signature().expect("committer parses");
        assert_eq!(committer.time.seconds, 1_700_000_001);
        // The committer used the -0000 sentinel; it must be preserved.
        assert!(committer.time.negative_utc);
        assert_eq!(committer.to_ident_bytes(), committer_raw);

        // The accessors did not mutate the raw fields, and write() still emits
        // them verbatim.
        assert_eq!(commit.author, author_raw);
        assert_eq!(commit.committer, committer_raw);
        let written = commit.write();
        assert_eq!(
            Commit::parse(ObjectFormat::Sha1, &written).expect("test operation should succeed"),
            commit
        );
    }

    #[test]
    fn commit_signature_accessor_is_none_for_malformed_ident() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .expect("test operation should succeed");
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"garbage without an email or time".to_vec(),
            committer: b"C O Mitter <c@example.invalid> 0 +0000".to_vec(),
            encoding: None,
            message: b"x\n".to_vec(),
        };
        assert!(commit.author_signature().is_none());
        assert!(commit.committer_signature().is_some());
    }

    #[test]
    fn tag_signature_accessor_parses_tagger_and_handles_absence() {
        let object = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15",
        )
        .expect("test operation should succeed");
        let tagger_raw = b"Example User <example@example.invalid> 1700000000 -0000".to_vec();
        let tag = Tag {
            object: object.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(tagger_raw.clone()),
            message: b"release\n".to_vec(),
        };
        let tagger = tag.tagger_signature().expect("tagger parses");
        assert_eq!(tagger.name.as_bytes(), b"Example User");
        assert!(tagger.time.negative_utc);
        assert_eq!(tagger.to_ident_bytes(), tagger_raw);
        // Raw field and serialization unaffected.
        assert_eq!(tag.tagger.as_deref(), Some(tagger_raw.as_slice()));

        // A tag with no tagger header yields None.
        let lightweight = Tag {
            object,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: None,
            message: b"x\n".to_vec(),
        };
        assert!(lightweight.tagger_signature().is_none());
    }

    fn write_tree_entry(body: &mut Vec<u8>, mode: u32, name: &[u8], oid: &ObjectId) {
        body.extend_from_slice(format!("{:o}", mode).as_bytes());
        body.push(b' ');
        body.extend_from_slice(name);
        body.push(0);
        body.extend_from_slice(oid.as_bytes());
    }

    fn assert_invalid_tree_entry(result: Result<TreeEntryRef<'_>>, expected: &str) {
        match result {
            Err(GitError::InvalidFormat(message)) => assert_eq!(message, expected),
            other => panic!("expected invalid format {expected:?}, got {other:?}"),
        }
    }

    fn assert_invalid_object<T: std::fmt::Debug>(result: Result<T>, expected: &str) {
        match result {
            Err(GitError::InvalidObject(message)) => assert_eq!(message, expected),
            other => panic!("expected invalid object {expected:?}, got {other:?}"),
        }
    }

    fn assert_borrows_from(body: &[u8], slice: &[u8], expected: &[u8]) {
        assert_eq!(slice, expected);
        let offset = body
            .windows(expected.len())
            .position(|window| window == expected)
            .expect("expected slice appears in body");
        assert!(std::ptr::eq(slice.as_ptr(), body[offset..].as_ptr()));
    }
}
