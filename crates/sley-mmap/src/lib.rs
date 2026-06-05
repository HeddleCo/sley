//! The single, isolated home for sley's only `unsafe`: read-only memory maps of
//! pack files. Every other crate in the workspace keeps `unsafe_code = "forbid"`;
//! this crate exists so that the one unavoidable unsafe call (mapping a file) lives
//! behind a small, audited, safe API instead of being scattered.
//!
//! # Why mmap is `unsafe`
//!
//! [`memmap2::Mmap::map`] is an `unsafe fn` because a memory map aliases a file
//! whose bytes another process could change. If the mapped file is **truncated**
//! while a map is live, touching the lost pages raises `SIGBUS`.
//!
//! # Safety invariant sley relies on
//!
//! sley only maps **pack files** (`*.pack` / `*.idx`). Those are written by atomic
//! rename of a fully-written temporary (see `write_pack_component`) and are never
//! truncated or rewritten in place — a repack/gc replaces a pack by writing a new
//! file and renaming, and unlinking a file that is still mapped keeps the inode
//! (and the mapping) valid on Unix. So the backing bytes never shrink under a live
//! map, which is the condition `Mmap::map` requires.

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

    /// Memory-map a git **pack file** (`*.pack` / `*.idx`) read-only.
    ///
    /// This is the safe, audited entry point for sley: pack files are created by
    /// writing a temporary and atomically renaming it into place, and are never
    /// truncated or rewritten in place (a repack writes a new file and renames;
    /// unlinking a still-mapped pack keeps the inode valid on Unix). That immutability
    /// is exactly the precondition [`MappedFile::open`] requires, so mapping a pack is
    /// sound. Callers must only pass paths to such pack files.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from opening the file or creating the mapping.
    pub fn open_pack(path: &Path) -> io::Result<Self> {
        // SAFETY: `path` is a git pack file, which sley writes atomically and never
        // mutates in place (see the doc comment), so the mapped bytes stay valid.
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

#[cfg(test)]
mod tests {
    use super::MappedFile;
    use std::io::Write;

    #[test]
    fn maps_file_contents() {
        let mut path = std::env::temp_dir();
        path.push(format!("sley-mmap-test-{}", std::process::id()));
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
}
