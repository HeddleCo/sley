#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

//! The single, isolated home for sley's only `unsafe`: read-only memory maps of
//! immutable git files. Every other crate in the workspace keeps
//! `unsafe_code = "forbid"`; this crate exists so that the one unavoidable unsafe
//! call (mapping a file) lives behind a small, audited, safe API instead of
//! being scattered.
//!
//! # Why mmap is `unsafe`
//!
//! [`memmap2::Mmap::map`] is an `unsafe fn` because a memory map aliases a file
//! whose bytes another process could change. If the mapped file is **truncated**
//! while a map is live, touching the lost pages raises `SIGBUS`.
//!
//! # Safety invariant sley relies on
//!
//! sley only maps git files that are written by atomic rename of a
//! fully-written temporary and are never truncated or rewritten in place:
//! pack/index files, the repository index, multi-pack-index files, and
//! commit-graph files. A repack/gc, index write, or commit-graph write replaces
//! the file by writing a new one and renaming it into place, and unlinking a
//! still-mapped file keeps the inode (and the mapping) valid on Unix. So the
//! backing bytes never shrink under a live map, which is the condition
//! `Mmap::map` requires.

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::path::Path;

use memmap2::Mmap;

/// A read-only memory map of a file. Dereferences to the mapped bytes, so it drops
/// in anywhere a `&[u8]` view of the file contents is wanted.
#[derive(Debug)]
pub struct MappedFile {
    mmap: Mmap,
}

impl MappedFile {
    /// Open `path` read-only and memory-map its entire contents.
    ///
    /// # Safety
    ///
    /// A memory map aliases a file whose bytes another process could change. The
    /// caller must guarantee `path` is **not modified or truncated** for the
    /// lifetime of the returned [`MappedFile`]; otherwise reading the map is
    /// undefined behavior (`SIGBUS` on truncation). For sley's pack files this
    /// invariant holds because they are written by atomic rename and never
    /// rewritten in place — prefer [`MappedFile::open_pack`], which documents that.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from opening the file or creating the mapping.
    pub unsafe fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the caller upholds the immutability contract documented above.
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { mmap })
    }

    /// Memory-map a git **pack/index file** (`*.pack` / `*.idx`) read-only.
    ///
    /// This is the safe, audited entry point for sley. Installed packs are
    /// created by atomic rename and never rewritten in place (a repack writes a
    /// new file and renames; unlinking a still-mapped pack keeps the inode valid
    /// on Unix). A completed staging pack is likewise closed and immutable while
    /// sley indexes its map, then atomically renamed after the map is dropped.
    /// Those lifecycles satisfy the precondition [`MappedFile::open`] requires.
    /// To keep this safe API scoped to them, this rejects symlinks, non-regular
    /// files, and paths whose extension is not `.pack` or `.idx`.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from inspecting/opening the file or creating the mapping.
    pub fn open_pack(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pack path is not a regular file",
            ));
        }
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("pack" | "idx")
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pack path must end in .pack or .idx",
            ));
        }
        // SAFETY: `path` is an installed or completed staging git pack file,
        // whose audited lifecycle does not mutate it while mapped (see above).
        unsafe { Self::open(path) }
    }

    /// Memory-map a repository **index** file read-only.
    ///
    /// Git-compatible writers update `.git/index` by writing `.git/index.lock`
    /// and atomically renaming it into place; they do not truncate the live
    /// index while readers hold it open. That gives the same stable-inode
    /// property as pack files. This accepts only regular files named `index`;
    /// callers with an unusual `GIT_INDEX_FILE` can fall back to `std::fs::read`.
    pub fn open_index(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "index path is not a regular file",
            ));
        }
        if path.file_name().and_then(|name| name.to_str()) != Some("index") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "index path must be named index",
            ));
        }
        // SAFETY: git replaces the repository index via lockfile rename instead
        // of truncating/reusing the mapped file in place.
        unsafe { Self::open(path) }
    }

    /// Memory-map a git **multi-pack-index** file read-only.
    ///
    /// Git writes the `objects/pack/multi-pack-index` file by creating a new file
    /// and atomically renaming it into place rather than mutating it in place.
    /// This keeps already-mapped bytes stable for readers. Symlinks and
    /// non-regular files are rejected.
    pub fn open_multi_pack_index(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "multi-pack-index path is not a regular file",
            ));
        }
        if path.file_name().and_then(|name| name.to_str()) != Some("multi-pack-index") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "multi-pack-index path must be named multi-pack-index",
            ));
        }
        // SAFETY: git replaces multi-pack-index files atomically; it does not
        // truncate or rewrite them while readers hold the old inode.
        unsafe { Self::open(path) }
    }

    /// Memory-map a git **commit-graph file** read-only.
    ///
    /// This accepts the monolithic `objects/info/commit-graph` file and split
    /// graph layers named `graph-<hash>.graph`. Git writes these files by creating
    /// a new file and atomically renaming it into place, so an existing mapped
    /// inode is not truncated under readers. Symlinks and non-regular files are
    /// rejected; callers that need legacy symlink behavior can fall back to
    /// `std::fs::read`.
    pub fn open_commit_graph(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "commit-graph path is not a regular file",
            ));
        }
        let file_name = path.file_name().and_then(|name| name.to_str());
        let is_commit_graph = file_name == Some("commit-graph")
            || file_name.is_some_and(|name| name.starts_with("graph-") && name.ends_with(".graph"));
        if !is_commit_graph {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "commit-graph path must be commit-graph or graph-*.graph",
            ));
        }
        // SAFETY: `path` is a git commit-graph file, which git writes by atomic
        // replacement rather than in-place truncation.
        unsafe { Self::open(path) }
    }

    /// The mapped bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.mmap
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.mmap
    }
}

impl AsRef<[u8]> for MappedFile {
    fn as_ref(&self) -> &[u8] {
        &self.mmap
    }
}

macro_rules! define_map_or_read {
    ($doc:literal, $name:ident, $inner:ident) => {
        #[doc = concat!("Load a git ", $doc, " read-only: memory-map it when possible, falling back to\n        /// a full heap read (`std::fs::read`) if mapping fails (e.g. symlinked,\n        /// unusually named, or otherwise unmappable paths).\n        ///\n        /// This bundles the try-mmap-else-read pattern every sley consumer needs\n        /// so the fallback policy lives next to the audited `unsafe` instead of\n        /// being hand-rolled per crate.\n        ///\n        /// # Errors\n        ///\n        /// Returns any I/O error from the final read attempt when both mapping\n        /// and reading fail.")]
        pub fn $name(path: &Path) -> io::Result<FileBytes> {
            match Self::$inner(path) {
                Ok(mapped) => Ok(FileBytes::Mapped(mapped)),
                Err(_) => Ok(FileBytes::Heap(std::fs::read(path)?)),
            }
        }
    };
}

impl MappedFile {
    define_map_or_read!(
        "index file (`index`)",
        open_index_or_read,
        open_index
    );
    define_map_or_read!("pack/index file (`*.pack` / `*.idx`)", open_pack_or_read, open_pack);
    define_map_or_read!(
        "multi-pack-index file",
        open_multi_pack_index_or_read,
        open_multi_pack_index
    );
    define_map_or_read!("commit-graph file", open_commit_graph_or_read, open_commit_graph);
}

/// Owned bytes of an immutable git file: memory-mapped when possible, otherwise
/// read into the heap. Dereferences to the same `&[u8]` either way, so decode
/// paths are written once against the borrowed view.
#[derive(Debug)]
pub enum FileBytes {
    Mapped(MappedFile),
    Heap(Vec<u8>),
}

impl Deref for FileBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl AsRef<[u8]> for FileBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mapped(mapped) => mapped.as_bytes(),
            Self::Heap(bytes) => bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MappedFile;
    use std::io::Write;

    #[test]
    fn maps_file_contents() {
        let mut path = std::env::temp_dir();
        path.push(format!("sley-mmap-test-{}.pack", std::process::id()));
        let payload = b"PACK contents under mmap\n";
        {
            let mut file = std::fs::File::create(&path).expect("create temp file");
            file.write_all(payload).expect("write payload");
            file.sync_all().expect("sync");
        }
        let mapped = MappedFile::open_pack(&path).expect("map file");
        assert_eq!(&*mapped, payload);
        assert_eq!(mapped.as_bytes(), payload);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_non_pack_extension() {
        let mut path = std::env::temp_dir();
        path.push(format!("sley-mmap-test-{}.txt", std::process::id()));
        {
            let mut file = std::fs::File::create(&path).expect("create temp file");
            file.write_all(b"not a pack").expect("write payload");
        }
        let err = MappedFile::open_pack(&path).expect_err("reject non-pack path");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn maps_multi_pack_index_name() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("sley-mmap-midx-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("multi-pack-index");
        {
            let mut file = std::fs::File::create(&path).expect("create temp file");
            file.write_all(b"MIDX bytes").expect("write payload");
        }
        let mapped = MappedFile::open_multi_pack_index(&path).expect("map multi-pack-index");
        assert_eq!(mapped.as_bytes(), b"MIDX bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maps_commit_graph_name() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("sley-mmap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("commit-graph");
        {
            let mut file = std::fs::File::create(&path).expect("create temp file");
            file.write_all(b"CGPH bytes").expect("write payload");
        }
        let mapped = MappedFile::open_commit_graph(&path).expect("map commit-graph");
        assert_eq!(mapped.as_bytes(), b"CGPH bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
