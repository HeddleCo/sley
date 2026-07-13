use std::io::{Cursor, Read};

use sley::ObjectFormat;
use sley::pack::{
    PackReadStream, index_pack_from_reader, index_pack_from_reader_to_trailer,
    index_pack_from_reader_to_trailer_with_progress, index_pack_from_stream,
    index_pack_from_stream_with_progress,
};
use sley::protocol::{
    SideBandChannel, SideBandPacket, StreamingSidebandReader, write_sideband_packet,
};

fn empty_sha1_pack() -> Vec<u8> {
    let mut pack = b"PACK\0\0\0\x02\0\0\0\0".to_vec();
    pack.extend_from_slice(&[
        0x02, 0x9d, 0x08, 0x82, 0x3b, 0xd8, 0xa8, 0xea, 0xb5, 0x10, 0xad, 0x6a, 0xc7, 0x5c, 0x82,
        0x3c, 0xfd, 0x3e, 0xd3, 0x1e,
    ]);
    pack
}

#[test]
fn public_streaming_primitives_index_and_demux_without_a_repository() {
    let pack = empty_sha1_pack();
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

    let mut seekable_pack = Cursor::new(demuxed_pack);
    let build = index_pack_from_reader(&mut seekable_pack, ObjectFormat::Sha1)
        .expect("seekable pack should index");
    assert!(build.entries.is_empty());
    assert_eq!(
        build.pack_checksum.to_hex(),
        "029d08823bd8a8eab510ad6ac75c823cfd3ed31e"
    );
    assert!(!build.index.is_empty());

    let mut trailer_reader = Cursor::new(pack.as_slice());
    index_pack_from_reader_to_trailer(&mut trailer_reader, ObjectFormat::Sha1)
        .expect("trailer-delimited pack should index");

    let mut progress_reader = Cursor::new(pack.as_slice());
    index_pack_from_reader_to_trailer_with_progress(
        &mut progress_reader,
        ObjectFormat::Sha1,
        |_| {},
    )
    .expect("trailer-delimited pack should index with progress");

    let mut stream_reader = Cursor::new(pack.as_slice());
    let stream = PackReadStream::new(
        &mut stream_reader,
        ObjectFormat::Sha1,
        Some(pack.len() as u64),
    )
    .expect("bounded pack stream should construct");
    index_pack_from_stream(stream, ObjectFormat::Sha1).expect("prepared pack stream should index");

    let mut stream_reader = Cursor::new(pack.as_slice());
    let stream = PackReadStream::new(
        &mut stream_reader,
        ObjectFormat::Sha1,
        Some(pack.len() as u64),
    )
    .expect("bounded pack stream should construct");
    index_pack_from_stream_with_progress(stream, ObjectFormat::Sha1, |_| {})
        .expect("prepared pack stream should index with progress");
}
