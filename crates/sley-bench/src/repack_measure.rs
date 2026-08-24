//! Deterministic measurements and regression limits for repack writers.
//!
//! [`measure_pack_writer`] is deliberately a pack-layer microcontract. The
//! repository-shaped [`measure_repository_repack`] exercises both the legacy
//! in-memory ODB repack and the prepared, file-backed ODB repack.

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_mmap::MappedFile;
use sley_object::{BString, Commit, EncodedObject, ObjectType, Tree, TreeEntry};
use sley_odb::{
    FileObjectDatabase, PreparedRepackOutcome, PreparedRepackResult, RepackOptions,
    prepare_repack_reachable_objects_with_options, repack_reachable_objects_with_options,
};
use sley_pack::{PackFile, PackIndex, PackWriteLimits, PackWriteOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The production ODB streaming threshold is 4,096 objects. Two graph objects
/// above this blob population keep the fixture on the metadata-first path.
pub const REPOSITORY_REPACK_BLOB_COUNT: usize = 4_096;
pub const REPOSITORY_REPACK_OBJECT_COUNT: usize = REPOSITORY_REPACK_BLOB_COUNT + 2;
pub const REPACK_QUALITY_FLOOR_PERCENT: u64 = 95;
pub const REPOSITORY_BASELINE_PACK_BYTES: u64 = 192_461;
pub const REPOSITORY_BASELINE_INDEX_BYTES: u64 = 115_816;
pub const REPOSITORY_BASELINE_DELTA_COUNT: u32 = 4_084;
pub const REPOSITORY_BASELINE_PREPARATION_READS: u64 = 8_196;

/// Material counters for the source-to-writer pack-layer microcontract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackWriterMeasurements {
    pub object_count: u32,
    /// Calls made across the pack writer's object-source boundary.
    pub object_reads: u64,
    /// Sum of decoded object body bytes returned to the pack writer.
    pub decoded_bytes: u64,
    /// Logical high-water mark charged by `PackWriteLimits`. This excludes the
    /// output sink, index, zlib buffers, allocator slack, and source storage.
    pub peak_charged_writer_bytes: u64,
    pub pack_size: u64,
    pub delta_count: u32,
    pub max_delta_depth: u32,
}

/// Upper/lower bounds for the deterministic pack-layer microcontract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackWriterRegressionContract {
    pub expected_object_count: u32,
    pub max_object_reads: u64,
    pub max_decoded_bytes: u64,
    pub max_peak_charged_writer_bytes: u64,
    pub max_pack_size: u64,
    pub min_delta_count: u32,
    pub max_delta_depth: u32,
}

impl PackWriterRegressionContract {
    pub fn check(self, measured: PackWriterMeasurements) -> Result<()> {
        check_equal(
            "object count",
            u64::from(measured.object_count),
            u64::from(self.expected_object_count),
        )?;
        check_max("object reads", measured.object_reads, self.max_object_reads)?;
        check_max(
            "decoded bytes",
            measured.decoded_bytes,
            self.max_decoded_bytes,
        )?;
        check_max(
            "peak charged writer bytes",
            measured.peak_charged_writer_bytes,
            self.max_peak_charged_writer_bytes,
        )?;
        check_max("pack size", measured.pack_size, self.max_pack_size)?;
        check_min(
            "delta count",
            u64::from(measured.delta_count),
            u64::from(self.min_delta_count),
        )?;
        check_max(
            "maximum delta depth",
            u64::from(measured.max_delta_depth),
            u64::from(self.max_delta_depth),
        )
    }
}

/// Measure only the public one-shot pack source-to-writer API.
///
/// This intentionally uses a `Vec<u8>` so it can verify the microcontract's
/// delta shape. It is not evidence for end-to-end ODB repack residency; use
/// [`measure_repository_repack`] for that.
pub fn measure_pack_writer<I, F>(
    selected_objects: I,
    object_count: u32,
    format: ObjectFormat,
    options: &PackWriteOptions,
    limits: PackWriteLimits,
    mut read_object: F,
) -> Result<PackWriterMeasurements>
where
    I: IntoIterator<Item = ObjectId>,
    F: FnMut(&ObjectId) -> Result<Arc<EncodedObject>>,
{
    let mut object_reads = 0u64;
    let mut decoded_bytes = 0u64;
    let mut pack = Vec::new();
    let summary = PackFile::write_packed_from_source_to_writer(
        selected_objects,
        object_count,
        format,
        options,
        limits,
        |oid| {
            object_reads = object_reads.saturating_add(1);
            let object = read_object(oid)?;
            decoded_bytes = decoded_bytes.saturating_add(object.body.len() as u64);
            Ok(object)
        },
        &mut pack,
    )?;
    let verified = PackFile::verify_pack_stats(&pack, format)?;
    let max_delta_depth = maximum_delta_depth(&verified.objects);
    if summary.pack_size != pack.len() as u64 {
        return Err(GitError::InvalidFormat(format!(
            "streaming writer reported {} pack bytes but emitted {}",
            summary.pack_size,
            pack.len()
        )));
    }
    if verified.objects.len() != object_count as usize {
        return Err(GitError::InvalidFormat(format!(
            "verified {} objects after writing {object_count}",
            verified.objects.len()
        )));
    }
    Ok(PackWriterMeasurements {
        object_count,
        object_reads,
        decoded_bytes,
        peak_charged_writer_bytes: summary.peak_working_set_bytes,
        pack_size: summary.pack_size,
        delta_count: summary.delta_count,
        max_delta_depth,
    })
}

/// Repository fixture with two seed packs, a commit/tree graph, and one blob
/// reachable under two distinct path names.
#[derive(Debug)]
pub struct RepositoryRepackFixture {
    repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub format: ObjectFormat,
    pub root: ObjectId,
}

impl RepositoryRepackFixture {
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

impl Drop for RepositoryRepackFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.repo_root);
    }
}

/// Build a mixed repository fixture that crosses the production streaming
/// threshold without thousands of loose-object filesystem operations.
pub fn create_repository_repack_fixture() -> Result<RepositoryRepackFixture> {
    let format = ObjectFormat::Sha1;
    let repo_root = crate::unique_temp_dir("sley-bench-prepared-repack");
    let git_dir = repo_root.join(".git");
    crate::init_minimal_repo(&git_dir)?;

    let mut objects = Vec::with_capacity(REPOSITORY_REPACK_OBJECT_COUNT);
    let mut entries = Vec::with_capacity(REPOSITORY_REPACK_BLOB_COUNT + 1);
    let mut first_blob = None;
    for index in 0..REPOSITORY_REPACK_BLOB_COUNT {
        let mut body = vec![b'r'; 256];
        body.extend_from_slice(format!("variant-{index:04}\n").as_bytes());
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = object.object_id(format)?;
        if first_blob.is_none() {
            first_blob = Some(oid);
        }
        entries.push(TreeEntry {
            mode: 0o100644,
            name: BString::from(format!("file-{index:04}.txt").into_bytes()),
            oid,
        });
        objects.push(object);
    }
    entries.push(TreeEntry {
        mode: 0o100644,
        name: BString::from(b"shared-copy.txt"),
        oid: first_blob.ok_or_else(|| GitError::InvalidFormat("fixture has no blob".into()))?,
    });
    let tree = EncodedObject::new(ObjectType::Tree, Tree { entries }.write());
    let tree_oid = tree.object_id(format)?;
    objects.push(tree);
    let identity = b"Benchmark User <bench@example.invalid> 1700000000 +0000".to_vec();
    let commit = EncodedObject::new(
        ObjectType::Commit,
        Commit {
            tree: tree_oid,
            parents: Vec::new(),
            author: identity.clone(),
            committer: identity,
            encoding: None,
            message: b"prepared repack fixture\n".to_vec(),
        }
        .write(),
    );
    let root = commit.object_id(format)?;
    objects.push(commit);

    let midpoint = objects.len() / 2;
    let seed_options = PackWriteOptions::new().with_depth(0).with_reorder(false);
    let first = PackFile::write_packed_with_options(&objects[..midpoint], format, &seed_options)?;
    let second = PackFile::write_packed_with_options(&objects[midpoint..], format, &seed_options)?;
    let database = FileObjectDatabase::from_git_dir(&git_dir, format);
    database.install_pack(&first)?;
    database.install_pack(&second)?;

    Ok(RepositoryRepackFixture {
        repo_root,
        git_dir,
        format,
        root,
    })
}

/// End-to-end comparison of legacy in-memory and prepared file-backed repack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryRepackMeasurements {
    pub object_count: u32,
    pub preparation_body_reads: u64,
    /// Pack payload retained in a heap buffer by the prepared result.
    pub prepared_resident_pack_output_bytes: u64,
    pub staged_pack_size: u64,
    pub prepared_index_bytes: u64,
    pub prepared_delta_count: u32,
    pub prepared_max_delta_depth: u32,
    pub legacy_resident_pack_output_bytes: u64,
    pub legacy_pack_size: u64,
    pub legacy_index_bytes: u64,
    pub legacy_delta_count: u32,
    pub legacy_max_delta_depth: u32,
    pub checksum_identical: bool,
    pub index_identical: bool,
}

/// Relative end-to-end contract. Pack size and delta density must retain at
/// least `minimum_quality_percent` of the legacy baseline from the same fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryRepackRegressionContract {
    pub expected_object_count: u32,
    pub max_preparation_body_reads: u64,
    pub max_staged_pack_size: u64,
    pub expected_index_bytes: u64,
    pub min_prepared_delta_count: u32,
    pub minimum_quality_percent: u64,
    pub max_delta_depth: u32,
}

impl RepositoryRepackRegressionContract {
    pub fn check(self, measured: RepositoryRepackMeasurements) -> Result<()> {
        check_equal(
            "repository object count",
            u64::from(measured.object_count),
            u64::from(self.expected_object_count),
        )?;
        check_max(
            "preparation body reads",
            measured.preparation_body_reads,
            self.max_preparation_body_reads,
        )?;
        check_equal(
            "prepared resident pack output bytes",
            measured.prepared_resident_pack_output_bytes,
            0,
        )?;
        check_max(
            "staged pack size",
            measured.staged_pack_size,
            self.max_staged_pack_size,
        )?;
        check_equal(
            "prepared index bytes",
            measured.prepared_index_bytes,
            self.expected_index_bytes,
        )?;
        check_min(
            "prepared delta count",
            u64::from(measured.prepared_delta_count),
            u64::from(self.min_prepared_delta_count),
        )?;
        if !measured.checksum_identical {
            return Err(regression("prepared checksum differs from legacy".into()));
        }
        if !measured.index_identical {
            return Err(regression("prepared index differs from legacy".into()));
        }
        check_quality_floor(
            "pack-size quality",
            measured.legacy_pack_size,
            measured.staged_pack_size,
            self.minimum_quality_percent,
        )?;
        check_retained_fraction(
            "delta density",
            u64::from(measured.prepared_delta_count),
            u64::from(measured.legacy_delta_count),
            self.minimum_quality_percent,
        )?;
        check_max(
            "prepared maximum delta depth",
            u64::from(measured.prepared_max_delta_depth),
            u64::from(self.max_delta_depth),
        )
    }
}

pub fn legacy_fixture_repack(fixture: &RepositoryRepackFixture) -> Result<sley_odb::RepackResult> {
    repack_reachable_objects_with_options(
        &fixture.git_dir,
        fixture.format,
        &[fixture.root],
        &RepackOptions::default(),
    )?
    .ok_or_else(|| GitError::InvalidFormat("legacy fixture repack was empty".into()))
}

pub fn prepare_fixture_repack(fixture: &RepositoryRepackFixture) -> Result<PreparedRepackResult> {
    match prepare_repack_reachable_objects_with_options(
        &fixture.git_dir,
        fixture.format,
        &[fixture.root],
        &RepackOptions::default(),
    )? {
        PreparedRepackOutcome::Prepared(prepared) => Ok(prepared),
        PreparedRepackOutcome::Empty => Err(GitError::InvalidFormat(
            "prepared fixture repack was empty".into(),
        )),
        _ => Err(GitError::InvalidFormat(
            "fixture did not produce a prepared repack".into(),
        )),
    }
}

/// Prepare both implementations, then verify and compare their completed
/// outputs. Verification is deliberately outside Criterion timed bodies.
pub fn measure_repository_repack(
    fixture: &RepositoryRepackFixture,
) -> Result<RepositoryRepackMeasurements> {
    let legacy = legacy_fixture_repack(fixture)?;
    let prepared = prepare_fixture_repack(fixture)?;
    let stage = prepared
        .staged_pack_path()
        .ok_or_else(|| GitError::InvalidPath("prepared result has no staging path".into()))?;
    let mapped_path = stage.with_extension("bench.pack");
    fs::hard_link(stage, &mapped_path)?;
    let prepared_analysis = (|| -> Result<_> {
        let mapped = MappedFile::open_pack(&mapped_path)?;
        let verified = PackFile::verify_pack_stats(mapped.as_bytes(), fixture.format)?;
        let index = PackIndex::write_v2_for_pack(mapped.as_bytes(), fixture.format)?;
        Ok((mapped.len() as u64, verified, index))
    })();
    let _ = fs::remove_file(&mapped_path);
    let (staged_pack_size, prepared_verified, prepared_index) = prepared_analysis?;
    let legacy_verified = PackFile::verify_pack_stats(&legacy.pack, fixture.format)?;
    let prepared_delta_count = delta_count(&prepared_verified.objects);
    let legacy_delta_count = delta_count(&legacy_verified.objects);
    let object_count = u32::try_from(prepared.object_count())
        .map_err(|_| GitError::InvalidFormat("fixture has too many objects".into()))?;

    Ok(RepositoryRepackMeasurements {
        object_count,
        preparation_body_reads: prepared.preparation_body_reads(),
        prepared_resident_pack_output_bytes: 0,
        staged_pack_size,
        prepared_index_bytes: prepared_index.index.len() as u64,
        prepared_delta_count,
        prepared_max_delta_depth: maximum_delta_depth(&prepared_verified.objects),
        legacy_resident_pack_output_bytes: legacy.pack.capacity() as u64,
        legacy_pack_size: legacy.pack.len() as u64,
        legacy_index_bytes: legacy.idx.len() as u64,
        legacy_delta_count,
        legacy_max_delta_depth: maximum_delta_depth(&legacy_verified.objects),
        checksum_identical: prepared_verified.checksum == legacy_verified.checksum,
        index_identical: prepared_index.index == legacy.idx,
    })
}

fn maximum_delta_depth(objects: &[sley_pack::PackVerifyStat]) -> u32 {
    objects
        .iter()
        .map(|object| object.delta_depth)
        .max()
        .unwrap_or(0)
}

fn delta_count(objects: &[sley_pack::PackVerifyStat]) -> u32 {
    objects
        .iter()
        .filter(|object| object.delta_depth > 0)
        .count() as u32
}

fn check_equal(label: &str, actual: u64, expected: u64) -> Result<()> {
    if actual != expected {
        return Err(regression(format!(
            "{label} {actual} does not equal expected {expected}"
        )));
    }
    Ok(())
}

fn check_max(label: &str, actual: u64, maximum: u64) -> Result<()> {
    if actual > maximum {
        return Err(regression(format!(
            "{label} {actual} exceeds maximum {maximum}"
        )));
    }
    Ok(())
}

fn check_min(label: &str, actual: u64, minimum: u64) -> Result<()> {
    if actual < minimum {
        return Err(regression(format!(
            "{label} {actual} is below required minimum {minimum}"
        )));
    }
    Ok(())
}

fn check_quality_floor(label: &str, baseline: u64, candidate: u64, percent: u64) -> Result<()> {
    if u128::from(candidate) * u128::from(percent) > u128::from(baseline) * 100 {
        return Err(regression(format!(
            "{label} candidate {candidate} retains less than {percent}% of baseline {baseline}"
        )));
    }
    Ok(())
}

fn check_retained_fraction(label: &str, candidate: u64, baseline: u64, percent: u64) -> Result<()> {
    if u128::from(candidate) * 100 < u128::from(baseline) * u128::from(percent) {
        return Err(regression(format!(
            "{label} candidate {candidate} retains less than {percent}% of baseline {baseline}"
        )));
    }
    Ok(())
}

fn regression(message: String) -> GitError {
    GitError::Command(format!("repack measurement regression: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ByteBudget;
    use std::collections::HashMap;

    #[test]
    fn deterministic_pack_writer_satisfies_material_contract() {
        let format = ObjectFormat::Sha1;
        let objects = (0..32u32)
            .map(|index| {
                let mut body = vec![b'x'; 4 * 1024];
                body.extend_from_slice(&index.to_le_bytes());
                EncodedObject::new(ObjectType::Blob, body)
            })
            .collect::<Vec<_>>();
        let decoded_bytes = objects.iter().map(|object| object.body.len() as u64).sum();
        let mut ids = Vec::with_capacity(objects.len());
        let mut by_id = HashMap::with_capacity(objects.len());
        for object in objects {
            let oid = object.object_id(format).expect("fixture object id");
            ids.push(oid);
            by_id.insert(oid, Arc::new(object));
        }
        let options = PackWriteOptions::new()
            .with_window(8)
            .with_depth(8)
            .with_reorder(false);
        let limits = PackWriteLimits::new()
            .with_compression_working_set(ByteBudget::new(32 * 1024))
            .with_delta_base(ByteBudget::new(64 * 1024))
            .with_decoded_object(ByteBudget::new(8 * 1024));
        let measured = measure_pack_writer(
            ids.iter().copied(),
            ids.len() as u32,
            format,
            &options,
            limits,
            |oid| {
                by_id
                    .get(oid)
                    .cloned()
                    .ok_or_else(|| GitError::not_found(format!("fixture object {oid}")))
            },
        )
        .expect("measure fixture pack writer");
        let contract = PackWriterRegressionContract {
            expected_object_count: ids.len() as u32,
            max_object_reads: ids.len() as u64,
            max_decoded_bytes: decoded_bytes,
            max_peak_charged_writer_bytes: 128 * 1024,
            max_pack_size: 16 * 1024,
            min_delta_count: 1,
            max_delta_depth: options.depth as u32,
        };
        contract.check(measured).expect("material contract");

        let regressions = [
            PackWriterMeasurements {
                object_count: measured.object_count + 1,
                ..measured
            },
            PackWriterMeasurements {
                object_reads: contract.max_object_reads + 1,
                ..measured
            },
            PackWriterMeasurements {
                decoded_bytes: contract.max_decoded_bytes + 1,
                ..measured
            },
            PackWriterMeasurements {
                peak_charged_writer_bytes: contract.max_peak_charged_writer_bytes + 1,
                ..measured
            },
            PackWriterMeasurements {
                pack_size: contract.max_pack_size + 1,
                ..measured
            },
            PackWriterMeasurements {
                delta_count: contract.min_delta_count - 1,
                ..measured
            },
            PackWriterMeasurements {
                max_delta_depth: contract.max_delta_depth + 1,
                ..measured
            },
        ];
        for regression in regressions {
            assert!(
                contract.check(regression).is_err(),
                "each material regression bound must be load-bearing: {regression:?}"
            );
        }
    }

    #[test]
    fn prepared_repository_repack_retains_legacy_quality_without_pack_residency() {
        let fixture = create_repository_repack_fixture().expect("repository fixture");
        let measured = measure_repository_repack(&fixture).expect("measure repository repack");
        let contract = RepositoryRepackRegressionContract {
            expected_object_count: REPOSITORY_REPACK_OBJECT_COUNT as u32,
            max_preparation_body_reads: REPOSITORY_BASELINE_PREPARATION_READS,
            max_staged_pack_size: REPOSITORY_BASELINE_PACK_BYTES * 100 / 95 + 1,
            expected_index_bytes: REPOSITORY_BASELINE_INDEX_BYTES,
            min_prepared_delta_count: (u64::from(REPOSITORY_BASELINE_DELTA_COUNT) * 95)
                .div_ceil(100) as u32,
            minimum_quality_percent: REPACK_QUALITY_FLOOR_PERCENT,
            max_delta_depth: 50,
        };
        contract.check(measured).expect("prepared repack contract");
        assert!(measured.legacy_resident_pack_output_bytes > 0);
        assert_eq!(measured.prepared_resident_pack_output_bytes, 0);

        let mut wrong_count = measured;
        wrong_count.object_count += 1;
        let mut extra_read = measured;
        extra_read.preparation_body_reads = contract.max_preparation_body_reads + 1;
        let mut no_file_backing = measured;
        no_file_backing.prepared_resident_pack_output_bytes = 1;
        let mut above_recorded_pack_baseline = measured;
        above_recorded_pack_baseline.staged_pack_size = contract.max_staged_pack_size + 1;
        above_recorded_pack_baseline.legacy_pack_size =
            above_recorded_pack_baseline.staged_pack_size;
        let mut wrong_index_size = measured;
        wrong_index_size.prepared_index_bytes = contract.expected_index_bytes + 1;
        let mut below_recorded_delta_baseline = measured;
        below_recorded_delta_baseline.prepared_delta_count = contract.min_prepared_delta_count - 1;
        below_recorded_delta_baseline.legacy_delta_count =
            below_recorded_delta_baseline.prepared_delta_count;
        let mut checksum_drift = measured;
        checksum_drift.checksum_identical = false;
        let mut index_drift = measured;
        index_drift.index_identical = false;
        let mut poor_relative_pack_quality = measured;
        poor_relative_pack_quality.legacy_pack_size = measured.staged_pack_size * 94 / 100;
        let mut poor_relative_delta_density = measured;
        poor_relative_delta_density.legacy_delta_count =
            (u64::from(measured.prepared_delta_count) * 100).div_ceil(94) as u32;
        let mut excessive_depth = measured;
        excessive_depth.prepared_max_delta_depth = contract.max_delta_depth + 1;

        for (label, regression) in [
            ("object count", wrong_count),
            ("preparation reads", extra_read),
            ("pack output residency", no_file_backing),
            ("recorded pack baseline", above_recorded_pack_baseline),
            ("index size", wrong_index_size),
            ("recorded delta baseline", below_recorded_delta_baseline),
            ("checksum identity", checksum_drift),
            ("index identity", index_drift),
            ("relative pack quality", poor_relative_pack_quality),
            ("relative delta density", poor_relative_delta_density),
            ("delta depth", excessive_depth),
        ] {
            assert!(
                contract.check(regression).is_err(),
                "{label} regression bound must be load-bearing: {regression:?}"
            );
        }
    }
}
