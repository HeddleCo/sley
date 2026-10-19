//! Positional streaming decode: <pack> <threads: 0=sequential> [window=1024].
//! Run fresh processes under /usr/bin/time for independent peak RSS samples.
use sley_core::{CancelFlag, GitError, ObjectFormat, StreamingDigest};
use sley_pack::{PackDecodeBudget, PackIndex, PackScan, ParallelScanOptions, ScanLimits};
use std::{path::PathBuf, sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().ok_or("missing pack path")?);
    let threads: usize = args.next().ok_or("missing threads")?.parse()?;
    let window = args
        .next()
        .map(|arg| arg.parse())
        .transpose()?
        .unwrap_or(1024);
    let index = PackIndex::parse(
        &std::fs::read(path.with_extension("idx"))?,
        ObjectFormat::Sha1,
    )?;
    let pack_bytes = std::fs::metadata(&path)?.len();
    let started = Instant::now();
    let scan = PackScan::new(
        std::fs::File::open(path)?,
        &index,
        ScanLimits::new(4095, 256 * 1024 * 1024, 256 * 1024 * 1024),
    )?;
    let plan = scan.plan(index.entries.iter().map(|entry| entry.oid))?;
    let budget = Arc::new(PackDecodeBudget::new(256 * 1024 * 1024));
    let mut digest = StreamingDigest::new(ObjectFormat::Sha1);
    let mut objects = 0u64;
    let mut decoded_bytes = 0u64;
    let mut consume = |object: sley_pack::PackScanObject| {
        objects += 1;
        decoded_bytes += object.object.body.len() as u64;
        digest.update(object.oid.as_bytes());
    };
    let lookup = |oid: &sley_core::ObjectId| Err(GitError::object_not_found(*oid).into());
    let stats = if threads == 0 {
        let mut cursor = plan.cursor(lookup, CancelFlag::never());
        for object in cursor.by_ref() {
            consume(object?);
        }
        cursor.stats()
    } else {
        let mut cursor = plan.parallel_cursor(
            lookup,
            CancelFlag::never(),
            ParallelScanOptions::new(threads, window),
            Arc::clone(&budget),
        );
        for object in cursor.by_ref() {
            consume(object?);
        }
        cursor.stats()
    };
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "threads={threads} window={window} objects={objects} pack_bytes={pack_bytes} decoded_bytes={decoded_bytes}"
    );
    println!(
        "seconds={seconds:.6} pack_MB_s={:.2} decoded_MB_s={:.2}",
        pack_bytes as f64 / seconds / 1e6,
        decoded_bytes as f64 / seconds / 1e6
    );
    println!(
        "entries_inflated={} peak_live_base_bytes={} peak_shared_bytes={} ordered_oid_digest={}",
        stats.entries_inflated,
        stats.peak_live_base_bytes,
        budget.peak_bytes(),
        digest.finalize()?
    );
    Ok(())
}
