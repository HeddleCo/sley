//! Benchmark `FileObjectDatabase::install_pack` on the deltified blob pack fixture.
//!
//! ```text
//! cargo bench -p sley-bench --bench pack_install -- --quick
//! ```

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use sley_bench::{FIXTURE_OBJECT_COUNT, build_blob_pack, create_pack_install_target};
use sley_odb::FileObjectDatabase;
use sley_pack::PackWrite;
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
            let result = db.install_pack(black_box(pack)).expect("install_pack");
            black_box(result.object_ids.len())
        });
    });

    group.bench_function("install_raw_pack", |b| {
        b.iter(|| {
            let target = create_pack_install_target().expect("pack install target");
            let db = FileObjectDatabase::from_git_dir(&target.git_dir, target.format);
            let result = db
                .install_raw_pack(black_box(&pack.pack))
                .expect("install_raw_pack");
            black_box(result.object_ids.len())
        });
    });

    group.finish();
}

criterion_group!(benches, install_pack_fresh_repo);
criterion_main!(benches);
