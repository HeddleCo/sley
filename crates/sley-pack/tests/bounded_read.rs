use sley_core::ObjectFormat;
use sley_object::{EncodedObject, ObjectType};
use sley_pack::{BoundedPackDecoder, PackFile, PackReadLimits, SlicePackSource};

#[test]
fn bounded_targeted_read_matches_existing_decoder_and_reports_usage() {
    let expected = EncodedObject::new(ObjectType::Blob, vec![b'x'; 32 * 1024]);
    let written = PackFile::write_undeltified(
        std::slice::from_ref(&expected),
        ObjectFormat::Sha1,
    )
    .expect("write pack");
    let limits = PackReadLimits {
        max_delta_depth: 8,
        max_materialized_bytes: 128 * 1024,
        max_cached_bytes: 64 * 1024,
    };
    let source = SlicePackSource::new(&written.pack);
    let mut decoder =
        BoundedPackDecoder::new(source, ObjectFormat::Sha1, limits).expect("open decoder");

    let decoded = decoder
        .read_object_at(written.entries[0].offset, |_| Ok(None))
        .expect("decode targeted object");

    assert_eq!(*decoded.object, expected);
    assert!(decoded.stats.compressed_bytes_read > 0);
    assert!(decoded.stats.peak_materialized_bytes <= limits.max_materialized_bytes);
    assert!(decoded.stats.cached_bytes <= limits.max_cached_bytes);
}
