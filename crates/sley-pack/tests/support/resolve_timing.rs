//! Standalone public-API example copied into each historical source export.
use sley_core::{CancelFlag, ObjectFormat, digest_bytes};
use sley_pack::{PackIndex, PackIndexOptions};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or("pack path required")?;
    let threads: usize = args.next().ok_or("thread count required")?.to_str().ok_or("threads UTF8")?.parse()?;
    let bytes = std::fs::read(path)?;
    let start = std::time::Instant::now();
    let mut ninety = None;
    let mut hundred = None;
    let built = PackIndex::write_v2_for_pack_with_options(
        &bytes, ObjectFormat::Sha1, |_| Ok(None),
        PackIndexOptions::default().with_threads(threads), CancelFlag::never(), |p| {
            if p.total_objects != 0 && p.completed_objects * 10 >= p.total_objects * 9 && ninety.is_none() {
                ninety = Some(start.elapsed());
            }
            if p.completed_objects == p.total_objects && hundred.is_none() { hundred = Some(start.elapsed()); }
        },
    )?;
    println!("seconds={:.6} objects={} idx_sha256={} tail={:.6}", start.elapsed().as_secs_f64(),
        built.objects.len(), digest_bytes(ObjectFormat::Sha256, &built.index)?,
        hundred.zip(ninety).map_or(0.0, |(end, begin)| end.saturating_sub(begin).as_secs_f64()));
    Ok(())
}
