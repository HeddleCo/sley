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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub version: u32,
    pub entries: Vec<IndexEntry>,
    pub extensions: Vec<u8>,
    pub checksum: Option<ObjectId>,
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
