use flate2::{Decompress, FlushDecompress, Status};
use sley_core::{
    CancelFlag, CancellableRead, GitError, MissingObjectContext, ObjectFormat, ObjectId, Result,
};
use sley_formats::{Bundle, BundleReference};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{
    PackFile, PackIndex, PackIndexBuild, PackIndexProgress, PackInput, PackWrite, PackWriteOptions,
    fix_thin_pack,
};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::{ObjectReader, ObjectWriter, unique_temp_path};

use crate::pack::{FileObjectDatabase, ObjectDatabase};
use crate::repack::pack_index_entries_match_writer;

pub struct BundleUnbundleResult {
    pub written_objects: Vec<ObjectId>,
    pub references: Vec<BundleReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackUnpackResult {
    pub written_objects: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackInstallResult {
    pub pack_name: String,
    pub pack_path: PathBuf,
    pub index_path: PathBuf,
    pub promisor_path: Option<PathBuf>,
    pub object_ids: Vec<ObjectId>,
}

/// Disposable object database for an untrusted incoming pack.
///
/// Objects are written below the destination object directory, while an
/// `info/alternates` entry makes the destination's existing objects available
/// as delta bases and during connectivity validation. Dropping an unpromoted
/// quarantine removes every incoming object. [`Self::promote`] moves accepted
/// object files into the destination with per-file atomic renames.
#[derive(Debug)]
pub struct IncomingPackQuarantine {
    git_dir: PathBuf,
    object_dir: PathBuf,
    destination_objects_dir: PathBuf,
    format: ObjectFormat,
    promisor_remote_present: bool,
    promoted: bool,
}

impl IncomingPackQuarantine {
    pub fn new(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<Self> {
        let source_git_dir = git_dir.as_ref().to_path_buf();
        let destination_objects_dir = crate::repository_objects_dir(&source_git_dir);
        fs::create_dir_all(&destination_objects_dir)?;
        let quarantine_git_dir = create_incoming_object_dir(&destination_objects_dir)?;
        let object_dir = quarantine_git_dir.join("objects");
        let result = (|| -> Result<()> {
            fs::create_dir_all(object_dir.join("pack"))?;
            fs::create_dir_all(object_dir.join("info"))?;
            let mut alternates = vec![
                fs::canonicalize(&destination_objects_dir)
                    .unwrap_or_else(|_| destination_objects_dir.clone()),
            ];
            // FileObjectDatabase deliberately treats alternate entries as a
            // flat search list. Preserve the destination's existing alternate
            // visibility explicitly so a quarantined fetch into a shared or
            // reference clone can validate objects that were already borrowed
            // from its source repository.
            alternates.extend(crate::registry::alternate_object_dirs(
                &destination_objects_dir,
            ));
            let mut alternate_file = String::new();
            for alternate in alternates {
                let alternate = fs::canonicalize(&alternate).unwrap_or(alternate);
                alternate_file.push_str(&alternate.to_string_lossy());
                alternate_file.push('\n');
            }
            fs::write(object_dir.join("info/alternates"), alternate_file)?;
            let shallow = quarantine_git_dir.join("shallow");
            let source_shallow = source_git_dir.join("shallow");
            if source_shallow.exists() {
                fs::copy(source_shallow, shallow)?;
            }
            Ok(())
        })();
        if let Err(err) = result {
            let _ = fs::remove_dir_all(&quarantine_git_dir);
            return Err(err);
        }
        Ok(Self {
            git_dir: quarantine_git_dir,
            object_dir,
            destination_objects_dir,
            format,
            promisor_remote_present: false,
            promoted: false,
        })
    }

    pub fn object_dir(&self) -> &Path {
        &self.object_dir
    }

    /// A minimal bare repository path whose object database is quarantined.
    /// Existing destination objects remain readable through its alternate.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Mark promised objects as valid missing links while validating a fetch
    /// into a partial-clone repository.
    pub fn with_promisor_remote_present(mut self, present: bool) -> Self {
        self.promisor_remote_present = present;
        self
    }

    pub fn database(&self) -> FileObjectDatabase {
        FileObjectDatabase::new(self.object_dir.clone(), self.format)
            .with_promisor_remote_present(self.promisor_remote_present)
    }

    /// Promote all accepted loose and packed objects into the destination.
    ///
    /// Pack indexes and sidecars are made visible before their `.pack`; a
    /// reader therefore never observes a pack without its index. If any rename
    /// fails, files newly moved by this call are rolled back into quarantine.
    pub fn promote(mut self) -> Result<()> {
        let mut files = incoming_object_files(&self.object_dir)?;
        files.sort_by_key(|path| {
            let is_pack = path.extension().is_some_and(|ext| ext == "pack");
            (is_pack, path.clone())
        });
        let mut moved = Vec::new();
        for source in files {
            let relative = source
                .strip_prefix(&self.object_dir)
                .map_err(|_| GitError::InvalidPath("incoming object escaped quarantine".into()))?;
            let destination = self.destination_objects_dir.join(relative);
            let parent = destination.parent().ok_or_else(|| {
                GitError::InvalidPath("incoming object has no destination parent".into())
            })?;
            fs::create_dir_all(parent)?;
            if destination.exists() {
                fs::remove_file(&source)?;
                continue;
            }
            if let Err(err) = fs::rename(&source, &destination) {
                for (promoted, staged) in moved.iter().rev() {
                    let _ = fs::rename(promoted, staged);
                }
                return Err(GitError::from(err));
            }
            moved.push((destination, source));
        }
        self.promoted = true;
        fs::remove_dir_all(&self.git_dir)?;
        Ok(())
    }
}

impl Drop for IncomingPackQuarantine {
    fn drop(&mut self) {
        if !self.promoted {
            let _ = fs::remove_dir_all(&self.git_dir);
        }
    }
}

fn create_incoming_object_dir(objects_dir: &Path) -> Result<PathBuf> {
    for _ in 0..100 {
        let object_dir = unique_temp_path(objects_dir).with_extension("incoming");
        match fs::create_dir(&object_dir) {
            Ok(()) => return Ok(object_dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(GitError::from(err)),
        }
    }
    Err(GitError::IoKind {
        kind: std::io::ErrorKind::Other,
        message: "could not create incoming object quarantine".into(),
    })
}

fn incoming_object_files(object_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let pack_dir = object_dir.join("pack");
    if pack_dir.exists() {
        for entry in fs::read_dir(pack_dir)? {
            let path = entry?.path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    for entry in fs::read_dir(object_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() != 2 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for loose in fs::read_dir(entry.path())? {
            let path = loose?.path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

#[derive(Debug)]
pub struct RawPackStreamingInstall {
    format: ObjectFormat,
    expected_pack_id: ObjectId,
    expected_pack_size: u64,
    options: RawPackInstallOptions,
    pack_dir: PathBuf,
    pack_name: String,
    pack_path: PathBuf,
    index_path: PathBuf,
    temp_pack_path: PathBuf,
    file: Option<fs::File>,
    written: u64,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackInstallResult {
    pub object_ids: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackIndexResult {
    pub pack_id: ObjectId,
    pub index: Vec<u8>,
    pub objects: Vec<RawPackIndexedObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackIndexedObject {
    pub oid: ObjectId,
    pub object_type: ObjectType,
    pub size: u64,
    pub offset: u64,
}

/// Monotonic receipt and indexing counters for one staged pack installation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackInstallProgress {
    /// Pack bytes received from the input so far.
    pub received_bytes: u64,
    /// Objects fully inflated, resolved, and hashed so far.
    pub indexed_objects: u64,
    /// Total objects declared by the pack header once it is available.
    pub total_objects: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachablePackFile {
    pub pack_path: PathBuf,
    pub pack_size: u64,
    pub checksum: ObjectId,
    pub object_count: usize,
    pub delta_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachablePackWriteSummary {
    pub index: Vec<u8>,
    pub checksum: ObjectId,
    pub object_count: usize,
    pub delta_count: u32,
    pub pack_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawPackInstallOptions {
    pub promisor: bool,
    /// Maximum raw pack bytes to accept from the reader. `None` means unlimited,
    /// mirroring unset `fetch.maxInputSize` / `transfer.maxSize`.
    pub max_input_size: Option<u64>,
}

pub trait RawPackInstaller {
    fn install_raw_pack_from_reader_with_options<R>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
    ) -> Result<RawPackInstallResult>
    where
        R: Read;

    fn install_raw_pack_from_reader<R>(&self, reader: &mut R) -> Result<RawPackInstallResult>
    where
        R: Read,
    {
        self.install_raw_pack_from_reader_with_options(reader, RawPackInstallOptions::default())
    }

    /// Install a raw pack while reporting receipt and indexing progress.
    ///
    /// Delegates to [`install_raw_pack_from_reader_with_progress_and_cancel`] with
    /// a never-cancel flag. The default implementation still polls cancel
    /// between reads via [`CancellableRead`].
    ///
    /// [`install_raw_pack_from_reader_with_progress_and_cancel`]: RawPackInstaller::install_raw_pack_from_reader_with_progress_and_cancel
    fn install_raw_pack_from_reader_with_progress<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        progress: F,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
        F: FnMut(PackInstallProgress),
    {
        self.install_raw_pack_from_reader_with_progress_and_cancel(
            reader,
            options,
            CancelFlag::never(),
            progress,
        )
    }

    /// Install a raw pack with cooperative cancellation and optional progress.
    ///
    /// The default implementation ignores `progress`, polls `cancel` before the
    /// install, and wraps `reader` in [`CancellableRead`] so a trip mid-stream
    /// surfaces as [`GitError::Cancelled`]. [`FileObjectDatabase`] overrides this
    /// to poll during both receipt and parallel indexing.
    fn install_raw_pack_from_reader_with_progress_and_cancel<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        cancel: CancelFlag<'_>,
        _progress: F,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
        F: FnMut(PackInstallProgress),
    {
        cancel.check()?;
        let mut cancellable = CancellableRead::new(reader, cancel.as_ref());
        self.install_raw_pack_from_reader_with_options(&mut cancellable, options)
            .map_err(map_install_cancel_error)
    }
}

/// Map cancel-flavored install failures (from [`CancellableRead`] I/O or pack
/// indexing checks) onto [`GitError::Cancelled`].
///
/// Cancellation is detected structurally via [`GitError::is_cancelled`]
/// (explicit variant, cancel-payload intercept, or Interrupted kind) — no
/// message-text sniffing.
fn map_install_cancel_error(err: GitError) -> GitError {
    if err.is_cancelled() {
        GitError::Cancelled
    } else {
        err
    }
}

const PACK_RECEIVE_BUFFER_BYTES: usize = 1024 * 1024;
const PACK_RECEIVE_QUEUE_DEPTH: usize = 4;
const PACK_RECEIVE_PROGRESS_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy)]
struct PackReceiveSummary {
    bytes: u64,
}

fn receive_pack_to_file<R, F>(
    reader: &mut R,
    mut file: fs::File,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    progress: &mut F,
) -> Result<PackReceiveSummary>
where
    R: Read,
    F: FnMut(PackInstallProgress),
{
    let (filled_sender, filled_receiver) = mpsc::sync_channel::<Vec<u8>>(PACK_RECEIVE_QUEUE_DEPTH);
    let (empty_sender, empty_receiver) = mpsc::sync_channel::<Vec<u8>>(PACK_RECEIVE_QUEUE_DEPTH);
    for _ in 0..PACK_RECEIVE_QUEUE_DEPTH {
        empty_sender
            .send(vec![0u8; PACK_RECEIVE_BUFFER_BYTES])
            .map_err(|_| GitError::IoKind {
                kind: std::io::ErrorKind::Other,
                message: "could not initialize pack receive buffers".into(),
            })?;
    }

    std::thread::scope(|scope| {
        let writer = scope.spawn(sley_core::diagnostics::inherit(move || -> Result<()> {
            for mut chunk in filled_receiver {
                #[cfg(feature = "fetch-profile")]
                let _profile_span = sley_core::fetch_profile::Span::enter(
                    sley_core::fetch_profile::Stage::ObjectStoreWrite,
                );
                file.write_all(&chunk)?;
                #[cfg(feature = "fetch-profile")]
                {
                    sley_core::fetch_profile::add_count(
                        sley_core::fetch_profile::Stage::ObjectStoreWrite,
                        1,
                    );
                    sley_core::fetch_profile::add_bytes(
                        sley_core::fetch_profile::Stage::ObjectStoreWrite,
                        chunk.len() as u64,
                    );
                }
                chunk.resize(PACK_RECEIVE_BUFFER_BYTES, 0);
                if empty_sender.send(chunk).is_err() {
                    break;
                }
            }
            file.flush()?;
            file.sync_all()?;
            #[cfg(feature = "fetch-profile")]
            sley_core::fetch_profile::add_fsync();
            Ok(())
        }));

        let receive_result = (|| -> Result<PackReceiveSummary> {
            let mut bytes = 0u64;
            let mut last_progress = 0u64;
            let mut header = Vec::with_capacity(12);
            let mut total_objects = 0u64;
            loop {
                cancel.check()?;
                let mut chunk = empty_receiver.recv().map_err(|_| GitError::IoKind {
                    kind: std::io::ErrorKind::Other,
                    message: "pack staging writer stopped before receive completed".into(),
                })?;
                let read = reader.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                chunk.truncate(read);
                bytes = bytes
                    .checked_add(read as u64)
                    .ok_or_else(|| GitError::InvalidFormat("pack size overflow".into()))?;
                if let Some(limit) = max_input_size
                    && bytes > limit
                {
                    return Err(GitError::InvalidFormat(format!(
                        "pack exceeds maximum allowed size ({limit})"
                    )));
                }
                if header.len() < 12 {
                    let needed = 12 - header.len();
                    header.extend_from_slice(&chunk[..needed.min(chunk.len())]);
                    if header.len() == 12 && &header[..4] == b"PACK" {
                        total_objects = u64::from(u32::from_be_bytes([
                            header[8], header[9], header[10], header[11],
                        ]));
                        progress(PackInstallProgress {
                            received_bytes: 12,
                            indexed_objects: 0,
                            total_objects,
                        });
                        last_progress = 12;
                        cancel.check()?;
                    }
                }
                filled_sender.send(chunk).map_err(|_| GitError::IoKind {
                    kind: std::io::ErrorKind::Other,
                    message: "pack staging writer stopped before receive completed".into(),
                })?;
                if bytes.saturating_sub(last_progress) >= PACK_RECEIVE_PROGRESS_BYTES {
                    last_progress = bytes;
                    progress(PackInstallProgress {
                        received_bytes: bytes,
                        indexed_objects: 0,
                        total_objects,
                    });
                    cancel.check()?;
                }
            }
            progress(PackInstallProgress {
                received_bytes: bytes,
                indexed_objects: 0,
                total_objects,
            });
            cancel.check()?;
            Ok(PackReceiveSummary { bytes })
        })();
        drop(filled_sender);
        let write_result = match writer.join() {
            Ok(result) => result,
            Err(_) => Err(GitError::IoKind {
                kind: std::io::ErrorKind::Other,
                message: "pack staging writer panicked".into(),
            }),
        };
        let summary = receive_result?;
        write_result?;
        Ok(summary)
    })
}

#[cfg(test)]
pub(crate) const REACHABLE_PACK_STREAMING_MIN_OBJECTS: usize = 32;
#[cfg(not(test))]
pub(crate) const REACHABLE_PACK_STREAMING_MIN_OBJECTS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectPrefixResolution {
    Missing,
    Unique(ObjectId),
    Ambiguous(Vec<ObjectId>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStorageInfo {
    pub disk_size: u64,
    pub deltabase: ObjectId,
}

impl RawPackInstaller for FileObjectDatabase {
    fn install_raw_pack_from_reader_with_options<R>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
    {
        let result =
            FileObjectDatabase::install_raw_pack_from_reader_with_options(self, reader, options)?;
        Ok(RawPackInstallResult {
            object_ids: result.object_ids,
        })
    }

    fn install_raw_pack_from_reader_with_progress_and_cancel<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        cancel: CancelFlag<'_>,
        progress: F,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
        F: FnMut(PackInstallProgress),
    {
        let result = FileObjectDatabase::install_raw_pack_from_reader_with_progress_and_cancel(
            self, reader, options, cancel, progress,
        )?;
        Ok(RawPackInstallResult {
            object_ids: result.object_ids,
        })
    }
}

impl RawPackInstaller for ObjectDatabase {
    fn install_raw_pack_from_reader_with_options<R>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
    {
        let mut pack_bytes = Vec::new();
        match options.max_input_size {
            Some(limit) => {
                reader
                    .take(limit.saturating_add(1))
                    .read_to_end(&mut pack_bytes)?;
                if pack_bytes.len() as u64 > limit {
                    return Err(GitError::InvalidFormat(format!(
                        "pack exceeds maximum allowed size ({limit})"
                    )));
                }
            }
            None => {
                reader.read_to_end(&mut pack_bytes)?;
            }
        }
        let result = unpack_packfile_objects(&pack_bytes, self.format, self)?;
        Ok(RawPackInstallResult {
            object_ids: result.written_objects,
        })
    }
}

impl RawPackStreamingInstall {
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    pub fn pack_path(&self) -> &Path {
        &self.pack_path
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    pub fn finish(mut self) -> Result<PackInstallResult> {
        let result = (|| -> Result<PackInstallResult> {
            let mut file = self.file.take().ok_or_else(|| {
                GitError::InvalidFormat("raw pack stream already finished".into())
            })?;
            #[cfg(feature = "fetch-profile")]
            let _profile_span = sley_core::fetch_profile::Span::enter(
                sley_core::fetch_profile::Stage::ObjectStoreWrite,
            );
            file.flush()?;
            file.sync_all()?;
            #[cfg(feature = "fetch-profile")]
            sley_core::fetch_profile::add_fsync();
            drop(file);

            if self.written != self.expected_pack_size {
                return Err(GitError::InvalidFormat(format!(
                    "raw pack stream length mismatch: expected {}, got {}",
                    self.expected_pack_size, self.written
                )));
            }

            let built = {
                let mapped = sley_mmap::MappedFile::open_pack(&self.temp_pack_path)?;
                PackIndex::write_v2_for_pack(mapped.as_bytes(), self.format)?
            };
            if built.pack_checksum != self.expected_pack_id {
                return Err(GitError::InvalidFormat(format!(
                    "raw pack stream checksum mismatch: expected {}, got {}",
                    self.expected_pack_id, built.pack_checksum
                )));
            }

            match fs::rename(&self.temp_pack_path, &self.pack_path) {
                Ok(()) => {}
                Err(_) if self.pack_path.exists() => {
                    let _ = fs::remove_file(&self.temp_pack_path);
                }
                Err(err) => return Err(GitError::from(err)),
            }
            write_pack_component(&self.index_path, &built.index)?;
            let promisor_path = write_promisor_pack_sidecar(
                &self.pack_dir,
                &self.pack_name,
                self.options.promisor,
            )?;
            Ok(PackInstallResult {
                pack_name: self.pack_name.clone(),
                pack_path: self.pack_path.clone(),
                index_path: self.index_path.clone(),
                promisor_path,
                object_ids: built.entries.iter().map(|entry| entry.oid).collect(),
            })
        })();

        if result.is_ok() {
            self.finished = true;
        } else {
            let _ = fs::remove_file(&self.temp_pack_path);
        }
        result
    }
}

impl Write for RawPackStreamingInstall {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let next_written = self.written.checked_add(buf.len() as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "pack size overflow")
        })?;
        if next_written > self.expected_pack_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "raw pack stream exceeds expected size {}; got at least {}",
                    self.expected_pack_size, next_written
                ),
            ));
        }
        let file = self.file.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "raw pack stream already finished",
            )
        })?;
        let written = file.write(buf)?;
        self.written = self.written.checked_add(written as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "pack size overflow")
        })?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for RawPackStreamingInstall {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.file.take();
            let _ = fs::remove_file(&self.temp_pack_path);
        }
    }
}

pub fn verify_bundle_prerequisites<R: ObjectReader>(bundle: &Bundle, reader: &R) -> Result<()> {
    let mut missing = Vec::new();
    for prerequisite in &bundle.prerequisites {
        match reader.read_object(&prerequisite.oid) {
            Ok(object) => {
                let actual = object.object_id(bundle.format)?;
                if actual != prerequisite.oid {
                    return Err(GitError::InvalidObject(format!(
                        "bundle prerequisite {} hashes to {actual}",
                        prerequisite.oid
                    )));
                }
            }
            Err(GitError::NotFound(_)) => missing.push(prerequisite.oid),
            Err(err) => return Err(err),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(GitError::object_not_found_in(
        missing[0],
        MissingObjectContext::PackInstall,
    ))
}

pub fn unbundle_objects<R, W>(
    bundle: &Bundle,
    prerequisite_reader: &R,
    writer: &mut W,
) -> Result<BundleUnbundleResult>
where
    R: ObjectReader,
    W: ObjectWriter,
{
    verify_bundle_prerequisites(bundle, prerequisite_reader)?;
    let pack = PackFile::parse_bundle(bundle)?;
    let written_objects = write_pack_objects(pack, writer, "bundle")?.written_objects;
    Ok(BundleUnbundleResult {
        written_objects,
        references: bundle.references.clone(),
    })
}

pub fn install_bundle_pack<R>(
    bundle: &Bundle,
    prerequisite_reader: &R,
    destination: &impl RawPackInstaller,
) -> Result<BundleUnbundleResult>
where
    R: ObjectReader,
{
    verify_bundle_prerequisites(bundle, prerequisite_reader)?;
    let mut reader = bundle.pack.as_slice();
    let install = destination.install_raw_pack_from_reader(&mut reader)?;
    Ok(BundleUnbundleResult {
        written_objects: install.object_ids,
        references: bundle.references.clone(),
    })
}

pub fn unpack_packfile_objects<W>(
    pack_bytes: &[u8],
    format: ObjectFormat,
    writer: &W,
) -> Result<PackUnpackResult>
where
    W: ObjectWriter,
{
    let pack = PackFile::parse(pack_bytes, format)?;
    write_pack_objects(pack, writer, "pack")
}

pub fn index_raw_pack(pack_bytes: &[u8], format: ObjectFormat) -> Result<RawPackIndexResult> {
    let built = PackIndex::write_v2_for_pack(pack_bytes, format)?;
    Ok(index_build_to_raw_result(built))
}

pub fn index_raw_pack_file(
    path: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<RawPackIndexResult> {
    let mapped = sley_mmap::MappedFile::open_pack(path.as_ref())?;
    Ok(index_build_to_raw_result(PackIndex::write_v2_for_pack(
        mapped.as_bytes(),
        format,
    )?))
}

fn index_build_to_raw_result(built: PackIndexBuild) -> RawPackIndexResult {
    let objects = built
        .objects
        .into_iter()
        .map(|object| RawPackIndexedObject {
            oid: object.oid,
            object_type: object.object_type,
            size: object.size,
            offset: object.offset,
        })
        .collect::<Vec<_>>();
    RawPackIndexResult {
        pack_id: built.pack_checksum,
        index: built.index,
        objects,
    }
}

fn write_pack_objects<W>(pack: PackFile, writer: &W, source: &str) -> Result<PackUnpackResult>
where
    W: ObjectWriter,
{
    let mut written_objects = Vec::with_capacity(pack.entries.len());
    for entry in pack.entries {
        let expected = entry.entry.oid;
        let actual = writer.write_object(entry.object)?;
        if actual != expected {
            return Err(GitError::InvalidObject(format!(
                "{source} object id mismatch: expected {expected}, wrote {actual}"
            )));
        }
        written_objects.push(actual);
    }
    Ok(PackUnpackResult { written_objects })
}
pub(crate) fn validate_pack_checksum(
    pack: &[u8],
    format: ObjectFormat,
    expected: &ObjectId,
    context: &str,
) -> Result<()> {
    if expected.format() != format {
        return Err(GitError::InvalidObjectId(format!(
            "{context} checksum format does not match object format"
        )));
    }
    let hash_len = format.raw_len();
    if pack.len() < 12 + hash_len {
        return Err(GitError::InvalidFormat(format!(
            "{context} pack file too short"
        )));
    }
    if &pack[..4] != b"PACK" {
        return Err(GitError::InvalidFormat(format!(
            "{context} pack file missing PACK signature"
        )));
    }
    let trailer_offset = pack.len() - hash_len;
    let actual = sley_core::digest_bytes(format, &pack[..trailer_offset])?;
    let trailer = ObjectId::from_raw(format, &pack[trailer_offset..])?;
    if &actual != expected || trailer != *expected {
        return Err(GitError::InvalidFormat(format!(
            "{context} pack checksum does not match generated pack"
        )));
    }
    Ok(())
}

impl FileObjectDatabase {
    pub fn install_pack(&self, pack: &PackWrite) -> Result<PackInstallResult> {
        self.install_pack_with_options(pack, RawPackInstallOptions::default())
    }

    pub fn write_blob_as_pack(
        &self,
        oid: ObjectId,
        object: &EncodedObject,
        compression_level: u32,
    ) -> Result<ObjectId> {
        if object.object_type != ObjectType::Blob {
            return Err(GitError::InvalidObject(
                "write_blob_as_pack requires a blob object".into(),
            ));
        }
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        if self.contains(&oid)? {
            return Ok(oid);
        }
        let input = [PackInput { oid: &oid, object }];
        let options = PackWriteOptions::new()
            .with_window(0)
            .with_depth(0)
            .with_reorder(false)
            .with_compression_level(compression_level);
        let pack =
            PackFile::write_packed_with_known_ids_and_options(&input, self.format, &options)?;
        self.install_pack(&pack)?;
        Ok(oid)
    }

    pub fn write_blobs_as_pack(
        &self,
        objects: &[(ObjectId, EncodedObject)],
        compression_level: u32,
    ) -> Result<()> {
        let mut seen = HashSet::with_capacity(objects.len());
        let mut inputs = Vec::new();
        for (oid, object) in objects {
            if object.object_type != ObjectType::Blob {
                return Err(GitError::InvalidObject(
                    "write_blobs_as_pack requires blob objects".into(),
                ));
            }
            if oid.format() != self.format {
                return Err(GitError::InvalidObjectId(format!(
                    "object {oid} uses {}, store uses {}",
                    oid.format().name(),
                    self.format.name()
                )));
            }
            if seen.insert(*oid) && !self.contains(oid)? {
                inputs.push(PackInput { oid, object });
            }
        }
        if inputs.is_empty() {
            return Ok(());
        }
        let options = PackWriteOptions::new()
            .with_window(0)
            .with_depth(0)
            .with_reorder(false)
            .with_compression_level(compression_level);
        let pack =
            PackFile::write_packed_with_known_ids_and_options(&inputs, self.format, &options)?;
        self.install_pack(&pack)?;
        Ok(())
    }

    pub fn install_pack_with_options(
        &self,
        pack: &PackWrite,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        if pack.checksum.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "pack checksum uses {}, store uses {}",
                pack.checksum.format().name(),
                self.format.name()
            )));
        }
        for entry in &pack.entries {
            if entry.oid.format() != self.format {
                return Err(GitError::InvalidObjectId(format!(
                    "pack entry {} uses {}, store uses {}",
                    entry.oid,
                    entry.oid.format().name(),
                    self.format.name()
                )));
            }
        }
        let canonical_index = PackIndex::write_v2_for_pack(&pack.pack, self.format)?;
        let parsed_index = PackIndex::parse(&pack.index, self.format)?;
        if canonical_index.pack_checksum != pack.checksum
            || parsed_index.pack_checksum != pack.checksum
        {
            return Err(GitError::InvalidFormat(
                "pack and index checksums do not match pack write".into(),
            ));
        }
        if pack.index != canonical_index.index {
            return Err(GitError::InvalidFormat(
                "pack index does not match pack contents".into(),
            ));
        }

        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", pack.checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        if !pack_path.exists() || !index_path.exists() {
            write_pack_component(&pack_path, &pack.pack)?;
            write_pack_component(&index_path, &pack.index)?;
        }
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids: canonical_index
                .entries
                .iter()
                .map(|entry| entry.oid)
                .collect(),
        })
    }

    /// Install a pack that was produced in this process by [`PackFile::write_packed`].
    ///
    /// Unlike [`Self::install_raw_pack_from_reader_with_options`], this does not re-inflate
    /// every pack entry to rebuild the index. It validates the generated pack
    /// trailer and generated index against the writer's object ids, CRCs, and
    /// offsets, then writes those bytes directly. Use the raw installer for
    /// arbitrary pack bytes received from an untrusted transport.
    pub fn install_written_pack(&self, pack: &PackWrite) -> Result<PackInstallResult> {
        self.install_written_pack_with_options(pack, RawPackInstallOptions::default())
    }

    pub fn install_written_pack_with_options(
        &self,
        pack: &PackWrite,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        validate_pack_checksum(&pack.pack, self.format, &pack.checksum, "pack write")?;
        let parsed_index = PackIndex::parse(&pack.index, self.format)?;
        if parsed_index.pack_checksum != pack.checksum {
            return Err(GitError::InvalidFormat(
                "pack write index checksum does not match pack".into(),
            ));
        }
        if !pack_index_entries_match_writer(&parsed_index.entries, &pack.entries) {
            return Err(GitError::InvalidFormat(
                "pack write index does not match generated entries".into(),
            ));
        }
        self.install_generated_pack_unchecked(pack, options)
    }

    fn install_generated_pack_unchecked(
        &self,
        pack: &PackWrite,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", pack.checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        if !pack_path.exists() || !index_path.exists() {
            write_pack_component(&pack_path, &pack.pack)?;
            write_pack_component(&index_path, &pack.index)?;
        }
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids: pack.entries.iter().map(|entry| entry.oid).collect(),
        })
    }

    pub(crate) fn install_pack_file_from_temp(
        &self,
        temp_pack_path: &Path,
        pack_checksum: ObjectId,
        index: &[u8],
        object_ids: Vec<ObjectId>,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", pack_checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        match fs::rename(temp_pack_path, &pack_path) {
            Ok(()) => {}
            Err(_) if pack_path.exists() => {
                let _ = fs::remove_file(temp_pack_path);
            }
            Err(err) => return Err(GitError::from(err)),
        }
        write_pack_component(&index_path, index)?;
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids,
        })
    }

    pub fn install_raw_pack_from_reader<R>(&self, reader: &mut R) -> Result<PackInstallResult>
    where
        R: Read,
    {
        self.install_raw_pack_from_reader_with_options(reader, RawPackInstallOptions::default())
    }

    /// Install a pack whose ref-deltas may use objects already available from
    /// this database (including alternates) as bases. Required bases are
    /// appended as full entries before the pack is stored, so the installed
    /// pack remains independently valid.
    pub fn install_raw_pack_from_reader_with_external_bases<R>(
        &self,
        reader: &mut R,
    ) -> Result<PackInstallResult>
    where
        R: Read,
    {
        let mut pack = Vec::new();
        reader.read_to_end(&mut pack)?;
        let fixed = fix_thin_pack(&pack, self.format, |oid| match self.read_object(oid) {
            Ok(object) => Ok(Some((*object).clone())),
            Err(GitError::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        })?;
        let pack = fixed.pack;
        let built = fixed.index;
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let temp_pack_path = unique_temp_path(&pack_dir).with_extension("pack");
        fs::write(&temp_pack_path, &pack)?;
        let result = self.install_pack_file_from_temp(
            &temp_pack_path,
            built.pack_checksum,
            &built.index,
            built.entries.iter().map(|entry| entry.oid).collect(),
            RawPackInstallOptions::default(),
        );
        if result.is_err() {
            let _ = fs::remove_file(&temp_pack_path);
        }
        result
    }

    pub fn begin_raw_pack_install(
        &self,
        expected_pack_id: ObjectId,
        expected_pack_size: u64,
    ) -> Result<RawPackStreamingInstall> {
        self.begin_raw_pack_install_with_options(
            expected_pack_id,
            expected_pack_size,
            RawPackInstallOptions::default(),
        )
    }

    pub fn begin_raw_pack_install_with_options(
        &self,
        expected_pack_id: ObjectId,
        expected_pack_size: u64,
        options: RawPackInstallOptions,
    ) -> Result<RawPackStreamingInstall> {
        if expected_pack_id.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "pack checksum uses {}, store uses {}",
                expected_pack_id.format().name(),
                self.format.name()
            )));
        }
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", expected_pack_id.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        let temp_pack_path = unique_temp_path(&pack_dir).with_extension("pack");
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_pack_path)?;
        Ok(RawPackStreamingInstall {
            format: self.format,
            expected_pack_id,
            expected_pack_size,
            options,
            pack_dir,
            pack_name,
            pack_path,
            index_path,
            temp_pack_path,
            file: Some(file),
            written: 0,
            finished: false,
        })
    }

    pub fn install_raw_pack_from_reader_with_options<R>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult>
    where
        R: Read,
    {
        self.install_raw_pack_from_reader_with_progress(reader, options, |_| {})
    }

    /// [`install_raw_pack_from_reader_with_options`] that reports receipt and
    /// indexing progress. The callback advances while the bounded spool drains
    /// `reader`, then while the immutable mapped pack is indexed.
    ///
    /// Delegates to [`Self::install_raw_pack_from_reader_with_progress_and_cancel`]
    /// with a never-cancel flag.
    ///
    /// [`install_raw_pack_from_reader_with_options`]: FileObjectDatabase::install_raw_pack_from_reader_with_options
    pub fn install_raw_pack_from_reader_with_progress<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        progress: F,
    ) -> Result<PackInstallResult>
    where
        R: Read,
        F: FnMut(PackInstallProgress),
    {
        self.install_raw_pack_from_reader_with_progress_and_cancel(
            reader,
            options,
            CancelFlag::never(),
            progress,
        )
    }

    /// Install a raw pack stream with cooperative cancellation and progress.
    ///
    /// Polls `cancel` between parallel indexing jobs and while receiving into
    /// the bounded spool, so a trip during either stage aborts promptly.
    /// On any failure — including [`GitError::Cancelled`] — the temporary pack
    /// staging file under `objects/pack` is removed.
    pub fn install_raw_pack_from_reader_with_progress_and_cancel<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        cancel: CancelFlag<'_>,
        progress: F,
    ) -> Result<PackInstallResult>
    where
        R: Read,
        F: FnMut(PackInstallProgress),
    {
        // Fail before creating a temp file when cancel is already set.
        cancel.check()?;
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let temp_pack_path = unique_temp_path(&pack_dir).with_extension("pack");
        let result = (|| -> Result<PackInstallResult> {
            // Stage directly in objects/pack so validation, mmap indexing, and
            // the checksum-named rename all use one immutable file.
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_pack_path)?;
            let mut progress = progress;
            let receive =
                receive_pack_to_file(reader, file, options.max_input_size, cancel, &mut progress)
                    .map_err(map_install_cancel_error)?;
            let built = {
                let mapped = sley_mmap::MappedFile::open_pack(&temp_pack_path)?;
                PackIndex::write_v2_for_pack_with_options(
                    mapped.as_bytes(),
                    self.format,
                    |_| Ok(None),
                    sley_pack::PackIndexOptions::default(),
                    cancel,
                    |indexed: PackIndexProgress| {
                        progress(PackInstallProgress {
                            received_bytes: receive.bytes,
                            indexed_objects: indexed.completed_objects,
                            total_objects: indexed.total_objects,
                        });
                    },
                )?
            };

            self.install_pack_file_from_temp(
                &temp_pack_path,
                built.pack_checksum,
                &built.index,
                built.entries.iter().map(|entry| entry.oid).collect(),
                options,
            )
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_pack_path);
        }
        result
    }
}

/// Reads one pack from a stream and then reports EOF.
///
/// Stock `send-pack` writes the pack and reads `report-status` on the same
/// connection. Closing the write side of a pipe delivers EOF; closing one
/// descriptor of a `git://` socket does not, because the client keeps the
/// socket open to read the report. Waiting for EOF there deadlocks. This
/// reader stops after the pack trailer (header, each entry's zlib member, and
/// the checksum) so the caller can answer on the still-open stream.
pub struct PackBoundedReader<R> {
    inner: R,
    end: PackEnd,
    finished: bool,
}

impl<R> PackBoundedReader<R> {
    /// Bound `inner` to a single pack whose trailer is `format`'s hash length.
    pub fn new(inner: R, format: ObjectFormat) -> Self {
        Self {
            inner,
            end: PackEnd::new(format.raw_len()),
            finished: false,
        }
    }
}

impl<R: Read> Read for PackBoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.finished || buf.is_empty() {
            return Ok(0);
        }
        let read = self.inner.read(buf)?;
        if read == 0 {
            if self.end.complete() {
                self.finished = true;
                return Ok(0);
            }
            return Err(bound_io("truncated pack"));
        }
        let keep = self.end.push(&buf[..read])?;
        if keep == 0 {
            return Err(bound_io("pack parser made no progress"));
        }
        if self.end.complete() {
            self.finished = true;
        }
        Ok(keep)
    }
}

const MAX_PACK_ENTRY_HEADER: usize = 16;
const MAX_OFS_DELTA_OFFSET: usize = 16;

struct PackEnd {
    hash_len: usize,
    phase: PackPhase,
    decompress: Decompress,
}

enum PackPhase {
    Header { buf: [u8; 12], filled: usize },
    Entry { left: u64, step: EntryStep },
    Trailer { filled: usize },
    Done,
}

enum EntryStep {
    Header { buf: Vec<u8> },
    Ofs { buf: Vec<u8> },
    Ref { filled: usize },
    Zlib,
}

impl PackEnd {
    fn new(hash_len: usize) -> Self {
        Self {
            hash_len,
            phase: PackPhase::Header {
                buf: [0; 12],
                filled: 0,
            },
            decompress: Decompress::new(true),
        }
    }

    fn complete(&self) -> bool {
        matches!(self.phase, PackPhase::Done)
    }

    fn push(&mut self, input: &[u8]) -> io::Result<usize> {
        let mut index = 0;
        while index < input.len() && !self.complete() {
            let phase = std::mem::replace(&mut self.phase, PackPhase::Done);
            let used = self.consume(phase, &input[index..])?;
            if used == 0 {
                return Err(bound_io("pack parser made no progress"));
            }
            index += used;
        }
        Ok(index)
    }

    fn consume(&mut self, phase: PackPhase, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            self.phase = phase;
            return Err(bound_io("pack parser made no progress"));
        }
        match phase {
            PackPhase::Header { mut buf, filled } => {
                let need = 12 - filled;
                let take = need.min(input.len());
                buf[filled..filled + take].copy_from_slice(&input[..take]);
                let filled = filled + take;
                if filled == 12 {
                    self.finish_header(buf)?;
                } else {
                    self.phase = PackPhase::Header { buf, filled };
                }
                Ok(take)
            }
            PackPhase::Entry { left, step } => self.consume_entry(left, step, input),
            PackPhase::Trailer { filled } => {
                let need = self.hash_len - filled;
                let take = need.min(input.len());
                let filled = filled + take;
                if filled == self.hash_len {
                    self.phase = PackPhase::Done;
                } else {
                    self.phase = PackPhase::Trailer { filled };
                }
                Ok(take)
            }
            PackPhase::Done => {
                self.phase = PackPhase::Done;
                Err(bound_io("pack parser made no progress"))
            }
        }
    }

    fn finish_header(&mut self, buf: [u8; 12]) -> io::Result<()> {
        if &buf[..4] != b"PACK" {
            return Err(bound_io("pack must start with PACK"));
        }
        let version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        if version != 2 && version != 3 {
            return Err(bound_io("unsupported pack version"));
        }
        let count = u64::from(u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]));
        if count == 0 {
            self.phase = PackPhase::Trailer { filled: 0 };
        } else {
            self.phase = PackPhase::Entry {
                left: count,
                step: EntryStep::Header { buf: Vec::new() },
            };
        }
        Ok(())
    }

    fn consume_entry(&mut self, left: u64, step: EntryStep, input: &[u8]) -> io::Result<usize> {
        match step {
            EntryStep::Header { mut buf } => {
                buf.push(input[0]);
                if buf.len() > MAX_PACK_ENTRY_HEADER {
                    return Err(bound_io("pack entry header is too long"));
                }
                if entry_header_done(&buf)? {
                    let kind = (buf[0] >> 4) & 7;
                    self.begin_after_header(left, kind)?;
                } else {
                    self.phase = PackPhase::Entry {
                        left,
                        step: EntryStep::Header { buf },
                    };
                }
                Ok(1)
            }
            EntryStep::Ofs { mut buf } => {
                let byte = input[0];
                buf.push(byte);
                if buf.len() > MAX_OFS_DELTA_OFFSET {
                    return Err(bound_io("ofs-delta offset is too long"));
                }
                if byte & 0x80 == 0 {
                    self.begin_zlib(left);
                } else {
                    self.phase = PackPhase::Entry {
                        left,
                        step: EntryStep::Ofs { buf },
                    };
                }
                Ok(1)
            }
            EntryStep::Ref { filled } => {
                let need = self.hash_len - filled;
                let take = need.min(input.len());
                let filled = filled + take;
                if filled == self.hash_len {
                    self.begin_zlib(left);
                } else {
                    self.phase = PackPhase::Entry {
                        left,
                        step: EntryStep::Ref { filled },
                    };
                }
                Ok(take)
            }
            EntryStep::Zlib => self.consume_zlib(left, input),
        }
    }

    fn begin_after_header(&mut self, left: u64, kind: u8) -> io::Result<()> {
        match kind {
            1..=4 => {
                self.begin_zlib(left);
                Ok(())
            }
            6 => {
                self.phase = PackPhase::Entry {
                    left,
                    step: EntryStep::Ofs { buf: Vec::new() },
                };
                Ok(())
            }
            7 => {
                self.phase = PackPhase::Entry {
                    left,
                    step: EntryStep::Ref { filled: 0 },
                };
                Ok(())
            }
            _ => Err(bound_io("invalid pack object type")),
        }
    }

    fn begin_zlib(&mut self, left: u64) {
        self.decompress.reset(true);
        self.phase = PackPhase::Entry {
            left,
            step: EntryStep::Zlib,
        };
    }

    fn finish_object(&mut self, left: u64) -> io::Result<()> {
        let Some(left) = left.checked_sub(1) else {
            return Err(bound_io("pack object count underflow"));
        };
        if left == 0 {
            self.phase = PackPhase::Trailer { filled: 0 };
        } else {
            self.phase = PackPhase::Entry {
                left,
                step: EntryStep::Header { buf: Vec::new() },
            };
        }
        Ok(())
    }

    fn consume_zlib(&mut self, left: u64, input: &[u8]) -> io::Result<usize> {
        let mut offset = 0;
        let mut output = [0u8; 8192];
        loop {
            let before_in = self.decompress.total_in();
            let before_out = self.decompress.total_out();
            let status = self
                .decompress
                .decompress(&input[offset..], &mut output, FlushDecompress::None)
                .map_err(|_| bound_io("invalid pack zlib stream"))?;
            let consumed = usize::try_from(self.decompress.total_in() - before_in)
                .map_err(|_| bound_io("pack zlib input overflow"))?;
            let produced = self.decompress.total_out() - before_out;
            offset += consumed;
            match status {
                Status::StreamEnd => {
                    self.finish_object(left)?;
                    return Ok(offset);
                }
                Status::Ok | Status::BufError => {
                    if consumed == 0 && produced == 0 {
                        if offset < input.len() {
                            return Err(bound_io("invalid pack zlib stream"));
                        }
                        self.phase = PackPhase::Entry {
                            left,
                            step: EntryStep::Zlib,
                        };
                        return Ok(offset);
                    }
                    if offset >= input.len() {
                        self.phase = PackPhase::Entry {
                            left,
                            step: EntryStep::Zlib,
                        };
                        return Ok(offset);
                    }
                }
            }
        }
    }
}

fn entry_header_done(buf: &[u8]) -> io::Result<bool> {
    let first = buf[0];
    let kind = (first >> 4) & 7;
    if kind == 0 || kind == 5 {
        return Err(bound_io("invalid pack object type"));
    }
    // The size varint shares its first byte with the type. Only the continuation
    // bits matter here: zlib members are self-delimiting, so the declared size
    // is not needed to find the next entry.
    let mut index = 0usize;
    let mut byte = first;
    while byte & 0x80 != 0 {
        index += 1;
        if index >= buf.len() {
            return Ok(false);
        }
        byte = buf[index];
    }
    Ok(true)
}

fn bound_io(message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        GitError::InvalidFormat(message.to_string()),
    )
}

pub(crate) fn write_pack_component(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| GitError::InvalidPath("pack component path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temp_path = unique_temp_path(parent);
    let write_result = (|| -> Result<()> {
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            #[cfg(feature = "fetch-profile")]
            let _profile_span = sley_core::fetch_profile::Span::enter(
                sley_core::fetch_profile::Stage::ObjectStoreWrite,
            );
            file.write_all(bytes)?;
            file.sync_all()?;
            #[cfg(feature = "fetch-profile")]
            {
                sley_core::fetch_profile::add_count(
                    sley_core::fetch_profile::Stage::ObjectStoreWrite,
                    1,
                );
                sley_core::fetch_profile::add_bytes(
                    sley_core::fetch_profile::Stage::ObjectStoreWrite,
                    bytes.len() as u64,
                );
                sley_core::fetch_profile::add_fsync();
            }
        }
        match fs::rename(&temp_path, path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => {
                let _ = fs::remove_file(&temp_path);
                Ok(())
            }
            Err(err) => Err(GitError::from(err)),
        }
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

/// Write a mutable pack sidecar through a completed temporary file, replacing
/// an existing destination. Unix can rename over the destination atomically;
/// platforms which reject that operation fall back to removing the old file
/// only after the replacement has been fully written and synced.
pub(crate) fn replace_pack_component(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| GitError::InvalidPath("pack component path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temp_path = unique_temp_path(parent);
    let write_result = (|| -> Result<()> {
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        match fs::rename(&temp_path, path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => {
                fs::remove_file(path)?;
                fs::rename(&temp_path, path)?;
                Ok(())
            }
            Err(err) => Err(GitError::from(err)),
        }
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

pub(crate) fn write_promisor_pack_sidecar(
    pack_dir: &Path,
    pack_name: &str,
    promisor: bool,
) -> Result<Option<PathBuf>> {
    if !promisor {
        return Ok(None);
    }
    let path = pack_dir.join(format!("{pack_name}.promisor"));
    write_pack_component(&path, b"")?;
    Ok(Some(path))
}

#[cfg(test)]
mod pack_bound_tests {
    use super::PackBoundedReader;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use sley_core::{ObjectFormat, StreamingDigest};
    use sley_object::{EncodedObject, ObjectType};
    use sley_pack::{PackFile, PackWriteOptions};
    use std::io::{self, Read, Write};

    /// Yields `data` and then fails. A reader that stops on the pack boundary
    /// never starts a read at or past `pack_len`.
    struct StrictTail {
        data: Vec<u8>,
        pack_len: usize,
        pos: usize,
    }

    impl Read for StrictTail {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.pack_len {
                return Err(io::Error::other("read started past the pack"));
            }
            let n = (self.data.len() - self.pos).min(buf.len());
            if n == 0 {
                return Ok(0);
            }
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    struct OneByte {
        data: Vec<u8>,
        pos: usize,
    }

    impl Read for OneByte {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            if self.pos >= self.data.len() {
                return Err(io::Error::other("read started past the pack"));
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    fn assert_stops_at_pack(pack: &[u8], format: ObjectFormat) {
        let mut data = pack.to_vec();
        data.extend_from_slice(b"EXTRA");
        let mut bounded = PackBoundedReader::new(
            StrictTail {
                data,
                pack_len: pack.len(),
                pos: 0,
            },
            format,
        );
        let mut out = Vec::new();
        bounded.read_to_end(&mut out).expect("bounded read");
        assert_eq!(out, pack);

        let mut bytewise = PackBoundedReader::new(
            OneByte {
                data: pack.to_vec(),
                pos: 0,
            },
            format,
        );
        let mut out = Vec::new();
        bytewise.read_to_end(&mut out).expect("bytewise read");
        assert_eq!(out, pack);
    }

    fn zlib_member(payload: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).expect("zlib payload");
        encoder.finish().expect("zlib finish")
    }

    fn write_entry_header(out: &mut Vec<u8>, kind: u8, size: u64) {
        let mut size_bits = size;
        let low = (size_bits & 0x0f) as u8;
        size_bits >>= 4;
        let mut byte = (kind << 4) | low;
        if size_bits != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        while size_bits != 0 {
            let mut next = (size_bits & 0x7f) as u8;
            size_bits >>= 7;
            if size_bits != 0 {
                next |= 0x80;
            }
            out.push(next);
        }
    }

    fn finish_pack(mut body: Vec<u8>, format: ObjectFormat) -> Vec<u8> {
        let mut digest = StreamingDigest::new(format);
        digest.update(&body);
        let oid = digest.finalize().expect("pack checksum");
        body.extend_from_slice(oid.as_bytes());
        body
    }

    fn framed_pack(entries: &[&[u8]], format: ObjectFormat) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"PACK");
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for entry in entries {
            body.extend_from_slice(entry);
        }
        finish_pack(body, format)
    }

    #[test]
    fn pack_bounded_reader_stops_without_eof() {
        let blob = EncodedObject::new(ObjectType::Blob, b"hello-interop".to_vec());
        let written = PackFile::write_packed(&[blob], ObjectFormat::Sha1).expect("pack");
        assert_stops_at_pack(&written.pack, ObjectFormat::Sha1);

        let mut blob_entry = Vec::new();
        let payload = b"0123456789abcdefghij";
        write_entry_header(&mut blob_entry, 3, payload.len() as u64);
        blob_entry.extend(zlib_member(payload));
        let mut ofs_entry = Vec::new();
        let delta = b"delta-bytes";
        write_entry_header(&mut ofs_entry, 6, delta.len() as u64);
        ofs_entry.push(12);
        ofs_entry.extend(zlib_member(delta));
        let mut ref_entry = Vec::new();
        write_entry_header(&mut ref_entry, 7, delta.len() as u64);
        ref_entry.extend_from_slice(&[0xab; 20]);
        ref_entry.extend(zlib_member(delta));
        let mixed = framed_pack(&[&blob_entry, &ofs_entry, &ref_entry], ObjectFormat::Sha1);
        assert_stops_at_pack(&mixed, ObjectFormat::Sha1);

        let empty = framed_pack(&[], ObjectFormat::Sha256);
        assert_stops_at_pack(&empty, ObjectFormat::Sha256);
    }

    #[test]
    fn pack_bounded_reader_matches_real_ofs_and_ref_packs() {
        let base = EncodedObject::new(ObjectType::Blob, vec![b'a'; 64]);
        let next = EncodedObject::new(ObjectType::Blob, vec![b'a'; 60]);
        let ofs = PackFile::write_packed_with_options(
            &[base.clone(), next.clone()],
            ObjectFormat::Sha1,
            &PackWriteOptions::new().with_prefer_ofs_delta(true),
        )
        .expect("ofs pack");
        assert_stops_at_pack(&ofs.pack, ObjectFormat::Sha1);
        let refs = PackFile::write_packed_with_options(
            &[base, next],
            ObjectFormat::Sha256,
            &PackWriteOptions::new().with_prefer_ofs_delta(false),
        )
        .expect("ref pack");
        assert_stops_at_pack(&refs.pack, ObjectFormat::Sha256);
    }
}
