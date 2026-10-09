use flate2::{Compression, write::ZlibEncoder};
use sley_core::{
    AtomicCancel, CancelFlag, GitError, NotFoundKind, ObjectFormat, ObjectId, digest_bytes,
};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{
    PackFile, PackIndex, PackIndexEntry, PackLimitKind, PackReadError, PackReadSource, PackScan,
    PackScanBase, PackScanKind, PackWriteOptions, ScanLimits, read_object_at_arc,
};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

fn limits() -> ScanLimits {
    ScanLimits::new(2048, 16 * 1024 * 1024, 8 * 1024 * 1024)
}

fn lookup(
    objects: HashMap<ObjectId, EncodedObject>,
) -> impl FnMut(&ObjectId) -> Result<std::sync::Arc<EncodedObject>, PackReadError> {
    move |oid| {
        objects
            .get(oid)
            .cloned()
            .map(std::sync::Arc::new)
            .ok_or_else(|| GitError::object_not_found(*oid).into())
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
    let mut cursor = plan.cursor(lookup(fixture.external.clone()), CancelFlag::never());
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
                ScanLimits::new(2048, 16 * 1024 * 1024, 4096),
            )
            .expect("scan");
            let target = scan.entries().last().expect("target").oid;
            let plan = scan.plan([target, target]).expect("plan");
            assert_eq!(plan.entries().len(), 1025);
            assert_eq!(plan.external_bases().len(), 0);
            let mut cursor = plan.cursor(lookup(HashMap::new()), CancelFlag::never());
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
    let mut cursor = plan.cursor(lookup(HashMap::new()), CancelFlag::never());
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
    let mut cursor = plan.cursor(lookup(HashMap::new()), CancelFlag::never());
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
        .cursor(lookup(HashMap::new()), CancelFlag::never());
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
            .cursor(lookup(HashMap::new()), CancelFlag::never());
        for object in cursor.by_ref() {
            object?;
        }
        Ok::<_, PackReadError>(cursor.stats())
    };
    let exact = ScanLimits::new(2, 8208, 4096);
    assert_eq!(run(exact).expect("exact limits").entries_inflated, 3);
    for (limited, kind, attempted) in [
        (
            {
                let mut limited = exact;
                limited.max_delta_depth = 1;
                limited
            },
            PackLimitKind::DeltaDepth,
            2,
        ),
        (
            {
                let mut limited = exact;
                limited.max_live_base_bytes = 4095;
                limited
            },
            PackLimitKind::LiveBaseBytes,
            4096,
        ),
        (
            {
                let mut limited = exact;
                limited.max_materialized_bytes = 8207;
                limited
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
        matches!(plan.cursor(lookup(HashMap::new()), CancelFlag::never()).next(), Some(Err(PackReadError::Pack(GitError::NotFound(NotFoundKind::Object { oid: missing, .. })))) if missing == oid)
    );
    let plan = scan
        .plan(thin.index.entries.iter().map(|entry| entry.oid))
        .expect("plan");
    assert!(matches!(
        plan.cursor(
            lookup(HashMap::from([(
                oid,
                EncodedObject::new(ObjectType::Blob, vec![0; 4096])
            )])),
            CancelFlag::never()
        )
        .next(),
        Some(Err(PackReadError::Pack(GitError::InvalidObject(_))))
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
                {
                    for object in plan.cursor(lookup(HashMap::new()), CancelFlag::never()) {
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
        .cursor(lookup(HashMap::new()), CancelFlag::never());
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
            .cursor(lookup(HashMap::new()), CancelFlag::never());
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
            .cursor(lookup(HashMap::new()), CancelFlag::never());
        assert!(cursor.next().is_none());
        assert_eq!(cursor.stats().entries_inflated, 0);
    }
}

fn encoded_fixture(entries: Vec<(EncodedObject, Vec<u8>)>) -> Fixture {
    let format = ObjectFormat::Sha1;
    let mut pack = b"PACK".to_vec();
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    let mut index_entries = Vec::new();
    let mut objects = HashMap::new();
    for (object, bytes) in entries {
        let oid = object.object_id(format).expect("oid");
        index_entries.push(PackIndexEntry {
            oid,
            offset: pack.len() as u64,
            crc32: crc32fast::hash(&bytes),
        });
        objects.insert(oid, object);
        pack.extend(bytes);
    }
    let checksum = digest_bytes(format, &pack).expect("checksum");
    pack.extend_from_slice(checksum.as_bytes());
    let index = PackIndex::parse(
        &PackIndex::write_v2(format, &index_entries, &checksum).expect("index bytes"),
        format,
    )
    .expect("index");
    Fixture {
        pack,
        index,
        objects,
        external: HashMap::new(),
    }
}

#[test]
fn scan_duplicate_objects_keep_first_copy_and_check_git() {
    let object = EncodedObject::new(ObjectType::Blob, vec![b'x'; 4096]);
    let bytes = entry(3, &object.body, &[]);
    let fixture = encoded_fixture(vec![(object.clone(), bytes.clone()), (object, bytes)]);
    let repo = TempRepo::new(ObjectFormat::Sha1);
    let pack_path = repo.0.join("duplicate.pack");
    std::fs::write(&pack_path, &fixture.pack).expect("pack");
    git(
        &repo.0,
        &["index-pack", pack_path.to_str().expect("path")],
        &[],
    );
    let oracle = PackIndex::parse(
        &std::fs::read(pack_path.with_extension("idx")).expect("git index"),
        ObjectFormat::Sha1,
    )
    .expect("git index parse");
    assert_eq!(oracle.entries.len(), 2, "git accepts both copies");
    let oid = oracle.entries[0].oid;
    git(&repo.0, &["index-pack", "--stdin"], &fixture.pack);
    let trace = repo.0.join("pack-access.trace");
    let output = Command::new("git")
        .args(["cat-file", "blob", &oid.to_string()])
        .current_dir(&repo.0)
        .env("GIT_TRACE_PACK_ACCESS", &trace)
        .output()
        .expect("cat-file");
    assert!(output.status.success());
    assert_eq!(output.stdout, fixture.objects[&oid].body);
    let accesses = std::fs::read_to_string(trace).expect("trace");
    assert_eq!(
        accesses
            .lines()
            .next()
            .expect("access")
            .split_whitespace()
            .last(),
        Some("65"),
        "Git 2.55.0 lookup chooses second of two copies: {accesses}"
    );
    assert_eq!(
        oracle.entries[0].offset, 12,
        "index-pack preserves the first copy in the index"
    );
    let scan = PackScan::from_slice(&fixture.pack, &oracle, limits()).expect("duplicate scan");
    let plan = scan.plan([oid, oid]).expect("plan");
    assert_eq!(plan.entries().len(), 1);
    assert_eq!(plan.entries().next().expect("entry").offset, 12);
    let mut cursor = plan.cursor(lookup(HashMap::new()), CancelFlag::never());
    assert_eq!(cursor.next().expect("object").expect("decode").offset, 12);
    assert!(cursor.next().is_none());
    assert_eq!(cursor.stats().entries_inflated, 1);
    // OFS references address the second physical copy; both base forms must
    // still add only the canonical first copy to the dependency closure.
    let base = fixture.objects[&oid].clone();
    let base_bytes = entry(3, &base.body, &[]);
    let mut body = base.body.clone();
    body.push(b'!');
    let child = EncodedObject::new(ObjectType::Blob, body);
    for ofs in [false, true] {
        let base_reference = if ofs {
            vec![base_bytes.len() as u8]
        } else {
            oid.as_bytes().to_vec()
        };
        let fixture = encoded_fixture(vec![
            (base.clone(), base_bytes.clone()),
            (base.clone(), base_bytes.clone()),
            (
                child.clone(),
                entry(
                    if ofs { 6 } else { 7 },
                    &copy_delta(4096, b"!"),
                    &base_reference,
                ),
            ),
        ]);
        let scan = PackScan::from_slice(&fixture.pack, &fixture.index, limits())
            .expect("duplicate base scan");
        let plan = scan
            .plan([child.object_id(ObjectFormat::Sha1).expect("oid")])
            .expect("plan");
        assert_eq!(plan.entries().len(), 2);
        assert_eq!(plan.entries().next().expect("base").offset, 12);
        let mut cursor = plan.cursor(lookup(HashMap::new()), CancelFlag::never());
        for output in cursor.by_ref() {
            output.expect("decode");
        }
        assert_eq!(cursor.stats().entries_inflated, 2);
    }
}

fn copy_delta(size: usize, insert: &[u8]) -> Vec<u8> {
    let mut delta = Vec::new();
    varint(size, &mut delta);
    varint(size + insert.len(), &mut delta);
    assert!(size < 1 << 24 && !insert.is_empty() && insert.len() < 128);
    delta.extend_from_slice(&[0xf0, size as u8, (size >> 8) as u8, (size >> 16) as u8]);
    delta.push(insert.len() as u8);
    delta.extend_from_slice(insert);
    delta
}

#[test]
fn scan_base_with_dependents_before_and_after_is_evicted_after_last() {
    let base = EncodedObject::new(ObjectType::Blob, vec![b'x'; 4096]);
    let oid = base.object_id(ObjectFormat::Sha1).expect("base oid");
    let child = |suffix: u8| {
        let mut body = base.body.clone();
        body.push(suffix);
        let object = EncodedObject::new(ObjectType::Blob, body);
        (
            object,
            entry(7, &copy_delta(4096, &[suffix]), oid.as_bytes()),
        )
    };
    let fixture = encoded_fixture(vec![
        child(b'a'),
        (base.clone(), entry(3, &base.body, &[])),
        child(b'b'),
    ]);
    let scan = PackScan::from_slice(
        &fixture.pack,
        &fixture.index,
        ScanLimits::new(1, 16384, 4096),
    )
    .expect("scan");
    let mut cursor = scan
        .plan(fixture.index.entries.iter().map(|entry| entry.oid))
        .expect("plan")
        .cursor(lookup(HashMap::new()), CancelFlag::never());
    let before = cursor.next().expect("before").expect("decode");
    assert_eq!(before.object.body.last(), Some(&b'a'));
    drop(before);
    let base_output = cursor.next().expect("base").expect("decode");
    let weak = std::sync::Arc::downgrade(&base_output.object);
    drop(base_output);
    assert!(
        weak.upgrade().is_some(),
        "base survives yielding and earlier dependent"
    );
    let after = cursor.next().expect("after").expect("decode");
    assert_eq!(after.object.body.last(), Some(&b'b'));
    assert!(weak.upgrade().is_none(), "last dependent releases base");
    assert!(cursor.next().is_none());
    assert_eq!(cursor.stats().entries_inflated, 3);
    assert_eq!(cursor.stats().peak_live_base_bytes, 4096);
}

#[test]
fn scan_external_bases_load_lazily_once_and_count_only_while_live() {
    let bases: Vec<_> = (b'a'..=b'b')
        .map(|byte| std::sync::Arc::new(EncodedObject::new(ObjectType::Blob, vec![byte; 4096])))
        .collect();
    let mut entries = Vec::new();
    for base in &bases {
        let oid = base.object_id(ObjectFormat::Sha1).expect("base oid");
        for suffix in *b"12" {
            let mut body = base.body.clone();
            body.push(suffix);
            entries.push((
                EncodedObject::new(ObjectType::Blob, body),
                entry(7, &copy_delta(4096, &[suffix]), oid.as_bytes()),
            ));
        }
    }
    let fixture = encoded_fixture(entries);
    let scan = PackScan::from_slice(
        &fixture.pack,
        &fixture.index,
        ScanLimits::new(1, 16384, 4096),
    )
    .expect("scan");
    let calls = AtomicUsize::new(0);
    let mut cursor = scan
        .plan(fixture.index.entries.iter().map(|entry| entry.oid))
        .expect("plan")
        .cursor(
            |oid| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(bases
                    .iter()
                    .find(|base| base.object_id(ObjectFormat::Sha1).expect("oid") == *oid)
                    .expect("base")
                    .clone())
            },
            CancelFlag::never(),
        );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(cursor.stats().peak_live_base_bytes, 0);
    for (position, expected_calls) in [1, 1, 2, 2].into_iter().enumerate() {
        let output = cursor.next().expect("entry").expect("decode");
        assert_eq!(output.object.as_ref(), &fixture.objects[&output.oid]);
        assert_eq!(calls.load(Ordering::Relaxed), expected_calls);
        assert_eq!(
            std::sync::Arc::strong_count(&bases[position / 2]),
            if position % 2 == 0 { 2 } else { 1 }
        );
    }
    assert!(cursor.next().is_none());
    assert_eq!(cursor.stats().entries_inflated, 4);
    assert_eq!(cursor.stats().peak_live_base_bytes, 4096);
}

#[test]
fn scan_cancelling_mid_scan_returns_typed_cancelled_and_fuses() {
    // Forward references buffer resolved outputs, so cancellation must also
    // be checked before returning an already resolved entry.
    for forward in [false, true] {
        let fixture = chain(3, forward, ObjectFormat::Sha1);
        let scan = PackScan::from_slice(&fixture.pack, &fixture.index, limits()).expect("scan");
        let cancel = AtomicCancel::new();
        let mut cursor = scan
            .plan(fixture.index.entries.iter().map(|entry| entry.oid))
            .expect("plan")
            .cursor(lookup(HashMap::new()), CancelFlag::new(&cancel));
        cursor.next().expect("first").expect("decode");
        let inflated = cursor.stats().entries_inflated;
        cancel.cancel();
        assert!(matches!(
            cursor.next(),
            Some(Err(PackReadError::Pack(GitError::Cancelled)))
        ));
        assert_eq!(cursor.stats().entries_inflated, inflated);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn scan_defaults_accept_git_shaped_live_bases_and_maximum_repack_depth() {
    // Six 6 MiB bases stay live across a 4095-deep chain until their tail
    // dependents. This exceeds the measured git.git peak (~35 MB), as well as
    // the old random-reader defaults (16 MiB bases and depth 50).
    let mut entries = Vec::new();
    let mut dependents = Vec::new();
    for byte in b'a'..=b'f' {
        let base = EncodedObject::new(ObjectType::Blob, vec![byte; 6 * 1024 * 1024]);
        let oid = base.object_id(ObjectFormat::Sha1).expect("oid");
        let mut result = base.body.clone();
        result.push(b'!');
        dependents.push((
            EncodedObject::new(ObjectType::Blob, result),
            entry(7, &copy_delta(base.body.len(), b"!"), oid.as_bytes()),
        ));
        entries.push((base.clone(), entry(3, &base.body, &[])));
    }
    let chain = chain(4096, false, ObjectFormat::Sha1);
    let mut ordered = chain.index.entries.clone();
    ordered.sort_unstable_by_key(|entry| entry.offset);
    for (position, indexed) in ordered.iter().enumerate() {
        let end = ordered
            .get(position + 1)
            .map(|entry| entry.offset as usize)
            .unwrap_or(chain.pack.len() - 20);
        entries.push((
            chain.objects[&indexed.oid].clone(),
            chain.pack[indexed.offset as usize..end].to_vec(),
        ));
    }
    entries.extend(dependents);
    let fixture = encoded_fixture(entries);
    let scan = PackScan::from_slice(&fixture.pack, &fixture.index, ScanLimits::default())
        .expect("default scan");
    let mut cursor = scan
        .plan(fixture.index.entries.iter().map(|entry| entry.oid))
        .expect("default plan")
        .cursor(lookup(HashMap::new()), CancelFlag::never());
    for output in cursor.by_ref() {
        let output = output.expect("default decode");
        assert_eq!(output.object.as_ref(), &fixture.objects[&output.oid]);
    }
    assert_eq!(cursor.stats().entries_inflated, 4108);
    assert_eq!(cursor.stats().peak_live_base_bytes, 36 * 1024 * 1024 + 4096);
}

fn scan_all(fixture: &Fixture) -> Result<(), PackReadError> {
    let scan = PackScan::from_slice(&fixture.pack, &fixture.index, ScanLimits::default())?;
    for object in scan
        .plan(fixture.index.entries.iter().map(|entry| entry.oid))?
        .cursor(lookup(HashMap::new()), CancelFlag::never())
    {
        object?;
    }
    Ok(())
}

#[test]
fn scan_huge_declared_size_and_out_of_range_delta_copy_are_typed() {
    let object = EncodedObject::new(ObjectType::Blob, vec![b'x'; 4096]);
    let mut huge = vec![0xb0]; // blob, low size bits zero, more size bytes
    let mut size = 1u64 << 56; // declared body size 2^60, no large body
    while size > 0 {
        let byte = (size & 127) as u8;
        size >>= 7;
        huge.push(byte | if size > 0 { 128 } else { 0 });
    }
    huge.extend_from_slice(&entry(3, &object.body, &[])[3..]);
    let fixture = encoded_fixture(vec![(object.clone(), huge)]);
    assert!(
        matches!(scan_all(&fixture), Err(PackReadError::Limit(error)) if error.kind == PackLimitKind::MaterializedBytes)
    );
    let oid = object.object_id(ObjectFormat::Sha1).expect("oid");
    let mut delta = Vec::new();
    varint(4096, &mut delta);
    varint(1, &mut delta);
    delta.extend_from_slice(&[0x93, 0xff, 0xff, 1]); // offset 65535, size one
    let fixture = encoded_fixture(vec![
        (object.clone(), entry(3, &object.body, &[])),
        (
            EncodedObject::new(ObjectType::Blob, vec![b'y']),
            entry(7, &delta, oid.as_bytes()),
        ),
    ]);
    assert!(
        matches!(scan_all(&fixture), Err(PackReadError::Pack(GitError::InvalidObject(message))) if message.contains("copy range exceeds base"))
    );
}

#[test]
fn scan_mutates_declared_sizes_delta_instructions_and_zlib_bodies() {
    let fixture = chain(3, false, ObjectFormat::Sha1);
    let mut ordered = fixture.index.entries.clone();
    ordered.sort_unstable_by_key(|entry| entry.offset);
    for (position, indexed) in ordered.iter().enumerate() {
        let start = indexed.offset as usize;
        let end = ordered
            .get(position + 1)
            .map(|entry| entry.offset as usize)
            .unwrap_or(fixture.pack.len() - 20);
        // Every header-size and compressed-body byte, beyond the old four-byte
        // prefix walk. Some zlib padding mutations remain valid; all terminate.
        for at in start..end {
            let mut mutated = Fixture {
                pack: fixture.pack.clone(),
                index: fixture.index.clone(),
                objects: HashMap::new(),
                external: HashMap::new(),
            };
            mutated.pack[at] ^= 0x80;
            match scan_all(&mutated) {
                Ok(()) | Err(PackReadError::Pack(_)) | Err(PackReadError::Limit(_)) => {}
                Err(error) => panic!("unexpected mutation error at {at}: {error}"),
            }
        }
    }
    for at in [ordered[0].offset as usize, ordered[1].offset as usize - 1] {
        let mut mutated = Fixture {
            pack: fixture.pack.clone(),
            index: fixture.index.clone(),
            objects: HashMap::new(),
            external: HashMap::new(),
        };
        mutated.pack[at] ^= 1;
        assert!(
            matches!(
                scan_all(&mutated),
                Err(PackReadError::Pack(GitError::InvalidObject(_)))
            ),
            "size/checksum mutation {at}"
        );
    }
    let base = &fixture.objects[&ordered[0].oid];
    let mut result = base.body.clone();
    result.push(b'!');
    let target = EncodedObject::new(ObjectType::Blob, result);
    let delta = copy_delta(4096, b"!");
    let valid = encoded_fixture(vec![
        (base.clone(), entry(3, &base.body, &[])),
        (target.clone(), entry(7, &delta, ordered[0].oid.as_bytes())),
    ]);
    scan_all(&valid).expect("unmutated instructions decode");
    // Recompress mutated instructions so malformed deltas reach the applier
    // instead of being rejected by zlib's checksum first.
    for at in 0..delta.len() {
        let mut instructions = delta.clone();
        instructions[at] ^= 0x40;
        let mutated = encoded_fixture(vec![
            (base.clone(), entry(3, &base.body, &[])),
            (
                target.clone(),
                entry(7, &instructions, ordered[0].oid.as_bytes()),
            ),
        ]);
        assert!(
            matches!(
                scan_all(&mutated),
                Err(PackReadError::Pack(GitError::InvalidObject(_))) | Err(PackReadError::Limit(_))
            ),
            "instruction mutation {at}"
        );
    }
}
