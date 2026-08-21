//! Real-pack throughput harness for parallel mmap index construction.

use sley_core::{CancelFlag, ObjectFormat};
use sley_pack::{PackIndex, PackIndexOptions};
use std::path::PathBuf;
use std::time::Instant;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pack-index-bench: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    let pack_path = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| "usage: pack-index-bench <pack> [threads]".to_string())?,
    );
    let threads = arguments
        .next()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("invalid thread count {value:?}"))
        })
        .transpose()?
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1)
        });
    if arguments.next().is_some() {
        return Err("usage: pack-index-bench <pack> [threads]".into());
    }

    let mapped = sley_mmap::MappedFile::open_pack(&pack_path).map_err(|error| error.to_string())?;
    let expected_index = std::fs::read(pack_path.with_extension("idx"))
        .map_err(|error| format!("read comparison index: {error}"))?;
    let started = Instant::now();
    let built = PackIndex::write_v2_for_pack_with_options(
        mapped.as_bytes(),
        ObjectFormat::Sha1,
        |_| Ok(None),
        PackIndexOptions::default().with_threads(threads),
        CancelFlag::never(),
        |_| {},
    )
    .map_err(|error| error.to_string())?;
    let elapsed = started.elapsed();
    if built.index != expected_index {
        return Err("generated index differs from the installed Git index".into());
    }
    let megabytes_per_second = mapped.len() as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    println!("pack_bytes={}", mapped.len());
    println!("objects={}", built.entries.len());
    println!("threads={threads}");
    println!("wall_seconds={:.6}", elapsed.as_secs_f64());
    println!("MB_per_second={megabytes_per_second:.3}");
    println!("index_byte_identical=true");
    Ok(())
}
