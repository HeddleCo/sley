//! Public visitor contract, malformed format vectors and bounded-memory scenarios.
use flate2::{Compression, write::ZlibEncoder};
use sley_core::{AtomicCancel, CancelFlag, GitError, ObjectFormat, digest_bytes};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{
    PackFile, PackIndex, PackIndexOptions, ResolveError, ResolveLimits, ResolveMetrics,
    ResolvedObject, resolve_pack, resolve_pack_with_metrics,
};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Barrier;
use std::sync::{
    Mutex,
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
    if size == 1 {
        d.extend([1, marker]);
        return d;
    }
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
        ResolveError::ResolveOps { limit, attempted } => {
            format!("resolve_ops budget {limit} exceeded at operation {attempted}")
        }
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
            assert_eq!(o.oid, ids[o.depth as usize]);
            assert_eq!(o.body, bodies[o.depth as usize]);
            if let Some(d) = &o.delta {
                assert_eq!(d.base, ids[o.depth as usize - 1]);
                assert_eq!(apply(&bodies[o.depth as usize - 1], d.instructions), o.body);
            }
            assert!(
                seen.lock()
                    .expect("seen")
                    .insert(o.offset, o.delta.as_ref().map(|d| (d.base, d.base_offset)))
                    .is_none()
            );
            Ok::<_, ()>(())
        },
    )
    .expect("forward refs resolved from final root");
    assert_eq!(built.entries.len(), depth as usize + 2);
    let seen = seen.into_inner().expect("seen");
    assert_eq!(seen.len(), depth as usize + 2);
    for (base_oid, base_offset) in seen.values().flatten() {
        assert!(
            built
                .entries
                .iter()
                .any(|entry| entry.offset == *base_offset && entry.oid == *base_oid)
        );
    }
}

#[cfg(target_os = "linux")]
fn status_bytes(field: &str) -> usize {
    let status = std::fs::read_to_string("/proc/self/status").expect("read process peak");
    let kb = status
        .lines()
        .find_map(|l| l.strip_prefix(field))
        .expect("VmHWM")
        .split_whitespace()
        .next()
        .expect("peak number")
        .parse::<usize>()
        .expect("parse peak");
    kb * 1024
}
#[cfg(target_os = "linux")]
fn peak_bytes() -> usize {
    status_bytes("VmHWM:")
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
    // A huge instruction stream is bounded independently of its tiny result.
    let mut instructions = pack_start();
    header(3, 0, &mut instructions);
    compressed(b"", &mut instructions);
    let at = instructions.len();
    header(6, 1 << 40, &mut instructions);
    ofs(at - 12, &mut instructions);
    compressed(&[0, 0], &mut instructions);
    assert!(error(&finish(instructions, 2), ResolveLimits::default()).contains("max_object_bytes"));
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
    #[cfg(target_os = "linux")]
    if !memory_child("delta_result_bomb_and_running_budgets_are_checked_before_visits") {
        return;
    }
    #[cfg(target_os = "linux")]
    let before = peak_bytes();
    #[cfg(target_os = "linux")]
    let virtual_before = status_bytes("VmPeak:");
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
    // Isolate the per-object allocation guard from the separate running cap.
    let diagnostic = error(
        &p,
        ResolveLimits {
            max_total_decoded: u64::MAX,
            ..ResolveLimits::default()
        },
    );
    #[cfg(target_os = "linux")]
    {
        let peak = peak_bytes();
        println!(
            "delta-result bomb: peak={peak}, growth={}",
            peak.saturating_sub(before)
        );
        assert!(peak.saturating_sub(before) < 8 * 1024 * 1024);
        let virtual_growth = status_bytes("VmPeak:").saturating_sub(virtual_before);
        println!("delta-result bomb: peak virtual growth={virtual_growth}");
        assert!(
            virtual_growth < 8 * 1024 * 1024,
            "declared result reserved virtual memory before refusal"
        );
    }
    assert!(diagnostic.contains("max_object_bytes"));
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

    let mut late = pack_start();
    header(3, 4, &mut late);
    compressed(b"base", &mut late);
    header(7, 2, &mut late);
    late.extend([77; 20]);
    compressed(&[4, 0], &mut late);
    let late = finish(late, 2);
    let seen = AtomicUsize::new(0);
    let staged = Mutex::new(Vec::new());
    let result = resolve_pack(
        &late,
        ObjectFormat::Sha1,
        2,
        ResolveLimits::default(),
        CancelFlag::never(),
        &|o| {
            assert!(o.delta.is_none());
            staged.lock().expect("provisional store").push(o.oid);
            seen.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    );
    assert!(
        matches!(result, Err(ResolveError::Pack(_))),
        "an unresolved suffix must not return an index"
    );
    assert_eq!(
        seen.load(Ordering::Relaxed),
        1,
        "valid-prefix visitor effects stay provisional"
    );
    let mut admitted = Vec::new();
    let mut staged = staged.into_inner().expect("staging store");
    if result.is_ok() {
        admitted.append(&mut staged);
    } else {
        staged.clear();
    }
    assert!(
        admitted.is_empty(),
        "thin suffix must discard provisional admission"
    );
    assert!(staged.is_empty());
    println!("late thin pack: 1 provisional callback, 0 admitted objects, no returned index");
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

#[test]
fn replay_work_cannot_exceed_the_shared_decoded_budget() {
    const SIZE: usize = 65536;
    let mut p = pack_start();
    let base = p.len();
    header(3, SIZE as u64, &mut p);
    compressed(&vec![0; SIZE], &mut p);
    for _ in 0..32 {
        let at = p.len();
        let mut d = Vec::new();
        varint(SIZE as u64, &mut d);
        varint(0, &mut d);
        header(6, d.len() as u64, &mut p);
        ofs(at - base, &mut p);
        compressed(&d, &mut p);
    }
    let p = finish(p, 33);
    let calls = AtomicUsize::new(0);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        1,
        ResolveLimits {
            max_total_decoded: (2 * SIZE + 1024) as u64,
            max_live_base_bytes: 0,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    )
    .expect_err("repeated root inflation must spend work budget");
    assert!(pack_message(e).contains("max_total_decoded"));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "refuse before the third root allocation"
    );
}

#[test]
fn replayed_delta_result_budget_is_shared_across_workers() {
    const SIZE: usize = 65536;
    // Each root's work fits the limit by itself, but the sixteen subtrees
    // together do not. The barrier prevents one worker consuming all roots.
    let p = chains(16, 4, SIZE, true);
    let barrier = Barrier::new(16);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_total_decoded: (160 * SIZE) as u64,
            max_live_base_bytes: 0,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|o| {
            if o.depth == 0 {
                barrier.wait();
            }
            Ok::<_, ()>(())
        },
    )
    .expect_err("replayed delta results must spend a shared budget");
    assert!(pack_message(e).contains("max_total_decoded"));
}

#[test]
fn overflowing_entry_and_delta_size_varints_are_refused() {
    let mut p = pack_start();
    // The final 0x10 at shift 60 represents 2^64, not an empty body.
    p.push(0xb0);
    p.extend([0x80; 8]);
    p.push(0x10);
    compressed(b"", &mut p);
    assert!(error(&finish(p, 1), ResolveLimits::default()).contains("pack size overflow"));
    for result in [false, true] {
        let mut p = pack_start();
        header(3, 0, &mut p);
        compressed(b"", &mut p);
        let at = p.len();
        let mut d = Vec::new();
        if result {
            d.push(0);
        }
        d.extend([0x80; 9]);
        d.push(2); // 2 << 63 must not wrap to zero.
        if !result {
            d.push(0);
        }
        header(6, d.len() as u64, &mut p);
        ofs(at - 12, &mut p);
        compressed(&d, &mut p);
        assert!(error(&finish(p, 2), ResolveLimits::default()).contains("delta size overflow"));
    }
}

#[test]
fn delta_commands_cannot_overrun_or_avoid_making_progress() {
    fn vector(base: &[u8], instructions: &[u8]) -> Vec<u8> {
        let mut p = pack_start();
        header(3, base.len() as u64, &mut p);
        compressed(base, &mut p);
        let at = p.len();
        header(6, instructions.len() as u64, &mut p);
        ofs(at - 12, &mut p);
        compressed(instructions, &mut p);
        finish(p, 2)
    }
    #[cfg(target_os = "linux")]
    if !memory_child("delta_commands_cannot_overrun_or_avoid_making_progress") {
        return;
    }
    let base = vec![0; 65536];
    let mut copies = Vec::new();
    varint(65536, &mut copies);
    varint(0, &mut copies);
    copies.resize(copies.len() + 4096, 0x80);
    #[cfg(target_os = "linux")]
    let before = peak_bytes();
    let e = error(&vector(&base, &copies), ResolveLimits::default());
    #[cfg(target_os = "linux")]
    {
        let peak = peak_bytes();
        println!(
            "copy overrun: peak={peak}, growth={}",
            peak.saturating_sub(before)
        );
        assert!(
            peak.saturating_sub(before) < 8 * 1024 * 1024,
            "copy output grew beyond its declared zero bytes"
        );
    }
    assert!(e.contains("exceed declared result"));
    for (base, d, diagnostic) in [
        (
            b"abcd".as_slice(),
            vec![4, 1, 0x90, 4],
            "exceed declared result",
        ),
        (b"abcd".as_slice(), vec![4, 1, 0x91, 4, 1], "exceeds base"),
        (
            b"abcd".as_slice(),
            vec![4, 1, 2, b'x'],
            "exceeds delta data",
        ),
        (b"abcd".as_slice(), vec![4, 0, 0], "reserved zero command"),
        (b"abcd".as_slice(), vec![4, 1], "result size mismatch"),
    ] {
        let seen = AtomicUsize::new(0);
        let e = resolve_pack(
            &vector(base, &d),
            ObjectFormat::Sha1,
            1,
            ResolveLimits::default(),
            CancelFlag::never(),
            &|o| {
                assert!(o.delta.is_none(), "invalid delta was visited");
                seen.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(())
            },
        )
        .expect_err("invalid instructions cannot return a partial index");
        assert!(pack_message(e).contains(diagnostic), "{diagnostic}");
        assert_eq!(seen.load(Ordering::Relaxed), 1);
    }
    // A copy opcode with a zero size means 64 KiB in Git, not zero work.
    let base = vec![42; 65536];
    let mut d = Vec::new();
    varint(65536, &mut d);
    varint(65536, &mut d);
    d.push(0x80);
    let p = vector(&base, &d);
    let seen = AtomicUsize::new(0);
    resolve_pack(
        &p,
        ObjectFormat::Sha1,
        2,
        ResolveLimits::default(),
        CancelFlag::never(),
        &|o| {
            assert_eq!(o.body, base);
            seen.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    )
    .expect("zero size copy is a bounded 64 KiB copy");
    assert_eq!(seen.load(Ordering::Relaxed), 2);
    let mut huge = vec![4, 0];
    huge.resize(4096, 0x80);
    assert!(
        error(
            &vector(b"abcd", &huge),
            ResolveLimits {
                max_object_bytes: 1024,
                ..ResolveLimits::default()
            }
        )
        .contains("max_object_bytes")
    );
}

#[test]
fn cyclic_or_missing_offsets_and_ref_bases_never_produce_an_index() {
    for (distance, diagnostic) in [(0, "cycle"), (1, "not found"), (13, "before pack start")] {
        let mut p = pack_start();
        header(6, 2, &mut p);
        ofs(distance, &mut p);
        compressed(&[0, 0], &mut p);
        assert!(error(&finish(p, 1), ResolveLimits::default()).contains(diagnostic));
    }
    let mut p = pack_start();
    for oid in [1, 2] {
        header(7, 2, &mut p);
        p.extend([oid; 20]);
        compressed(&[0, 0], &mut p);
    }
    assert!(error(&finish(p, 2), ResolveLimits::default()).contains("unresolved"));
}

#[test]
fn caller_admission_rejection_is_preserved_without_returning_a_build() {
    #[derive(Debug, PartialEq)]
    struct AdmissionRejected;
    let p = include_bytes!("fixtures/resolved/sha1.pack");
    let cancel = AtomicCancel::new();
    let calls = AtomicUsize::new(0);
    let result = resolve_pack(
        p,
        ObjectFormat::Sha1,
        1,
        ResolveLimits::default(),
        CancelFlag::new(&cancel),
        &|o| {
            // Heddle runs sha1dc over this canonical header and body here. This test
            // models rejection by that caller-owned verifier, not the sha1dc algorithm.
            let header = format!("{} {}\0", o.object_type.as_str(), o.body.len());
            assert!(header.ends_with('\0'));
            calls.fetch_add(1, Ordering::Relaxed);
            Err(AdmissionRejected)
        },
    );
    assert!(matches!(
        result,
        Err(ResolveError::Visitor(AdmissionRejected))
    ));
    assert!(cancel.is_cancelled());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn many_empty_objects_are_bounded_before_metadata_allocation() {
    if !memory_child("many_empty_objects_are_bounded_before_metadata_allocation") {
        return;
    }
    let one = sley_pack::PackFile::write_undeltified(
        &[EncodedObject::new(ObjectType::Blob, Vec::new())],
        ObjectFormat::Sha1,
    )
    .expect("real empty-object writer")
    .pack;
    // A format-conformance vector with repeated physical entries. The body and
    // zlib member are taken unchanged from the real writer.
    let mut p = pack_start();
    p.extend(one[12..one.len() - 20].repeat(8192));
    let p = finish(p, 8192);
    let before = peak_bytes();
    let calls = AtomicUsize::new(0);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_objects: 4096,
            max_total_decoded: 0,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    )
    .expect_err("count bound before metadata");
    assert!(pack_message(e).contains("max_objects"));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(peak_bytes().saturating_sub(before) < 8 * 1024 * 1024);
    let built = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_objects: 8192,
            max_total_decoded: 0,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| Ok::<_, ()>(()),
    )
    .expect("zero decoded bytes still charge metadata count");
    assert_eq!(built.entries.len(), 8192);
}

#[cfg(target_os = "linux")]
#[test]
fn embedded_zlib_members_have_linear_discovery_cpu_cost() {
    if !memory_child("embedded_zlib_members_have_linear_discovery_cpu_cost") {
        return;
    }
    // Thousands of plausible entry headers/zlib members inside a single stored
    // blob: the old every-offset path inflates 4 GiB of irrelevant candidates.
    let mut candidate = Vec::new();
    header(3, 1024 * 1024, &mut candidate);
    let mut nested = ZlibEncoder::new(Vec::new(), Compression::best());
    nested
        .write_all(&vec![0; 1024 * 1024])
        .expect("nested zero body");
    candidate.extend(nested.finish().expect("nested member"));

    let body = candidate.repeat(4096);
    let mut p = pack_start();
    header(3, body.len() as u64, &mut p);
    let mut z = ZlibEncoder::new(Vec::new(), Compression::none());
    z.write_all(&body).expect("stored outer member");
    p.extend(z.finish().expect("stored member end"));
    let p = finish(p, 1);
    let result = bounded_cpu("crafted nested-member pack", |cancel| {
        resolve_pack(
            &p,
            ObjectFormat::Sha1,
            16,
            ResolveLimits::default(),
            cancel,
            &|o| {
                assert_eq!(o.body, body);
                Ok::<_, ()>(())
            },
        )
    });
    assert_eq!(
        result
            .expect("real member only; no speculative candidate work")
            .entries
            .len(),
        1
    );
}

#[cfg(target_os = "linux")]
fn bounded_cpu<R>(label: &str, work: impl FnOnce(CancelFlag<'_>) -> R) -> R {
    fn ticks() -> u64 {
        let s = std::fs::read_to_string("/proc/self/stat").expect("process CPU ticks");
        let fields: Vec<_> = s
            .rsplit_once(')')
            .expect("stat command")
            .1
            .split_whitespace()
            .collect();
        fields[11].parse::<u64>().expect("user ticks")
            + fields[12].parse::<u64>().expect("system ticks")
    }
    let hz = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .expect("clock ticks");
    assert!(hz.status.success());
    let hz = String::from_utf8(hz.stdout)
        .expect("tick text")
        .trim()
        .parse::<u64>()
        .expect("tick rate");
    let source = AtomicCancel::new();
    let done = (Mutex::new(false), std::sync::Condvar::new());
    let start = ticks();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            let lock = done.0.lock().expect("watchdog");
            let (guard, timeout) = done
                .1
                .wait_timeout_while(lock, std::time::Duration::from_secs(8), |finished| {
                    !*finished
                })
                .expect("watchdog wait");
            if timeout.timed_out() && !*guard {
                source.cancel();
            }
        });
        let result = work(CancelFlag::new(&source));
        *done.0.lock().expect("finish") = true;
        done.1.notify_one();
        result
    });
    let spent = ticks() - start;
    println!("{label}: CPU ticks={spent}, bound={}", 4 * hz);
    assert!(spent <= 4 * hz, "CPU exceeded linear-input budget");
    result
}

#[cfg(target_os = "linux")]
#[test]
fn many_missing_ref_bases_have_linear_lookup_cpu_cost() {
    if !memory_child("many_missing_ref_bases_have_linear_lookup_cpu_cost") {
        return;
    }
    const COUNT: u32 = 32768;
    let mut member = Vec::new();
    compressed(&[0, 0], &mut member);
    let mut p = pack_start();
    for i in 0..COUNT {
        header(7, 2, &mut p);
        let mut oid = [0; 20];
        oid[..4].copy_from_slice(&i.to_be_bytes());
        p.extend(oid);
        p.extend_from_slice(&member);
    }
    let p = finish(p, COUNT);
    let visits = AtomicUsize::new(0);
    let result = bounded_cpu("distinct missing REF bases", |cancel| {
        resolve_pack(
            &p,
            ObjectFormat::Sha1,
            16,
            ResolveLimits::default(),
            cancel,
            &|_| {
                visits.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(())
            },
        )
    });
    assert!(pack_message(result.expect_err("thin pack returns no index")).contains("unresolved"));
    assert_eq!(visits.load(Ordering::Relaxed), 0);
}

#[test]
fn low_output_deflate_replay_spends_encoded_work_budget() {
    let mut p = pack_start();
    header(3, 1, &mut p);
    p.extend([0x78, 0x01]);
    for _ in 0..8192 {
        p.extend([0, 0, 0, 255, 255]);
    }
    // Final stored block containing x, followed by Adler-32(x).
    p.extend([1, 1, 0, 254, 255, b'x', 0, 0x79, 0, 0x79]);
    for _ in 0..32 {
        let at = p.len();
        header(6, 2, &mut p);
        ofs(at - 12, &mut p);
        compressed(&[1, 0], &mut p);
    }
    let p = finish(p, 33);
    let visits = AtomicUsize::new(0);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        1,
        ResolveLimits {
            max_total_decoded: 256,
            max_live_base_bytes: 0,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| {
            visits.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    )
    .expect_err("encoded replay work must also be bounded");
    assert!(pack_message(e).contains("max_total_decoded"));
    assert_eq!(
        visits.load(Ordering::Relaxed),
        1,
        "refuse before re-inflating the low-output base"
    );
}

#[test]
fn trusted_legacy_defaults_remain_unlimited_and_explicit_caps_apply() {
    let p = chains(1, 2, 64, false);
    let capped = ResolveLimits {
        max_total_decoded: 128,
        ..ResolveLimits::default()
    };
    let defaults = PackIndexOptions::default();
    assert_eq!(defaults.resolve_limits().max_total_decoded, u64::MAX);
    assert_eq!(defaults.resolve_limits().max_object_bytes, u64::MAX);
    assert_eq!(defaults.resolve_limits().max_objects, usize::MAX);
    assert_eq!(defaults.resolve_limits().max_resolve_ops, u64::MAX);
    assert_eq!(defaults.resolve_limits().max_in_flight_bytes, usize::MAX);
    let run = |options| {
        PackIndex::write_v2_for_pack_with_options(
            &p,
            ObjectFormat::Sha1,
            |_| Ok(None),
            options,
            CancelFlag::never(),
            |_| {},
        )
    };
    assert!(
        run(defaults.with_resolve_limits(capped))
            .expect_err("explicit 128-byte cap")
            .to_string()
            .contains("max_total_decoded")
    );
    assert!(
        PackFile::parse_with_options(&p, ObjectFormat::Sha1, defaults.with_resolve_limits(capped))
            .expect_err("explicit parse cap")
            .to_string()
            .contains("max_total_decoded")
    );
    let expected = run(defaults).expect("trusted default index");
    assert_eq!(
        PackIndex::write_v2_for_pack(&p, ObjectFormat::Sha1).expect("legacy index"),
        expected
    );
    let parsed = PackFile::parse(&p, ObjectFormat::Sha1).expect("trusted parse");
    assert_eq!(parsed.entries.len(), 3);
    for threads in [1, 2, 16] {
        assert_eq!(
            run(defaults.with_threads(threads)).expect("trusted thread count"),
            expected
        );
    }
    println!(
        "trusted defaults: unlimited; explicit 128-byte cap refused; 192 decoded bytes accepted by both legacy paths"
    );
}

#[test]
fn in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory() {
    #[cfg(target_os = "linux")]
    if !memory_child("in_flight_gate_serializes_sixteen_roots_and_bounds_peak_memory") {
        return;
    }
    const SIZE: usize = 8 * 1024 * 1024;
    const GATE: usize = 3 * SIZE;
    const BASES: usize = SIZE;
    let p = chains(16, 1, SIZE, false);
    let metrics = ResolveMetrics::default();
    let calls = AtomicUsize::new(0);
    let built = resolve_pack_with_metrics(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_in_flight_bytes: GATE,
            max_live_base_bytes: BASES,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|o| {
            calls.fetch_add(1, Ordering::Relaxed);
            if o.depth == 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok::<_, ()>(())
        },
        &metrics,
    )
    .expect("workers serialize within one shared gate");
    println!(
        "flight: peak={}, gate={GATE}, after={}",
        metrics.peak_in_flight_bytes(),
        metrics.in_flight_bytes()
    );
    #[cfg(target_os = "linux")]
    {
        let peak = peak_bytes();
        // Isolated child: 64 MiB covers the input, test builder, allocator,
        // thread stacks, zlib state and metadata for these 32 objects.
        let bound = GATE + BASES + 64 * 1024 * 1024;
        println!("flight RSS: peak={peak}, gate+live+64MiB={bound}");
        assert!(peak < bound, "in-flight working memory exceeded RSS bound");
    }
    assert!(
        metrics.peak_in_flight_bytes() <= GATE,
        "in-flight counter exceeded shared gate"
    );
    assert_eq!(metrics.in_flight_bytes(), 0);
    assert_eq!(calls.load(Ordering::Relaxed), 32);
    assert_eq!(built.entries.len(), 32);
}

#[test]
fn flight_reservations_release_after_errors_and_waiters_observe_cancel() {
    let p = chains(16, 2, 32768, false);
    for visitor_error in [false, true] {
        let metrics = ResolveMetrics::default();
        let cancel = AtomicCancel::new();
        let calls = AtomicUsize::new(0);
        let result = resolve_pack_with_metrics(
            &p,
            ObjectFormat::Sha1,
            16,
            ResolveLimits {
                max_in_flight_bytes: 3 * 32768,
                ..ResolveLimits::default()
            },
            CancelFlag::new(&cancel),
            &|_| {
                calls.fetch_add(1, Ordering::Relaxed);
                if visitor_error {
                    cancel.cancel();
                    Err(7)
                } else {
                    cancel.cancel();
                    Ok(())
                }
            },
            &metrics,
        );
        if visitor_error {
            assert!(matches!(result, Err(ResolveError::Visitor(7))));
        } else {
            assert!(matches!(
                result,
                Err(ResolveError::Pack(GitError::Cancelled))
            ));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(cancel.is_cancelled());
        assert_eq!(
            metrics.in_flight_bytes(),
            0,
            "all workspace leases must drop on error"
        );
    }
    let metrics = ResolveMetrics::default();
    let err = resolve_pack_with_metrics(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_in_flight_bytes: 1,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| Ok::<_, ()>(()),
        &metrics,
    )
    .expect_err("one workspace cannot fit");
    assert!(pack_message(err).contains("max_in_flight_bytes"));
    assert_eq!(metrics.peak_in_flight_bytes(), 0);
    let calls = AtomicUsize::new(0);
    let e = resolve_pack(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_in_flight_bytes: 2 * 32768,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
    )
    .expect_err("two bodies alone leave no room for the delta stream");
    assert!(pack_message(e).contains("max_in_flight_bytes"));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    println!(
        "flight: visitor/cancel errors release all leases; oversized workspace refused before allocation"
    );
}

#[test]
fn resolve_ops_counts_discovery_inflate_apply_and_replay() {
    let p = chains(1, 2, 32, false);
    for (live, expected) in [(1024, 8), (0, 12)] {
        let metrics = ResolveMetrics::default();
        let limits = ResolveLimits {
            max_live_base_bytes: live,
            max_resolve_ops: expected,
            ..ResolveLimits::default()
        };
        resolve_pack_with_metrics(
            &p,
            ObjectFormat::Sha1,
            1,
            limits,
            CancelFlag::never(),
            &|_| Ok::<_, ()>(()),
            &metrics,
        )
        .expect("exact operation budget");
        assert_eq!(metrics.resolve_ops(), expected);
        let metrics = ResolveMetrics::default();
        let e = resolve_pack_with_metrics(
            &p,
            ObjectFormat::Sha1,
            1,
            ResolveLimits {
                max_resolve_ops: expected - 1,
                ..limits
            },
            CancelFlag::never(),
            &|_| Ok::<_, ()>(()),
            &metrics,
        )
        .expect_err("next op refused");
        assert!(
            matches!(e, ResolveError::ResolveOps { limit, attempted } if limit == expected - 1 && attempted == expected)
        );
        assert_eq!(metrics.resolve_ops(), expected - 1);
        assert_eq!(metrics.in_flight_bytes(), 0);
        println!(
            "resolve ops: live={live}, exact successful count={expected}; budget={} refused attempted={expected}",
            expected - 1
        );
    }
    let metrics = ResolveMetrics::default();
    let calls = AtomicUsize::new(0);
    let e = resolve_pack_with_metrics(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_resolve_ops: 2,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(())
        },
        &metrics,
    )
    .expect_err("discovery spends budget too");
    assert!(matches!(
        e,
        ResolveError::ResolveOps {
            limit: 2,
            attempted: 3
        }
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let metrics = ResolveMetrics::default();
    let p = chains(16, 2, 32, false);
    let e = resolve_pack_with_metrics(
        &p,
        ObjectFormat::Sha1,
        16,
        ResolveLimits {
            max_resolve_ops: 120,
            ..ResolveLimits::default()
        },
        CancelFlag::never(),
        &|_| Ok::<_, ()>(()),
        &metrics,
    )
    .expect_err("all workers share one op ceiling");
    assert!(matches!(
        e,
        ResolveError::ResolveOps {
            limit: 120,
            attempted: 121
        }
    ));
    assert_eq!(metrics.resolve_ops(), 120);
    assert_eq!(metrics.in_flight_bytes(), 0);
    println!("resolve ops: 16 roots share cap=120, attempted=121");
}

#[test]
fn million_tiny_leaves_stop_at_resolve_op_budget() {
    const LEAVES: usize = 1_000_000;
    // Keep the one-byte root retained with a second child; the 49 intermediate
    // bodies cannot enter the full live pool. Each depth-50 leaf replays them.
    let mut p = pack_start();
    header(3, 1, &mut p);
    compressed(&[0], &mut p);
    let mut parent = 12;
    for level in 1..=49 {
        parent = append_delta(&mut p, parent, 1, level);
    }
    let mut encoded = Vec::new();
    compressed(&[1, 1, 1, 99], &mut encoded);
    for _ in 0..LEAVES {
        let at = p.len();
        header(6, 4, &mut p);
        ofs(at - parent, &mut p);
        p.extend_from_slice(&encoded);
    }
    append_delta(&mut p, 12, 1, 128);
    let objects = LEAVES + 51;
    let p = finish(p, objects as u32);
    let limit = objects as u64 + 10_000;
    let metrics = ResolveMetrics::default();
    let calls = AtomicUsize::new(0);
    let cancel = AtomicCancel::new();
    let done = (Mutex::new(false), std::sync::Condvar::new());
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            let guard = done.0.lock().expect("watchdog");
            let (finished, timeout) = done
                .1
                .wait_timeout_while(guard, std::time::Duration::from_secs(30), |v| !*v)
                .expect("watchdog wait");
            if timeout.timed_out() && !*finished {
                cancel.cancel();
            }
        });
        let result = resolve_pack_with_metrics(
            &p,
            ObjectFormat::Sha1,
            16,
            ResolveLimits {
                max_live_base_bytes: 1,
                max_resolve_ops: limit,
                ..ResolveLimits::default()
            },
            CancelFlag::new(&cancel),
            &|_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(())
            },
            &metrics,
        );
        *done.0.lock().expect("finish") = true;
        done.1.notify_one();
        result
    });
    println!(
        "tiny replay: pack={} bytes, objects={objects}, ops={}, limit={limit}, visits={}",
        p.len(),
        metrics.resolve_ops(),
        calls.load(Ordering::Relaxed)
    );
    assert!(
        matches!(result, Err(ResolveError::ResolveOps { limit: n, attempted }) if n == limit && attempted == limit + 1),
        "operation budget must fire during replay: {result:?}"
    );
    assert_eq!(metrics.resolve_ops(), limit);
    assert_eq!(metrics.in_flight_bytes(), 0);
    assert!(cancel.is_cancelled());
    assert!(
        calls.load(Ordering::Relaxed) < 1000,
        "must stop before walking the million leaves"
    );
}

#[test]
fn declared_count_preallocation_is_capped_before_parsing() {
    #[cfg(target_os = "linux")]
    if !memory_child("declared_count_preallocation_is_capped_before_parsing") {
        return;
    }
    let mut p = pack_start();
    // Enough bytes for the existing conservative count/length test, but no real
    // entries: parsing fails immediately after bounded initial reservation.
    p.resize(12 + 3_000_000, 0);
    let p = finish(p, 1_000_000);
    #[cfg(target_os = "linux")]
    let before = status_bytes("VmPeak:");
    let e = error(&p, ResolveLimits::unlimited());
    #[cfg(target_os = "linux")]
    {
        let growth = status_bytes("VmPeak:").saturating_sub(before);
        println!("declared count: virtual peak growth={growth}, bound=8388608");
        assert!(
            growth < 8 * 1024 * 1024,
            "reservation followed unparsed header count"
        );
    }
    assert!(e.contains("pack object type"), "{e}");
}
