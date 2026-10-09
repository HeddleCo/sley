//! Compare scanning a real indexed pack with uncached random reads.
//!
//! Run each mode in a fresh process under `/usr/bin/time -v` to measure peak
//! RSS independently: `pack-scan-bench <pack> <scan|random> [sha1|sha256]`.
//! Scan time includes header inspection and dependency planning. Both modes
//! verify every resolved object ID and drop each output after counting it.

use sley_core::ObjectFormat;
use sley_pack::{
    BoundedPackDecoder, PackIndex, PackObjectLocation, PackReadLimits, PackScan, RefDeltaBases,
    SlicePackSource, read_object_at_arc,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pack-scan-bench: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let usage = "usage: pack-scan-bench <pack> <scan|random> [sha1|sha256]";
    let mut arguments = std::env::args().skip(1);
    let path = PathBuf::from(arguments.next().ok_or(usage)?);
    let mode = arguments.next().ok_or(usage)?;
    let format = match arguments.next().as_deref() {
        None | Some("sha1") => ObjectFormat::Sha1,
        Some("sha256") => ObjectFormat::Sha256,
        _ => return Err(usage.into()),
    };
    if arguments.next().is_some() || !matches!(mode.as_str(), "scan" | "random") {
        return Err(usage.into());
    }
    let mapped = sley_mmap::MappedFile::open_pack(&path).map_err(|error| error.to_string())?;
    let index = PackIndex::parse(
        &std::fs::read(path.with_extension("idx")).map_err(|error| error.to_string())?,
        format,
    )
    .map_err(|error| error.to_string())?;
    let limits = PackReadLimits {
        max_delta_depth: 4096,
        max_materialized_bytes: 1024 * 1024 * 1024,
        max_cached_bytes: 512 * 1024 * 1024,
    };
    let mut objects = 0u64;
    let mut object_bytes = 0u64;
    let started = Instant::now();
    if mode == "scan" {
        let scan = PackScan::from_slice(mapped.as_bytes(), &index, limits)
            .map_err(|error| error.to_string())?;
        let mut cursor = scan
            .plan(index.entries.iter().map(|entry| entry.oid))
            .map_err(|error| error.to_string())?
            .cursor(HashMap::new())
            .map_err(|error| error.to_string())?;
        for outcome in cursor.by_ref() {
            let outcome = outcome.map_err(|error| error.to_string())?;
            objects += 1;
            object_bytes += outcome.object.body.len() as u64;
        }
        let stats = cursor.stats();
        if stats.entries_inflated != objects {
            return Err("scan inflated count differs from object count".into());
        }
        println!("entries_inflated={}", stats.entries_inflated);
        println!("bytes_inflated={}", stats.bytes_inflated);
        println!("peak_live_base_bytes={}", stats.peak_live_base_bytes);
    } else {
        // Only REF callbacks need this iterative resolver. It has no decoded
        // cache, so shared bases are re-read just as in read_object_at_arc.
        let mut refs = BoundedPackDecoder::new(
            SlicePackSource::new(mapped.as_bytes()),
            format,
            PackReadLimits {
                max_cached_bytes: 0,
                ..limits
            },
        )
        .map_err(|error| error.to_string())?;
        let mut bases = RefDeltaBases::new();
        for entry in &index.entries {
            bases.insert_location(
                entry.oid,
                PackObjectLocation::new(refs.primary_source(), entry.offset),
            );
        }
        for entry in &index.entries {
            let object = read_object_at_arc(mapped.as_bytes(), entry.offset, format, |oid| {
                index
                    .find(oid)
                    .map(|entry| {
                        refs.read_object_at(entry.offset, &bases)
                            .map(|outcome| Arc::new(outcome.object().clone()))
                            .map_err(|error| sley_core::GitError::InvalidObject(error.to_string()))
                    })
                    .transpose()
            })
            .map_err(|error| error.to_string())?;
            if object
                .object_id(format)
                .map_err(|error| error.to_string())?
                != entry.oid
            {
                return Err("random-read object identity differs from index".into());
            }
            objects += 1;
            object_bytes += object.body.len() as u64;
        }
    }
    println!("mode={mode}");
    println!("pack_bytes={}", mapped.len());
    println!("objects={objects}");
    println!("object_bytes={object_bytes}");
    println!("wall_seconds={:.6}", started.elapsed().as_secs_f64());
    Ok(())
}
