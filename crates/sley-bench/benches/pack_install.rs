//! Benchmark `FileObjectDatabase::install_pack` on the deltified blob pack fixture.
//!
//! Also measures streaming raw-pack install with cooperative cancel:
//! - baseline `install_raw_pack_from_reader`
//! - same path with [`CancelFlag::never`] (should match baseline)
//! - mid-stream cancel correctness (unit-style, exercised under criterion)
//!
//! ```text
//! cargo bench -p sley-bench --bench pack_install -- --quick
//! cargo bench -p sley-bench --bench pack_install -- --test
//! ```

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use sley_bench::{FIXTURE_OBJECT_COUNT, build_blob_pack, create_pack_install_target};
use sley_core::{AtomicCancel, CancelFlag, GitError};
use sley_odb::{
    FileObjectDatabase, PackStreamProgress, RawPackInstallOptions, RawPackInstallResult,
    RawPackInstaller,
};
use sley_pack::PackWrite;
use std::io::Read;
use std::sync::OnceLock;

fn pack() -> &'static PackWrite {
    static PACK: OnceLock<PackWrite> = OnceLock::new();
    PACK.get_or_init(|| match build_blob_pack() {
        Ok(pack) => pack,
        Err(err) => panic!("blob pack fixture setup failed: {err}"),
    })
}

fn install_pack_fresh_repo(c: &mut Criterion) {
    let pack = pack();
    let mut group = c.benchmark_group("install_pack");
    group.throughput(Throughput::Elements(FIXTURE_OBJECT_COUNT as u64));

    group.bench_function("install_pack", |b| {
        b.iter(|| {
            let target = create_pack_install_target().expect("pack install target");
            let db = FileObjectDatabase::from_git_dir(&target.git_dir, target.format);
            let result = db.install_pack(std::hint::black_box(pack)).expect("install_pack");
            std::hint::black_box(result.object_ids.len())
        });
    });

    group.bench_function("install_raw_pack", |b| {
        b.iter(|| {
            let target = create_pack_install_target().expect("pack install target");
            let db = FileObjectDatabase::from_git_dir(&target.git_dir, target.format);
            let mut reader = std::hint::black_box(pack.pack.as_slice());
            let result = db
                .install_raw_pack_from_reader(&mut reader)
                .expect("install_raw_pack");
            std::hint::black_box(result.object_ids.len())
        });
    });

    // Never-cancel flag should optimize to the same path as the baseline
    // install_raw_pack_from_reader (CancelFlag::never is the default).
    group.bench_function("install_raw_pack_cancel_never", |b| {
        b.iter(|| {
            let target = create_pack_install_target().expect("pack install target");
            let db = FileObjectDatabase::from_git_dir(&target.git_dir, target.format);
            let mut reader = std::hint::black_box(pack.pack.as_slice());
            let result = db
                .install_raw_pack_from_reader_with_progress_and_cancel(
                    &mut reader,
                    RawPackInstallOptions::default(),
                    CancelFlag::never(),
                    |_| {},
                )
                .expect("install_raw_pack cancel never");
            std::hint::black_box(result.object_ids.len())
        });
    });

    group.finish();
}

/// Unit-style mid-stream cancel: trip [`AtomicCancel`] after the first object
/// and ensure install returns [`GitError::Cancelled`] without leaving a pack.
///
/// Run under criterion so `--test` / `--quick` compile and exercise the path
/// without measuring a full install loop.
fn cancel_mid_stream_install(c: &mut Criterion) {
    let pack = pack();
    let mut group = c.benchmark_group("install_pack_cancel");
    // Correctness micro-harness: one iteration is enough under --test.
    group.sample_size(10);
    group.bench_function("cancel_after_first_object", |b| {
        b.iter(|| {
            let target = create_pack_install_target().expect("pack install target");
            let db = FileObjectDatabase::from_git_dir(&target.git_dir, target.format);
            let source = AtomicCancel::new();
            let installer = CancelOnProgressInstaller {
                inner: &db,
                source: &source,
                saw_object: std::cell::Cell::new(false),
            };
            let mut reader = std::hint::black_box(pack.pack.as_slice());
            let err = installer
                .install_raw_pack_from_reader_with_progress_and_cancel(
                    &mut reader,
                    RawPackInstallOptions::default(),
                    CancelFlag::new(&source),
                    |_| {},
                )
                .expect_err("mid-stream cancel should fail");
            assert!(
                installer.saw_object.get(),
                "progress should report at least one object before cancel"
            );
            assert!(
                matches!(err, GitError::Cancelled),
                "expected Cancelled, got {err:?}"
            );
            let pack_dir = target.git_dir.join("objects").join("pack");
            let installed = std::fs::read_dir(&pack_dir)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok())
                        .filter(|entry| {
                            entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack")
                        })
                        .count()
                })
                .unwrap_or_default();
            assert_eq!(installed, 0, "cancelled install must not leave pack files");
            std::hint::black_box(err);
        });
    });
    group.finish();
}

/// Trip cancel from pack-indexer progress after the first object so the
/// cooperative path observes mid-stream cancel.
struct CancelOnProgressInstaller<'a> {
    inner: &'a FileObjectDatabase,
    source: &'a AtomicCancel,
    saw_object: std::cell::Cell<bool>,
}

impl RawPackInstaller for CancelOnProgressInstaller<'_> {
    fn install_raw_pack_from_reader_with_options<R>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
    ) -> sley_core::Result<RawPackInstallResult>
    where
        R: Read,
    {
        RawPackInstaller::install_raw_pack_from_reader_with_options(self.inner, reader, options)
    }

    fn install_raw_pack_from_reader_with_progress_and_cancel<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        cancel: CancelFlag<'_>,
        mut progress: F,
    ) -> sley_core::Result<RawPackInstallResult>
    where
        R: Read,
        F: FnMut(PackStreamProgress),
    {
        RawPackInstaller::install_raw_pack_from_reader_with_progress_and_cancel(
            self.inner,
            reader,
            options,
            cancel,
            |p| {
                if p.received_objects >= 1 {
                    self.saw_object.set(true);
                    self.source.cancel();
                }
                progress(p);
            },
        )
    }
}

criterion_group!(benches, install_pack_fresh_repo, cancel_mid_stream_install);
criterion_main!(benches);
