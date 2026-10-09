//! Hand-built packs whose delta chains are far deeper than any stack-bound
//! reader could follow.
//!
//! Every object is a 4-byte blob holding its position in the chain. Each delta
//! replaces its base's four bytes, so the deepest object's body is `DEPTH`.

use flate2::{Compression, write::ZlibEncoder};
use sley_core::{ObjectFormat, ObjectId, digest_bytes};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{PackIndex, PackIndexEntry};
use std::io::Write;

pub const DEPTH: usize = 10_000;

/// Objects per pack before a mixed chain hops to the other pack.
const RUN: usize = 100;

pub struct ChainPack {
    pub bytes: Vec<u8>,
    pub entries: Vec<PackIndexEntry>,
}

/// How an entry refers to its base.
pub enum Base {
    None,
    Ofs(u64),
    Ref(ObjectId),
}

fn blob_oid(number: usize) -> ObjectId {
    EncodedObject::new(ObjectType::Blob, (number as u32).to_be_bytes().to_vec())
        .object_id(ObjectFormat::Sha1)
        .expect("hash fixture object")
}

impl ChainPack {
    fn new() -> Self {
        Self {
            bytes: b"PACK\0\0\0\x02\0\0\0\0".to_vec(),
            entries: Vec::new(),
        }
    }

    /// Append blob `number`, indexed under `oid`, as a delta on `base`.
    fn push_as(&mut self, number: usize, oid: ObjectId, base: Base) {
        let offset = self.bytes.len() as u64;
        let body = (number as u32).to_be_bytes();
        let mut stream = Vec::new();
        if !matches!(base, Base::None) {
            // Base size 4, result size 4, insert the 4 new bytes.
            stream.extend_from_slice(&[4, 4, 4]);
        }
        match base {
            // Type 3 (blob), size 4.
            Base::None => self.bytes.push(0x34),
            Base::Ofs(base_offset) => {
                // Type 6 (OFS delta), size 7.
                self.bytes.push(0x67);
                let mut distance = offset - base_offset;
                let mut encoded = vec![(distance & 0x7f) as u8];
                while distance > 0x7f {
                    distance = (distance >> 7) - 1;
                    encoded.push(0x80 | (distance & 0x7f) as u8);
                }
                self.bytes.extend(encoded.into_iter().rev());
            }
            Base::Ref(base_oid) => {
                // Type 7 (REF delta), size 7.
                self.bytes.push(0x77);
                self.bytes.extend_from_slice(base_oid.as_bytes());
            }
        }
        stream.extend_from_slice(&body);
        let mut zlib = ZlibEncoder::new(Vec::new(), Compression::fast());
        zlib.write_all(&stream).expect("compress fixture entry");
        self.bytes
            .extend(zlib.finish().expect("finish fixture entry"));
        self.entries.push(PackIndexEntry {
            oid,
            offset,
            crc32: crc32fast::hash(&self.bytes[offset as usize..]),
        });
    }

    fn push(&mut self, number: usize, base: Base) {
        self.push_as(number, blob_oid(number), base);
    }

    fn last(&self) -> &PackIndexEntry {
        self.entries.last().expect("fixture entry")
    }

    fn finish(&mut self) {
        self.bytes[8..12].copy_from_slice(&(self.entries.len() as u32).to_be_bytes());
        let checksum = digest_bytes(ObjectFormat::Sha1, &self.bytes).expect("hash pack");
        self.bytes.extend_from_slice(checksum.as_bytes());
    }

    pub fn index(&self) -> Vec<u8> {
        let checksum = ObjectId::from_raw(ObjectFormat::Sha1, &self.bytes[self.bytes.len() - 20..])
            .expect("pack checksum");
        PackIndex::write_v2(ObjectFormat::Sha1, &self.entries, &checksum).expect("write index")
    }
}

/// One pack: blob 0, then `DEPTH` OFS deltas each on the previous entry.
pub fn ofs_chain() -> ChainPack {
    let mut pack = ChainPack::new();
    pack.push(0, Base::None);
    for number in 1..=DEPTH {
        let base = pack.last().offset;
        pack.push(number, Base::Ofs(base));
    }
    pack.finish();
    pack
}

/// Two packs: every object is a REF delta on the previous object, which is
/// always in the other pack. The deepest object is in pack 0.
pub fn cross_pack_ref_chain() -> [ChainPack; 2] {
    let mut packs = [ChainPack::new(), ChainPack::new()];
    packs[0].push(0, Base::None);
    for number in 1..=DEPTH {
        packs[number % 2].push(number, Base::Ref(blob_oid(number - 1)));
    }
    packs.iter_mut().for_each(ChainPack::finish);
    packs
}

/// Two packs alternating every `RUN` objects: OFS deltas inside a run, and a
/// REF delta onto the other pack's OFS-delta tip where a run starts.
pub fn mixed_cross_pack_chain() -> [ChainPack; 2] {
    let mut packs = [ChainPack::new(), ChainPack::new()];
    packs[0].push(0, Base::None);
    for number in 1..=DEPTH {
        let pack = (number / RUN) % 2;
        let base = if number % RUN == 0 {
            Base::Ref(blob_oid(number - 1))
        } else {
            Base::Ofs(packs[pack].last().offset)
        };
        packs[pack].push(number, base);
    }
    packs.iter_mut().for_each(ChainPack::finish);
    packs
}

/// Two packs whose only objects are REF deltas on each other.
pub fn cross_pack_ref_cycle() -> ([ChainPack; 2], ObjectId) {
    let first = blob_oid(DEPTH + 1);
    let second = blob_oid(DEPTH + 2);
    let mut packs = [ChainPack::new(), ChainPack::new()];
    packs[0].push_as(1, first, Base::Ref(second));
    packs[1].push_as(2, second, Base::Ref(first));
    packs.iter_mut().for_each(ChainPack::finish);
    (packs, first)
}

/// The oid of the deepest object in every chain above.
pub fn deepest_oid() -> ObjectId {
    blob_oid(DEPTH)
}

pub fn assert_deep_object(object: &EncodedObject) {
    assert_eq!(object.object_type, ObjectType::Blob);
    assert_eq!(object.body, (DEPTH as u32).to_be_bytes());
}
