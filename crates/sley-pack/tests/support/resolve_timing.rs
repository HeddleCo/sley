//! Standalone example copied into each source export by resolve_timing.py.
use sley_core::{CancelFlag, ObjectFormat, digest_bytes};
use sley_pack::{PackIndex, PackIndexOptions};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("pack path required")?;
    let bytes = std::fs::read(path)?;
    let start = std::time::Instant::now();
    let built = PackIndex::write_v2_for_pack_with_options(
        &bytes, ObjectFormat::Sha1, |_| Ok(None),
        PackIndexOptions::default().with_threads(16), CancelFlag::never(), |_| {},
    )?;
    println!("seconds={:.6} objects={} idx_sha256={}", start.elapsed().as_secs_f64(),
        built.objects.len(), digest_bytes(ObjectFormat::Sha256, &built.index)?);
    Ok(())
}
