use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use git_core::{GitError, ObjectFormat, ObjectId, Result};
use git_formats::{Bundle, BundleReference, EncodedObject, parse_framed_object};
use git_pack::{MultiPackIndex, PackFile, PackIndex};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs};

static TEMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub trait ObjectReader {
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject>;
}

pub trait ObjectWriter {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleUnbundleResult {
    pub written_objects: Vec<ObjectId>,
    pub references: Vec<BundleReference>,
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
            Err(GitError::NotFound(_)) => missing.push(prerequisite.oid.clone()),
            Err(err) => return Err(err),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let missing = missing
        .iter()
        .map(ObjectId::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Err(GitError::NotFound(format!(
        "bundle prerequisites missing: {missing}"
    )))
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
    let mut written_objects = Vec::with_capacity(pack.entries.len());
    for entry in pack.entries {
        let expected = entry.entry.oid;
        let actual = writer.write_object(entry.object)?;
        if actual != expected {
            return Err(GitError::InvalidObject(format!(
                "bundle object id mismatch: expected {expected}, wrote {actual}"
            )));
        }
        written_objects.push(actual);
    }
    Ok(BundleUnbundleResult {
        written_objects,
        references: bundle.references.clone(),
    })
}

#[derive(Debug, Clone)]
pub struct ObjectDatabase {
    format: ObjectFormat,
    objects: HashMap<ObjectId, EncodedObject>,
    promisor: bool,
}

impl ObjectDatabase {
    pub fn new(format: ObjectFormat) -> Self {
        Self {
            format,
            objects: HashMap::new(),
            promisor: false,
        }
    }

    pub fn with_promisor(mut self, promisor: bool) -> Self {
        self.promisor = promisor;
        self
    }

    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.objects.contains_key(oid)
    }

    pub fn validate(&self, oid: &ObjectId) -> Result<()> {
        let object = self.read_object(oid)?;
        let actual = object.object_id(self.format)?;
        if &actual == oid {
            Ok(())
        } else {
            Err(GitError::InvalidObject(format!(
                "object id mismatch: expected {oid}, got {actual}"
            )))
        }
    }
}

impl ObjectReader for ObjectDatabase {
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        self.objects
            .get(oid)
            .cloned()
            .ok_or_else(|| GitError::NotFound(format!("object {oid}")))
    }
}

impl ObjectWriter for ObjectDatabase {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        self.objects.entry(oid.clone()).or_insert(object);
        Ok(oid)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternate {
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialClonePolicy {
    pub promisor_remote: Option<String>,
    pub allow_missing_promised_objects: bool,
}

#[derive(Debug, Clone)]
pub struct FileObjectDatabase {
    loose: LooseObjectStore,
    objects_dir: PathBuf,
    alternates: Vec<PathBuf>,
    format: ObjectFormat,
}

pub fn repository_objects_dir(git_dir: impl AsRef<Path>) -> PathBuf {
    env::var_os("GIT_OBJECT_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.as_ref().join("objects"))
}

impl FileObjectDatabase {
    pub fn new(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        let objects_dir = objects_dir.into();
        Self {
            loose: LooseObjectStore::new(objects_dir.clone(), format),
            alternates: alternate_object_dirs(&objects_dir),
            objects_dir,
            format,
        }
    }

    fn without_alternates(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        let objects_dir = objects_dir.into();
        Self {
            loose: LooseObjectStore::new(objects_dir.clone(), format),
            alternates: Vec::new(),
            objects_dir,
            format,
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
    }

    pub fn loose(&self) -> &LooseObjectStore {
        &self.loose
    }

    pub fn contains(&self, oid: &ObjectId) -> Result<bool> {
        if self.loose.exists(oid)? {
            return Ok(true);
        }
        if self.find_pack_containing(oid)?.is_some() {
            return Ok(true);
        }
        for alternate in &self.alternates {
            if Self::without_alternates(alternate, self.format).contains(oid)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn read_packed_object(&self, oid: &ObjectId) -> Result<Option<EncodedObject>> {
        let Some(pack_paths) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        let pack = PackFile::parse(&fs::read(pack_paths.pack)?, self.format)?;
        for entry in pack.entries {
            if &entry.entry.oid == oid {
                return Ok(Some(entry.object));
            }
        }
        Err(GitError::InvalidFormat(format!(
            "pack index listed object {oid}, but pack did not contain it"
        )))
    }

    fn find_pack_containing(&self, oid: &ObjectId) -> Result<Option<PackPaths>> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        let pack_dir = self.objects_dir.join("pack");
        if !pack_dir.exists() {
            return Ok(None);
        }
        if let Some(pack_paths) = self.find_midx_pack_containing(&pack_dir, oid)? {
            return Ok(Some(pack_paths));
        }
        for entry in fs::read_dir(pack_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let index = PackIndex::parse(&fs::read(&path)?, self.format)?;
            if index.find(oid).is_some() {
                let Some(stem) = path.file_stem() else {
                    continue;
                };
                let pack = path.with_file_name(format!("{}.pack", stem.to_string_lossy()));
                if !pack.exists() {
                    return Err(GitError::NotFound(format!(
                        "pack file {} for index {}",
                        pack.display(),
                        path.display()
                    )));
                }
                return Ok(Some(PackPaths { pack }));
            }
        }
        Ok(None)
    }

    fn find_midx_pack_containing(
        &self,
        pack_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<PackPaths>> {
        let midx_path = pack_dir.join("multi-pack-index");
        if !midx_path.exists() {
            return Ok(None);
        }
        let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, self.format)?;
        let Some(entry) = midx.find(oid) else {
            return Ok(None);
        };
        let Some(pack_name) = midx.pack_names.get(entry.pack_int_id as usize) else {
            return Err(GitError::InvalidFormat(
                "multi-pack-index object points past pack table".into(),
            ));
        };
        let pack_file_name = pack_name
            .strip_suffix(".idx")
            .map(|stem| format!("{stem}.pack"))
            .unwrap_or_else(|| pack_name.clone());
        let pack = pack_dir.join(pack_file_name);
        if !pack.exists() {
            return Err(GitError::NotFound(format!(
                "pack file {} for multi-pack-index {}",
                pack.display(),
                midx_path.display()
            )));
        }
        Ok(Some(PackPaths { pack }))
    }
}

fn alternate_object_dirs(objects_dir: &Path) -> Vec<PathBuf> {
    let mut alternates = Vec::new();
    if let Some(value) = env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
        for raw in value.to_string_lossy().split(':') {
            if !raw.is_empty() {
                alternates.push(PathBuf::from(raw));
            }
        }
    }
    let alternates_path = objects_dir.join("info").join("alternates");
    if let Ok(contents) = fs::read(&alternates_path) {
        for raw in contents.split(|byte| *byte == b'\n') {
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            let Ok(value) = std::str::from_utf8(line) else {
                continue;
            };
            let path = Path::new(value);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                objects_dir.join(path)
            };
            alternates.push(absolute);
        }
    }
    alternates
}

impl ObjectReader for FileObjectDatabase {
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        match self.loose.read_object(oid) {
            Ok(object) => return Ok(object),
            Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        if let Some(object) = self.read_packed_object(oid)? {
            return Ok(object);
        }
        for alternate in &self.alternates {
            match Self::without_alternates(alternate, self.format).read_object(oid) {
                Ok(object) => return Ok(object),
                Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Err(GitError::NotFound(format!("object {oid}")))
    }
}

impl ObjectWriter for FileObjectDatabase {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
        self.loose.write_object(object)
    }
}

#[derive(Debug, Clone)]
struct PackPaths {
    pack: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LooseObjectStore {
    objects_dir: PathBuf,
    format: ObjectFormat,
}

impl LooseObjectStore {
    pub fn new(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        Self {
            objects_dir: objects_dir.into(),
            format,
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
    }

    pub fn object_path(&self, oid: &ObjectId) -> Result<PathBuf> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        let hex = oid.to_hex();
        Ok(self.objects_dir.join(&hex[..2]).join(&hex[2..]))
    }

    pub fn exists(&self, oid: &ObjectId) -> Result<bool> {
        Ok(self.object_path(oid)?.exists())
    }
}

impl ObjectReader for LooseObjectStore {
    fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        let path = self.object_path(oid)?;
        if !path.exists() {
            return Err(GitError::NotFound(format!("object {oid}")));
        }
        let compressed = fs::read(&path)?;
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut framed = Vec::new();
        decoder.read_to_end(&mut framed)?;
        let object = parse_framed_object(&framed)?;
        let actual = object.object_id(self.format)?;
        if &actual != oid {
            return Err(GitError::InvalidObject(format!(
                "loose object {} hashes to {actual}",
                path.display()
            )));
        }
        Ok(object)
    }
}

impl ObjectWriter for LooseObjectStore {
    fn write_object(&mut self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        let path = self.object_path(&oid)?;
        if path.exists() {
            return Ok(oid);
        }
        let parent = path
            .parent()
            .ok_or_else(|| GitError::InvalidPath("loose object path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let temp_path = unique_temp_path(parent);
        let write_result = (|| -> Result<()> {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&object.framed_bytes())?;
            let compressed = encoder.finish()?;
            {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp_path)?;
                file.write_all(&compressed)?;
                file.sync_all()?;
            }
            match fs::rename(&temp_path, &path) {
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
        write_result?;
        Ok(oid)
    }
}

fn unique_temp_path(parent: &Path) -> PathBuf {
    let id = TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("tmp_obj_{}_{}", std::process::id(), id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_formats::{EncodedObject, ObjectType};
    use git_pack::PackFile;

    #[test]
    fn write_and_validate_blob() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .unwrap();
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        db.validate(&oid).unwrap();
    }

    #[test]
    fn loose_store_writes_and_reads_object() {
        let root = std::env::temp_dir().join(format!(
            "git-rs-loose-store-{}-{}",
            std::process::id(),
            TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut store = LooseObjectStore::new(root.join("objects"), ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec());
        let oid = store.write_object(object.clone()).unwrap();
        assert_eq!(store.read_object(&oid).unwrap(), object);
        assert!(store.object_path(&oid).unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_reads_object_from_pack_index() {
        let root = temp_root("git-rs-file-odb-pack");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"packed\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .unwrap();

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_reads_sha256_object_from_pack_index() {
        let root = temp_root("git-rs-file-odb-pack-sha256");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"packed sha256\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let written =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
                .unwrap();
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .unwrap();

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_reads_object_from_multi_pack_index() {
        let root = temp_root("git-rs-file-odb-midx");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let first = EncodedObject::new(ObjectType::Blob, b"first packed\n".to_vec());
        let second = EncodedObject::new(ObjectType::Blob, b"second packed\n".to_vec());
        let first_oid = first.object_id(ObjectFormat::Sha1).unwrap();
        let second_oid = second.object_id(ObjectFormat::Sha1).unwrap();
        let first_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&first)).unwrap();
        let second_pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&second)).unwrap();
        let first_pack_name = format!("pack-{}.idx", first_pack.checksum.to_hex());
        let second_pack_name = format!("pack-{}.idx", second_pack.checksum.to_hex());
        fs::write(
            pack_dir.join(first_pack_name.replace(".idx", ".pack")),
            first_pack.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(second_pack_name.replace(".idx", ".pack")),
            second_pack.pack,
        )
        .unwrap();
        let midx = MultiPackIndex::write(
            ObjectFormat::Sha1,
            2,
            &[first_pack_name, second_pack_name],
            &[
                git_pack::MultiPackIndexEntry {
                    oid: first_oid.clone(),
                    pack_int_id: 0,
                    offset: first_pack.entries[0].offset,
                },
                git_pack::MultiPackIndexEntry {
                    oid: second_oid.clone(),
                    pack_int_id: 1,
                    offset: second_pack.entries[0].offset,
                },
            ],
        )
        .unwrap();
        fs::write(pack_dir.join("multi-pack-index"), midx).unwrap();

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        assert!(db.contains(&second_oid).unwrap());
        assert_eq!(db.read_object(&second_oid).unwrap(), second);
        assert_eq!(db.read_object(&first_oid).unwrap(), first);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_prefers_loose_object_over_packed_object() {
        let root = temp_root("git-rs-file-odb-prefer-loose");
        let git_dir = root.join(".git");
        let pack_dir = git_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"same\n".to_vec());
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let pack_name = written.checksum.to_hex();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.pack")),
            written.pack,
        )
        .unwrap();
        fs::write(
            pack_dir.join(format!("pack-{pack_name}.idx")),
            written.index,
        )
        .unwrap();

        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db.write_object(object.clone()).unwrap();
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundle_prerequisite_verification_reads_existing_objects() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"base\n".to_vec()))
            .unwrap();
        let bundle_bytes = format!("# v2 git bundle\n-{oid} base\n\n").into_bytes();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        verify_bundle_prerequisites(&bundle, &db).unwrap();
    }

    #[test]
    fn bundle_prerequisite_verification_reports_missing_objects() {
        let db = ObjectDatabase::new(ObjectFormat::Sha1);
        let missing =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"missing\n").unwrap();
        let bundle_bytes = format!("# v2 git bundle\n-{missing} missing\n\n").into_bytes();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        assert!(verify_bundle_prerequisites(&bundle, &db).is_err());
    }

    #[test]
    fn unbundle_objects_writes_pack_entries_and_returns_refs() {
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"bundle object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack.pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        let result = unbundle_objects(&bundle, &prerequisite_reader, &mut writer).unwrap();
        assert_eq!(result.written_objects, vec![oid.clone()]);
        assert_eq!(result.references, bundle.references);
        assert_eq!(writer.read_object(&oid).unwrap(), object);
    }

    #[test]
    fn unbundle_objects_rejects_missing_prerequisites_before_writing() {
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha1);
        let missing =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"missing\n").unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"bundle object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let bundle_bytes =
            format!("# v2 git bundle\n-{missing} missing\n{oid} refs/heads/main\n\n")
                .into_bytes()
                .into_iter()
                .chain(pack.pack)
                .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        assert!(unbundle_objects(&bundle, &prerequisite_reader, &mut writer).is_err());
        assert!(!writer.contains(&oid));
    }

    fn temp_root(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
