use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use git_core::{GitError, ObjectFormat, ObjectId, Result};
use git_formats::{
    parse_framed_object, Bundle, BundleReference, Commit, EncodedObject, ObjectType, Tag, Tree,
};
use git_pack::{MultiPackIndex, PackFile, PackIndex, PackWrite};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPackInstallResult {
    pub object_ids: Vec<ObjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawPackInstallOptions {
    pub promisor: bool,
}

pub trait RawPackInstaller {
    fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectPrefixResolution {
    Missing,
    Unique(ObjectId),
    Ambiguous(Vec<ObjectId>),
}

impl RawPackInstaller for FileObjectDatabase {
    fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult> {
        let result = FileObjectDatabase::install_raw_pack(self, pack_bytes)?;
        Ok(RawPackInstallResult {
            object_ids: result.object_ids,
        })
    }
}

impl RawPackInstaller for std::cell::RefCell<ObjectDatabase> {
    fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult> {
        let mut database = self.borrow_mut();
        let format = database.format;
        let result = unpack_packfile_objects(pack_bytes, format, &mut *database)?;
        Ok(RawPackInstallResult {
            object_ids: result.written_objects,
        })
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
    let install = destination.install_raw_pack(&bundle.pack)?;
    Ok(BundleUnbundleResult {
        written_objects: install.object_ids,
        references: bundle.references.clone(),
    })
}

pub fn unpack_packfile_objects<W>(
    pack_bytes: &[u8],
    format: ObjectFormat,
    writer: &mut W,
) -> Result<PackUnpackResult>
where
    W: ObjectWriter,
{
    let pack = PackFile::parse(pack_bytes, format)?;
    write_pack_objects(pack, writer, "pack")
}

fn write_pack_objects<W>(pack: PackFile, writer: &mut W, source: &str) -> Result<PackUnpackResult>
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

pub fn collect_reachable_object_ids<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
) -> Result<HashSet<ObjectId>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut seen = HashSet::new();
    let mut objects = Vec::new();
    for oid in starts {
        collect_reachable_object(
            reader,
            format,
            oid,
            &HashSet::new(),
            &mut seen,
            &mut objects,
        )?;
    }
    Ok(seen)
}

pub fn collect_reachable_objects<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Vec<EncodedObject>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let mut seen = HashSet::new();
    let mut objects = Vec::new();
    for oid in starts {
        collect_reachable_object(reader, format, oid, excluded, &mut seen, &mut objects)?;
    }
    Ok(objects)
}

pub fn install_reachable_pack<I>(
    source: &impl ObjectReader,
    destination: &impl RawPackInstaller,
    format: ObjectFormat,
    starts: I,
) -> Result<Option<RawPackInstallResult>>
where
    I: IntoIterator<Item = ObjectId>,
{
    install_reachable_pack_excluding(source, destination, format, starts, &HashSet::new())
}

pub fn install_reachable_pack_excluding<I>(
    source: &impl ObjectReader,
    destination: &impl RawPackInstaller,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Option<RawPackInstallResult>>
where
    I: IntoIterator<Item = ObjectId>,
{
    let pack = match build_reachable_pack(source, format, starts, excluded)? {
        Some(pack) => pack,
        None => return Ok(None),
    };
    destination.install_raw_pack(&pack.pack).map(Some)
}

pub fn build_reachable_pack<R, I>(
    reader: &R,
    format: ObjectFormat,
    starts: I,
    excluded: &HashSet<ObjectId>,
) -> Result<Option<PackWrite>>
where
    R: ObjectReader,
    I: IntoIterator<Item = ObjectId>,
{
    let objects = collect_reachable_objects(reader, format, starts, excluded)?;
    if objects.is_empty() {
        return Ok(None);
    }
    // Delta-compress reachable packs (used by install/push/fetch) via git-pack's
    // sliding-window selection. Self-contained, ofs-delta by default; round-trips
    // through the existing parser. PackWrite shape is unchanged, so callers are
    // unaffected.
    PackFile::write_packed(&objects, format).map(Some)
}

fn collect_reachable_object<R>(
    reader: &R,
    format: ObjectFormat,
    oid: ObjectId,
    excluded: &HashSet<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    objects: &mut Vec<EncodedObject>,
) -> Result<()>
where
    R: ObjectReader,
{
    if excluded.contains(&oid) {
        return Ok(());
    }
    if !seen.insert(oid.clone()) {
        return Ok(());
    }
    let object = reader.read_object(&oid)?;
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse(format, &object.body)?;
            objects.push(object);
            collect_reachable_object(reader, format, commit.tree, excluded, seen, objects)?;
            for parent in commit.parents {
                collect_reachable_object(reader, format, parent, excluded, seen, objects)?;
            }
        }
        ObjectType::Tree => {
            let tree = Tree::parse(format, &object.body)?;
            objects.push(object);
            for entry in tree.entries {
                collect_reachable_object(reader, format, entry.oid, excluded, seen, objects)?;
            }
        }
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            objects.push(object);
            collect_reachable_object(reader, format, tag.object, excluded, seen, objects)?;
        }
        ObjectType::Blob => objects.push(object),
    }
    Ok(())
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
        .unwrap_or_else(|| repository_common_dir(git_dir).join("objects"))
}

pub fn repository_common_dir(git_dir: impl AsRef<Path>) -> PathBuf {
    if let Some(common_dir) = env::var_os("GIT_COMMON_DIR") {
        return PathBuf::from(common_dir);
    }
    let git_dir = git_dir.as_ref();
    let commondir = git_dir.join("commondir");
    if let Ok(value) = fs::read_to_string(&commondir) {
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        return fs::canonicalize(&common).unwrap_or(common);
    }
    git_dir.to_path_buf()
}

pub fn repository_object_ids(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    object_ids_in_objects_dir(&repository_objects_dir(git_dir), format)
}

pub fn object_ids_in_objects_dir(
    objects_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let objects_dir = objects_dir.as_ref();
    let mut oids = HashSet::new();
    collect_loose_object_ids(objects_dir, format, &mut oids)?;
    collect_packed_object_ids(&objects_dir.join("pack"), format, &mut oids)?;
    let mut oids = oids.into_iter().collect::<Vec<_>>();
    oids.sort_by_key(ObjectId::to_hex);
    Ok(oids)
}

fn collect_loose_object_ids(
    objects_dir: &Path,
    format: ObjectFormat,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !objects_dir.exists() {
        return Ok(());
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(fanout) = name.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let name = object_entry.file_name();
            let Some(suffix) = name.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            oids.insert(ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?);
        }
    }
    Ok(())
}

fn collect_packed_object_ids(
    pack_dir: &Path,
    format: ObjectFormat,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !pack_dir.exists() {
        return Ok(());
    }
    let midx_path = pack_dir.join("multi-pack-index");
    if midx_path.exists() {
        let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, format)?;
        oids.extend(midx.objects.into_iter().map(|entry| entry.oid));
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        oids.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(())
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

    pub fn install_pack(&self, pack: &PackWrite) -> Result<PackInstallResult> {
        self.install_pack_with_options(pack, RawPackInstallOptions::default())
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
        let parsed_pack = PackFile::parse(&pack.pack, self.format)?;
        let parsed_index = PackIndex::parse(&pack.index, self.format)?;
        if parsed_pack.checksum != pack.checksum || parsed_index.pack_checksum != pack.checksum {
            return Err(GitError::InvalidFormat(
                "pack and index checksums do not match pack write".into(),
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
            object_ids: pack.entries.iter().map(|entry| entry.oid.clone()).collect(),
        })
    }

    pub fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<PackInstallResult> {
        self.install_raw_pack_with_options(pack_bytes, RawPackInstallOptions::default())
    }

    pub fn install_raw_pack_with_options(
        &self,
        pack_bytes: &[u8],
        options: RawPackInstallOptions,
    ) -> Result<PackInstallResult> {
        let built = PackIndex::write_v2_for_pack(pack_bytes, self.format)?;
        let pack_dir = self.objects_dir.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_name = format!("pack-{}", built.pack_checksum.to_hex());
        let pack_path = pack_dir.join(format!("{pack_name}.pack"));
        let index_path = pack_dir.join(format!("{pack_name}.idx"));
        if !pack_path.exists() || !index_path.exists() {
            write_pack_component(&pack_path, pack_bytes)?;
            write_pack_component(&index_path, &built.index)?;
        }
        let promisor_path = write_promisor_pack_sidecar(&pack_dir, &pack_name, options.promisor)?;
        Ok(PackInstallResult {
            pack_name,
            pack_path,
            index_path,
            promisor_path,
            object_ids: built
                .entries
                .iter()
                .map(|entry| entry.oid.clone())
                .collect(),
        })
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

    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        let mut oids = object_ids_in_objects_dir(&self.objects_dir, self.format)?
            .into_iter()
            .collect::<HashSet<_>>();
        for alternate in &self.alternates {
            oids.extend(Self::without_alternates(alternate, self.format).object_ids()?);
        }
        let mut oids = oids.into_iter().collect::<Vec<_>>();
        oids.sort_by_key(ObjectId::to_hex);
        Ok(oids)
    }

    pub fn resolve_prefix(&self, prefix: &str) -> Result<ObjectPrefixResolution> {
        validate_object_id_prefix(self.format, prefix)?;
        let mut matches = Vec::new();
        for oid in self.object_ids()? {
            if object_id_matches_prefix(&oid, prefix) {
                matches.push(oid);
            }
        }
        Ok(match matches.len() {
            0 => ObjectPrefixResolution::Missing,
            1 => ObjectPrefixResolution::Unique(matches.remove(0)),
            _ => ObjectPrefixResolution::Ambiguous(matches),
        })
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

fn validate_object_id_prefix(format: ObjectFormat, prefix: &str) -> Result<()> {
    if prefix.len() < 4 || prefix.len() > format.hex_len() {
        return Err(GitError::InvalidObjectId(format!(
            "expected 4 to {} hex digits for {}, got {}",
            format.hex_len(),
            format.name(),
            prefix.len()
        )));
    }
    if !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitError::InvalidObjectId(format!(
            "non-hex object id prefix {prefix}"
        )));
    }
    Ok(())
}

fn object_id_matches_prefix(oid: &ObjectId, prefix: &str) -> bool {
    oid.to_hex()
        .as_bytes()
        .iter()
        .zip(prefix.as_bytes())
        .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
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

fn write_pack_component(path: &Path, bytes: &[u8]) -> Result<()> {
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

fn write_promisor_pack_sidecar(
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
    use git_formats::{Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntry};
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
    fn file_database_resolves_unique_loose_object_prefix() {
        let root = temp_root("git-rs-file-odb-prefix-loose");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"prefix loose\n".to_vec());
        let oid = db.write_object(object).unwrap();
        let prefix = &oid.to_hex()[..8];

        assert_eq!(
            db.resolve_prefix(prefix).unwrap(),
            ObjectPrefixResolution::Unique(oid.clone())
        );
        assert!(db.object_ids().unwrap().contains(&oid));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_resolves_unique_packed_object_prefix() {
        let root = temp_root("git-rs-file-odb-prefix-packed");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"prefix packed\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        db.install_pack(&pack).unwrap();
        let prefix = &oid.to_hex()[..8];

        assert_eq!(
            db.resolve_prefix(prefix).unwrap(),
            ObjectPrefixResolution::Unique(oid)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_reports_ambiguous_object_prefix() {
        let root = temp_root("git-rs-file-odb-prefix-ambiguous");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let mut seen = HashMap::new();
        let (prefix, first, second) = (0..10_000)
            .find_map(|idx| {
                let object =
                    EncodedObject::new(ObjectType::Blob, format!("ambiguous {idx}\n").into_bytes());
                let oid = db.write_object(object).unwrap();
                let prefix = oid.to_hex()[..4].to_string();
                if let Some(first) = seen.insert(prefix.clone(), oid.clone()) {
                    Some((prefix, first, oid))
                } else {
                    None
                }
            })
            .expect("test should find a 4-hex collision");

        let ObjectPrefixResolution::Ambiguous(mut matches) = db.resolve_prefix(&prefix).unwrap()
        else {
            panic!("expected ambiguous prefix {prefix}");
        };
        matches.sort_by_key(ObjectId::to_hex);
        let mut expected = vec![first, second];
        expected.sort_by_key(ObjectId::to_hex);
        assert_eq!(matches, expected);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_rejects_too_short_object_prefix() {
        let root = temp_root("git-rs-file-odb-prefix-short");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        assert!(matches!(
            db.resolve_prefix("abc"),
            Err(GitError::InvalidObjectId(_))
        ));
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
    fn file_database_installs_sha256_pack_without_loose_objects() {
        let root = temp_root("git-rs-file-odb-install-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"installed sha256 pack\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);

        let result = db.install_pack(&pack).unwrap();

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid.clone()]);
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert_eq!(result.promisor_path, None);
        assert!(!db.loose().object_path(&oid).unwrap().exists());
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_installs_raw_sha256_pack_without_loose_objects() {
        let root = temp_root("git-rs-file-odb-install-raw-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"installed raw sha256 pack\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);

        let result = db.install_raw_pack(&pack.pack).unwrap();

        assert_eq!(result.pack_name, format!("pack-{}", pack.checksum.to_hex()));
        assert_eq!(result.object_ids, vec![oid.clone()]);
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert_eq!(result.promisor_path, None);
        assert!(!db.loose().object_path(&oid).unwrap().exists());
        assert!(db.contains(&oid).unwrap());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_database_installs_raw_promisor_pack_with_sidecar() {
        let root = temp_root("git-rs-file-odb-install-raw-promisor-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let object = EncodedObject::new(ObjectType::Blob, b"installed promisor pack\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha1).unwrap();
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);

        let result = db
            .install_raw_pack_with_options(&pack.pack, RawPackInstallOptions { promisor: true })
            .unwrap();

        let promisor_path = result.promisor_path.expect("promisor sidecar");
        assert_eq!(promisor_path.file_stem(), result.pack_path.file_stem());
        assert_eq!(
            promisor_path.extension().and_then(|ext| ext.to_str()),
            Some("promisor")
        );
        assert!(promisor_path.exists());
        assert_eq!(fs::read(&promisor_path).unwrap(), b"");
        assert!(result.pack_path.exists());
        assert!(result.index_path.exists());
        assert!(!db.loose().object_path(&oid).unwrap().exists());
        assert_eq!(db.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_objects_dir_uses_linked_worktree_common_dir() {
        let root = temp_root("git-rs-odb-common-dir");
        let common = root.join(".git");
        let admin = common.join("worktrees").join("linked");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();

        let common = fs::canonicalize(common).unwrap();
        assert_eq!(repository_common_dir(&admin), common);
        assert_eq!(repository_objects_dir(&admin), common.join("objects"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reachable_object_helpers_walk_graph_and_install_pack() {
        let root = temp_root("git-rs-reachable-pack");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"reachable payload\n".to_vec());
        let blob_oid = source.write_object(blob.clone()).unwrap();
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: b"payload.txt".to_vec(),
                    oid: blob_oid.clone(),
                }],
            }
            .write(),
        );
        let tree_oid = source.write_object(tree.clone()).unwrap();
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid.clone(),
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = source.write_object(commit.clone()).unwrap();

        let reachable =
            collect_reachable_object_ids(&source, format, std::iter::once(commit_oid.clone()))
                .unwrap();
        assert!(reachable.contains(&commit_oid));
        assert!(reachable.contains(&tree_oid));
        assert!(reachable.contains(&blob_oid));

        let install = install_reachable_pack(
            &source,
            &destination,
            format,
            std::iter::once(commit_oid.clone()),
        )
        .unwrap()
        .expect("reachable pack should be written");
        assert_eq!(install.object_ids.len(), 3);
        for (oid, object) in [
            (&commit_oid, &commit),
            (&tree_oid, &tree),
            (&blob_oid, &blob),
        ] {
            assert!(!destination.loose().object_path(oid).unwrap().exists());
            assert!(destination.contains(oid).unwrap());
            assert_eq!(destination.read_object(oid).unwrap(), *object);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reachable_object_helpers_respect_exclusions_and_duplicate_starts() {
        let root = temp_root("git-rs-reachable-exclusions");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"excluded payload\n".to_vec());
        let blob_oid = db.write_object(blob).unwrap();
        let tree = EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: b"payload.txt".to_vec(),
                    oid: blob_oid.clone(),
                }],
            }
            .write(),
        );
        let tree_oid = db.write_object(tree).unwrap();
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree_oid.clone(),
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"initial\n".to_vec(),
            }
            .write(),
        );
        let commit_oid = db.write_object(commit).unwrap();
        let excluded = HashSet::from([tree_oid.clone()]);

        let objects = collect_reachable_objects(
            &db,
            format,
            [commit_oid.clone(), commit_oid.clone()],
            &excluded,
        )
        .unwrap();

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id(format).unwrap(), commit_oid);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_reachable_pack_returns_raw_pack_and_respects_empty_exclusions() {
        let root = temp_root("git-rs-build-reachable-pack");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let object = EncodedObject::new(ObjectType::Blob, b"raw reachable pack\n".to_vec());
        let oid = db.write_object(object.clone()).unwrap();
        let pack = build_reachable_pack(&db, format, std::iter::once(oid.clone()), &HashSet::new())
            .unwrap()
            .expect("reachable pack should be built");
        assert!(pack.pack.starts_with(b"PACK"));
        assert_eq!(pack.entries.len(), 1);
        assert_eq!(pack.entries[0].oid, oid.clone());

        let excluded = HashSet::from([oid]);
        assert!(build_reachable_pack(
            &db,
            format,
            pack.entries.into_iter().map(|entry| entry.oid),
            &excluded
        )
        .unwrap()
        .is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reachable_object_helpers_follow_tags_and_report_missing_objects() {
        let root = temp_root("git-rs-reachable-tags");
        let git_dir = root.join("repo.git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"tagged payload\n".to_vec());
        let blob_oid = db.write_object(blob).unwrap();
        let tag = EncodedObject::new(
            ObjectType::Tag,
            Tag {
                object: blob_oid.clone(),
                object_type: ObjectType::Blob,
                name: b"v1".to_vec(),
                tagger: Some(b"Example <example@example.invalid> 0 +0000".to_vec()),
                message: b"tag message\n".to_vec(),
            }
            .write(),
        );
        let tag_oid = db.write_object(tag).unwrap();

        let reachable =
            collect_reachable_object_ids(&db, format, std::iter::once(tag_oid.clone())).unwrap();
        assert!(reachable.contains(&tag_oid));
        assert!(reachable.contains(&blob_oid));

        let missing =
            ObjectId::from_hex(format, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(matches!(
            collect_reachable_object_ids(&db, format, std::iter::once(missing)),
            Err(GitError::NotFound(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_reachable_pack_empty_starts_create_no_pack() {
        let root = temp_root("git-rs-reachable-empty");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);

        let result =
            install_reachable_pack(&source, &destination, format, Vec::<ObjectId>::new()).unwrap();

        assert!(result.is_none());
        assert!(!destination_git_dir.join("objects").join("pack").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_reachable_pack_excluding_skips_fully_excluded_starts() {
        let root = temp_root("git-rs-reachable-install-excluding");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha1;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"excluded install\n".to_vec());
        let oid = source.write_object(object).unwrap();
        let excluded = HashSet::from([oid.clone()]);

        let result = install_reachable_pack_excluding(
            &source,
            &destination,
            format,
            std::iter::once(oid),
            &excluded,
        )
        .unwrap();

        assert!(result.is_none());
        assert!(!destination_git_dir.join("objects").join("pack").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_reachable_pack_supports_sha256() {
        let root = temp_root("git-rs-reachable-pack-sha256");
        let source_git_dir = root.join("source.git");
        let destination_git_dir = root.join("destination.git");
        fs::create_dir_all(source_git_dir.join("objects")).unwrap();
        fs::create_dir_all(destination_git_dir.join("objects")).unwrap();
        let format = ObjectFormat::Sha256;
        let mut source = FileObjectDatabase::from_git_dir(&source_git_dir, format);
        let destination = FileObjectDatabase::from_git_dir(&destination_git_dir, format);
        let object = EncodedObject::new(ObjectType::Blob, b"sha256 reachable pack\n".to_vec());
        let oid = source.write_object(object.clone()).unwrap();

        let pack = build_reachable_pack(
            &source,
            format,
            std::iter::once(oid.clone()),
            &HashSet::new(),
        )
        .unwrap()
        .expect("sha256 reachable pack should be built");
        assert!(pack.pack.starts_with(b"PACK"));
        assert_eq!(pack.entries[0].oid, oid);

        let result =
            install_reachable_pack(&source, &destination, format, std::iter::once(oid.clone()))
                .unwrap()
                .expect("sha256 reachable pack should be written");

        assert_eq!(result.object_ids, vec![oid.clone()]);
        assert!(!destination.loose().object_path(&oid).unwrap().exists());
        assert_eq!(destination.read_object(&oid).unwrap(), object);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_helpers_accept_custom_raw_pack_installer() {
        #[derive(Default)]
        struct RecordingInstaller {
            packs: std::cell::RefCell<Vec<Vec<u8>>>,
            installed: std::cell::RefCell<Vec<ObjectId>>,
        }

        impl RawPackInstaller for RecordingInstaller {
            fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult> {
                self.packs.borrow_mut().push(pack_bytes.to_vec());
                let object_ids = self.installed.borrow().clone();
                Ok(RawPackInstallResult { object_ids })
            }
        }

        let format = ObjectFormat::Sha1;
        let mut source = ObjectDatabase::new(format);
        let object = EncodedObject::new(ObjectType::Blob, b"custom raw installer\n".to_vec());
        let oid = source.write_object(object).unwrap();
        let installer = RecordingInstaller::default();
        installer.installed.borrow_mut().push(oid.clone());

        let result = install_reachable_pack(&source, &installer, format, std::iter::once(oid))
            .unwrap()
            .expect("custom installer should receive pack");

        assert_eq!(result.object_ids, installer.installed.into_inner());
        let packs = installer.packs.into_inner();
        assert_eq!(packs.len(), 1);
        assert!(packs[0].starts_with(b"PACK"));
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
        assert_eq!(
            db.resolve_prefix(&second_oid.to_hex()[..8]).unwrap(),
            ObjectPrefixResolution::Unique(second_oid.clone())
        );
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
    fn install_bundle_pack_writes_pack_and_returns_refs() {
        let root = temp_root("git-rs-install-bundle-pack");
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let prerequisite_reader = ObjectDatabase::new(ObjectFormat::Sha1);
        let database = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = EncodedObject::new(ObjectType::Blob, b"bundle pack object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        let pack = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack.pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        let result = install_bundle_pack(&bundle, &prerequisite_reader, &database).unwrap();

        assert_eq!(result.written_objects, vec![oid.clone()]);
        assert_eq!(result.references, bundle.references);
        assert!(database.contains(&oid).unwrap());
        assert_eq!(database.read_object(&oid).unwrap(), object);
        assert!(!database.loose().object_path(&oid).unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unpack_packfile_objects_writes_sha256_pack_entries() {
        let mut writer = ObjectDatabase::new(ObjectFormat::Sha256);
        let object = EncodedObject::new(ObjectType::Blob, b"transport pack object\n".to_vec());
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
            .unwrap();

        let result =
            unpack_packfile_objects(&pack.pack, ObjectFormat::Sha256, &mut writer).unwrap();

        assert_eq!(result.written_objects, vec![oid.clone()]);
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
