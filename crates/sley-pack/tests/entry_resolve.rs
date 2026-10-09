//! Targeted entry decoding: self-referencing OFS bases and base/kind
//! mismatches in `DecodedPackEntry::resolve`.

use flate2::{Compression, write::ZlibEncoder};
use sley_core::{GitError, ObjectFormat, digest_bytes};
use sley_object::{EncodedObject, ObjectType};
use std::io::Write;

const HEADER_LEN: u64 = 12;

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).expect("compress fixture entry");
    encoder.finish().expect("finish fixture entry")
}

/// A pack of raw entries (header bytes already included), with its count and
/// trailing checksum filled in.
fn pack(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = b"PACK\0\0\0\x02".to_vec();
    bytes.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for entry in entries {
        bytes.extend_from_slice(entry);
    }
    let checksum = digest_bytes(ObjectFormat::Sha1, &bytes).expect("hash pack");
    bytes.extend_from_slice(checksum.as_bytes());
    bytes
}

/// Blob `abcd` (type 3, size 4).
fn blob_entry() -> Vec<u8> {
    let mut entry = vec![0x34];
    entry.extend(zlib(b"abcd"));
    entry
}

/// OFS delta (type 6, size 7) `distance` bytes back, rewriting a 4-byte base
/// to `wxyz`.
fn ofs_delta_entry(distance: u8) -> Vec<u8> {
    assert!(distance < 0x80, "single-byte OFS distance");
    let mut entry = vec![0x67, distance];
    entry.extend(zlib(b"\x04\x04\x04wxyz"));
    entry
}

fn assert_invalid_format(error: GitError, needle: &str) {
    match error {
        GitError::InvalidFormat(message) => {
            assert!(message.contains(needle), "unexpected message: {message}");
        }
        other => panic!("expected InvalidFormat, got {other:?}"),
    }
}

#[test]
fn zero_ofs_back_offset_is_a_cycle_error() {
    // An OFS delta whose back-offset is 0 names itself as its base.
    let bytes = pack(&[ofs_delta_entry(0)]);
    let error = sley_pack::read_object_at_arc(&bytes, HEADER_LEN, ObjectFormat::Sha1, |_| Ok(None))
        .expect_err("a self-referencing OFS entry cannot resolve");
    assert_invalid_format(error, "pack delta cycle detected");

    let error = sley_pack::read_pack_entry_at(&bytes, HEADER_LEN, ObjectFormat::Sha1)
        .expect_err("the entry parser rejects the self reference");
    assert_invalid_format(error, "pack delta cycle detected");
}

#[test]
fn resolve_rejects_a_base_for_an_undeltified_entry() {
    let bytes = pack(&[blob_entry()]);
    let entry = sley_pack::read_pack_entry_at(&bytes, HEADER_LEN, ObjectFormat::Sha1)
        .expect("parse blob entry");
    assert!(entry.base().is_none());
    let base = EncodedObject::new(ObjectType::Blob, b"abcd".to_vec());
    let error = entry
        .resolve(Some(&base))
        .expect_err("an undeltified entry takes no base");
    assert_invalid_format(error, "undeltified pack entry given a delta base");
}

#[test]
fn resolve_rejects_a_delta_without_its_base() {
    let blob = blob_entry();
    let delta_offset = HEADER_LEN + blob.len() as u64;
    let bytes = pack(&[blob.clone(), ofs_delta_entry(blob.len() as u8)]);
    let delta = || {
        sley_pack::read_pack_entry_at(&bytes, delta_offset, ObjectFormat::Sha1)
            .expect("parse delta entry")
    };
    assert_eq!(
        delta().base(),
        Some(&sley_pack::DeltaBase::Offset(HEADER_LEN))
    );
    let error = delta()
        .resolve(None)
        .expect_err("a delta entry needs its base");
    assert_invalid_format(error, "delta pack entry decoded without a base");

    // The matching shape resolves.
    let base = EncodedObject::new(ObjectType::Blob, b"abcd".to_vec());
    let object = delta().resolve(Some(&base)).expect("apply delta");
    assert_eq!(object.object_type, ObjectType::Blob);
    assert_eq!(object.body, b"wxyz");
}
