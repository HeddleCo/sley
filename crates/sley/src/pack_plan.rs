//! Reachable-pack planning facade for embedders.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sley_object::ObjectType;
use sley_odb::ObjectReader;
use sley_pack::{PackFile, PackWriteOptions, PackWriteSummary};

use crate::{GitError, ObjectFormat, ObjectId, Repository, Result};

/// Builder for a stable reachable-pack plan.
#[derive(Debug)]
pub struct ReachablePackPlanBuilder<'repo> {
    repo: &'repo Repository,
    roots: Vec<ObjectId>,
    excluded: HashSet<ObjectId>,
    options: PackWriteOptions,
}

/// A stable selection of reachable objects plus pack write options.
#[derive(Debug, Clone)]
pub struct ReachablePackPlan<'repo> {
    repo: &'repo Repository,
    object_ids: Vec<ObjectId>,
    format: ObjectFormat,
    options: PackWriteOptions,
}

/// Summary of a streamed or prepared pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachablePackSummary {
    pub checksum: ObjectId,
    pub object_count: usize,
    pub delta_count: u32,
    pub pack_size: u64,
}

/// A pack prepared in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedReachablePack {
    pub pack: Vec<u8>,
    pub index: Vec<u8>,
    pub summary: ReachablePackSummary,
}

/// A pack prepared on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedReachablePackFile {
    pub pack_path: PathBuf,
    pub index: Vec<u8>,
    pub summary: ReachablePackSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReachablePackObjectMeta {
    oid: ObjectId,
    object_type: ObjectType,
    size: u64,
}

impl Repository {
    /// Start planning a reachable pack from this repository.
    pub fn reachable_pack_plan(&self) -> ReachablePackPlanBuilder<'_> {
        ReachablePackPlanBuilder {
            repo: self,
            roots: Vec::new(),
            excluded: HashSet::new(),
            options: PackWriteOptions::new(),
        }
    }
}

impl<'repo> ReachablePackPlanBuilder<'repo> {
    pub fn root(mut self, root: ObjectId) -> Self {
        self.roots.push(root);
        self
    }

    pub fn roots<I>(mut self, roots: I) -> Self
    where
        I: IntoIterator<Item = ObjectId>,
    {
        self.roots.extend(roots);
        self
    }

    pub fn exclude(mut self, oid: ObjectId) -> Self {
        self.excluded.insert(oid);
        self
    }

    pub fn exclusions<I>(mut self, excluded: I) -> Self
    where
        I: IntoIterator<Item = ObjectId>,
    {
        self.excluded.extend(excluded);
        self
    }

    pub fn pack_options(mut self, options: PackWriteOptions) -> Self {
        self.options = options;
        self
    }

    /// Resolve roots/exclusions once and freeze the object order.
    pub fn build(self) -> Result<Option<ReachablePackPlan<'repo>>> {
        let format = self.repo.object_format();
        let objects = self.repo.objects();
        let reachable = sley_odb::collect_reachable_object_ids_excluding(
            objects.as_ref(),
            format,
            self.roots,
            &self.excluded,
        )?;
        if reachable.is_empty() {
            return Ok(None);
        }
        let mut metadata = Vec::with_capacity(reachable.len());
        for oid in reachable {
            let (object_type, size) = match self.repo.read_object_header(&oid)? {
                Some(header) => header,
                None => {
                    let object = self.repo.read_object(&oid)?;
                    (object.object_type, object.body.len() as u64)
                }
            };
            metadata.push(ReachablePackObjectMeta {
                oid,
                object_type,
                size,
            });
        }
        sort_pack_metadata(&mut metadata);
        Ok(Some(ReachablePackPlan {
            repo: self.repo,
            object_ids: metadata.into_iter().map(|meta| meta.oid).collect(),
            format,
            options: self.options,
        }))
    }
}

impl ReachablePackPlan<'_> {
    pub fn object_ids(&self) -> &[ObjectId] {
        &self.object_ids
    }

    pub fn object_count(&self) -> usize {
        self.object_ids.len()
    }

    pub fn object_format(&self) -> ObjectFormat {
        self.format
    }

    pub fn pack_options(&self) -> &PackWriteOptions {
        &self.options
    }

    /// Stream this exact plan to `writer`.
    pub fn stream_to<W>(&self, writer: &mut W) -> Result<ReachablePackSummary>
    where
        W: Write,
    {
        if self.object_ids.is_empty() {
            return Err(GitError::Unsupported(
                "empty reachable pack plan cannot be streamed".into(),
            ));
        }
        let objects = self.repo.objects();
        let summary = PackFile::write_packed_from_source_to_writer(
            &self.object_ids,
            self.format,
            &self.options,
            |oid| objects.read_object(oid),
            writer,
        )?;
        Ok(reachable_pack_summary(&summary))
    }

    /// Prepare this exact plan in memory, computing size/checksum without a
    /// second compression pass.
    pub fn prepare_to_memory(&self) -> Result<PreparedReachablePack> {
        let mut pack = Vec::new();
        let objects = self.repo.objects();
        let summary = PackFile::write_packed_from_source_to_writer(
            &self.object_ids,
            self.format,
            &self.options,
            |oid| objects.read_object(oid),
            &mut pack,
        )?;
        Ok(PreparedReachablePack {
            pack,
            index: summary.index.clone(),
            summary: reachable_pack_summary(&summary),
        })
    }

    /// Prepare this exact plan on disk, computing size/checksum without a
    /// second compression pass.
    pub fn prepare_to_file(
        &self,
        pack_path: impl AsRef<Path>,
    ) -> Result<PreparedReachablePackFile> {
        let pack_path = pack_path.as_ref();
        if let Some(parent) = pack_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(pack_path)?;
        let objects = self.repo.objects();
        let summary = PackFile::write_packed_from_source_to_writer(
            &self.object_ids,
            self.format,
            &self.options,
            |oid| objects.read_object(oid),
            &mut file,
        )?;
        file.sync_all()?;
        Ok(PreparedReachablePackFile {
            pack_path: pack_path.to_path_buf(),
            index: summary.index.clone(),
            summary: reachable_pack_summary(&summary),
        })
    }
}

fn reachable_pack_summary(summary: &PackWriteSummary) -> ReachablePackSummary {
    ReachablePackSummary {
        checksum: summary.checksum,
        object_count: summary.entries.len(),
        delta_count: summary.delta_count,
        pack_size: summary.pack_size,
    }
}

fn sort_pack_metadata(metadata: &mut [ReachablePackObjectMeta]) {
    metadata.sort_by(|left, right| {
        reachable_pack_type_rank(left.object_type)
            .cmp(&reachable_pack_type_rank(right.object_type))
            .then_with(|| right.size.cmp(&left.size))
            .then_with(|| left.oid.as_bytes().cmp(right.oid.as_bytes()))
    });
}

fn reachable_pack_type_rank(object_type: ObjectType) -> u8 {
    match object_type {
        ObjectType::Commit => 0,
        ObjectType::Tag => 1,
        ObjectType::Tree => 2,
        ObjectType::Blob => 3,
    }
}
