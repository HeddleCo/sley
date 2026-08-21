use std::io::{Cursor, Read};

use sley::ObjectFormat;
use sley::pack::{PackIndex, PackIndexOptions};
use sley::protocol::{
    SideBandChannel, SideBandPacket, StreamingSidebandReader, write_sideband_packet,
};

fn single_blob_sha1_pack() -> Vec<u8> {
    vec![
        0x50, 0x41, 0x43, 0x4b, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x36, 0x78, 0x9c,
        0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0xe7, 0x02, 0x00, 0x08, 0x4b, 0x02, 0x1f, 0xde, 0x04, 0x12,
        0x40, 0x1f, 0x4a, 0x9e, 0x5f, 0x05, 0x41, 0x1f, 0x44, 0xea, 0xf9, 0xc8, 0x6d, 0x46, 0x09,
        0x67, 0x46,
    ]
}

#[test]
fn public_parallel_pack_primitives_index_and_demux_without_a_repository() {
    let pack = single_blob_sha1_pack();
    let mut response = Vec::new();
    write_sideband_packet(
        &mut response,
        &SideBandPacket {
            channel: SideBandChannel::Data,
            data: pack.clone(),
        },
    )
    .expect("sideband frame should encode");
    response.extend_from_slice(b"0000");

    let mut sideband = StreamingSidebandReader::new(Cursor::new(response), |_: &[u8]| {});
    let mut demuxed_pack = Vec::new();
    sideband
        .read_to_end(&mut demuxed_pack)
        .expect("band-1 pack bytes should demux");
    sideband
        .drain_to_end()
        .expect("sideband stream should end at flush");
    assert_eq!(demuxed_pack, pack);

    let build = PackIndex::write_v2_for_pack_with_options(
        &demuxed_pack,
        ObjectFormat::Sha1,
        |_| Ok(None),
        PackIndexOptions::default().with_threads(8),
        sley::CancelFlag::never(),
        |_| {},
    )
    .expect("immutable pack bytes should index in parallel");
    assert_eq!(build.entries.len(), 1);
    assert_eq!(build.objects.len(), 1);
    assert_eq!(
        build.entries[0].oid.to_hex(),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
    assert_eq!(build.objects[0].oid, build.entries[0].oid);
    assert_eq!(build.objects[0].object_type.as_str(), "blob");
    assert_eq!(build.objects[0].size, 6);
    assert_eq!(
        build.pack_checksum.to_hex(),
        "de0412401f4a9e5f05411f44eaf9c86d46096746"
    );
    assert!(!build.index.is_empty());

    let one_worker = PackIndex::write_v2_for_pack_with_options(
        &pack,
        ObjectFormat::Sha1,
        |_| Ok(None),
        PackIndexOptions::default().with_threads(1),
        sley::CancelFlag::never(),
        |_| {},
    )
    .expect("one-worker scheduling should index");
    assert_eq!(one_worker, build);
}
