//! Malformed pack entry headers must produce a parse error, never a panic
//! (sley#162).
//!
//! The varint readers behind `parse_entry_header` and
//! `parse_ofs_delta_base_offset` bound their cursor against the pack's total
//! length, which *includes* the trailing checksum. A varint whose continuation
//! bit never clears therefore walks the cursor into the trailer, and the
//! entry-body slice `bytes[cursor..trailer_offset]` is then built with
//! `start > end` — a panic on remote input, since packs arrive straight off the
//! wire.

use sley_core::ObjectFormat;
use sley_pack::{PackFile, read_object_at_arc, read_object_header_at};

/// Pack whose single entry has a type/size header varint that never terminates
/// before the trailing checksum begins.
///
/// The entry region is three bytes wide so the pack still satisfies the "an
/// entry cannot be smaller than three bytes" floor an object-count bound
/// checks: the cursor has to run away for a reason other than the pack being
/// too small to hold what it declares.
fn runaway_header_pack(format: ObjectFormat) -> Vec<u8> {
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&1u32.to_be_bytes());
    // Type 3 (blob) with the continuation bit set, then continuation-only
    // bytes: the varint asks for one more byte than the entry region holds.
    pack.extend_from_slice(&[0xb0, 0x80, 0x80]);
    let checksum = sley_core::digest_bytes(format, &pack).expect("digest test pack");
    pack.extend_from_slice(checksum.as_bytes());
    pack
}

/// Pack whose final entry has a well-formed header but an ofs-delta base-offset
/// varint that runs into the trailing checksum. Returns the pack and the
/// in-pack offset of that entry.
///
/// The varint's last byte is read out of the trailer, so whether it terminates
/// there — and what base offset it yields — depends on the checksum. The filler
/// length is varied until the digest produces a byte that both terminates the
/// varint and names a base this entry could legally reference, so the parse
/// reaches the body slice rather than failing earlier on an out-of-range base.
/// Every candidate is an equally well-formed pack prefix; only the digest over
/// it differs.
fn runaway_ofs_delta_base_pack(format: ObjectFormat) -> (Vec<u8>, u64) {
    for filler in 256..512 {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        pack.resize(12 + filler, 0);
        let entry_offset = pack.len() as u64;
        // Type 6 (ofs-delta), size 0 — the header varint terminates here.
        pack.push(0x60);
        // Base-offset varint with the continuation bit set: the next byte is
        // the first byte of the trailer.
        pack.push(0x80);
        let checksum = sley_core::digest_bytes(format, &pack).expect("digest test pack");
        let terminates = checksum.as_bytes()[0] & 0x80 == 0;
        pack.extend_from_slice(checksum.as_bytes());
        if terminates {
            return (pack, entry_offset);
        }
    }
    panic!("no filler length produced a terminating ofs-delta base-offset varint");
}

#[test]
fn parse_rejects_entry_header_running_past_trailer() {
    let pack = runaway_header_pack(ObjectFormat::Sha1);
    let error = PackFile::parse(&pack, ObjectFormat::Sha1)
        .expect_err("entry header running into the trailer must not parse");
    println!("parse: {error}");
}

#[test]
fn verify_pack_stats_rejects_entry_header_running_past_trailer() {
    let pack = runaway_header_pack(ObjectFormat::Sha1);
    let error = PackFile::verify_pack_stats(&pack, ObjectFormat::Sha1)
        .expect_err("entry header running into the trailer must not verify");
    println!("verify_pack_stats: {error}");
}

#[test]
fn targeted_read_rejects_entry_header_running_past_trailer() {
    let pack = runaway_header_pack(ObjectFormat::Sha1);
    let error = read_object_at_arc(&pack, 12, ObjectFormat::Sha1, |_| Ok(None))
        .expect_err("entry header running into the trailer must not decode");
    println!("read_object_at_arc: {error}");
}

#[test]
fn header_read_rejects_ofs_delta_base_running_past_trailer() {
    let (pack, offset) = runaway_ofs_delta_base_pack(ObjectFormat::Sha1);
    let error = read_object_header_at(&pack, offset, ObjectFormat::Sha1, 0, |_, _| Ok(None))
        .expect_err("ofs-delta base offset running into the trailer must not decode");
    println!("read_object_header_at: {error}");
}

#[test]
fn targeted_read_rejects_ofs_delta_base_running_past_trailer() {
    let (pack, offset) = runaway_ofs_delta_base_pack(ObjectFormat::Sha1);
    let error = read_object_at_arc(&pack, offset, ObjectFormat::Sha1, |_| Ok(None))
        .expect_err("ofs-delta base offset running into the trailer must not decode");
    println!("read_object_at_arc ofs-delta: {error}");
}
