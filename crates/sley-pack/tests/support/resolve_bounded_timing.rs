//! Opt-in bounded-path RSS/accounting run on the identical hot-family pack.
use sley_core::{CancelFlag, ObjectFormat};
use sley_pack::{ResolveLimits, ResolveMetrics, resolve_pack_with_metrics};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("pack path required")?;
    let pack = std::fs::read(path)?;
    let count = u32::from_be_bytes(pack[8..12].try_into()?);
    let limits = ResolveLimits {
        max_objects: 4_000_000,
        max_live_base_bytes: 512 * 1024 * 1024,
        max_in_flight_bytes: 1024 * 1024 * 1024,
        max_resolve_ops: (8 * u64::from(count)).max(1_000_000),
        ..ResolveLimits::default()
    };
    let metrics = ResolveMetrics::default();
    let start = std::time::Instant::now();
    let built = resolve_pack_with_metrics(
        &pack,
        ObjectFormat::Sha1,
        16,
        limits,
        CancelFlag::never(),
        &|_| Ok::<_, std::convert::Infallible>(()),
        &metrics,
    )?;
    assert_eq!(built.objects.len(), count as usize);
    assert!(metrics.peak_in_flight_bytes() <= limits.max_in_flight_bytes);
    assert!(metrics.peak_live_base_bytes() <= limits.max_live_base_bytes);
    assert_eq!(metrics.in_flight_bytes(), 0);
    assert_eq!(metrics.live_base_bytes(), 0);
    println!(
        "objects={count} seconds={:.6} flight_peak={} flight_gate={} live_peak={} live_gate={} flight_after={} live_after={} metadata={} ops={}",
        start.elapsed().as_secs_f64(),
        metrics.peak_in_flight_bytes(),
        limits.max_in_flight_bytes,
        metrics.peak_live_base_bytes(),
        limits.max_live_base_bytes,
        metrics.in_flight_bytes(),
        metrics.live_base_bytes(),
        metrics.peak_metadata_bytes(),
        metrics.resolve_ops()
    );
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status")?;
        let rss: usize = status
            .lines()
            .find_map(|s| s.strip_prefix("VmHWM:"))
            .ok_or("VmHWM unavailable")?
            .split_whitespace()
            .next()
            .ok_or("VmHWM empty")?
            .parse::<usize>()?
            * 1024;
        // 128 MiB covers worker/zlib state, allocator and index scratch. The
        // borrowed input and all per-object metadata have separate terms.
        let bound = limits.max_in_flight_bytes
            + limits.max_live_base_bytes
            + pack.len()
            + count as usize * 256
            + 128 * 1024 * 1024;
        println!("rss_peak={rss} bound={bound} fixed_slack=134217728");
        assert!(rss <= bound, "bounded work stealing exceeded RSS allowance");
    }
    Ok(())
}
