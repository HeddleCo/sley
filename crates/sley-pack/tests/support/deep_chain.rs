use flate2::{Compression, write::ZlibEncoder};
use sley_core::{ObjectFormat, ObjectId, digest_bytes};
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{PackIndex, PackIndexEntry};
use std::io::Write;

pub const DEPTH: usize = 10_000;

pub struct ChainPack {
    pub bytes: Vec<u8>,
    pub entries: Vec<PackIndexEntry>,
}

impl ChainPack {
    fn new() -> Self {
        Self {
            bytes: b"PACK\0\0\0\x02\0\0\0\0".to_vec(),
            entries: Vec::new(),
        }
    }

    fn push(&mut self, number: usize, base: Option<(ObjectId, u64)>, ofs: bool) {
        let offset = self.bytes.len() as u64;
        let body = (number as u32).to_be_bytes();
        let oid = EncodedObject::new(ObjectType::Blob, body.to_vec())
            .object_id(ObjectFormat::Sha1)
            .expect("hash fixture object");
        let mut stream = Vec::new();
        match base {
            None => self.bytes.push(0x34),
            Some((base_oid, base_offset)) => {
                self.bytes.push(if ofs { 0x67 } else { 0x77 });
                if ofs {
                    let mut distance = offset - base_offset;
                    let mut encoded = vec![(distance & 0x7f) as u8];
                    while distance > 0x7f {
                        distance = (distance >> 7) - 1;
                        encoded.push(0x80 | (distance & 0x7f) as u8);
                    }
                    self.bytes.extend(encoded.into_iter().rev());
                } else {
                    self.bytes.extend_from_slice(base_oid.as_bytes());
                }
                stream.extend_from_slice(&[4, 4, 4]);
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

pub fn ofs_chain() -> ChainPack {
    let mut pack = ChainPack::new();
    for number in 0..=DEPTH {
        let base = pack.entries.last().map(|entry| (entry.oid, entry.offset));
        pack.push(number, base, true);
    }
    pack.finish();
    pack
}

pub fn cross_pack_ref_chain() -> [ChainPack; 2] {
    let mut packs = [ChainPack::new(), ChainPack::new()];
    let mut base = None;
    for number in 0..=DEPTH {
        let pack = &mut packs[number % 2];
        pack.push(number, base, false);
        let entry = pack.entries.last().expect("new fixture entry");
        base = Some((entry.oid, entry.offset));
    }
    for pack in &mut packs {
        pack.finish();
    }
    packs
}

pub fn assert_deep_object(object: &EncodedObject) {
    assert_eq!(object.object_type, ObjectType::Blob);
    assert_eq!(object.body, (DEPTH as u32).to_be_bytes());
}
