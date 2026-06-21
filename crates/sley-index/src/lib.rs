//! git-index — Git's index (staging area) file format.
//!
//! This crate owns the [`Index`] / [`IndexEntry`] model and the readers and
//! writers for index versions 2, 3, and 4 (including v4 path prefix
//! compression and v3 extended flags), plus the cache-tree (`TREE`) extension
//! ([`CacheTree`]) that caches the tree object ids a fully-staged index would
//! write.

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;
use std::{env, fs};

pub use sley_core::BString;

// ===========================================================================
// Gitlink (submodule) primitive — the SINGLE source of truth for "is this
// entry a gitlink, and what does it mean for a gitlink to be up to date?".
//
// A gitlink is git's `S_IFGITLINK` (raw file mode `0o160000`): the index/tree
// entry for an embedded submodule. Its oid names a *commit in the submodule's
// own repository* (never a blob in this repository's object store), and its
// working-tree representation is a *directory* (the submodule checkout), not a
// file. This crate is the leaf that every index/diff/status/refresh consumer
// already depends on, so the gitlink predicate and stat semantics live here so
// no consumer re-derives `mode == 0o160000` or open-codes git's
// `ce_match_stat`/`ce_match_stat_basic` gitlink arm. The *HEAD resolution* of a
// populated submodule (`resolve_gitlink_ref`) needs the ref store and so lives
// one layer up in `sley-diff-merge`; this crate models everything that does not
// require reading the embedded repository.
// ===========================================================================

/// The raw git file mode of a gitlink (submodule) entry: git's `S_IFGITLINK`.
/// An index or tree entry with this mode records the commit oid an embedded
/// repository has checked out; the entry has no blob in this object store and
/// its worktree representation is a directory.
pub const GITLINK_MODE: u32 = 0o160000;

/// The git file-type mask (`S_IFMT`): isolates the file-type bits of a raw git
/// mode so a mode with extra permission bits (e.g. `0o100755`) still classifies
/// by type. Gitlinks carry no permission bits, but masking keeps the predicate
/// honest against any caller that ORs bits in.
pub const GIT_MODE_TYPE_MASK: u32 = 0o170000;

/// git's `S_ISGITLINK(mode)`: whether a raw git file mode names a gitlink
/// (submodule) entry. This is the ONE definition every consumer must call
/// rather than testing `mode == 0o160000` inline, so the gitlink concept has a
/// single, greppable owner.
#[inline]
pub fn is_gitlink(mode: u32) -> bool {
    (mode & GIT_MODE_TYPE_MASK) == GITLINK_MODE
}

/// git's `ce_match_stat_basic` `S_IFGITLINK` arm, factored out so every
/// stat-verdict consumer (`update-index --refresh`, `diff-files`, `status`)
/// shares one gitlink rule instead of re-deriving it.
///
/// For a gitlink entry git **ignores almost all of `st_xxx`**: it only checks
/// the on-disk type and, when that is a directory, the embedded HEAD.
///
/// * On-disk is **not a directory** (the submodule checkout is missing, or a
///   file/symlink sits where the submodule should be) → git's `TYPE_CHANGED`,
///   reported here as [`StatVerdict::Dirty`].
/// * On-disk **is a directory** → git compares the submodule's resolved HEAD
///   against the entry oid (`ce_compare_gitlink`). An *unpopulated* submodule
///   (HEAD unresolvable — no `.git`, unborn branch) **always matches** (git
///   "consider it always to match"); a populated submodule whose HEAD differs
///   from the recorded oid is dirty. Because resolving the HEAD needs the ref
///   store (a higher layer), this returns [`GitlinkStatVerdict::Populated`] so
///   the caller can run the cheap HEAD comparison and finish the verdict; an
///   index-only / HEAD-blind caller treats `Populated` as clean, exactly git's
///   unpopulated default.
///
/// Racy-clean never applies to a gitlink (git's `is_racy_timestamp` returns 0
/// for gitlinks), so this never yields a content-recheck verdict.
pub fn gitlink_stat_verdict(worktree_metadata: &fs::Metadata) -> GitlinkStatVerdict {
    if worktree_metadata.is_dir() {
        GitlinkStatVerdict::Populated
    } else {
        GitlinkStatVerdict::TypeChanged
    }
}

/// The outcome of [`gitlink_stat_verdict`]: the part of git's gitlink
/// `ce_match_stat` decision this crate can make without the ref store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitlinkStatVerdict {
    /// The worktree path is not a directory (missing / replaced by a file):
    /// git's `TYPE_CHANGED`. Always dirty.
    TypeChanged,
    /// The worktree path is a directory. The entry is clean unless the embedded
    /// submodule's resolved HEAD differs from the entry oid — a comparison the
    /// caller completes (it owns the ref store). An unpopulated submodule has no
    /// resolvable HEAD and so always matches (clean).
    Populated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub version: u32,
    pub entries: Vec<IndexEntry>,
    pub extensions: Vec<u8>,
    pub checksum: Option<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UntrackedCache {
    pub ident: Vec<u8>,
    pub info_exclude: UntrackedCacheOidStat,
    pub excludes_file: UntrackedCacheOidStat,
    pub dir_flags: u32,
    pub exclude_per_dir: Vec<u8>,
    pub root: Option<UntrackedCacheDir>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedCacheOidStat {
    pub stat: UntrackedCacheStatData,
    pub oid: ObjectId,
}

impl Default for UntrackedCacheOidStat {
    fn default() -> Self {
        Self {
            stat: UntrackedCacheStatData::default(),
            oid: ObjectId::null(ObjectFormat::Sha1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UntrackedCacheStatData {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UntrackedCacheDir {
    pub name: Vec<u8>,
    pub stat: UntrackedCacheStatData,
    pub exclude_oid: Option<ObjectId>,
    pub untracked: Vec<Vec<u8>>,
    pub dirs: Vec<UntrackedCacheDir>,
    pub valid: bool,
    pub check_only: bool,
    pub recurse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: ObjectId,
    pub flags: u16,
    pub flags_extended: u16,
    pub path: BString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntryRef<'a> {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: ObjectId,
    pub flags: u16,
    pub flags_extended: u16,
    pub path: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorrowedIndex<'a> {
    pub version: u32,
    pub entries: Vec<IndexEntryRef<'a>>,
    pub extensions: &'a [u8],
    pub checksum: ObjectId,
}

impl IndexEntry {
    /// Build an intent-to-add placeholder entry for `path` (the shape `git add
    /// -N` writes): zeroed stat, the canonical empty-blob id, mode `100644`,
    /// the intent-to-add + extended flags set, and the name length encoded. The
    /// containing [`Index`] must be version 3+ (see
    /// [`Index::upgrade_version_for_flags`]).
    pub fn intent_to_add(format: ObjectFormat, path: impl Into<BString>) -> Self {
        let path = path.into();
        let name_len = (path.len().min(INDEX_FLAG_NAME_LENGTH_MASK as usize)) as u16;
        Self {
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
            oid: ObjectId::empty_blob(format),
            flags: name_len | INDEX_FLAG_EXTENDED,
            flags_extended: INDEX_EXTENDED_FLAG_INTENT_TO_ADD,
            path,
        }
    }

    /// The merge stage encoded in this entry's flags.
    pub fn stage(&self) -> Stage {
        Stage::from_flags(self.flags)
    }

    /// Whether this is an intent-to-add (`git add -N`) placeholder.
    pub fn is_intent_to_add(&self) -> bool {
        self.flags_extended & INDEX_EXTENDED_FLAG_INTENT_TO_ADD != 0
    }

    /// Whether this entry is marked skip-worktree (sparse checkout).
    pub fn is_skip_worktree(&self) -> bool {
        self.flags_extended & INDEX_EXTENDED_FLAG_SKIP_WORKTREE != 0
    }

    /// Whether this is a *sparse-directory* entry: a collapsed cone-excluded
    /// directory stored as a single tree entry (mode `040000`, skip-worktree,
    /// path ending in `/`) instead of every blob under it. This is the on-disk
    /// shape of a sparse index (`extensions.sparseIndex`).
    pub fn is_sparse_dir(&self) -> bool {
        self.mode == SPARSE_DIR_MODE && self.is_skip_worktree()
    }

    /// Set or clear the intent-to-add bit, keeping the `extended` flag in sync.
    /// (The writer only emits extended entries for index version 3+.)
    pub fn set_intent_to_add(&mut self, intent: bool) {
        if intent {
            self.flags_extended |= INDEX_EXTENDED_FLAG_INTENT_TO_ADD;
            self.flags |= INDEX_FLAG_EXTENDED;
        } else {
            self.flags_extended &= !INDEX_EXTENDED_FLAG_INTENT_TO_ADD;
            if self.flags_extended == 0 {
                self.flags &= !INDEX_FLAG_EXTENDED;
            }
        }
    }

    /// Set or clear the skip-worktree bit, keeping the `extended` flag in sync.
    /// (The writer only emits extended entries for index version 3+.)
    pub fn set_skip_worktree(&mut self, skip: bool) {
        if skip {
            self.flags_extended |= INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
            self.flags |= INDEX_FLAG_EXTENDED;
        } else {
            self.flags_extended &= !INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
            if self.flags_extended == 0 {
                self.flags &= !INDEX_FLAG_EXTENDED;
            }
        }
    }

    /// Re-encode the name-length bits (low 12 bits of `flags`, capped at
    /// `0xfff`) from `path`, matching what git stores.
    pub fn refresh_name_length(&mut self) {
        let len = (self.path.len().min(INDEX_FLAG_NAME_LENGTH_MASK as usize)) as u16;
        self.flags = (self.flags & !INDEX_FLAG_NAME_LENGTH_MASK) | len;
    }
}

impl IndexEntryRef<'_> {
    /// The merge stage encoded in this entry's flags.
    pub fn stage(&self) -> Stage {
        Stage::from_flags(self.flags)
    }

    /// Whether this is an intent-to-add (`git add -N`) placeholder.
    pub fn is_intent_to_add(&self) -> bool {
        self.flags_extended & INDEX_EXTENDED_FLAG_INTENT_TO_ADD != 0
    }

    /// Whether this entry is marked skip-worktree (sparse checkout).
    pub fn is_skip_worktree(&self) -> bool {
        self.flags_extended & INDEX_EXTENDED_FLAG_SKIP_WORKTREE != 0
    }
}

impl<'a> BorrowedIndex<'a> {
    /// Parse an index whose path names can be borrowed directly from the index
    /// bytes. Index v4 uses prefix-compressed paths, so callers should fall back
    /// to the owned [`Index::parse`] path for that version.
    pub fn parse(bytes: &'a [u8], format: ObjectFormat) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 12 + hash_len {
            return Err(GitError::InvalidFormat("index header too short".into()));
        }
        let checksum_offset = bytes.len() - hash_len;
        let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "index checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
        }
        if &bytes[..4] != b"DIRC" {
            return Err(GitError::InvalidFormat("missing DIRC signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if version == 4 {
            return Err(GitError::Unsupported(
                "borrowed index parse does not support index version 4".into(),
            ));
        }
        if !(2..=3).contains(&version) {
            return Err(GitError::Unsupported(format!("index version {version}")));
        }
        let count = u32_be(&bytes[8..12]) as usize;
        let mut offset = 12;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let entry_header_len = 40 + hash_len + 2;
            if checksum_offset.saturating_sub(offset) < entry_header_len {
                return Err(GitError::InvalidFormat("truncated index entry".into()));
            }
            let start = offset;
            let oid_start = offset + 40;
            let oid_end = oid_start + hash_len;
            let oid = ObjectId::from_raw(format, &bytes[oid_start..oid_end])?;
            let flags = u16_be(&bytes[oid_end..oid_end + 2]);
            offset = oid_end + 2;
            let flags_extended = if flags & INDEX_FLAG_EXTENDED != 0 {
                if checksum_offset.saturating_sub(offset) < 2 {
                    return Err(GitError::InvalidFormat(
                        "truncated index extended flags".into(),
                    ));
                }
                let flags_extended = u16_be(&bytes[offset..offset + 2]);
                offset += 2;
                flags_extended
            } else {
                0
            };
            let path_start = offset;
            while bytes.get(offset).copied() != Some(0) {
                offset += 1;
                if offset >= checksum_offset {
                    return Err(GitError::InvalidFormat("unterminated index path".into()));
                }
            }
            let path = &bytes[path_start..offset];
            offset += 1;
            while (offset - start) % 8 != 0 {
                offset += 1;
                if offset > checksum_offset {
                    return Err(GitError::InvalidFormat("truncated index padding".into()));
                }
            }
            entries.push(IndexEntryRef {
                ctime_seconds: u32_be(&bytes[start..start + 4]),
                ctime_nanoseconds: u32_be(&bytes[start + 4..start + 8]),
                mtime_seconds: u32_be(&bytes[start + 8..start + 12]),
                mtime_nanoseconds: u32_be(&bytes[start + 12..start + 16]),
                dev: u32_be(&bytes[start + 16..start + 20]),
                ino: u32_be(&bytes[start + 20..start + 24]),
                mode: u32_be(&bytes[start + 24..start + 28]),
                uid: u32_be(&bytes[start + 28..start + 32]),
                gid: u32_be(&bytes[start + 32..start + 36]),
                size: u32_be(&bytes[start + 36..start + 40]),
                oid,
                flags,
                flags_extended,
                path,
            });
        }
        Ok(Self {
            version,
            entries,
            extensions: &bytes[offset..checksum_offset],
            checksum,
        })
    }

    /// Iterate the optional/required extension chunks stored in
    /// `self.extensions`.
    pub fn extension_chunks(&self) -> Result<Vec<([u8; 4], &[u8])>> {
        parse_index_extension_chunks(self.extensions)
    }

    /// Return the raw body of the first extension chunk with `signature`, if
    /// any.
    pub fn extension(&self, signature: &[u8; 4]) -> Result<Option<&[u8]>> {
        Ok(self
            .extension_chunks()?
            .into_iter()
            .find(|(id, _)| id == signature)
            .map(|(_, body)| body))
    }

    /// Parse the `TREE` (cache-tree) extension, if present.
    pub fn cache_tree(&self, format: ObjectFormat) -> Result<Option<CacheTree>> {
        match self.extension(b"TREE")? {
            Some(body) => Ok(Some(CacheTree::parse(format, body)?)),
            None => Ok(None),
        }
    }
}

impl Index {
    pub fn for_each_path<F>(bytes: &[u8], format: ObjectFormat, mut visit: F) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        let hash_len = format.raw_len();
        if bytes.len() < 12 + hash_len {
            return Err(GitError::InvalidFormat("index header too short".into()));
        }
        let checksum_offset = bytes.len() - hash_len;
        let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "index checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
        }
        if &bytes[..4] != b"DIRC" {
            return Err(GitError::InvalidFormat("missing DIRC signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if !(2..=4).contains(&version) {
            return Err(GitError::Unsupported(format!("index version {version}")));
        }
        let count = u32_be(&bytes[8..12]) as usize;
        let mut offset = 12;
        let mut previous_path = Vec::new();
        for _ in 0..count {
            let entry_header_len = 40 + hash_len + 2;
            if checksum_offset.saturating_sub(offset) < entry_header_len {
                return Err(GitError::InvalidFormat("truncated index entry".into()));
            }
            let start = offset;
            let flags_start = offset + 40 + hash_len;
            let flags = u16_be(&bytes[flags_start..flags_start + 2]);
            offset = flags_start + 2;
            if flags & INDEX_FLAG_EXTENDED != 0 {
                if checksum_offset.saturating_sub(offset) < 2 {
                    return Err(GitError::InvalidFormat(
                        "truncated index extended flags".into(),
                    ));
                }
                offset += 2;
            }
            if version == 4 {
                let strip_len =
                    decode_index_v4_path_strip_len(bytes, &mut offset, checksum_offset)?;
                if strip_len > previous_path.len() {
                    return Err(GitError::InvalidFormat(
                        "index v4 path compression removes too much prefix".into(),
                    ));
                }
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                let prefix_len = previous_path.len() - strip_len;
                let suffix = &bytes[path_start..offset];
                let mut path = Vec::with_capacity(prefix_len + suffix.len());
                path.extend_from_slice(&previous_path[..prefix_len]);
                path.extend_from_slice(suffix);
                offset += 1;
                visit(&path)?;
                previous_path = path;
            } else {
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                visit(&bytes[path_start..offset])?;
                offset += 1;
                while (offset - start) % 8 != 0 {
                    offset += 1;
                    if offset > checksum_offset {
                        return Err(GitError::InvalidFormat("truncated index padding".into()));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 12 + hash_len {
            return Err(GitError::InvalidFormat("index header too short".into()));
        }
        let checksum_offset = bytes.len() - hash_len;
        let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "index checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
        }
        if &bytes[..4] != b"DIRC" {
            return Err(GitError::InvalidFormat("missing DIRC signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if !(2..=4).contains(&version) {
            return Err(GitError::Unsupported(format!("index version {version}")));
        }
        let count = u32_be(&bytes[8..12]) as usize;
        let mut offset = 12;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let entry_header_len = 40 + hash_len + 2;
            if checksum_offset.saturating_sub(offset) < entry_header_len {
                return Err(GitError::InvalidFormat("truncated index entry".into()));
            }
            let start = offset;
            let oid_start = offset + 40;
            let oid_end = oid_start + hash_len;
            let oid = ObjectId::from_raw(format, &bytes[oid_start..oid_end])?;
            let flags = u16_be(&bytes[oid_end..oid_end + 2]);
            offset = oid_end + 2;
            let flags_extended = if flags & INDEX_FLAG_EXTENDED != 0 {
                if checksum_offset.saturating_sub(offset) < 2 {
                    return Err(GitError::InvalidFormat(
                        "truncated index extended flags".into(),
                    ));
                }
                let flags_extended = u16_be(&bytes[offset..offset + 2]);
                offset += 2;
                flags_extended
            } else {
                0
            };
            let path = if version == 4 {
                let previous_path = entries
                    .last()
                    .map(|entry: &IndexEntry| entry.path.as_bytes())
                    .unwrap_or(&[]);
                let strip_len =
                    decode_index_v4_path_strip_len(bytes, &mut offset, checksum_offset)?;
                if strip_len > previous_path.len() {
                    return Err(GitError::InvalidFormat(
                        "index v4 path compression removes too much prefix".into(),
                    ));
                }
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                let mut path = previous_path[..previous_path.len() - strip_len].to_vec();
                path.extend_from_slice(&bytes[path_start..offset]);
                offset += 1;
                BString::from(path)
            } else {
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                let path = BString::from_bytes(&bytes[path_start..offset]);
                offset += 1;
                while (offset - start) % 8 != 0 {
                    offset += 1;
                    if offset > checksum_offset {
                        return Err(GitError::InvalidFormat("truncated index padding".into()));
                    }
                }
                path
            };
            entries.push(IndexEntry {
                ctime_seconds: u32_be(&bytes[start..start + 4]),
                ctime_nanoseconds: u32_be(&bytes[start + 4..start + 8]),
                mtime_seconds: u32_be(&bytes[start + 8..start + 12]),
                mtime_nanoseconds: u32_be(&bytes[start + 12..start + 16]),
                dev: u32_be(&bytes[start + 16..start + 20]),
                ino: u32_be(&bytes[start + 20..start + 24]),
                mode: u32_be(&bytes[start + 24..start + 28]),
                uid: u32_be(&bytes[start + 28..start + 32]),
                gid: u32_be(&bytes[start + 32..start + 36]),
                size: u32_be(&bytes[start + 36..start + 40]),
                oid,
                flags,
                flags_extended,
                path,
            });
        }
        Ok(Self {
            version,
            entries,
            extensions: bytes[offset..checksum_offset].to_vec(),
            checksum: Some(checksum),
        })
    }

    pub fn parse_v2_sha1(bytes: &[u8]) -> Result<Self> {
        Self::parse(bytes, ObjectFormat::Sha1)
    }

    pub fn write_v2_sha1(&self) -> Result<Vec<u8>> {
        if self.version != 2 {
            return Err(GitError::Unsupported(
                "canonical writer currently emits index v2".into(),
            ));
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0)
        {
            return Err(GitError::Unsupported(
                "index v2 writer cannot emit extended flags".into(),
            ));
        }
        self.write_sha1()
    }

    pub fn write_sha1(&self) -> Result<Vec<u8>> {
        self.write(ObjectFormat::Sha1)
    }

    pub fn write(&self, format: ObjectFormat) -> Result<Vec<u8>> {
        if !(2..=4).contains(&self.version) {
            return Err(GitError::Unsupported(
                "canonical writer currently emits index v2/v3/v4".into(),
            ));
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        let mut previous_path = Vec::new();
        for entry in &self.entries {
            let start = out.len();
            out.extend_from_slice(&entry.ctime_seconds.to_be_bytes());
            out.extend_from_slice(&entry.ctime_nanoseconds.to_be_bytes());
            out.extend_from_slice(&entry.mtime_seconds.to_be_bytes());
            out.extend_from_slice(&entry.mtime_nanoseconds.to_be_bytes());
            out.extend_from_slice(&entry.dev.to_be_bytes());
            out.extend_from_slice(&entry.ino.to_be_bytes());
            out.extend_from_slice(&entry.mode.to_be_bytes());
            out.extend_from_slice(&entry.uid.to_be_bytes());
            out.extend_from_slice(&entry.gid.to_be_bytes());
            out.extend_from_slice(&entry.size.to_be_bytes());
            if entry.oid.format() != format {
                return Err(GitError::Unsupported(format!(
                    "index writer expects {} ids",
                    format.name()
                )));
            }
            out.extend_from_slice(entry.oid.as_bytes());
            let has_extended_flags =
                entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0;
            if has_extended_flags && self.version < 3 {
                return Err(GitError::Unsupported(
                    "index extended flags require version 3".into(),
                ));
            }
            let flags = if has_extended_flags {
                entry.flags | INDEX_FLAG_EXTENDED
            } else {
                entry.flags & !INDEX_FLAG_EXTENDED
            };
            out.extend_from_slice(&flags.to_be_bytes());
            if has_extended_flags {
                out.extend_from_slice(&entry.flags_extended.to_be_bytes());
            }
            if self.version == 4 {
                let common_prefix_len = common_prefix_len(&previous_path, entry.path.as_bytes());
                let strip_len = previous_path.len() - common_prefix_len;
                encode_index_v4_path_strip_len(strip_len, &mut out);
                out.extend_from_slice(&entry.path.as_bytes()[common_prefix_len..]);
                out.push(0);
                previous_path = entry.path.as_bytes().to_vec();
            } else {
                out.extend_from_slice(entry.path.as_bytes());
                out.push(0);
                while (out.len() - start) % 8 != 0 {
                    out.push(0);
                }
            }
        }
        out.extend_from_slice(&self.extensions);
        let checksum = sley_core::digest_bytes(format, &out)?;
        out.extend_from_slice(checksum.as_bytes());
        Ok(out)
    }

    /// Raise `version` to the minimum the current entries require: version 3
    /// when any entry carries extended flags (intent-to-add or skip-worktree),
    /// which cannot be represented in v2.
    pub fn upgrade_version_for_flags(&mut self) {
        if self.version < 3
            && self
                .entries
                .iter()
                .any(|entry| entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0)
        {
            self.version = 3;
        }
    }

    /// Iterate the optional/required extension chunks stored in `self.extensions`.
    ///
    /// The extension area of an index is a flat sequence of
    /// `[4-byte signature][4-byte big-endian length][body]` records, terminated
    /// by the trailing object-id checksum (which is *not* part of
    /// `self.extensions`). This returns `(signature, body)` for each record in
    /// order, or an error if the area is malformed.
    pub fn extension_chunks(&self) -> Result<Vec<([u8; 4], &[u8])>> {
        parse_index_extension_chunks(&self.extensions)
    }

    /// Return the raw body of the first extension chunk with `signature`, if any.
    pub fn extension(&self, signature: &[u8; 4]) -> Result<Option<&[u8]>> {
        Ok(self
            .extension_chunks()?
            .into_iter()
            .find(|(id, _)| id == signature)
            .map(|(_, body)| body))
    }

    /// Parse the `TREE` (cache-tree) extension, if present.
    ///
    /// `format` selects the object-id width used for the embedded tree ids.
    pub fn cache_tree(&self, format: ObjectFormat) -> Result<Option<CacheTree>> {
        match self.extension(b"TREE")? {
            Some(body) => Ok(Some(CacheTree::parse(format, body)?)),
            None => Ok(None),
        }
    }

    /// Parse the `UNTR` (untracked-cache) extension, if present.
    pub fn untracked_cache(&self, format: ObjectFormat) -> Result<Option<UntrackedCache>> {
        match self.extension(b"UNTR")? {
            Some(body) => Ok(Some(UntrackedCache::parse(format, body)?)),
            None => Ok(None),
        }
    }

    /// Parse the `link` split-index extension, if present.
    pub fn split_index_link(&self, format: ObjectFormat) -> Result<Option<SplitIndexLink>> {
        match self.extension(&INDEX_EXT_LINK)? {
            Some(body) => Ok(Some(SplitIndexLink::parse(format, body)?)),
            None => Ok(None),
        }
    }

    /// Replace (or insert) the `TREE` extension with `cache_tree`, keeping every
    /// other extension chunk in its original order.
    ///
    /// Passing `None` removes the `TREE` extension. The serialized cache-tree is
    /// byte-compatible with upstream git, so the rewritten `self.extensions`
    /// round-trips through [`Index::cache_tree`] and is readable by `git`.
    pub fn set_cache_tree(&mut self, cache_tree: Option<&CacheTree>) -> Result<()> {
        let chunks = self.extension_chunks()?;
        let mut rebuilt = Vec::with_capacity(self.extensions.len());
        let mut replaced = false;
        for (id, body) in chunks {
            if &id == b"TREE" {
                if let Some(cache_tree) = cache_tree {
                    encode_index_extension(&mut rebuilt, b"TREE", &cache_tree.write()?)?;
                }
                replaced = true;
            } else {
                encode_index_extension(&mut rebuilt, &id, body)?;
            }
        }
        if !replaced && let Some(cache_tree) = cache_tree {
            // git emits `TREE` ahead of most other extensions; when none was
            // present we prepend it so freshly written indexes match git's
            // ordering.
            let mut prefixed = Vec::with_capacity(rebuilt.len() + cache_tree_estimate(cache_tree));
            encode_index_extension(&mut prefixed, b"TREE", &cache_tree.write()?)?;
            prefixed.extend_from_slice(&rebuilt);
            rebuilt = prefixed;
        }
        self.extensions = rebuilt;
        Ok(())
    }

    /// Replace (or insert) the `UNTR` extension, keeping every other extension
    /// chunk in its original order. Passing `None` removes the extension.
    pub fn set_untracked_cache(
        &mut self,
        format: ObjectFormat,
        cache: Option<&UntrackedCache>,
    ) -> Result<()> {
        self.replace_extension(b"UNTR", cache.map(|cache| cache.write(format)).transpose()?)
    }

    /// Replace (or remove) the split-index `link` extension.
    pub fn set_split_index_link(&mut self, link: Option<&SplitIndexLink>) -> Result<()> {
        self.replace_extension(
            &INDEX_EXT_LINK,
            link.map(SplitIndexLink::write).transpose()?,
        )
    }

    /// Remove the split-index `link` extension.
    pub fn clear_split_index_link(&mut self) -> Result<()> {
        self.set_split_index_link(None)
    }

    fn replace_extension(&mut self, signature: &[u8; 4], body: Option<Vec<u8>>) -> Result<()> {
        let chunks = self.extension_chunks()?;
        let mut rebuilt = Vec::with_capacity(self.extensions.len());
        let mut replaced = false;
        for (id, chunk_body) in chunks {
            if &id == signature {
                if let Some(body) = body.as_ref() {
                    encode_index_extension(&mut rebuilt, signature, body)?;
                }
                replaced = true;
            } else {
                encode_index_extension(&mut rebuilt, &id, chunk_body)?;
            }
        }
        if !replaced && let Some(body) = body.as_ref() {
            encode_index_extension(&mut rebuilt, signature, body)?;
        }
        self.extensions = rebuilt;
        Ok(())
    }
}

impl SplitIndexLink {
    pub fn new(base_oid: ObjectId) -> Self {
        Self {
            base_oid,
            delete_positions: Vec::new(),
            replace_positions: Vec::new(),
        }
    }

    pub fn parse(format: ObjectFormat, body: &[u8]) -> Result<Self> {
        let hash_len = format.raw_len();
        if body.len() < hash_len {
            return Err(GitError::InvalidFormat(
                "corrupt link extension (too short)".into(),
            ));
        }
        let base_oid = ObjectId::from_raw(format, &body[..hash_len])?;
        let mut offset = hash_len;
        if offset == body.len() {
            return Ok(Self::new(base_oid));
        }
        let delete_positions = read_ewah_positions(body, &mut offset, body.len())?;
        let replace_positions = read_ewah_positions(body, &mut offset, body.len())?;
        if offset != body.len() {
            return Err(GitError::InvalidFormat(
                "garbage at the end of link extension".into(),
            ));
        }
        Ok(Self {
            base_oid,
            delete_positions,
            replace_positions,
        })
    }

    pub fn write(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(self.base_oid.as_bytes());
        if self.delete_positions.is_empty() && self.replace_positions.is_empty() {
            return Ok(out);
        }
        let delete_bits = self
            .delete_positions
            .iter()
            .copied()
            .max()
            .map(|position| position + 1)
            .unwrap_or(0);
        let replace_bits = self
            .replace_positions
            .iter()
            .copied()
            .max()
            .map(|position| position + 1)
            .unwrap_or(0);
        write_ewah_positions(delete_bits, &self.delete_positions, &mut out);
        write_ewah_positions(replace_bits, &self.replace_positions, &mut out);
        Ok(out)
    }
}

impl UntrackedCache {
    pub fn new(format: ObjectFormat, ident: Vec<u8>, dir_flags: u32) -> Self {
        Self {
            ident,
            info_exclude: UntrackedCacheOidStat::new(format),
            excludes_file: UntrackedCacheOidStat::new(format),
            dir_flags,
            exclude_per_dir: b".gitignore".to_vec(),
            root: None,
        }
    }

    pub fn parse(format: ObjectFormat, body: &[u8]) -> Result<Self> {
        let hash_len = format.raw_len();
        if body.len() <= 1 || body.last().copied() != Some(0) {
            return Err(GitError::InvalidFormat(
                "invalid untracked-cache extension terminator".into(),
            ));
        }
        let end = body.len() - 1;
        let mut offset = 0;
        let ident_len = decode_untracked_varint(body, &mut offset, end)?;
        if offset
            .checked_add(ident_len)
            .filter(|next| *next <= end)
            .is_none()
        {
            return Err(GitError::InvalidFormat(
                "truncated untracked-cache ident".into(),
            ));
        }
        let ident = body[offset..offset + ident_len].to_vec();
        offset += ident_len;
        let header_len = UNTRACKED_STAT_DATA_LEN * 2 + 4 + hash_len * 2;
        if offset
            .checked_add(header_len + 1)
            .filter(|next| *next <= end)
            .is_none()
        {
            return Err(GitError::InvalidFormat(
                "truncated untracked-cache header".into(),
            ));
        }
        let info_stat = UntrackedCacheStatData::parse(&body[offset..offset + 36])?;
        offset += 36;
        let excludes_stat = UntrackedCacheStatData::parse(&body[offset..offset + 36])?;
        offset += 36;
        let dir_flags = u32_be(&body[offset..offset + 4]);
        offset += 4;
        let info_oid = ObjectId::from_raw(format, &body[offset..offset + hash_len])?;
        offset += hash_len;
        let excludes_oid = ObjectId::from_raw(format, &body[offset..offset + hash_len])?;
        offset += hash_len;
        let exclude_end = memchr_zero(&body[offset..end]).ok_or_else(|| {
            GitError::InvalidFormat("unterminated untracked-cache exclude_per_dir".into())
        })?;
        let exclude_per_dir = body[offset..offset + exclude_end].to_vec();
        offset += exclude_end + 1;
        if offset >= end {
            return Ok(Self {
                ident,
                info_exclude: UntrackedCacheOidStat {
                    stat: info_stat,
                    oid: info_oid,
                },
                excludes_file: UntrackedCacheOidStat {
                    stat: excludes_stat,
                    oid: excludes_oid,
                },
                dir_flags,
                exclude_per_dir,
                root: None,
            });
        }
        let dir_count = decode_untracked_varint(body, &mut offset, end)?;
        let mut dirs = Vec::with_capacity(dir_count);
        let root = if dir_count == 0 {
            None
        } else {
            Some(read_untracked_cache_dir(body, &mut offset, end, &mut dirs)?)
        };
        if dir_count == 0 {
            if offset != end {
                return Err(GitError::InvalidFormat(
                    "trailing bytes in empty untracked-cache extension".into(),
                ));
            }
            return Ok(Self {
                ident,
                info_exclude: UntrackedCacheOidStat {
                    stat: info_stat,
                    oid: info_oid,
                },
                excludes_file: UntrackedCacheOidStat {
                    stat: excludes_stat,
                    oid: excludes_oid,
                },
                dir_flags,
                exclude_per_dir,
                root: None,
            });
        }
        if dirs.len() != dir_count {
            return Err(GitError::InvalidFormat(
                "untracked-cache directory count mismatch".into(),
            ));
        }
        let valid = read_ewah_positions(body, &mut offset, end)?;
        let check_only = read_ewah_positions(body, &mut offset, end)?;
        let oid_valid = read_ewah_positions(body, &mut offset, end)?;
        for pos in check_only {
            let Some(dir) = dirs.get_mut(pos as usize) else {
                return Err(GitError::InvalidFormat(
                    "untracked-cache check_only bit out of range".into(),
                ));
            };
            dir.check_only = true;
        }
        for pos in valid {
            let Some(dir) = dirs.get_mut(pos as usize) else {
                return Err(GitError::InvalidFormat(
                    "untracked-cache valid bit out of range".into(),
                ));
            };
            if offset + UNTRACKED_STAT_DATA_LEN > end {
                return Err(GitError::InvalidFormat(
                    "truncated untracked-cache directory stat".into(),
                ));
            }
            dir.stat = UntrackedCacheStatData::parse(&body[offset..offset + 36])?;
            dir.valid = true;
            offset += UNTRACKED_STAT_DATA_LEN;
        }
        for pos in oid_valid {
            let Some(dir) = dirs.get_mut(pos as usize) else {
                return Err(GitError::InvalidFormat(
                    "untracked-cache oid bit out of range".into(),
                ));
            };
            if offset + hash_len > end {
                return Err(GitError::InvalidFormat(
                    "truncated untracked-cache directory oid".into(),
                ));
            }
            dir.exclude_oid = Some(ObjectId::from_raw(
                format,
                &body[offset..offset + hash_len],
            )?);
            offset += hash_len;
        }
        if offset != end {
            return Err(GitError::InvalidFormat(
                "trailing bytes in untracked-cache extension".into(),
            ));
        }
        let mut root = root;
        if let Some(root) = root.as_mut() {
            apply_untracked_dir_side_data(root, &dirs, &mut 0);
        }
        Ok(Self {
            ident,
            info_exclude: UntrackedCacheOidStat {
                stat: info_stat,
                oid: info_oid,
            },
            excludes_file: UntrackedCacheOidStat {
                stat: excludes_stat,
                oid: excludes_oid,
            },
            dir_flags,
            exclude_per_dir,
            root,
        })
    }

    pub fn write(&self, format: ObjectFormat) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        encode_untracked_varint(self.ident.len(), &mut out);
        out.extend_from_slice(&self.ident);
        self.info_exclude.stat.write(&mut out);
        self.excludes_file.stat.write(&mut out);
        out.extend_from_slice(&self.dir_flags.to_be_bytes());
        ensure_oid_format(&self.info_exclude.oid, format)?;
        ensure_oid_format(&self.excludes_file.oid, format)?;
        out.extend_from_slice(self.info_exclude.oid.as_bytes());
        out.extend_from_slice(self.excludes_file.oid.as_bytes());
        out.extend_from_slice(&self.exclude_per_dir);
        out.push(0);
        let Some(root) = self.root.as_ref() else {
            encode_untracked_varint(0, &mut out);
            out.push(0);
            return Ok(out);
        };
        let mut dirs = Vec::new();
        root.write_preorder(&mut dirs);
        encode_untracked_varint(dirs.len(), &mut out);
        let mut dir_bytes = Vec::new();
        for dir in &dirs {
            dir.write_record(&mut dir_bytes);
        }
        out.extend_from_slice(&dir_bytes);
        let valid = dirs
            .iter()
            .enumerate()
            .filter_map(|(idx, dir)| dir.valid.then_some(idx as u32))
            .collect::<Vec<_>>();
        let check_only = dirs
            .iter()
            .enumerate()
            .filter_map(|(idx, dir)| dir.check_only.then_some(idx as u32))
            .collect::<Vec<_>>();
        let oid_valid = dirs
            .iter()
            .enumerate()
            .filter_map(|(idx, dir)| dir.exclude_oid.is_some().then_some(idx as u32))
            .collect::<Vec<_>>();
        write_ewah_positions(dirs.len() as u32, &valid, &mut out);
        write_ewah_positions(dirs.len() as u32, &check_only, &mut out);
        write_ewah_positions(dirs.len() as u32, &oid_valid, &mut out);
        for dir in &dirs {
            if dir.valid {
                dir.stat.write(&mut out);
            }
        }
        for dir in &dirs {
            if let Some(oid) = dir.exclude_oid.as_ref() {
                ensure_oid_format(oid, format)?;
                out.extend_from_slice(oid.as_bytes());
            }
        }
        out.push(0);
        Ok(out)
    }
}

impl UntrackedCacheOidStat {
    pub fn new(format: ObjectFormat) -> Self {
        Self {
            stat: UntrackedCacheStatData::default(),
            oid: ObjectId::null(format),
        }
    }
}

impl UntrackedCacheStatData {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < UNTRACKED_STAT_DATA_LEN {
            return Err(GitError::InvalidFormat(
                "truncated untracked-cache stat data".into(),
            ));
        }
        Ok(Self {
            ctime_seconds: u32_be(&bytes[0..4]),
            ctime_nanoseconds: u32_be(&bytes[4..8]),
            mtime_seconds: u32_be(&bytes[8..12]),
            mtime_nanoseconds: u32_be(&bytes[12..16]),
            dev: u32_be(&bytes[16..20]),
            ino: u32_be(&bytes[20..24]),
            uid: u32_be(&bytes[24..28]),
            gid: u32_be(&bytes[28..32]),
            size: u32_be(&bytes[32..36]),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ctime_seconds.to_be_bytes());
        out.extend_from_slice(&self.ctime_nanoseconds.to_be_bytes());
        out.extend_from_slice(&self.mtime_seconds.to_be_bytes());
        out.extend_from_slice(&self.mtime_nanoseconds.to_be_bytes());
        out.extend_from_slice(&self.dev.to_be_bytes());
        out.extend_from_slice(&self.ino.to_be_bytes());
        out.extend_from_slice(&self.uid.to_be_bytes());
        out.extend_from_slice(&self.gid.to_be_bytes());
        out.extend_from_slice(&self.size.to_be_bytes());
    }
}

impl UntrackedCacheDir {
    fn write_preorder<'a>(&'a self, dirs: &mut Vec<&'a Self>) {
        dirs.push(self);
        for dir in self.dirs.iter().filter(|dir| dir.recurse) {
            dir.write_preorder(dirs);
        }
    }

    fn write_record(&self, out: &mut Vec<u8>) {
        let mut untracked = self.untracked.clone();
        if !self.valid {
            untracked.clear();
        }
        encode_untracked_varint(untracked.len(), out);
        let recurse_dirs = self.dirs.iter().filter(|dir| dir.recurse).count();
        encode_untracked_varint(recurse_dirs, out);
        out.extend_from_slice(&self.name);
        out.push(0);
        for path in untracked {
            out.extend_from_slice(&path);
            out.push(0);
        }
    }
}

const UNTRACKED_STAT_DATA_LEN: usize = 36;
const UNTRACKED_CACHE_NORMAL_FLAGS: u32 = 0x0000_0006;

pub fn untracked_cache_normal_flags() -> u32 {
    UNTRACKED_CACHE_NORMAL_FLAGS
}

fn ensure_oid_format(oid: &ObjectId, format: ObjectFormat) -> Result<()> {
    if oid.format() != format {
        return Err(GitError::Unsupported(format!(
            "untracked-cache writer expects {} ids",
            format.name()
        )));
    }
    Ok(())
}

fn encode_untracked_varint(mut value: usize, out: &mut Vec<u8>) {
    let mut bytes = [0u8; 16];
    let mut len = 0;
    bytes[len] = (value & 0x7f) as u8;
    len += 1;
    value >>= 7;
    while value != 0 {
        value -= 1;
        bytes[len] = 0x80 | (value & 0x7f) as u8;
        len += 1;
        value >>= 7;
    }
    out.extend(bytes[..len].iter().rev());
}

fn decode_untracked_varint(bytes: &[u8], offset: &mut usize, end: usize) -> Result<usize> {
    let mut value = 0usize;
    loop {
        if *offset >= end {
            return Err(GitError::InvalidFormat(
                "truncated untracked-cache varint".into(),
            ));
        }
        let byte = bytes[*offset];
        *offset += 1;
        value = value
            .checked_add((byte & 0x7f) as usize)
            .ok_or_else(|| GitError::InvalidFormat("untracked-cache varint overflow".into()))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .ok_or_else(|| GitError::InvalidFormat("untracked-cache varint overflow".into()))?;
    }
}

fn memchr_zero(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|byte| *byte == 0)
}

fn read_untracked_cache_dir(
    bytes: &[u8],
    offset: &mut usize,
    end: usize,
    flat: &mut Vec<UntrackedCacheDir>,
) -> Result<UntrackedCacheDir> {
    let untracked_nr = decode_untracked_varint(bytes, offset, end)?;
    let dirs_nr = decode_untracked_varint(bytes, offset, end)?;
    let name_end = memchr_zero(&bytes[*offset..end]).ok_or_else(|| {
        GitError::InvalidFormat("unterminated untracked-cache directory name".into())
    })?;
    let name = bytes[*offset..*offset + name_end].to_vec();
    *offset += name_end + 1;
    let mut untracked = Vec::with_capacity(untracked_nr);
    for _ in 0..untracked_nr {
        let entry_end = memchr_zero(&bytes[*offset..end]).ok_or_else(|| {
            GitError::InvalidFormat("unterminated untracked-cache entry name".into())
        })?;
        untracked.push(bytes[*offset..*offset + entry_end].to_vec());
        *offset += entry_end + 1;
    }
    let flat_index = flat.len();
    flat.push(UntrackedCacheDir {
        name: name.clone(),
        untracked: untracked.clone(),
        recurse: true,
        ..UntrackedCacheDir::default()
    });
    let mut dirs = Vec::with_capacity(dirs_nr);
    for _ in 0..dirs_nr {
        dirs.push(read_untracked_cache_dir(bytes, offset, end, flat)?);
    }
    let dir = UntrackedCacheDir {
        name,
        untracked,
        dirs,
        recurse: true,
        ..UntrackedCacheDir::default()
    };
    flat[flat_index] = dir.clone();
    Ok(dir)
}

fn apply_untracked_dir_side_data(
    dir: &mut UntrackedCacheDir,
    flat: &[UntrackedCacheDir],
    index: &mut usize,
) {
    if let Some(source) = flat.get(*index) {
        dir.stat = source.stat;
        dir.exclude_oid = source.exclude_oid;
        dir.valid = source.valid;
        dir.check_only = source.check_only;
    }
    *index += 1;
    for child in &mut dir.dirs {
        apply_untracked_dir_side_data(child, flat, index);
    }
}

fn read_ewah_positions(bytes: &[u8], offset: &mut usize, end: usize) -> Result<Vec<u32>> {
    if end.saturating_sub(*offset) < 12 {
        return Err(GitError::InvalidFormat(
            "truncated untracked-cache ewah bitmap".into(),
        ));
    }
    let bit_size = u32_be(&bytes[*offset..*offset + 4]);
    *offset += 4;
    let word_count = u32_be(&bytes[*offset..*offset + 4]) as usize;
    *offset += 4;
    let words_end = offset
        .checked_add(word_count * 8)
        .filter(|next| *next <= end.saturating_sub(4))
        .ok_or_else(|| GitError::InvalidFormat("truncated untracked-cache ewah words".into()))?;
    let mut words = Vec::with_capacity(word_count);
    while *offset < words_end {
        words.push(u64_be(&bytes[*offset..*offset + 8]));
        *offset += 8;
    }
    let _rlw_position = u32_be(&bytes[*offset..*offset + 4]);
    *offset += 4;
    let mut raw = Vec::new();
    let mut idx = 0;
    while idx < words.len() {
        let rlw = words[idx];
        idx += 1;
        let run_bit = rlw & 1;
        let run_words = ((rlw >> 1) & 0xffff_ffff) as usize;
        let literal_words = (rlw >> 33) as usize;
        raw.extend(std::iter::repeat_n(
            if run_bit == 1 { u64::MAX } else { 0 },
            run_words,
        ));
        if idx + literal_words > words.len() {
            return Err(GitError::InvalidFormat(
                "untracked-cache ewah literal overflow".into(),
            ));
        }
        raw.extend_from_slice(&words[idx..idx + literal_words]);
        idx += literal_words;
    }
    let required_words = (bit_size as usize).div_ceil(64);
    if raw.len() < required_words {
        raw.resize(required_words, 0);
    }
    let mut positions = Vec::new();
    for (word_index, word) in raw.iter().take(required_words).enumerate() {
        let mut remaining = *word;
        while remaining != 0 {
            let bit = remaining.trailing_zeros();
            let position = (word_index as u64) * 64 + u64::from(bit);
            if position < u64::from(bit_size) {
                positions.push(position as u32);
            }
            remaining &= remaining - 1;
        }
    }
    Ok(positions)
}

fn write_ewah_positions(bit_size: u32, positions: &[u32], out: &mut Vec<u8>) {
    let word_count = (bit_size as usize).div_ceil(64);
    let mut raw = vec![0u64; word_count];
    for position in positions {
        if *position >= bit_size {
            continue;
        }
        raw[(*position / 64) as usize] |= 1u64 << (*position % 64);
    }
    let mut words = Vec::new();
    let mut rlw_position = 0u32;
    let mut idx = 0;
    while idx < raw.len() {
        rlw_position = words.len() as u32;
        words.push(0);
        let mut run_bit = false;
        let mut run_len = 0usize;
        if raw[idx] == 0 || raw[idx] == u64::MAX {
            run_bit = raw[idx] == u64::MAX;
            while idx < raw.len()
                && raw[idx] == if run_bit { u64::MAX } else { 0 }
                && run_len < 0xffff_ffff
            {
                run_len += 1;
                idx += 1;
            }
        }
        let literal_start = idx;
        while idx < raw.len() && raw[idx] != 0 && raw[idx] != u64::MAX {
            idx += 1;
        }
        let literal_len = idx - literal_start;
        let rlw = (run_bit as u64) | ((run_len as u64) << 1) | ((literal_len as u64) << 33);
        words[rlw_position as usize] = rlw;
        words.extend_from_slice(&raw[literal_start..literal_start + literal_len]);
    }
    out.extend_from_slice(&bit_size.to_be_bytes());
    out.extend_from_slice(&(words.len() as u32).to_be_bytes());
    for word in words {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out.extend_from_slice(&rlw_position.to_be_bytes());
}

/// The `CE_VALID`/assume-unchanged bit in [`IndexEntry::flags`] (git's
/// `CE_VALID`). When set, git trusts the cached stat unconditionally and never
/// re-checks the worktree file: `ce_match_stat` short-circuits to "unchanged"
/// regardless of the on-disk stat (see `git update-index --assume-unchanged`).
pub const INDEX_FLAG_VALID: u16 = 0x8000;

/// The `extended` bit in [`IndexEntry::flags`]: when set, a second
/// [`IndexEntry::flags_extended`] `u16` follows on disk (index v3+).
pub const INDEX_FLAG_EXTENDED: u16 = 0x4000;

/// Mask for the 2-bit merge stage stored in [`IndexEntry::flags`].
pub const INDEX_FLAG_STAGE_MASK: u16 = 0x3000;
const INDEX_FLAG_STAGE_SHIFT: u16 = 12;
/// Mask for the encoded name length in the low bits of [`IndexEntry::flags`].
pub const INDEX_FLAG_NAME_LENGTH_MASK: u16 = 0x0fff;

/// Intent-to-add (`git add -N`) bit in [`IndexEntry::flags_extended`].
pub const INDEX_EXTENDED_FLAG_INTENT_TO_ADD: u16 = 0x2000;
/// Skip-worktree (sparse checkout) bit in [`IndexEntry::flags_extended`].
pub const INDEX_EXTENDED_FLAG_SKIP_WORKTREE: u16 = 0x4000;

/// File mode of a sparse-directory entry in a sparse index: a directory
/// (`040000`). Git treats an entry with this mode as a collapsed cone-excluded
/// subtree (`S_ISSPARSEDIR`), its OID the tree object for that directory.
pub const SPARSE_DIR_MODE: u32 = 0o040000;

/// The four-byte signature of the optional `sdir` index extension
/// (`CACHE_EXT_SPARSE_DIRECTORIES`, `0x73646972`). The extension carries no
/// body; its mere presence marks the index as collapsed so a reader knows to
/// expand sparse-directory entries before operating on individual paths.
pub const INDEX_EXT_SPARSE_DIRECTORIES: [u8; 4] = *b"sdir";

/// The four-byte signature of git's split-index link extension (`link`).
pub const INDEX_EXT_LINK: [u8; 4] = *b"link";

/// Parsed body of the split-index `link` extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitIndexLink {
    pub base_oid: ObjectId,
    pub delete_positions: Vec<u32>,
    pub replace_positions: Vec<u32>,
}

impl Index {
    /// Whether this index is collapsed (carries `sdir` or any sparse-directory
    /// entry). A collapsed index must be expanded before per-path operations.
    pub fn is_sparse(&self) -> bool {
        self.entries.iter().any(IndexEntry::is_sparse_dir)
            || self
                .extension_chunks()
                .map(|chunks| {
                    chunks
                        .iter()
                        .any(|(sig, _)| *sig == INDEX_EXT_SPARSE_DIRECTORIES)
                })
                .unwrap_or(false)
    }

    /// Appends the zero-length `sdir` extension to this index's raw extension
    /// block if it is not already present, marking the index as a sparse index.
    /// Idempotent.
    pub fn set_sparse_extension(&mut self) {
        let already = self
            .extension_chunks()
            .map(|chunks| {
                chunks
                    .iter()
                    .any(|(sig, _)| *sig == INDEX_EXT_SPARSE_DIRECTORIES)
            })
            .unwrap_or(false);
        if already {
            return;
        }
        self.extensions
            .extend_from_slice(&INDEX_EXT_SPARSE_DIRECTORIES);
        self.extensions.extend_from_slice(&0u32.to_be_bytes());
    }

    /// Removes the `sdir` extension chunk (and any sparse-directory marker) from
    /// the raw extension block, leaving the index recorded as a full index.
    pub fn clear_sparse_extension(&mut self) -> Result<()> {
        let chunks = self.extension_chunks()?;
        let mut rebuilt = Vec::with_capacity(self.extensions.len());
        for (sig, body) in chunks {
            if sig == INDEX_EXT_SPARSE_DIRECTORIES {
                continue;
            }
            rebuilt.extend_from_slice(&sig);
            rebuilt.extend_from_slice(&(body.len() as u32).to_be_bytes());
            rebuilt.extend_from_slice(body);
        }
        self.extensions = rebuilt;
        Ok(())
    }
}

/// The merge stage encoded in an index entry's flags (a closed 0..=3 domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    /// Stage 0: a normal, non-conflicted entry.
    Normal,
    /// Stage 1: the common ancestor ("base") side of a conflict.
    Base,
    /// Stage 2: "our" side of a conflict.
    Ours,
    /// Stage 3: "their" side of a conflict.
    Theirs,
}

impl Stage {
    /// Extract the stage from a raw `flags` value.
    pub const fn from_flags(flags: u16) -> Self {
        match (flags & INDEX_FLAG_STAGE_MASK) >> INDEX_FLAG_STAGE_SHIFT {
            1 => Self::Base,
            2 => Self::Ours,
            3 => Self::Theirs,
            _ => Self::Normal,
        }
    }

    /// The numeric stage (0..=3).
    pub const fn as_u16(self) -> u16 {
        match self {
            Self::Normal => 0,
            Self::Base => 1,
            Self::Ours => 2,
            Self::Theirs => 3,
        }
    }
}

/// Resolve the index path for a repository, honoring `GIT_INDEX_FILE`.
pub fn repository_index_path(git_dir: impl AsRef<Path>) -> PathBuf {
    env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.as_ref().join("index"))
}

/// Read this repository's index and expand a split index through its
/// `sharedindex.<hash>` base when a `link` extension is present.
pub fn read_repository_index(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<Index> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    read_index_file_expanded(&index_path, git_dir, format)
}

fn read_index_file_expanded(
    index_path: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Index> {
    let mut index = Index::parse(&fs::read(index_path)?, format)?;
    let Some(link) = index.split_index_link(format)? else {
        return Ok(index);
    };
    if link.base_oid.is_null() {
        index.clear_split_index_link()?;
        return Ok(index);
    }
    let shared_path = git_dir.join(format!("sharedindex.{}", link.base_oid));
    let shared = Index::parse(&fs::read(&shared_path)?, format)?;
    let shared_checksum = shared.checksum.ok_or_else(|| {
        GitError::InvalidFormat(format!(
            "shared index {} has no checksum",
            shared_path.display()
        ))
    })?;
    if shared_checksum != link.base_oid {
        return Err(GitError::InvalidFormat(format!(
            "shared index checksum mismatch: expected {}, got {}",
            link.base_oid, shared_checksum
        )));
    }
    index.entries = merge_split_index_entries(shared.entries, index.entries, &link)?;
    Ok(index)
}

fn merge_split_index_entries(
    mut base_entries: Vec<IndexEntry>,
    delta_entries: Vec<IndexEntry>,
    link: &SplitIndexLink,
) -> Result<Vec<IndexEntry>> {
    let mut replacement_iter = delta_entries.into_iter();
    for position in &link.replace_positions {
        let position = *position as usize;
        if position >= base_entries.len() {
            return Err(GitError::InvalidFormat(
                "position for replacement exceeds base index size".into(),
            ));
        }
        let Some(replacement) = replacement_iter.next() else {
            return Err(GitError::InvalidFormat(
                "too few replacement entries".into(),
            ));
        };
        let mut replacement = replacement;
        if replacement.path.is_empty() {
            replacement.path = base_entries[position].path.clone();
            replacement.refresh_name_length();
        }
        base_entries[position] = replacement;
    }

    let mut delete_positions = link
        .delete_positions
        .iter()
        .map(|position| *position as usize)
        .collect::<Vec<_>>();
    delete_positions.sort_unstable();
    delete_positions.dedup();
    for position in delete_positions.iter().rev() {
        if *position >= base_entries.len() {
            return Err(GitError::InvalidFormat(
                "position for delete exceeds base index size".into(),
            ));
        }
        base_entries.remove(*position);
    }

    for entry in replacement_iter {
        if entry.path.is_empty() {
            return Err(GitError::InvalidFormat(
                "corrupt link extension replacement/addition ordering".into(),
            ));
        }
        base_entries.push(entry);
    }
    base_entries.sort_by(|left, right| {
        left.path
            .as_bytes()
            .cmp(right.path.as_bytes())
            .then_with(|| left.stage().as_u16().cmp(&right.stage().as_u16()))
    });
    Ok(base_entries)
}

/// The file's modification time split into whole seconds and nanoseconds,
/// matching how git stores mtimes in index entries.
pub fn file_mtime_parts(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((duration.as_secs(), u64::from(duration.subsec_nanos())))
}

/// Git file mode for the filesystem entry described by `metadata`.
pub fn worktree_metadata_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        0o120000
    } else if metadata.is_dir() {
        0o040000
    } else {
        worktree_file_mode(metadata)
    }
}

#[cfg(unix)]
fn worktree_file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn worktree_file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

/// Reusable stage-0 index entries plus the index file mtime used for racy-git
/// stat validation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexStatCache {
    entries: HashMap<Vec<u8>, IndexEntry>,
    index_mtime: Option<(u64, u64)>,
}

impl IndexStatCache {
    /// Build a stat cache from a parsed index and the index file path on disk.
    pub fn from_index(index: &Index, index_path: impl AsRef<Path>) -> Self {
        let index_mtime = fs::metadata(index_path.as_ref())
            .ok()
            .and_then(|metadata| file_mtime_parts(&metadata));
        Self::from_index_mtime(index, index_mtime)
    }

    /// Build a stat cache from a parsed index and an already captured index
    /// file mtime.
    pub fn from_index_mtime(index: &Index, index_mtime: Option<(u64, u64)>) -> Self {
        Self {
            entries: stage0_index_entries(index),
            index_mtime,
        }
    }

    /// Build a stat cache that can validate caller-provided index entries
    /// against worktree metadata without owning its own path lookup table.
    pub fn from_index_mtime_only(index_mtime: Option<(u64, u64)>) -> Self {
        Self {
            entries: HashMap::new(),
            index_mtime,
        }
    }

    /// Read and parse an index file into a stat cache. A missing index returns
    /// an empty cache.
    pub fn from_index_file(index_path: impl AsRef<Path>, format: ObjectFormat) -> Result<Self> {
        let index_path = index_path.as_ref();
        let metadata = match fs::metadata(index_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err.into()),
        };
        Self::from_index_file_with_metadata(index_path, format, file_mtime_parts(&metadata))
    }

    fn from_index_file_with_metadata(
        index_path: &Path,
        format: ObjectFormat,
        index_mtime: Option<(u64, u64)>,
    ) -> Result<Self> {
        let bytes = fs::read(index_path)?;
        let index = Index::parse(&bytes, format)?;
        Ok(Self::from_index_mtime(&index, index_mtime))
    }

    /// Read this repository's index into a reusable stat cache.
    pub fn from_repository_index(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<Self> {
        Self::from_index_file(repository_index_path(git_dir), format)
    }

    /// Return the cached stage-0 entry for `git_path`, if one exists.
    pub fn entry_for_git_path(&self, git_path: &[u8]) -> Option<&IndexEntry> {
        self.entries.get(git_path)
    }

    /// Whether this cache has a stage-0 entry for `git_path`.
    pub fn contains_git_path(&self, git_path: &[u8]) -> bool {
        self.entries.contains_key(git_path)
    }

    /// Number of stage-0 entries in this cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this cache has no stage-0 entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The index file mtime used as the racy-clean reference.
    pub fn index_mtime(&self) -> Option<(u64, u64)> {
        self.index_mtime
    }

    /// Whether `entry` is racily clean in git's sense.
    pub fn is_racily_clean(&self, entry: &IndexEntry) -> bool {
        index_entry_is_racily_clean(entry, self.index_mtime)
    }

    /// Return the cached entry for `git_path` only when `metadata` proves the
    /// worktree file is unchanged and not racily clean.
    pub fn reusable_entry<'a>(
        &'a self,
        git_path: &[u8],
        worktree_metadata: &fs::Metadata,
    ) -> Option<&'a IndexEntry> {
        let entry = self.entries.get(git_path)?;
        self.reusable_index_entry(entry, worktree_metadata)
    }

    /// Return `entry` only when `metadata` proves the worktree file is unchanged
    /// and not racily clean.
    pub fn reusable_index_entry<'a>(
        &'a self,
        entry: &'a IndexEntry,
        worktree_metadata: &fs::Metadata,
    ) -> Option<&'a IndexEntry> {
        // A gitlink is reusable as-is whenever its worktree path is a directory
        // (git never re-hashes a submodule; the cached stat is ignored). The
        // populated-HEAD comparison is the caller's, but a reusable-entry probe
        // is for the "skip the content re-read" shortcut, and a gitlink has no
        // content to read — so a directory on disk reuses the entry. A
        // non-directory (`TYPE_CHANGED`) is not reusable.
        if is_gitlink(entry.mode) {
            return match gitlink_stat_verdict(worktree_metadata) {
                GitlinkStatVerdict::Populated => Some(entry),
                GitlinkStatVerdict::TypeChanged => None,
            };
        }
        if entry.mode != worktree_metadata_mode(worktree_metadata) {
            return None;
        }
        if !index_entry_stat_is_uptodate(entry, worktree_metadata) {
            return None;
        }
        if self.is_racily_clean(entry) {
            return None;
        }
        Some(entry)
    }

    /// Whether `entry` describes the current worktree metadata and is not
    /// racily clean.
    pub fn reusable_index_entry_ref(
        &self,
        entry: &IndexEntryRef<'_>,
        worktree_metadata: &fs::Metadata,
    ) -> bool {
        // Gitlink: reusable whenever the worktree path is a directory (no
        // content to re-hash); see [`Self::reusable_index_entry`].
        if is_gitlink(entry.mode) {
            return matches!(
                gitlink_stat_verdict(worktree_metadata),
                GitlinkStatVerdict::Populated
            );
        }
        if entry.mode != worktree_metadata_mode(worktree_metadata) {
            return false;
        }
        if !index_entry_ref_stat_is_uptodate(entry, worktree_metadata) {
            return false;
        }
        if index_entry_ref_is_racily_clean(entry, self.index_mtime) {
            return false;
        }
        true
    }

    /// git's `ce_match_stat` verdict for `entry` against the worktree file's
    /// `metadata`, used by `diff-files` to decide which entries to select.
    ///
    /// Precedence (mirrors `read-cache.c:ie_match_stat`):
    ///   1. `CE_VALID`/assume-unchanged set → [`StatVerdict::Clean`] (git trusts
    ///      the cache blindly, regardless of the on-disk stat).
    ///   2. mode or cached stat mismatch → [`StatVerdict::Dirty`]. A zeroed/invalid
    ///      cached stat (e.g. a freshly `rm --cached`-then-`reset --no-refresh`
    ///      entry, whose ctime/mtime are all zero) fails the stat-uptodate check
    ///      and so is reported dirty — git does NOT re-hash to "rescue" it.
    ///   3. stat matches but the entry is racily clean (its mtime is at/after the
    ///      index's, so a same-second edit could be invisible) →
    ///      [`StatVerdict::RacyNeedsContentCheck`]: the caller must re-hash the
    ///      content and report dirty only if it actually differs from the cached
    ///      oid (git's `ce_compare_data` in the racy branch).
    ///   4. stat matches and is not racy → [`StatVerdict::Clean`].
    ///
    /// This never reads or hashes the file; the racy content check is the caller's
    /// responsibility (it owns the worktree-blob access).
    pub fn index_entry_worktree_stat_verdict(
        &self,
        entry: &IndexEntry,
        worktree_metadata: &fs::Metadata,
    ) -> StatVerdict {
        if entry.flags & INDEX_FLAG_VALID != 0 {
            return StatVerdict::Clean;
        }
        // git's `ce_match_stat_basic` `S_IFGITLINK` arm: a gitlink ignores the
        // cached stat entirely. A non-directory on disk is `TYPE_CHANGED`
        // (dirty); a directory is clean unless the embedded HEAD differs, which
        // the caller resolves (`Populated` → treat as clean here, the
        // unpopulated-submodule default). Crucially a gitlink is NEVER reported
        // dirty merely because a directory's `worktree_metadata_mode` (040000)
        // does not equal the gitlink mode (160000) — that mode mismatch is the
        // bug the single primitive exists to prevent.
        if is_gitlink(entry.mode) {
            return match gitlink_stat_verdict(worktree_metadata) {
                GitlinkStatVerdict::TypeChanged => StatVerdict::Dirty,
                GitlinkStatVerdict::Populated => StatVerdict::Clean,
            };
        }
        if entry.mode != worktree_metadata_mode(worktree_metadata) {
            return StatVerdict::Dirty;
        }
        if !index_entry_stat_is_uptodate(entry, worktree_metadata) {
            return StatVerdict::Dirty;
        }
        if self.is_racily_clean(entry) {
            return StatVerdict::RacyNeedsContentCheck;
        }
        StatVerdict::Clean
    }
}

/// The outcome of [`IndexStatCache::index_entry_worktree_stat_verdict`] — git's
/// `ce_match_stat` result, split so the caller can resolve the racy case by a
/// content re-hash without this crate needing worktree-blob access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatVerdict {
    /// The cached stat proves the entry unchanged (or `CE_VALID` is set): no
    /// content check needed, the entry is clean.
    Clean,
    /// Mode or stat mismatch (including a zeroed/invalid cached stat): the entry
    /// is changed without any content re-hash.
    Dirty,
    /// Stat matches but the entry is racily clean: the caller must compare the
    /// worktree content to the cached oid to decide.
    RacyNeedsContentCheck,
}

/// Stage-0 index stat data that can prove a worktree path clean without
/// re-reading and re-hashing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatProbe {
    entry: IndexEntry,
    index_mtime: Option<(u64, u64)>,
}

impl IndexStatProbe {
    /// Build a probe from a parsed stage-0 index entry and the index file's
    /// mtime split as `(seconds, nanoseconds)`.
    pub fn from_index_entry(entry: IndexEntry, index_mtime: Option<(u64, u64)>) -> Self {
        Self { entry, index_mtime }
    }

    /// Build a probe from a parsed index entry and the path of the index file on
    /// disk, using that file's mtime as the racy-clean reference timestamp.
    pub fn from_index_entry_and_index_path(
        entry: IndexEntry,
        index_path: impl AsRef<Path>,
    ) -> Self {
        let index_mtime = fs::metadata(index_path.as_ref())
            .ok()
            .and_then(|metadata| file_mtime_parts(&metadata));
        Self { entry, index_mtime }
    }

    /// Read this repository's index and return a probe for `git_path` when a
    /// stage-0 entry exists.
    pub fn from_repository_index(
        git_dir: impl AsRef<Path>,
        format: ObjectFormat,
        git_path: &[u8],
    ) -> Result<Option<Self>> {
        let index_path = repository_index_path(git_dir);
        cached_repository_index_stat_probe(&index_path, format, git_path)
    }

    /// The parsed index entry this probe was built from.
    pub fn entry(&self) -> &IndexEntry {
        &self.entry
    }

    /// The index file mtime used as the racy-clean reference timestamp.
    pub fn index_mtime(&self) -> Option<(u64, u64)> {
        self.index_mtime
    }
}

/// Reusable stage-0 index stat probes for many worktree paths.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexStatProbeCache {
    stat_cache: IndexStatCache,
}

impl IndexStatProbeCache {
    /// Build a reusable probe cache from an already parsed index and index-file
    /// mtime.
    pub fn from_index(index: &Index, index_mtime: Option<(u64, u64)>) -> Self {
        Self {
            stat_cache: IndexStatCache::from_index_mtime(index, index_mtime),
        }
    }

    /// Read this repository's index once and build reusable stat probes.
    pub fn from_repository_index(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<Self> {
        Ok(Self {
            stat_cache: IndexStatCache::from_repository_index(git_dir, format)?,
        })
    }

    /// Return a per-path probe for a stage-0 entry, if present.
    pub fn probe_for_git_path(&self, git_path: &[u8]) -> Option<IndexStatProbe> {
        self.stat_cache
            .entry_for_git_path(git_path)
            .cloned()
            .map(|entry| IndexStatProbe {
                entry,
                index_mtime: self.stat_cache.index_mtime,
            })
    }

    /// Whether this cache has a stage-0 entry for `git_path`.
    pub fn contains_git_path(&self, git_path: &[u8]) -> bool {
        self.stat_cache.contains_git_path(git_path)
    }

    /// Number of stage-0 entries in the cache.
    pub fn len(&self) -> usize {
        self.stat_cache.len()
    }

    /// Whether the cache has no stage-0 entries.
    pub fn is_empty(&self) -> bool {
        self.stat_cache.is_empty()
    }

    /// The index file mtime used as the racy-clean reference timestamp.
    pub fn index_mtime(&self) -> Option<(u64, u64)> {
        self.stat_cache.index_mtime()
    }
}

#[derive(Clone)]
struct CachedRepositoryIndexStatProbes {
    index_path: PathBuf,
    format: ObjectFormat,
    len: u64,
    mtime: Option<(u64, u64)>,
    probes: IndexStatProbeCache,
}

static REPOSITORY_INDEX_STAT_PROBES: OnceLock<Mutex<Option<CachedRepositoryIndexStatProbes>>> =
    OnceLock::new();

fn cached_repository_index_stat_probe(
    index_path: &Path,
    format: ObjectFormat,
    git_path: &[u8],
) -> Result<Option<IndexStatProbe>> {
    let metadata = match fs::metadata(index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(cache) = REPOSITORY_INDEX_STAT_PROBES.get()
                && let Ok(mut guard) = cache.lock()
            {
                *guard = None;
            }
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    let len = metadata.len();
    let mtime = file_mtime_parts(&metadata);
    let cache = REPOSITORY_INDEX_STAT_PROBES.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.as_ref()
        && cached.index_path == index_path
        && cached.format == format
        && cached.len == len
        && cached.mtime == mtime
    {
        return Ok(cached.probes.probe_for_git_path(git_path));
    }

    let stat_cache = IndexStatCache::from_index_file_with_metadata(index_path, format, mtime)?;
    let probes = IndexStatProbeCache { stat_cache };
    let probe = probes.probe_for_git_path(git_path);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedRepositoryIndexStatProbes {
            index_path: index_path.to_path_buf(),
            format,
            len,
            mtime,
            probes,
        });
    }
    Ok(probe)
}

fn stage0_index_entries(index: &Index) -> HashMap<Vec<u8>, IndexEntry> {
    let mut entries = HashMap::new();
    for entry in &index.entries {
        if entry.stage() == Stage::Normal {
            entries.insert(entry.path.as_bytes().to_vec(), entry.clone());
        }
    }
    entries
}

fn index_entry_is_racily_clean(entry: &IndexEntry, index_mtime: Option<(u64, u64)>) -> bool {
    let Some(index_mtime) = index_mtime else {
        return true;
    };
    if index_mtime == (0, 0) {
        return true;
    }
    let entry_mtime = (
        u64::from(entry.mtime_seconds),
        u64::from(entry.mtime_nanoseconds),
    );
    if entry_mtime == (0, 0) {
        return true;
    }
    index_mtime <= entry_mtime
}

fn index_entry_stat_is_uptodate(entry: &IndexEntry, metadata: &fs::Metadata) -> bool {
    if u64::from(entry.size) != metadata.len() {
        return false;
    }
    let Some((mtime_seconds, mtime_nanoseconds)) = file_mtime_parts(metadata) else {
        return false;
    };
    u64::from(entry.mtime_seconds) == mtime_seconds
        && u64::from(entry.mtime_nanoseconds) == mtime_nanoseconds
}

fn index_entry_ref_is_racily_clean(
    entry: &IndexEntryRef<'_>,
    index_mtime: Option<(u64, u64)>,
) -> bool {
    let Some(index_mtime) = index_mtime else {
        return true;
    };
    if index_mtime == (0, 0) {
        return true;
    }
    let entry_mtime = (
        u64::from(entry.mtime_seconds),
        u64::from(entry.mtime_nanoseconds),
    );
    if entry_mtime == (0, 0) {
        return true;
    }
    index_mtime <= entry_mtime
}

fn index_entry_ref_stat_is_uptodate(entry: &IndexEntryRef<'_>, metadata: &fs::Metadata) -> bool {
    if u64::from(entry.size) != metadata.len() {
        return false;
    }
    let Some((mtime_seconds, mtime_nanoseconds)) = file_mtime_parts(metadata) else {
        return false;
    };
    u64::from(entry.mtime_seconds) == mtime_seconds
        && u64::from(entry.mtime_nanoseconds) == mtime_nanoseconds
}

/// The cache-tree (`TREE`) extension: a recursive cache of the tree object ids
/// that a fully-staged index would write, mirroring `struct cache_tree` in git.
///
/// On disk the extension body is a pre-order (depth-first) sequence of records,
/// one per node of the directory tree. Each record is:
///
/// ```text
/// <path-component> NUL          (the root node's component is empty)
/// <ASCII entry_count> SP <ASCII subtree_count> LF
/// <raw object id>               (present iff entry_count >= 0)
/// ```
///
/// `entry_count` is the number of blobs/subtrees the cached tree object spans,
/// or `-1` when the entry is invalid (dirty); an invalid entry stores no object
/// id. `subtree_count` is the number of immediate child directories, whose
/// records follow recursively. [`CacheTree`] models the root node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTree {
    /// Number of index entries covered by this (sub)tree, or `-1` if invalid.
    pub entry_count: i32,
    /// The cached tree object id, present iff `entry_count >= 0`.
    pub oid: Option<ObjectId>,
    /// Immediate child directories, in the order git stores them.
    pub subtrees: Vec<CacheTreeChild>,
}

/// A named child of a [`CacheTree`] node (a subdirectory's cached tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTreeChild {
    /// The directory's path component (no separators, never empty).
    pub name: Vec<u8>,
    /// The cached subtree rooted at this directory.
    pub tree: CacheTree,
}

impl CacheTree {
    /// Parse the body of a `TREE` extension (the bytes after the 8-byte
    /// signature/length header).
    pub fn parse(format: ObjectFormat, body: &[u8]) -> Result<Self> {
        let mut offset = 0usize;
        let (entry_count, oid, subtrees) = parse_cache_tree_node(format, body, &mut offset, &[])?;
        if offset != body.len() {
            return Err(GitError::InvalidFormat(
                "trailing bytes after cache-tree root".into(),
            ));
        }
        Ok(Self {
            entry_count,
            oid,
            subtrees,
        })
    }

    /// Serialize this cache-tree to a `TREE` extension body, byte-compatible
    /// with upstream git. Returns an error if an entry's validity flag and its
    /// object id disagree, or if a child name contains a NUL or `/`.
    pub fn write(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        write_cache_tree_node(self, b"", &mut out)?;
        Ok(out)
    }

    /// Serialize this cache-tree as a complete extension chunk (the
    /// `TREE` signature, big-endian length, and body) ready to splice into an
    /// index's extension area.
    pub fn to_extension_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        encode_index_extension(&mut out, b"TREE", &self.write()?)?;
        Ok(out)
    }
}

/// Rough upper bound on a serialized cache-tree extension chunk, used only to
/// pre-size buffers.
fn cache_tree_estimate(tree: &CacheTree) -> usize {
    fn node(tree: &CacheTree) -> usize {
        let own = 8
            + tree
                .oid
                .as_ref()
                .map(|oid| oid.as_bytes().len())
                .unwrap_or(0)
            + 16;
        tree.subtrees.iter().fold(own, |acc, child| {
            acc + child.name.len() + 1 + node(&child.tree)
        })
    }
    8 + node(tree)
}

fn parse_cache_tree_node(
    format: ObjectFormat,
    body: &[u8],
    offset: &mut usize,
    expected_name: &[u8],
) -> Result<(i32, Option<ObjectId>, Vec<CacheTreeChild>)> {
    // <name> NUL
    let name_start = *offset;
    while body.get(*offset).copied() != Some(0) {
        *offset += 1;
        if *offset >= body.len() {
            return Err(GitError::InvalidFormat(
                "unterminated cache-tree path component".into(),
            ));
        }
    }
    if &body[name_start..*offset] != expected_name {
        return Err(GitError::InvalidFormat(
            "cache-tree path component does not match parent record".into(),
        ));
    }
    *offset += 1; // consume NUL

    // <entry_count> SP <subtree_count> LF
    let count_start = *offset;
    while body.get(*offset).copied() != Some(b' ') {
        *offset += 1;
        if *offset >= body.len() {
            return Err(GitError::InvalidFormat(
                "unterminated cache-tree entry count".into(),
            ));
        }
    }
    let entry_count: i32 = std::str::from_utf8(&body[count_start..*offset])
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| GitError::InvalidFormat("invalid cache-tree entry count".into()))?;
    *offset += 1; // consume SP

    let subtree_start = *offset;
    while body.get(*offset).copied() != Some(b'\n') {
        *offset += 1;
        if *offset >= body.len() {
            return Err(GitError::InvalidFormat(
                "unterminated cache-tree subtree count".into(),
            ));
        }
    }
    let subtree_count: usize = std::str::from_utf8(&body[subtree_start..*offset])
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| GitError::InvalidFormat("invalid cache-tree subtree count".into()))?;
    *offset += 1; // consume LF

    // <object id> only when the entry is valid (entry_count >= 0).
    let oid = if entry_count >= 0 {
        let oid_end = offset
            .checked_add(format.raw_len())
            .ok_or_else(|| GitError::InvalidFormat("cache-tree object id overflow".into()))?;
        if oid_end > body.len() {
            return Err(GitError::InvalidFormat(
                "truncated cache-tree object id".into(),
            ));
        }
        let oid = ObjectId::from_raw(format, &body[*offset..oid_end])?;
        *offset = oid_end;
        Some(oid)
    } else {
        None
    };

    let mut subtrees = Vec::with_capacity(subtree_count);
    for _ in 0..subtree_count {
        // Peek the child's name while still delegating the NUL handling to the
        // recursive call.
        let child_name_start = *offset;
        let mut scan = *offset;
        while body.get(scan).copied() != Some(0) {
            scan += 1;
            if scan >= body.len() {
                return Err(GitError::InvalidFormat(
                    "unterminated cache-tree path component".into(),
                ));
            }
        }
        let child_name = body[child_name_start..scan].to_vec();
        if child_name.is_empty() {
            return Err(GitError::InvalidFormat(
                "cache-tree subtree has empty name".into(),
            ));
        }
        let (child_entry_count, child_oid, grandchildren) =
            parse_cache_tree_node(format, body, offset, &child_name)?;
        subtrees.push(CacheTreeChild {
            name: child_name,
            tree: CacheTree {
                entry_count: child_entry_count,
                oid: child_oid,
                subtrees: grandchildren,
            },
        });
    }

    Ok((entry_count, oid, subtrees))
}

fn write_cache_tree_node(tree: &CacheTree, name: &[u8], out: &mut Vec<u8>) -> Result<()> {
    if name.contains(&0) || name.contains(&b'/') {
        return Err(GitError::InvalidFormat(
            "cache-tree path component contains NUL or separator".into(),
        ));
    }
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(tree.entry_count.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(tree.subtrees.len().to_string().as_bytes());
    out.push(b'\n');
    match (&tree.oid, tree.entry_count >= 0) {
        (Some(oid), true) => out.extend_from_slice(oid.as_bytes()),
        (None, false) => {}
        (Some(_), false) => {
            return Err(GitError::InvalidFormat(
                "invalid cache-tree entry must not carry an object id".into(),
            ));
        }
        (None, true) => {
            return Err(GitError::InvalidFormat(
                "valid cache-tree entry is missing its object id".into(),
            ));
        }
    }
    for child in &tree.subtrees {
        write_cache_tree_node(&child.tree, &child.name, out)?;
    }
    Ok(())
}

/// Walk the flat extension area of an index, returning each
/// `(4-byte signature, body)` record in order.
fn parse_index_extension_chunks(extensions: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut chunks = Vec::new();
    let mut offset = 0usize;
    while offset < extensions.len() {
        if extensions.len() - offset < 8 {
            return Err(GitError::InvalidFormat(
                "truncated index extension header".into(),
            ));
        }
        let signature = [
            extensions[offset],
            extensions[offset + 1],
            extensions[offset + 2],
            extensions[offset + 3],
        ];
        let len = u32_be(&extensions[offset + 4..offset + 8]) as usize;
        let body_start = offset + 8;
        let body_end = body_start
            .checked_add(len)
            .ok_or_else(|| GitError::InvalidFormat("index extension length overflow".into()))?;
        if body_end > extensions.len() {
            return Err(GitError::InvalidFormat(
                "index extension body extends past end".into(),
            ));
        }
        chunks.push((signature, &extensions[body_start..body_end]));
        offset = body_end;
    }
    Ok(chunks)
}

/// Append a single extension chunk (`signature`, big-endian length, `body`) to
/// `out`.
fn encode_index_extension(out: &mut Vec<u8>, signature: &[u8; 4], body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len())
        .map_err(|_| GitError::InvalidFormat("index extension body too large".into()))?;
    out.extend_from_slice(signature);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(())
}

fn decode_index_v4_path_strip_len(
    bytes: &[u8],
    offset: &mut usize,
    checksum_offset: usize,
) -> Result<usize> {
    let Some(first) = bytes.get(*offset).copied() else {
        return Err(GitError::InvalidFormat(
            "truncated index v4 path compression".into(),
        ));
    };
    *offset += 1;
    let mut value = (first & 0x7f) as usize;
    let mut byte = first;
    while byte & 0x80 != 0 {
        if *offset >= checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated index v4 path compression".into(),
            ));
        }
        byte = bytes[*offset];
        *offset += 1;
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add((byte & 0x7f) as usize))
            .ok_or_else(|| GitError::InvalidFormat("index v4 path compression overflow".into()))?;
    }
    Ok(value)
}

fn encode_index_v4_path_strip_len(strip_len: usize, out: &mut Vec<u8>) {
    let mut bytes = Vec::new();
    bytes.push((strip_len & 0x7f) as u8);
    let mut value = strip_len >> 7;
    while value != 0 {
        value -= 1;
        bytes.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    for byte in bytes.iter().rev() {
        out.push(*byte);
    }
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn u16_be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn is_gitlink_matches_git_s_isgitlink() {
        // Only the gitlink file-type bits classify as a gitlink; blobs,
        // executables, symlinks, and trees do not.
        assert!(is_gitlink(GITLINK_MODE));
        assert!(is_gitlink(0o160000));
        // Permission bits ORed onto the gitlink type still classify (mask).
        assert!(is_gitlink(0o160000 | 0o755));
        assert!(!is_gitlink(0o100644));
        assert!(!is_gitlink(0o100755));
        assert!(!is_gitlink(0o120000)); // symlink
        assert!(!is_gitlink(0o040000)); // tree / on-disk directory mode
        assert!(!is_gitlink(0));
    }

    #[test]
    fn gitlink_stat_verdict_directory_is_populated_else_typechanged() {
        let dir = unique_temp_dir("gitlink-verdict");
        fs::create_dir_all(&dir).expect("test operation should succeed");
        // A directory on disk → Populated (clean unless HEAD differs, the
        // caller's job). The 040000-vs-160000 mode mismatch must NOT make it
        // dirty — the whole point of the primitive.
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).expect("test operation should succeed");
        assert_eq!(
            gitlink_stat_verdict(
                &fs::symlink_metadata(&sub).expect("test operation should succeed")
            ),
            GitlinkStatVerdict::Populated
        );
        // A regular file where the submodule should be → TypeChanged (dirty).
        let file = dir.join("file");
        fs::write(&file, b"x").expect("test operation should succeed");
        assert_eq!(
            gitlink_stat_verdict(
                &fs::symlink_metadata(&file).expect("test operation should succeed")
            ),
            GitlinkStatVerdict::TypeChanged
        );
    }

    #[test]
    fn stat_verdict_treats_populated_gitlink_as_clean_not_dirty() {
        // Regression for the consolidation: a gitlink entry (mode 160000) whose
        // worktree path is a directory (mode 040000) must be Clean, NOT Dirty.
        // Before the primitive, `entry.mode != worktree_metadata_mode` reported
        // every populated submodule dirty (`update-index --refresh: needs
        // update`).
        let dir = unique_temp_dir("gitlink-stat-clean");
        fs::create_dir_all(&dir).expect("test operation should succeed");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).expect("test operation should succeed");
        let entry = IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode: GITLINK_MODE,
            uid: 0,
            gid: 0,
            size: 0,
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "ce013625030ba8dba906f756967f9e9ca394464a",
            )
            .expect("test operation should succeed"),
            flags: 3,
            flags_extended: 0,
            path: BString::from(b"sub"),
        };
        let cache = IndexStatCache::default();
        let md = fs::symlink_metadata(&sub).expect("test operation should succeed");
        assert_eq!(
            cache.index_entry_worktree_stat_verdict(&entry, &md),
            StatVerdict::Clean,
            "populated gitlink directory must be clean, not dirty"
        );
        // Replace the directory with a file → TypeChanged → Dirty.
        fs::remove_dir(&sub).expect("test operation should succeed");
        fs::write(&sub, b"x").expect("test operation should succeed");
        let md = fs::symlink_metadata(&sub).expect("test operation should succeed");
        assert_eq!(
            cache.index_entry_worktree_stat_verdict(&entry, &md),
            StatVerdict::Dirty,
            "a file where the gitlink dir should be is a type change (dirty)"
        );
    }

    #[test]
    fn index_v2_round_trips_entry() {
        let index = Index {
            version: 2,
            entries: vec![IndexEntry {
                ctime_seconds: 1,
                ctime_nanoseconds: 2,
                mtime_seconds: 3,
                mtime_nanoseconds: 4,
                dev: 5,
                ino: 6,
                mode: 0o100644,
                uid: 7,
                gid: 8,
                size: 6,
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "ce013625030ba8dba906f756967f9e9ca394464a",
                )
                .expect("test operation should succeed"),
                flags: 5,
                flags_extended: 0,
                path: BString::from(b"a.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index
            .write_v2_sha1()
            .expect("test operation should succeed");
        let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
    }

    #[test]
    fn index_v2_round_trips_sha256_entry() {
        let index = Index {
            version: 2,
            entries: vec![IndexEntry {
                ctime_seconds: 1,
                ctime_nanoseconds: 2,
                mtime_seconds: 3,
                mtime_nanoseconds: 4,
                dev: 5,
                ino: 6,
                mode: 0o100644,
                uid: 7,
                gid: 8,
                size: 6,
                oid: sley_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"hello\n")
                    .expect("test operation should succeed"),
                flags: 5,
                flags_extended: 0,
                path: BString::from(b"a.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index
            .write(ObjectFormat::Sha256)
            .expect("test operation should succeed");
        let parsed =
            Index::parse(&bytes, ObjectFormat::Sha256).expect("test operation should succeed");
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
        assert!(Index::parse_v2_sha1(&bytes).is_err());
    }

    #[test]
    fn index_v4_round_trips_prefix_compressed_paths() {
        let long_path = vec![b'a'; 140];
        let index = Index {
            version: 4,
            entries: vec![
                IndexEntry {
                    ctime_seconds: 1,
                    ctime_nanoseconds: 2,
                    mtime_seconds: 3,
                    mtime_nanoseconds: 4,
                    dev: 5,
                    ino: 6,
                    mode: 0o100644,
                    uid: 7,
                    gid: 8,
                    size: 1,
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "ce013625030ba8dba906f756967f9e9ca394464a",
                    )
                    .expect("test operation should succeed"),
                    flags: long_path.len() as u16,
                    flags_extended: 0,
                    path: BString::from(long_path.as_slice()),
                },
                IndexEntry {
                    ctime_seconds: 9,
                    ctime_nanoseconds: 10,
                    mtime_seconds: 11,
                    mtime_nanoseconds: 12,
                    dev: 13,
                    ino: 14,
                    mode: 0o100644,
                    uid: 15,
                    gid: 16,
                    size: 1,
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2e65efe2a145dda7ee51d1741299f848e5bf752e",
                    )
                    .expect("test operation should succeed"),
                    flags: 1,
                    flags_extended: 0,
                    path: BString::from(b"b"),
                },
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_sha1().expect("test operation should succeed");
        assert!(bytes.windows(3).any(|window| window == [0x80, 0x0c, b'b']));
        let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
    }

    #[test]
    fn index_rejects_bad_checksum() {
        let index = Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        };
        let mut bytes = index
            .write_v2_sha1()
            .expect("test operation should succeed");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(Index::parse_v2_sha1(&bytes).is_err());
    }

    /// Build a minimal index entry for the given path / object id.
    fn index_entry(path: &[u8], hex: &str) -> IndexEntry {
        IndexEntry {
            ctime_seconds: 1,
            ctime_nanoseconds: 2,
            mtime_seconds: 3,
            mtime_nanoseconds: 4,
            dev: 5,
            ino: 6,
            mode: 0o100644,
            uid: 7,
            gid: 8,
            size: 9,
            oid: oid(hex),
            // git stores `min(path_len, 0xfff)` in the low 12 bits of `flags`.
            flags: u16::try_from(path.len().min(0xfff)).expect("test operation should succeed"),
            flags_extended: 0,
            path: BString::from(path),
        }
    }

    /// Three entries with shared prefixes; the middle one carries the
    /// skip-worktree extended flag (on-disk value `0x4000`).
    fn sample_index(version: u32) -> Index {
        let mut skip = index_entry(b"src/lib.rs", "2e65efe2a145dda7ee51d1741299f848e5bf752e");
        skip.flags |= INDEX_FLAG_EXTENDED;
        skip.flags_extended = INDEX_SKIP_WORKTREE_ON_DISK;
        Index {
            version,
            entries: vec![
                index_entry(b"README.md", "ce013625030ba8dba906f756967f9e9ca394464a"),
                index_entry(b"src/bin.rs", "1234567890123456789012345678901234567890"),
                skip,
                index_entry(b"src/main.rs", "abcdef0123456789abcdef0123456789abcdef01"),
            ],
            extensions: Vec::new(),
            checksum: None,
        }
    }

    /// On-disk extended-flags bit for skip-worktree (git's `CE_SKIP_WORKTREE`
    /// stored as `flags >> 16`).
    const INDEX_SKIP_WORKTREE_ON_DISK: u16 = 0x4000;

    #[test]
    fn index_v3_round_trips_skip_worktree_extended_flags() {
        let index = sample_index(3);
        let bytes = index.write_sha1().expect("test operation should succeed");
        // Header advertises version 3.
        assert_eq!(&bytes[0..4], b"DIRC");
        assert_eq!(u32_be(&bytes[4..8]), 3);
        let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        // The skip-worktree entry must round-trip its extended bit + flag.
        let skip = parsed
            .entries
            .iter()
            .find(|entry| entry.path == b"src/lib.rs")
            .expect("test operation should succeed");
        assert_ne!(skip.flags & INDEX_FLAG_EXTENDED, 0);
        assert_eq!(skip.flags_extended, INDEX_SKIP_WORKTREE_ON_DISK);
        // Plain entries stay v2-style (no extended bit, no extended field).
        let plain = parsed
            .entries
            .iter()
            .find(|entry| entry.path == b"README.md")
            .expect("test operation should succeed");
        assert_eq!(plain.flags & INDEX_FLAG_EXTENDED, 0);
        assert_eq!(plain.flags_extended, 0);
    }

    #[test]
    fn index_all_versions_round_trip_same_entries() {
        // The same logical entries should survive a write/parse cycle for every
        // supported on-disk version, differing only in `version`.
        for version in [2u32, 3, 4] {
            let mut index = sample_index(version);
            if version == 2 {
                // v2 cannot encode extended flags; drop them for this case.
                for entry in &mut index.entries {
                    entry.flags &= !INDEX_FLAG_EXTENDED;
                    entry.flags_extended = 0;
                }
            }
            let bytes = index.write_sha1().expect("test operation should succeed");
            assert_eq!(u32_be(&bytes[4..8]), version);
            let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
            assert_eq!(parsed.version, version, "version {version}");
            assert_eq!(parsed.entries, index.entries, "entries for v{version}");
        }
    }

    #[test]
    fn index_for_each_path_matches_full_parse() {
        for version in [2u32, 3, 4] {
            let mut index = sample_index(version);
            if version == 2 {
                for entry in &mut index.entries {
                    entry.flags &= !INDEX_FLAG_EXTENDED;
                    entry.flags_extended = 0;
                }
            }
            let bytes = index.write_sha1().expect("test operation should succeed");
            let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
            let expected = parsed
                .entries
                .iter()
                .map(|entry| entry.path.as_bytes().to_vec())
                .collect::<Vec<_>>();
            let mut actual = Vec::new();
            Index::for_each_path(&bytes, ObjectFormat::Sha1, |path| {
                actual.push(path.to_vec());
                Ok(())
            })
            .expect("test operation should succeed");
            assert_eq!(actual, expected, "paths for v{version}");
        }
    }

    #[test]
    fn borrowed_index_parse_matches_owned_index_for_uncompressed_versions() {
        for version in [2u32, 3] {
            let mut index = sample_index(version);
            if version == 2 {
                for entry in &mut index.entries {
                    entry.flags &= !INDEX_FLAG_EXTENDED;
                    entry.flags_extended = 0;
                }
            }
            let bytes = index.write_sha1().expect("test operation should succeed");
            let owned = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
            let borrowed = BorrowedIndex::parse(&bytes, ObjectFormat::Sha1)
                .expect("test operation should succeed");
            assert_eq!(borrowed.version, owned.version);
            assert_eq!(borrowed.entries.len(), owned.entries.len());
            for (actual, expected) in borrowed.entries.iter().zip(owned.entries.iter()) {
                assert_eq!(actual.ctime_seconds, expected.ctime_seconds);
                assert_eq!(actual.ctime_nanoseconds, expected.ctime_nanoseconds);
                assert_eq!(actual.mtime_seconds, expected.mtime_seconds);
                assert_eq!(actual.mtime_nanoseconds, expected.mtime_nanoseconds);
                assert_eq!(actual.dev, expected.dev);
                assert_eq!(actual.ino, expected.ino);
                assert_eq!(actual.mode, expected.mode);
                assert_eq!(actual.uid, expected.uid);
                assert_eq!(actual.gid, expected.gid);
                assert_eq!(actual.size, expected.size);
                assert_eq!(actual.oid, expected.oid);
                assert_eq!(actual.flags, expected.flags);
                assert_eq!(actual.flags_extended, expected.flags_extended);
                assert_eq!(actual.path, expected.path.as_bytes());
            }
        }
    }

    #[test]
    fn index_v2_writer_rejects_extended_flags() {
        let index = sample_index(2);
        // The entries still carry extended flags but the version is 2.
        assert!(index.write_v2_sha1().is_err());
    }

    #[test]
    fn index_v4_path_compression_emits_documented_bytes() {
        // Two entries sharing the prefix "src/": "src/main.rs" then
        // "src/main.txt". The shared prefix is "src/main." (9 bytes), so the
        // second entry strips `len("src/main.rs") - 9 = 2` bytes and stores the
        // suffix "txt".
        let index = Index {
            version: 4,
            entries: vec![
                index_entry(b"src/main.rs", "ce013625030ba8dba906f756967f9e9ca394464a"),
                index_entry(b"src/main.txt", "2e65efe2a145dda7ee51d1741299f848e5bf752e"),
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_sha1().expect("test operation should succeed");
        // strip_len = 2 -> single varint byte 0x02, then "txt", then NUL.
        let needle = [0x02, b't', b'x', b't', 0x00];
        assert!(
            bytes.windows(needle.len()).any(|window| window == needle),
            "expected compressed suffix bytes {needle:02x?} in v4 index"
        );
        // First entry's full path "src/main.rs" is preceded by strip_len 0.
        let first = [0x00, b's', b'r', b'c', b'/', b'm', b'a', b'i', b'n'];
        assert!(bytes.windows(first.len()).any(|window| window == first));
        // Round-trips.
        let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
        assert_eq!(parsed.entries, index.entries);

        // A long strip length uses the multi-byte offset varint git expects.
        let long = vec![b'a'; 200];
        let index = Index {
            version: 4,
            entries: vec![
                index_entry(&long, "ce013625030ba8dba906f756967f9e9ca394464a"),
                index_entry(b"b", "2e65efe2a145dda7ee51d1741299f848e5bf752e"),
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_sha1().expect("test operation should succeed");
        // strip_len = 200: encode_varint(200) = [0x80, 0x48] (offset form),
        // followed by suffix "b" and NUL.
        let needle = [0x80, 0x48, b'b', 0x00];
        assert!(
            bytes.windows(needle.len()).any(|window| window == needle),
            "expected multi-byte strip varint {needle:02x?}"
        );
        assert_eq!(
            Index::parse_v2_sha1(&bytes)
                .expect("test operation should succeed")
                .entries,
            index.entries
        );
    }

    #[test]
    fn cache_tree_round_trips_documented_layout() {
        // Mirrors the byte layout git writes: a root spanning 3 entries with one
        // subtree "sub" spanning 2 entries.
        let root_oid = oid("13a501cf10c293c9dcb9e49bf6b9bf5f312633d9");
        let sub_oid = oid("e891cb7572e731c71e3fe1be567821fa5298612d");
        let mut expected = Vec::new();
        expected.push(0); // empty root name
        expected.extend_from_slice(b"3 1\n");
        expected.extend_from_slice(root_oid.as_bytes());
        expected.extend_from_slice(b"sub\0");
        expected.extend_from_slice(b"2 0\n");
        expected.extend_from_slice(sub_oid.as_bytes());

        let cache_tree = CacheTree {
            entry_count: 3,
            oid: Some(root_oid.clone()),
            subtrees: vec![CacheTreeChild {
                name: b"sub".to_vec(),
                tree: CacheTree {
                    entry_count: 2,
                    oid: Some(sub_oid.clone()),
                    subtrees: Vec::new(),
                },
            }],
        };
        assert_eq!(
            cache_tree.write().expect("test operation should succeed"),
            expected
        );
        assert_eq!(
            CacheTree::parse(ObjectFormat::Sha1, &expected).expect("test operation should succeed"),
            cache_tree
        );
    }

    #[test]
    fn cache_tree_invalid_entry_has_no_oid() {
        // An invalid (dirty) node uses entry_count -1 and stores no object id.
        let mut body = Vec::new();
        body.push(0);
        body.extend_from_slice(b"-1 0\n");
        let cache_tree =
            CacheTree::parse(ObjectFormat::Sha1, &body).expect("test operation should succeed");
        assert_eq!(cache_tree.entry_count, -1);
        assert!(cache_tree.oid.is_none());
        assert_eq!(
            cache_tree.write().expect("test operation should succeed"),
            body
        );
        // A valid count with a missing id, or invalid count with an id, is rejected.
        let bad = CacheTree {
            entry_count: -1,
            oid: Some(oid("ce013625030ba8dba906f756967f9e9ca394464a")),
            subtrees: Vec::new(),
        };
        assert!(bad.write().is_err());
    }

    #[test]
    fn cache_tree_preserves_git_subtree_order_without_requiring_sort() {
        let root_oid = oid("332618ff1401e7c767abab9efc710c66525befdc");
        let bin_oid = oid("7134efad6424a13f600061d98cec36fedfcbfee8");
        let dot_oid = oid("299beb4d195a743a1f82dfd7a0264289da06e00e");
        let mut body = Vec::new();
        body.push(0);
        body.extend_from_slice(b"3 2\n");
        body.extend_from_slice(root_oid.as_bytes());
        body.extend_from_slice(b"bin\0");
        body.extend_from_slice(b"1 0\n");
        body.extend_from_slice(bin_oid.as_bytes());
        body.extend_from_slice(b".gemini\0");
        body.extend_from_slice(b"1 0\n");
        body.extend_from_slice(dot_oid.as_bytes());

        let cache_tree =
            CacheTree::parse(ObjectFormat::Sha1, &body).expect("test operation should succeed");
        assert_eq!(
            cache_tree
                .subtrees
                .iter()
                .map(|child| child.name.as_slice())
                .collect::<Vec<_>>(),
            vec![b"bin".as_slice(), b".gemini".as_slice()]
        );
        assert_eq!(
            cache_tree.write().expect("test operation should succeed"),
            body
        );
    }

    #[test]
    fn index_set_and_get_cache_tree_round_trips_through_index() {
        let mut index = sample_index(2);
        for entry in &mut index.entries {
            entry.flags &= !INDEX_FLAG_EXTENDED;
            entry.flags_extended = 0;
        }
        let cache_tree = CacheTree {
            entry_count: 4,
            oid: Some(oid("13a501cf10c293c9dcb9e49bf6b9bf5f312633d9")),
            subtrees: vec![CacheTreeChild {
                name: b"src".to_vec(),
                tree: CacheTree {
                    entry_count: 3,
                    oid: Some(oid("e891cb7572e731c71e3fe1be567821fa5298612d")),
                    subtrees: Vec::new(),
                },
            }],
        };
        index
            .set_cache_tree(Some(&cache_tree))
            .expect("test operation should succeed");
        // Extensions now carry a TREE chunk that the generic walker can find.
        assert!(
            index
                .extension(b"TREE")
                .expect("test operation should succeed")
                .is_some()
        );
        // It survives a full index write/parse cycle.
        let bytes = index.write_sha1().expect("test operation should succeed");
        let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
        assert_eq!(parsed.extensions, index.extensions);
        assert_eq!(
            parsed
                .cache_tree(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
            Some(cache_tree)
        );
        // Removing it clears the chunk.
        let mut without = parsed.clone();
        without
            .set_cache_tree(None)
            .expect("test operation should succeed");
        assert!(
            without
                .extension(b"TREE")
                .expect("test operation should succeed")
                .is_none()
        );
        assert!(
            without
                .cache_tree(ObjectFormat::Sha1)
                .expect("test operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn index_preserves_unknown_raw_extensions() {
        // An extension the typed layer does not understand must round-trip
        // untouched, and the generic walker must still locate it.
        let mut extensions = Vec::new();
        encode_index_extension(&mut extensions, b"link", b"\x00\x01\x02opaque")
            .expect("test operation should succeed");
        let mut index = sample_index(2);
        for entry in &mut index.entries {
            entry.flags &= !INDEX_FLAG_EXTENDED;
            entry.flags_extended = 0;
        }
        index.extensions = extensions.clone();
        let bytes = index.write_sha1().expect("test operation should succeed");
        let parsed = Index::parse_v2_sha1(&bytes).expect("test operation should succeed");
        assert_eq!(parsed.extensions, extensions);
        assert_eq!(
            parsed
                .extension(b"link")
                .expect("test operation should succeed"),
            Some(&b"\x00\x01\x02opaque"[..])
        );
        // Setting a cache tree leaves the unknown chunk intact.
        let mut updated = parsed.clone();
        updated
            .set_cache_tree(Some(&CacheTree {
                entry_count: -1,
                oid: None,
                subtrees: Vec::new(),
            }))
            .expect("test operation should succeed");
        assert_eq!(
            updated
                .extension(b"link")
                .expect("test operation should succeed"),
            Some(&b"\x00\x01\x02opaque"[..])
        );
    }

    #[test]
    fn git_reads_rust_written_v4_index() {
        // Gate on a usable `git`.
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping: git not available");
            return;
        }
        let dir = unique_temp_dir("index-v4-git");
        fs::create_dir_all(&dir).expect("test operation should succeed");
        run_success("git", &dir, &["init", "-q"]);

        // Write two blobs via git so the object ids exist in the repo, then
        // build a v4 index referencing them with a matching cache tree and let
        // `git ls-files` read it back.
        let readme_oid = String::from_utf8(run_success_with_stdin(
            "git",
            &dir,
            &["hash-object", "-w", "--stdin"],
            b"readme\n",
        ))
        .expect("test operation should succeed");
        let main_oid = String::from_utf8(run_success_with_stdin(
            "git",
            &dir,
            &["hash-object", "-w", "--stdin"],
            b"fn main() {}\n",
        ))
        .expect("test operation should succeed");
        let readme_oid = readme_oid.trim();
        let main_oid = main_oid.trim();

        let index = Index {
            version: 4,
            entries: vec![
                index_entry(b"README.md", readme_oid),
                index_entry(b"src/main.rs", main_oid),
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_sha1().expect("test operation should succeed");
        fs::write(dir.join(".git").join("index"), &bytes).expect("test operation should succeed");

        let listed = run_success("git", &dir, &["ls-files"]);
        let listed = String::from_utf8(listed).expect("test operation should succeed");
        assert_eq!(listed, "README.md\nsrc/main.rs\n", "git ls-files output");

        // git must agree the on-disk version is 4.
        let version = run_success("git", &dir, &["update-index", "--show-index-version"]);
        let version = String::from_utf8(version).expect("test operation should succeed");
        assert_eq!(version.trim(), "4");

        fs::remove_dir_all(&dir).ok();
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).expect("test operation should succeed")
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
        assert!(
            output.status.success(),
            "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn run_success_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
        let mut child = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
        child
            .stdin
            .as_mut()
            .expect("stdin is piped")
            .write_all(stdin)
            .expect("write stdin");
        let output = child
            .wait_with_output()
            .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"));
        assert!(
            output.status.success(),
            "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
}
