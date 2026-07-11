use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::{Decompress, FlushDecompress};
use parking_lot::RwLock;
use std::sync::Mutex;
use sley_core::{GitError, MissingObjectContext, ObjectFormat, ObjectId, Result};
use sley_formats::{Bundle, BundleReference};
use sley_object::{
    Commit, EncodedObject, ObjectType, Tag, TreeEntries, parse_framed_object,
    tree_entry_object_type,
};
use sley_pack::{
    MultiPackIndex, MultiPackIndexOidLookup, PackBitmapIndex, PackBitmapWriter, PackFile,
    PackIndex, PackIndexByteSource, PackIndexEntry, PackIndexViewData, PackInput,
    PackReverseIndex, PackStreamIndexBuild, PackStreamProgress, PackWrite, PackWriteOptions,
    PackWriteSummary,
};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::{env, fs};

use crate::{
    grafted_parents, implied_empty_tree_object, unique_temp_path, with_missing_object_context,
    ObjectReader, ObjectWriter,
};

use crate::pack::{FileObjectDatabase, ObjectDatabase};
use crate::loose::LooseObjectStore;
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

struct PackInstallTeeReader<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
    max_input_size: Option<u64>,
    written: u64,
}

impl<R, W> Read for PackInstallTeeReader<'_, R, W>
where
    R: Read,
    W: Write,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.reader.read(buf)?;
        if len > 0 {
            let next_written = self.written.checked_add(len as u64).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "pack size overflow")
            })?;
            if let Some(limit) = self.max_input_size
                && next_written > limit
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("pack exceeds maximum allowed size ({limit})"),
                ));
            }
            self.writer.write_all(&buf[..len])?;
            self.written = next_written;
        }
        Ok(len)
    }
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

    /// Install a raw pack while reporting streaming pack progress via `progress`.
    ///
    /// The default implementation ignores `progress` and delegates to
    /// [`install_raw_pack_from_reader_with_options`], so installers that do not
    /// stream through the pack indexer (e.g. the in-memory store) keep working
    /// unchanged. [`FileObjectDatabase`] overrides this to thread real counters.
    ///
    /// [`install_raw_pack_from_reader_with_options`]: RawPackInstaller::install_raw_pack_from_reader_with_options
    fn install_raw_pack_from_reader_with_progress<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        _progress: F,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
        F: FnMut(PackStreamProgress),
    {
        self.install_raw_pack_from_reader_with_options(reader, options)
    }
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

    fn install_raw_pack_from_reader_with_progress<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        progress: F,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
        F: FnMut(PackStreamProgress),
    {
        let result = FileObjectDatabase::install_raw_pack_from_reader_with_progress(
            self, reader, options, progress,
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
            file.flush()?;
            file.sync_all()?;
            drop(file);

            if self.written != self.expected_pack_size {
                return Err(GitError::InvalidFormat(format!(
                    "raw pack stream length mismatch: expected {}, got {}",
                    self.expected_pack_size, self.written
                )));
            }

            let built = PackIndex::write_v2_for_pack_path(&self.temp_pack_path, self.format)?;
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
    let pack = PackFile::parse(pack_bytes, format)?;
    let built = PackIndex::write_v2_for_pack(pack_bytes, format)?;
    if built.pack_checksum != pack.checksum {
        return Err(GitError::InvalidFormat(
            "pack index checksum does not match parsed pack checksum".to_string(),
        ));
    }

    let offsets = built
        .entries
        .iter()
        .map(|entry| (entry.oid, entry.offset))
        .collect::<HashMap<_, _>>();
    let mut objects = Vec::with_capacity(pack.entries.len());
    for object in pack.entries {
        let offset = offsets.get(&object.entry.oid).copied().ok_or_else(|| {
            GitError::InvalidFormat(format!(
                "pack index is missing object {}",
                object.entry.oid.to_hex()
            ))
        })?;
        objects.push(RawPackIndexedObject {
            oid: object.entry.oid,
            object_type: object.object.object_type,
            size: object.object.body.len() as u64,
            offset,
        });
    }

    Ok(RawPackIndexResult {
        pack_id: built.pack_checksum,
        index: built.index,
        objects,
    })
}

pub fn index_raw_pack_from_reader<R>(
    reader: &mut R,
    format: ObjectFormat,
) -> Result<RawPackIndexResult>
where
    R: Read,
{
    Ok(stream_index_build_to_raw_result(
        PackIndex::write_v2_for_pack_reader_to_trailer(reader, format)?,
    ))
}

pub fn index_raw_pack_from_reader_with_len<R>(
    reader: &mut R,
    format: ObjectFormat,
    pack_len: u64,
) -> Result<RawPackIndexResult>
where
    R: Read,
{
    Ok(stream_index_build_to_raw_result(
        PackIndex::write_v2_for_pack_reader_with_len(reader, format, pack_len)?,
    ))
}

pub fn index_raw_pack_file(
    path: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<RawPackIndexResult> {
    Ok(stream_index_build_to_raw_result(
        PackIndex::write_v2_for_pack_path(path, format)?,
    ))
}

fn stream_index_build_to_raw_result(built: PackStreamIndexBuild) -> RawPackIndexResult {
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
    /// Unlike [`Self::install_raw_pack_with_options`], this does not re-inflate
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
        let temp_pack_path = unique_temp_path(&pack_dir);
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

    /// [`install_raw_pack_from_reader_with_options`] that reports streaming pack
    /// progress. `progress` is threaded to the pack indexer, so it advances as
    /// the pack is received off `reader` (during download for streaming
    /// transports; during the index walk for already-buffered ones).
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
        F: FnMut(PackStreamProgress),
    {
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let temp_pack_path = unique_temp_path(&pack_dir);
        let result = (|| -> Result<PackInstallResult> {
            // Stage directly in objects/pack so validation, indexing, and the
            // eventual checksum-named rename use one streamed write.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_pack_path)?;
            let built = {
                let mut tee = PackInstallTeeReader {
                    reader,
                    writer: &mut file,
                    max_input_size: options.max_input_size,
                    written: 0,
                };
                PackIndex::write_v2_for_pack_reader_to_trailer_with_progress(
                    &mut tee,
                    self.format,
                    progress,
                )?
            };
            file.flush()?;
            file.sync_all()?;
            drop(file);

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
            file.write_all(bytes)?;
            file.sync_all()?;
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
