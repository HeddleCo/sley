use flate2::{Compression, write::ZlibEncoder};
use sley_core::{GitError, NotFoundKind, ObjectFormat, ObjectId, digest_bytes};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{
    PackFile, PackIndex, PackIndexEntry, PackLimitKind, PackReadError, PackReadLimits,
    PackReadSource, PackScan, PackScanBase, PackScanKind, PackWriteOptions, read_object_at_arc,
};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

fn limits() -> PackReadLimits {
    PackReadLimits {
        max_delta_depth: 2048,
        max_materialized_bytes: 16 * 1024 * 1024,
        max_cached_bytes: 8 * 1024 * 1024,
    }
}

struct Fixture {
    pack: Vec<u8>,
    index: PackIndex,
    objects: HashMap<ObjectId, EncodedObject>,
    external: HashMap<ObjectId, EncodedObject>,
}

fn fixture(format: ObjectFormat, ofs: bool, thin: bool) -> Fixture {
    let objects: Vec<_> = (0u64..12)
        .map(|number| {
            let mut body = vec![b'x'; 4096];
            body[4088..].copy_from_slice(&number.to_be_bytes());
            EncodedObject::new(ObjectType::Blob, body)
        })
        .collect();
    let external = if thin {
        HashMap::from([(
            objects[0].object_id(format).expect("oid"),
            objects[0].clone(),
        )])
    } else {
        HashMap::new()
    };
    let packed = PackFile::write_packed_with_options(
        &objects[usize::from(thin)..],
        format,
        &PackWriteOptions::new()
            .with_window(1)
            .with_depth(32)
            .with_reorder(false)
            .with_prefer_ofs_delta(ofs)
            .with_thin_bases(external.clone()),
    )
    .expect("fixture pack");
    assert!(packed.delta_count > 0);
    let index = PackIndex::parse(&packed.index, format).expect("index");
    Fixture {
        pack: packed.pack,
        index,
        objects: objects
            .into_iter()
            .map(|object| (object.object_id(format).expect("oid"), object))
            .collect(),
        external,
    }
}

struct TempRepo(PathBuf);
impl TempRepo {
    fn new(format: ObjectFormat) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sley-scan-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("temp directory");
        git(
            &path,
            &[
                "init",
                "--bare",
                &format!(
                    "--object-format={}",
                    if format == ObjectFormat::Sha1 {
                        "sha1"
                    } else {
                        "sha256"
                    }
                ),
            ],
            &[],
        );
        Self(path)
    }
}
impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(repo: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git");
    // Concurrent writer also permits large pack inputs without pipe deadlock.
    let mut stdin = child.stdin.take().expect("stdin");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        stdin.write_all(&input).expect("git input");
    });
    let output = child.wait_with_output().expect("git output");
    writer.join().expect("writer");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn assert_differential(fixture: &Fixture, format: ObjectFormat) {
    let repo = TempRepo::new(format);
    for (oid, object) in &fixture.external {
        let actual = git(
            &repo.0,
            &[
                "hash-object",
                "-w",
                "--stdin",
                "-t",
                object.object_type.as_str(),
            ],
            &object.body,
        );
        assert_eq!(
            String::from_utf8(actual).expect("oid text").trim(),
            oid.to_string()
        );
    }
    let args = if fixture.external.is_empty() {
        vec!["index-pack", "--stdin"]
    } else {
        vec!["index-pack", "--stdin", "--fix-thin"]
    };
    git(&repo.0, &args, &fixture.pack);
    let scan = PackScan::from_slice(&fixture.pack, &fixture.index, limits()).expect("scan");
    let plan = scan
        .plan(fixture.index.entries.iter().map(|entry| entry.oid))
        .expect("plan");
    let mut cursor = plan.cursor(fixture.external.clone()).expect("cursor");
    let mut last_offset = 0;
    for actual in cursor.by_ref() {
        let actual = actual.expect("scan object");
        assert!(actual.offset > last_offset);
        last_offset = actual.offset;
        let random = read_object_at_arc(&fixture.pack, actual.offset, format, |oid| {
            Ok(fixture.objects.get(oid).cloned().map(std::sync::Arc::new))
        })
        .expect("random reader");
        assert_eq!(actual.object, random);
        assert_eq!(actual.object.as_ref(), &fixture.objects[&actual.oid]);
        assert_eq!(
            actual.object.object_id(format).expect("actual oid"),
            actual.oid
        );
        let oracle = git(
            &repo.0,
            &[
                "cat-file",
                actual.object.object_type.as_str(),
                &actual.oid.to_string(),
            ],
            &[],
        );
        assert_eq!(actual.object.body, oracle);
        let oracle_oid = git(
            &repo.0,
            &[
                "hash-object",
                "--stdin",
                "-t",
                actual.object.object_type.as_str(),
            ],
            &oracle,
        );
        assert_eq!(
            String::from_utf8(oracle_oid).expect("oid text").trim(),
            actual.oid.to_string()
        );
    }
    assert_eq!(
        cursor.stats().entries_inflated,
        fixture.index.entries.len() as u64
    );
    assert_eq!(
        cursor.stats().bytes_inflated,
        scan.entries().map(|entry| entry.declared_size).sum::<u64>()
    );
}

#[test]
fn scan_differential_ofs_ref_thin_sha1_sha256() {
    assert_eq!(
        Command::new("git")
            .arg("--version")
            .output()
            .expect("version")
            .stdout,
        b"git version 2.55.0\n"
    );
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        for ofs in [true, false] {
            assert_differential(&fixture(format, ofs, false), format);
        }
        assert_differential(&fixture(format, false, true), format);
    }
}

// Fixture encoders deliberately construct chains independently of the writer's
// delta-selection heuristics. The production decoder uses the shared grammar.
fn varint(mut size: usize, output: &mut Vec<u8>) {
    loop {
        let byte = (size & 127) as u8;
        size >>= 7;
        output.push(byte | if size > 0 { 128 } else { 0 });
        if size == 0 {
            break;
        }
    }
}

fn entry(kind: u8, body: &[u8], base: &[u8]) -> Vec<u8> {
    let mut size = body.len();
    let mut bytes = vec![(kind << 4) | (size & 15) as u8 | if size > 15 { 128 } else { 0 }];
    size >>= 4;
    if size > 0 {
        varint(size, &mut bytes);
    }
    bytes.extend_from_slice(base);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(body).expect("compress");
    bytes.extend(encoder.finish().expect("compressed"));
    bytes
}

fn chain(count: usize, forward: bool, format: ObjectFormat) -> Fixture {
    let objects: Vec<_> = (0..count)
        .map(|number| {
            let mut body = vec![b'x'; 4096];
            body[4088..].copy_from_slice(&(number as u64).to_be_bytes());
            EncodedObject::new(ObjectType::Blob, body)
        })
        .collect();
    let oids: Vec<_> = objects
        .iter()
        .map(|object| object.object_id(format).expect("oid"))
        .collect();
    let mut pack = b"PACK".to_vec();
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(count as u32).to_be_bytes());
    let mut entries = Vec::new();
    let order: Vec<_> = if forward {
        (0..count).rev().collect()
    } else {
        (0..count).collect()
    };
    for position in order {
        let bytes = if position == 0 {
            entry(3, &objects[0].body, &[])
        } else {
            let mut delta = Vec::new();
            varint(4096, &mut delta);
            varint(4096, &mut delta);
            delta.extend_from_slice(&[0xb0, 0xf8, 0x0f, 8]); // copy 4088 bytes, then insert eight
            delta.extend_from_slice(&(position as u64).to_be_bytes());
            entry(7, &delta, oids[position - 1].as_bytes())
        };
        entries.push(PackIndexEntry {
            oid: oids[position],
            crc32: crc32fast::hash(&bytes),
            offset: pack.len() as u64,
        });
        pack.extend(bytes);
    }
    let checksum = digest_bytes(format, &pack).expect("checksum");
    pack.extend_from_slice(checksum.as_bytes());
    let index = PackIndex::parse(
        &PackIndex::write_v2(format, &entries, &checksum).expect("index bytes"),
        format,
    )
    .expect("index");
    Fixture {
        pack,
        index,
        objects: oids.into_iter().zip(objects).collect(),
        external: HashMap::new(),
    }
}

#[test]
fn scan_forward_ref_differential_and_pack_order() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        assert_differential(&chain(12, true, format), format);
    }
}

#[test]
fn scan_long_chain_single_inflation_and_eviction_on_two_mib_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let fixture = chain(1025, false, ObjectFormat::Sha1);
            let scan = PackScan::from_slice(
                &fixture.pack,
                &fixture.index,
                PackReadLimits {
                    max_cached_bytes: 4096,
                    ..limits()
                },
            )
            .expect("scan");
            let target = scan.entries().last().expect("target").oid;
            let plan = scan.plan([target, target]).expect("plan");
            assert_eq!(plan.entries().len(), 1025);
            assert_eq!(plan.external_bases().len(), 0);
            let mut cursor = plan.cursor(HashMap::new()).expect("cursor");
            let mut requested = 0;
            for object in cursor.by_ref() {
                let object = object.expect("chain decode");
                assert_eq!(object.object.as_ref(), &fixture.objects[&object.oid]);
                requested += usize::from(object.requested);
            }
            assert_eq!(requested, 1);
            assert_eq!(cursor.stats().entries_inflated, 1025);
            assert_eq!(cursor.stats().bytes_inflated, 4096 + 1024 * 16);
            assert_eq!(cursor.stats().peak_live_base_bytes, 4096);
        })
        .expect("thread")
        .join()
        .expect("2 MiB stack scan");
}

#[test]
fn scan_shared_base_survives_until_last_planned_dependent() {
    let mut fixture = chain(6, false, ObjectFormat::Sha1);
    let mut ordered = fixture.index.entries.clone();
    ordered.sort_unstable_by_key(|entry| entry.offset);
    for entry in &ordered[1..] {
        let start = entry.offset as usize + 2;
        fixture.pack[start..start + 20].copy_from_slice(ordered[0].oid.as_bytes());
    }
    let scan = PackScan::from_slice(&fixture.pack, &fixture.index, limits()).expect("fanout scan");
    let plan = scan
        .plan([ordered[2].oid, ordered[5].oid])
        .expect("fanout plan");
    assert_eq!(plan.entries().len(), 3);
    let mut cursor = plan.cursor(HashMap::new()).expect("fanout cursor");
    // Hold earlier outputs as well: the cursor's bound excludes caller-owned
    // outputs and must count the shared base once.
    let outputs: Vec<_> = cursor
        .by_ref()
        .map(|entry| entry.expect("fanout entry"))
        .collect();
    assert_eq!(outputs.len(), 3);
    for output in outputs {
        assert_eq!(output.object.as_ref(), &fixture.objects[&output.oid]);
    }
    assert_eq!(cursor.stats().entries_inflated, 3);
    assert_eq!(cursor.stats().peak_live_base_bytes, 4096);
}

struct ShortSource<'a> {
    bytes: &'a [u8],
    read_bytes: std::sync::Arc<AtomicUsize>,
}
impl PackReadSource for ShortSource<'_> {
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        let start = usize::try_from(offset).expect("offset");
        let input = self.bytes.get(start..).unwrap_or_default();
        let count = input.len().min(output.len()).min(7);
        output[..count].copy_from_slice(&input[..count]);
        self.read_bytes.fetch_add(count, Ordering::Relaxed);
        Ok(count)
    }
}

#[test]
fn scan_positional_short_reads_file_and_sparse_plan() {
    let fixture = chain(20, false, ObjectFormat::Sha1);
    let reads = std::sync::Arc::new(AtomicUsize::new(0));
    let source = ShortSource {
        bytes: &fixture.pack,
        read_bytes: reads.clone(),
    };
    let scan = PackScan::new(source, &fixture.index, limits()).expect("positional scan");
    let target = scan.entries().nth(5).expect("sixth").oid;
    let plan = scan.plan([target]).expect("sparse plan");
    assert_eq!(plan.entries().len(), 6);
    let mut cursor = plan.cursor(HashMap::new()).expect("cursor");
    let metadata_reads = reads.load(Ordering::Relaxed);
    assert_eq!(cursor.stats().entries_inflated, 0);
    for object in cursor.by_ref() {
        let object = object.expect("short read object");
        assert_eq!(object.object.as_ref(), &fixture.objects[&object.oid]);
    }
    assert!(reads.load(Ordering::Relaxed) > metadata_reads);
    assert_eq!(cursor.stats().entries_inflated, 6);
    let repo = TempRepo::new(ObjectFormat::Sha1);
    let path = repo.0.join("test.pack");
    std::fs::write(&path, &fixture.pack).expect("pack file");
    let scan = PackScan::new(
        std::fs::File::open(path).expect("file"),
        &fixture.index,
        limits(),
    )
    .expect("file scan");
    let mut cursor = scan
        .plan([target])
        .expect("file plan")
        .cursor(HashMap::new())
        .expect("file cursor");
    for object in cursor.by_ref() {
        let object = object.expect("file object");
        assert_eq!(object.object.as_ref(), &fixture.objects[&object.oid]);
    }
    assert_eq!(cursor.stats().entries_inflated, 6);
}

#[test]
fn scan_limits_missing_external_and_identity_errors_are_typed() {
    let fixture = chain(3, false, ObjectFormat::Sha1);
    let run = |limits| {
        let scan = PackScan::from_slice(&fixture.pack, &fixture.index, limits)?;
        let mut cursor = scan
            .plan(fixture.index.entries.iter().map(|entry| entry.oid))?
            .cursor(HashMap::new())?;
        for object in cursor.by_ref() {
            object?;
        }
        Ok::<_, PackReadError>(cursor.stats())
    };
    let exact = PackReadLimits {
        max_delta_depth: 2,
        max_cached_bytes: 4096,
        max_materialized_bytes: 8208,
    };
    assert_eq!(run(exact).expect("exact limits").entries_inflated, 3);
    for (limited, kind, attempted) in [
        (
            PackReadLimits {
                max_delta_depth: 1,
                ..exact
            },
            PackLimitKind::DeltaDepth,
            2,
        ),
        (
            PackReadLimits {
                max_cached_bytes: 4095,
                ..exact
            },
            PackLimitKind::LiveBaseBytes,
            4096,
        ),
        (
            PackReadLimits {
                max_materialized_bytes: 8207,
                ..exact
            },
            PackLimitKind::MaterializedBytes,
            8208,
        ),
    ] {
        assert!(
            matches!(run(limited), Err(PackReadError::Limit(error)) if error.kind == kind && error.attempted == attempted)
        );
    }
    let thin = self::fixture(ObjectFormat::Sha1, false, true);
    let scan = PackScan::from_slice(&thin.pack, &thin.index, limits()).expect("thin scan");
    let plan = scan
        .plan(thin.index.entries.iter().map(|entry| entry.oid))
        .expect("thin plan");
    let oid = *plan.external_bases().next().expect("external base");
    assert!(
        scan.entries()
            .any(|entry| entry.base == Some(PackScanBase::External(oid)))
    );
    assert!(
        matches!(plan.cursor(HashMap::new()), Err(PackReadError::Pack(GitError::NotFound(NotFoundKind::Object { oid: missing, .. }))) if missing == oid)
    );
    let plan = scan
        .plan(thin.index.entries.iter().map(|entry| entry.oid))
        .expect("plan");
    assert!(matches!(
        plan.cursor(HashMap::from([(
            oid,
            EncodedObject::new(ObjectType::Blob, vec![0; 4096])
        )])),
        Err(PackReadError::Pack(GitError::InvalidObject(_)))
    ));
    assert!(matches!(
        scan.plan([oid]),
        Err(PackReadError::Pack(GitError::NotFound(_)))
    ));
}

#[test]
fn scan_truncated_corrupt_headers_streams_and_cycles_never_panic() {
    let fixture = chain(3, false, ObjectFormat::Sha1);
    for length in 0..fixture.pack.len() {
        let result = PackScan::from_slice(&fixture.pack[..length], &fixture.index, limits());
        assert!(
            matches!(result, Err(PackReadError::Pack(_))),
            "truncated length {length}"
        );
    }
    // Mutation coverage for the header-only walk; no dedicated fuzz harness
    // exists in this repository. Exhaust all byte values in each entry prefix.
    for entry in &fixture.index.entries {
        for at in entry.offset as usize..entry.offset as usize + 4 {
            for byte in 0u8..=255 {
                let mut pack = fixture.pack.clone();
                pack[at] = byte;
                if let Ok(scan) = PackScan::from_slice(&pack, &fixture.index, limits())
                    && let Ok(plan) = scan.plan(fixture.index.entries.iter().map(|entry| entry.oid))
                    && let Ok(cursor) = plan.cursor(HashMap::new())
                {
                    for object in cursor {
                        if object.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    let first = fixture
        .index
        .entries
        .iter()
        .min_by_key(|entry| entry.offset)
        .expect("first");
    let mut corrupt = fixture.pack.clone();
    corrupt[first.offset as usize + 3] ^= 0xff;
    let scan = PackScan::from_slice(&corrupt, &fixture.index, limits()).expect("header scan");
    let mut cursor = scan
        .plan([first.oid])
        .expect("plan")
        .cursor(HashMap::new())
        .expect("cursor");
    assert!(matches!(
        cursor.next(),
        Some(Err(PackReadError::Pack(GitError::InvalidObject(_))))
    ));
    assert!(cursor.next().is_none(), "errors fuse the cursor");
    let delta = fixture
        .index
        .entries
        .iter()
        .max_by_key(|entry| entry.offset)
        .expect("last");
    let mut cycle = fixture.pack.clone();
    let base_start = delta.offset as usize + 2; // 16-byte delta has two header bytes
    cycle[base_start..base_start + 20].copy_from_slice(delta.oid.as_bytes());
    let scan = PackScan::from_slice(&cycle, &fixture.index, limits()).expect("cycle headers");
    assert!(matches!(
        scan.plan([delta.oid]),
        Err(PackReadError::Pack(GitError::InvalidObject(_)))
    ));
}

#[test]
fn scan_empty_and_non_blob_packs() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let objects = vec![
            EncodedObject::new(ObjectType::Tree, Vec::new()),
            EncodedObject::new(
                ObjectType::Commit,
                b"tree 0000000000000000000000000000000000000000\n\nsubject\n".to_vec(),
            ),
            EncodedObject::new(ObjectType::Tag, b"tag v1\n".to_vec()),
        ];
        let packed = PackFile::write_undeltified(&objects, format).expect("pack");
        let index = PackIndex::parse(&packed.index, format).expect("index");
        let scan = PackScan::from_slice(&packed.pack, &index, limits()).expect("scan");
        assert!(
            scan.entries()
                .all(|entry| matches!(entry.kind, PackScanKind::Object(_)))
        );
        let mut cursor = scan
            .plan(index.entries.iter().map(|entry| entry.oid))
            .expect("plan")
            .cursor(HashMap::new())
            .expect("cursor");
        let mut expected: Vec<_> = packed.entries.iter().zip(&objects).collect();
        expected.sort_unstable_by_key(|(entry, _)| entry.offset);
        for (entry, object) in expected {
            let actual = cursor.next().expect("entry").expect("decode");
            assert_eq!(actual.object.as_ref(), object);
            assert_eq!(actual.oid, entry.oid);
        }
        assert!(cursor.next().is_none());
        let packed =
            PackFile::write_undeltified(&Vec::<EncodedObject>::new(), format).expect("empty pack");
        let index = PackIndex::parse(&packed.index, format).expect("empty index");
        let scan = PackScan::from_slice(&packed.pack, &index, limits()).expect("empty scan");
        let mut cursor = scan
            .plan([])
            .expect("empty plan")
            .cursor(HashMap::new())
            .expect("empty cursor");
        assert!(cursor.next().is_none());
        assert_eq!(cursor.stats().entries_inflated, 0);
    }
}
