//! Public visitor contract, malformed format vectors and bounded-memory scenarios.
use flate2::{Compression, write::ZlibEncoder};
use sley_core::{AtomicCancel, CancelFlag, GitError, ObjectFormat, digest_bytes};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{
    PackIndex, PackIndexOptions, ResolveError, ResolveLimits, ResolvedObject, resolve_pack,
};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{
    Barrier, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 127) as u8;
        n >>= 7;
        out.push(byte | if n == 0 { 0 } else { 128 });
        if n == 0 {
            break;
        }
    }
}
fn header(kind: u8, mut size: u64, out: &mut Vec<u8>) {
    let first = kind << 4 | (size & 15) as u8;
    size >>= 4;
    out.push(first | if size == 0 { 0 } else { 128 });
    while size != 0 {
        let byte = (size & 127) as u8;
        size >>= 7;
        out.push(byte | if size == 0 { 0 } else { 128 });
    }
}
fn compressed(body: &[u8], out: &mut Vec<u8>) {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
    z.write_all(body).expect("compress vector");
    out.extend(z.finish().expect("finish vector"));
}
fn finish(mut pack: Vec<u8>, count: u32) -> Vec<u8> {
    pack[8..12].copy_from_slice(&count.to_be_bytes());
    let checksum = digest_bytes(ObjectFormat::Sha1, &pack).expect("checksum vector");
    pack.extend_from_slice(checksum.as_bytes());
    pack
}
fn pack_start() -> Vec<u8> {
    b"PACK\0\0\0\x02\0\0\0\0".to_vec()
}
fn ofs(distance: usize, out: &mut Vec<u8>) {
    let mut n = distance;
    let mut bytes = vec![(n & 127) as u8];
    n >>= 7;
    while n != 0 {
        n -= 1;
        bytes.push((n & 127) as u8 | 128);
        n >>= 7;
    }
    out.extend(bytes.into_iter().rev());
}
fn delta(size: usize, marker: u8) -> Vec<u8> {
    let mut d = Vec::new();
    varint(size as u64, &mut d);
    varint(size as u64, &mut d);
    // Copy all but the final byte, then insert the replacement byte.
    let n = size - 1;
    d.push(0xf0);
    d.extend([
        (n & 255) as u8,
        ((n >> 8) & 255) as u8,
        ((n >> 16) & 255) as u8,
    ]);
    d.extend([1, marker]);
    d
}
fn append_delta(pack: &mut Vec<u8>, base: usize, size: usize, marker: u8) -> usize {
    let offset = pack.len();
    let d = delta(size, marker);
    header(6, d.len() as u64, pack);
    ofs(offset - base, pack);
    compressed(&d, pack);
    offset
}
fn chains(roots: usize, depth: usize, size: usize, branching: bool) -> Vec<u8> {
    let mut pack = pack_start();
    let mut count = 0;
    for root in 0..roots {
        let mut parent = pack.len();
        header(3, size as u64, &mut pack);
        compressed(&vec![root as u8; size], &mut pack);
        count += 1;
        let mut ancestors = Vec::new();
        for level in 1..=depth {
            ancestors.push(parent);
            parent = append_delta(&mut pack, parent, size, level as u8);
            count += 1;
        }
        if branching {
            for (level, parent) in ancestors.into_iter().enumerate() {
                append_delta(&mut pack, parent, size, 128 + level as u8);
                count += 1;
            }
        }
    }
    finish(pack, count)
}
fn error(pack: &[u8], limits: ResolveLimits) -> String {
    pack_message(
        resolve_pack(
            pack,
            ObjectFormat::Sha1,
            2,
            limits,
            CancelFlag::never(),
            &|_| Ok::<_, ()>(()),
        )
        .expect_err("vector must be refused"),
    )
}
fn pack_message(error: ResolveError<()>) -> String {
    match error {
        ResolveError::Pack(e) => e.to_string(),
        ResolveError::Visitor(()) => panic!("no failing visitor"),
    }
}
fn apply(base: &[u8], d: &[u8]) -> Vec<u8> {
    fn take(d: &[u8], i: &mut usize) -> u64 {
        let mut n = 0;
        let mut shift = 0;
        loop {
            let b = d[*i];
            *i += 1;
            n |= u64::from(b & 127) << shift;
            if b & 128 == 0 {
                return n;
            }
            shift += 7;
        }
    }
    let mut i = 0;
    assert_eq!(take(d, &mut i), base.len() as u64);
    let expected = take(d, &mut i);
    let mut out = Vec::new();
    while i < d.len() {
        let op = d[i];
        i += 1;
        if op & 128 == 0 {
            assert_ne!(op, 0);
            out.extend_from_slice(&d[i..i + op as usize]);
            i += op as usize;
        } else {
            let mut offset = 0usize;
            let mut size = 0usize;
            for bit in 0..7 {
                if op & (1 << bit) != 0 {
                    let b = d[i] as usize;
                    i += 1;
                    if bit < 4 {
                        offset |= b << (bit * 8);
                    } else {
                        size |= b << ((bit - 4) * 8);
                    }
                }
            }
            if size == 0 {
                size = 65536;
            }
            out.extend_from_slice(&base[offset..offset + size]);
        }
    }
    assert_eq!(out.len() as u64, expected);
    out
}

#[test]
fn real_packs_match_original_indexes_and_visit_once_with_reproducible_instructions() {
    for (pack, expected, format) in [
        (
            include_bytes!("fixtures/resolved/sha1.pack").as_slice(),
            include_bytes!("fixtures/resolved/sha1.idx").as_slice(),
            ObjectFormat::Sha1,
        ),
        (
            include_bytes!("fixtures/resolved/sha256.pack").as_slice(),
            include_bytes!("fixtures/resolved/sha256.idx").as_slice(),
            ObjectFormat::Sha256,
        ),
    ] {
        for threads in [1, 2, 16] {
            for live in [0, 1024, ResolveLimits::default().max_live_base_bytes] {
                let seen = Mutex::new(HashMap::new());
                let caller = std::thread::current().id();
                let built = resolve_pack(
                    pack,
                    format,
                    threads,
                    ResolveLimits {
                        max_live_base_bytes: live,
                        ..ResolveLimits::default()
                    },
                    CancelFlag::never(),
                    &|o: &ResolvedObject<'_>| {
                        assert_ne!(std::thread::current().id(), caller, "visits run on workers");
                        assert_eq!(
                            EncodedObject::new(o.object_type, o.body.to_vec())
                                .object_id(format)
                                .expect("hash body"),
                            o.oid
                        );
                        let value = (
                            o.body.to_vec(),
                            o.delta
                                .as_ref()
                                .map(|d| (d.base, d.base_offset, d.instructions.to_vec())),
                            o.oid,
                            o.depth,
                        );
                        assert!(
                            seen.lock().expect("seen").insert(o.offset, value).is_none(),
                            "entry visited twice"
                        );
                        Ok::<_, ()>(())
                    },
                )
                .expect("resolve real Git pack");
                assert_eq!(
                    built.index, expected,
                    "original engine index, threads={threads}, live={live}"
                );
                let seen = seen.into_inner().expect("seen");
                assert_eq!(seen.len(), built.entries.len());
                assert_eq!(
                    seen.len(),
                    u32::from_be_bytes(pack[8..12].try_into().expect("count")) as usize
                );
                let mut deltas = 0;
                for (body, delta, _, depth) in seen.values() {
                    if let Some((base, offset, instructions)) = delta {
                        let (base_body, _, base_oid, base_depth) = &seen[offset];
                        assert_eq!(base, base_oid);
                        assert_eq!(*depth, base_depth + 1);
                        assert_eq!(apply(base_body, instructions), *body);
                        deltas += 1;
                    } else {
                        assert_eq!(*depth, 0);
                    }
                }
                assert!(deltas > 0, "fixture must exercise deltas");
                let legacy = PackIndex::write_v2_for_pack_with_options(
                    pack,
                    format,
                    |_| Ok(None),
                    PackIndexOptions::default().with_threads(threads),
                    CancelFlag::never(),
                    |_| {},
                )
                .expect("compatibility wrapper");
                assert_eq!(legacy, built);
            }
        }
    }
}

#[test]
fn forward_ref_chain_and_duplicate_oids_visit_each_entry() {
    let size = 32;
    let depth = 6;
    let bodies: Vec<_> = (0..=depth)
        .map(|n| {
            let mut b = vec![0; size];
            b[size - 1] = n;
            b
        })
        .collect();
    let ids: Vec<_> = bodies
        .iter()
        .map(|b| {
            EncodedObject::new(ObjectType::Blob, b.clone())
                .object_id(ObjectFormat::Sha1)
                .expect("oid")
        })
        .collect();
    let mut p = pack_start();
    for n in (1..=depth as usize).rev() {
        let d = delta(size, n as u8);
        header(7, d.len() as u64, &mut p);
        p.extend_from_slice(ids[n - 1].as_bytes());
        compressed(&d, &mut p);
    }
    for _ in 0..2 {
        header(3, size as u64, &mut p);
        compressed(&bodies[0], &mut p);
    }
    let p = finish(p, u32::from(depth) + 2);
    let seen = Mutex::new(HashMap::new());
    let built = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_live_base_bytes: 0,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|o| {
            assert!(
                seen.lock()
                    .expect("seen")
                    .insert(o.offset, o.depth)
                    .is_none()
            );
            Ok::<_, ()>(())
        },
    )
    .expect("forward refs resolved from final root");
    assert_eq!(built.entries.len(), depth as usize + 2);
    assert_eq!(seen.into_inner().expect("seen").len(), depth as usize + 2);
}

#[cfg(target_os = "linux")]
fn peak_bytes() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").expect("read process peak");
    let kb = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .expect("VmHWM")
        .split_whitespace()
        .next()
        .expect("peak number")
        .parse::<usize>()
        .expect("parse peak");
    kb * 1024
}
#[cfg(target_os = "linux")]
fn memory_child(name: &str) -> bool {
    if std::env::var("SLEY_RESOLVE_MEMORY_CHILD").ok().as_deref() == Some(name) {
        return true;
    }
    let result = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", name, "--nocapture"])
        .env("SLEY_RESOLVE_MEMORY_CHILD", name)
        .output()
        .expect("memory subprocess");
    println!("{}", String::from_utf8_lossy(&result.stdout));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    false
}
#[test]
fn declared_size_bomb_is_refused_before_allocation() {
    #[cfg(target_os = "linux")]
    if !memory_child("declared_size_bomb_is_refused_before_allocation") {
        return;
    }
    let mut p = pack_start();
    header(3, 1 << 40, &mut p);
    compressed(b"", &mut p);
    let p = finish(p, 1);
    #[cfg(target_os = "linux")]
    let before = peak_bytes();
    assert!(error(&p, ResolveLimits::default()).contains("max_object_bytes"));
    #[cfg(target_os = "linux")]
    {
        let peak = peak_bytes();
        println!(
            "bomb: peak={peak} bytes, growth={} bytes",
            peak.saturating_sub(before)
        );
        assert!(peak.saturating_sub(before) < 8 * 1024 * 1024);
    }
}
#[test]
fn delta_result_bomb_and_running_budgets_are_checked_before_visits() {
    let mut p = pack_start();
    header(3, 4, &mut p);
    compressed(b"base", &mut p);
    let mut d = vec![4];
    varint(1 << 40, &mut d);
    header(6, d.len() as u64, &mut p);
    let distance = p.len() - 1 - 12;
    ofs(distance, &mut p);
    compressed(&d, &mut p);
    let p = finish(p, 2);
    assert!(error(&p, ResolveLimits::default()).contains("max_object_bytes"));
    let p = chains(1, 2, 32, false);
    for (limits, field) in [
        (
            ResolveLimits {
                max_objects: 2,
                ..ResolveLimits::default()
            },
            "max_objects",
        ),
        (
            ResolveLimits {
                max_total_decoded: 95,
                ..ResolveLimits::default()
            },
            "max_total_decoded",
        ),
    ] {
        let calls = AtomicUsize::new(0);
        let e = resolve_pack(
            &p,
            ObjectFormat::Sha1,
            2,
            limits,
            CancelFlag::never(),
            &|_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(())
            },
        )
        .expect_err("budget error");
        assert!(pack_message(e).contains(field));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}
#[test]
fn depth_51_is_refused_before_visiting_the_deep_object() {
    let p = chains(1, 51, 32, false);
    let count = AtomicUsize::new(0);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        2,
        ResolveLimits::default(),
        CancelFlag::never(),
        &|o| {
            assert!(o.depth <= 50);
            count.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    )
    .expect_err("depth-51 refused");
    assert!(pack_message(e).contains("observed depth 51"));
    assert_eq!(count.load(Ordering::Relaxed), 51);
    assert!(
        resolve_pack(
            &p,
            ObjectFormat::Sha1,
            1,
            ResolveLimits {
                max_delta_depth: 51,
                ..ResolveLimits::default()
            },
            CancelFlag::never(),
            &|_| Ok::<_, ()>(())
        )
        .is_ok()
    );
}
#[test]
fn thin_pack_is_an_unresolved_base_error() {
    let mut p = pack_start();
    let d = delta(32, 1);
    header(7, d.len() as u64, &mut p);
    p.extend_from_slice(&[42; 20]);
    compressed(&d, &mut p);
    let p = finish(p, 1);
    assert!(error(&p, ResolveLimits::default()).contains("unresolved"));
}
#[test]
fn truncation_and_trailing_bytes_are_errors() {
    let p = chains(1, 2, 32, false);
    for end in [0, 12, p.len() - 1] {
        assert!(error(&p[..end], ResolveLimits::default()).contains("pack"));
    }
    // Valid checksum, incomplete zlib member: checksum rejection cannot mask this.
    let mut truncated = p[..p.len() - 20 - 2].to_vec();
    truncated[8..12].copy_from_slice(&3u32.to_be_bytes());
    let truncated = finish(truncated, 3);
    let calls = AtomicUsize::new(0);
    let e = resolve_pack(
        &truncated,
        ObjectFormat::Sha1,
        2,
        ResolveLimits::default(),
        CancelFlag::never(),
        &|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    )
    .expect_err("truncation is caught in discovery");
    assert!(pack_message(e).contains("truncated"));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "discovery must complete before visits"
    );
    let mut trailing = p[..p.len() - 20].to_vec();
    trailing.push(0);
    let trailing = finish(trailing, 3);
    assert!(error(&trailing, ResolveLimits::default()).contains("trailing"));
}
#[test]
fn cancel_mid_pass_and_visitor_error_set_cancel_and_preserve_first_error() {
    let p = chains(16, 2, 32, false);
    let source = AtomicCancel::new();
    let count = AtomicUsize::new(0);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        1,
        ResolveLimits::default(),
        CancelFlag::new(&source),
        &|_| {
            count.fetch_add(1, Ordering::Relaxed);
            source.cancel();
            Ok::<_, ()>(())
        },
    )
    .expect_err("mid-pass cancel");
    assert!(matches!(e, ResolveError::Pack(GitError::Cancelled)));
    assert_eq!(count.load(Ordering::Relaxed), 1);
    source.clear();
    #[derive(Debug, PartialEq)]
    struct Stop(u8);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits::default(),
        CancelFlag::new(&source),
        &|_| Err(Stop(7)),
    )
    .expect_err("visitor error");
    assert!(matches!(e, ResolveError::Visitor(Stop(7))));
    assert!(source.is_cancelled());
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        2,
        ResolveLimits::default(),
        CancelFlag::new(&source),
        &|_| -> Result<(), ()> { panic!("pre-cancelled visitor must not run") },
    )
    .expect_err("pre-cancelled");
    assert!(matches!(e, ResolveError::Pack(GitError::Cancelled)));
}
#[cfg(target_os = "linux")]
#[test]
fn live_base_bound_is_shared_and_replay_stays_within_peak_memory() {
    if !memory_child("live_base_bound_is_shared_and_replay_stays_within_peak_memory") {
        return;
    }
    const WORKERS: usize = 16;
    const DEPTH: usize = 20;
    const SIZE: usize = 1024 * 1024;
    let p = chains(WORKERS, DEPTH, SIZE, true);
    let barrier = Barrier::new(WORKERS);
    let visits = AtomicUsize::new(0);
    let baseline = peak_bytes();
    let built = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        WORKERS,
        ResolveLimits {
            max_live_base_bytes: 8 * SIZE,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|o| {
            visits.fetch_add(1, Ordering::Relaxed);
            if o.depth as usize == DEPTH && o.body.last() == Some(&(DEPTH as u8)) {
                barrier.wait();
            }
            Ok::<_, ()>(())
        },
    )
    .expect("bounded replay succeeds");
    assert_eq!(visits.load(Ordering::Relaxed), WORKERS * (1 + 2 * DEPTH));
    let peak = peak_bytes();
    println!(
        "live bases: baseline={baseline}, peak={peak}, cap={} bytes (16 workers)",
        8 * SIZE
    );
    assert!(
        peak < 110 * SIZE,
        "retained bases exceeded shared gate: peak={peak}"
    );
    let no_cache = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        WORKERS,
        ResolveLimits {
            max_live_base_bytes: 0,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| Ok::<_, ()>(()),
    )
    .expect("zero-cache fallback");
    assert_eq!(built, no_cache);
}

#[cfg(target_os = "linux")]
#[test]
fn last_dependent_releases_base_before_visiting_child() {
    if !memory_child("last_dependent_releases_base_before_visiting_child") {
        return;
    }
    let p = chains(1, 50, 1024 * 1024, false);
    resolve_pack(
        &p,
        ObjectFormat::Sha1,
        1,
        ResolveLimits {
            max_live_base_bytes: 100 * 1024 * 1024,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| Ok::<_, ()>(()),
    )
    .expect("chain resolves");
    let peak = peak_bytes();
    println!("last-dependent release: peak={peak} bytes");
    assert!(
        peak < 24 * 1024 * 1024,
        "completed ancestors still retained: {peak}"
    );
}
