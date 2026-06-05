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
    /// # Errors
    ///
    /// Returns any I/O error from opening the file or creating the mapping.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: `file` is opened read-only, and sley only maps pack files, which
        // are produced by atomic rename and never truncated in place (see the
        // module-level "Safety invariant" docs). The backing bytes therefore stay
        // valid and unchanged for the lifetime of the returned map. This is the
        // sole `unsafe` in the workspace.
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { mmap })
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
        let mapped = MappedFile::open(&path).expect("map file");
        assert_eq!(&*mapped, payload);
        assert_eq!(mapped.as_bytes(), payload);
        let _ = std::fs::remove_file(&path);
    }
}
