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
use std::io::{Read, Write};
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
                return Err(GitError::Io(err.to_string()));
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
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    Err(GitError::Io(
        "could not create incoming object quarantine".into(),
    ))
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
            .map_err(|_| GitError::Io("could not initialize pack receive buffers".into()))?;
    }

    std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> Result<()> {
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
        });

        let receive_result = (|| -> Result<PackReceiveSummary> {
            let mut bytes = 0u64;
            let mut last_progress = 0u64;
            let mut header = Vec::with_capacity(12);
            let mut total_objects = 0u64;
            loop {
                cancel.check()?;
                let mut chunk = empty_receiver.recv().map_err(|_| {
                    GitError::Io("pack staging writer stopped before receive completed".into())
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
                filled_sender.send(chunk).map_err(|_| {
                    GitError::Io("pack staging writer stopped before receive completed".into())
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
            Err(_) => Err(GitError::Io("pack staging writer panicked".into())),
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
                Err(err) => return Err(GitError::Io(err.to_string())),
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
            Err(err) => return Err(GitError::Io(err.to_string())),
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
            Err(err) => Err(GitError::Io(err.to_string())),
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
            Err(err) => Err(GitError::Io(err.to_string())),
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
