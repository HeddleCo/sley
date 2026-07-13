// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_core::{Cancel, CancelFlag, ObjectFormat, Result};
use sley_odb::{FileObjectDatabase, RawPackInstallOptions, RawPackInstallResult, RawPackInstaller};
use sley_protocol::{
    ProtocolV2FetchResponseHeader, ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo,
    StreamingSidebandReader, read_protocol_v2_fetch_response_header,
    read_upload_pack_raw_packfile_response_header,
    read_upload_pack_shallow_info_and_raw_packfile_response_header,
};
use std::io::{Cursor, Read};

fn raw_pack_install_options(
    promisor: bool,
    max_input_size: Option<u64>,
) -> RawPackInstallOptions {
    RawPackInstallOptions {
        promisor,
        max_input_size,
    }
}

/// Map sideband `Read` failures back to the GitError variants the buffered
/// demux path used, so fetch/clone diagnostics stay parity-stable.
fn map_sideband_stream_io_error(err: std::io::Error) -> sley_core::GitError {
    let message = err.to_string();
    if message.contains("sideband fatal:")
        || message.contains("sideband stream")
        || message.contains("side-band")
        || message.contains("pkt-line")
    {
        // demux_sideband_packets / parse_sideband used InvalidFormat for these.
        sley_core::GitError::InvalidFormat(message)
    } else if err.kind() == std::io::ErrorKind::Interrupted && message.contains("cancelled") {
        sley_core::GitError::Cancelled
    } else {
        sley_core::GitError::from(err)
    }
}

fn map_sideband_install_error(err: sley_core::GitError) -> sley_core::GitError {
    match err {
        sley_core::GitError::Io(message)
            if message.contains("sideband fatal:")
                || message.contains("sideband stream")
                || message.contains("side-band") =>
        {
            sley_core::GitError::InvalidFormat(message)
        }
        other => other,
    }
}

/// Install a demuxed protocol-v2 packfile section via streaming sideband.
///
/// The v2 packfile section is pure sideband (no upload-pack ACK preamble), so
/// we do **not** call [`StreamingSidebandReader::skip_upload_pack_acks`].
fn install_protocol_v2_packfile_section_with_cancel<I, R, C>(
    reader: &mut R,
    destination: &I,
    promisor: bool,
    max_input_size: Option<u64>,
    cancel: &CancelFlag<C>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
    C: Cancel,
{
    // Stream demux sideband channel-1 bytes as frames arrive so pack install
    // can cancel mid-transfer without buffering the full response. Channel-2
    // progress is ignored here (callers can wrap the installer for progress).
    //
    // Parity with the old ProtocolV2PackfileReader + buffer-then-install path:
    // - after the pack trailer is complete the indexer stops reading, but the
    //   wire may still carry trailing progress frames + flush — drain them;
    // - sideband fatal/protocol errors surface as InvalidFormat (not bare Io).
    let mut pack_reader = StreamingSidebandReader::new(reader, |_: &[u8]| {});
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(promisor, max_input_size),
        cancel,
        |_| {},
    );
    let drain = pack_reader
        .drain_to_end()
        .map_err(map_sideband_stream_io_error);
    let result = match (result, drain) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(drain_err)) => Err(drain_err),
        (Err(install_err), _) => Err(map_sideband_install_error(install_err)),
    }?;
    Ok(if promisor {
        RawPackInstallResult {
            object_ids: result.object_ids,
        }
    } else {
        result
    })
}

pub fn install_upload_pack_raw_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_raw_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        &CancelFlag::never(),
    )
}

/// Raw (non-sideband) upload-pack pack install with cooperative cancellation.
pub fn install_upload_pack_raw_response_from_reader_with_cancel<I, R, C>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: &CancelFlag<C>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
    C: Cancel,
{
    let header = read_upload_pack_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(false, max_input_size),
        cancel,
        |_| {},
    )
}

pub fn install_upload_pack_raw_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    R: Read,
{
    install_upload_pack_raw_promisor_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        &CancelFlag::never(),
    )
}

/// Promisor raw upload-pack install with cooperative cancellation.
pub fn install_upload_pack_raw_promisor_response_from_reader_with_cancel<R, C>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: &CancelFlag<C>,
) -> Result<RawPackInstallResult>
where
    R: Read,
    C: Cancel,
{
    let header = read_upload_pack_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(true, max_input_size),
        cancel,
        |_| {},
    )?;
    Ok(RawPackInstallResult {
        object_ids: result.object_ids,
    })
}

pub fn install_upload_pack_shallow_raw_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_shallow_raw_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        &CancelFlag::never(),
    )
}

/// Shallow raw upload-pack install with cooperative cancellation.
pub fn install_upload_pack_shallow_raw_response_from_reader_with_cancel<I, R, C>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: &CancelFlag<C>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
    C: Cancel,
{
    let (shallow, header) =
        read_upload_pack_shallow_info_and_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(false, max_input_size),
        cancel,
        |_| {},
    )?;
    Ok((shallow, result))
}

pub fn install_upload_pack_shallow_raw_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    R: Read,
{
    install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        &CancelFlag::never(),
    )
}

/// Shallow promisor raw upload-pack install with cooperative cancellation.
pub fn install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel<R, C>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: &CancelFlag<C>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    R: Read,
    C: Cancel,
{
    let (shallow, header) =
        read_upload_pack_shallow_info_and_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(true, max_input_size),
        cancel,
        |_| {},
    )?;
    Ok((
        shallow,
        RawPackInstallResult {
            object_ids: result.object_ids,
        },
    ))
}

pub fn install_protocol_v2_fetch_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    I: RawPackInstaller,
    R: Read,
{
    install_protocol_v2_fetch_response_from_reader_with_cancel(
        format,
        reader,
        sideband_all,
        destination,
        max_input_size,
        &CancelFlag::never(),
    )
}

/// Protocol v2 fetch response install with cooperative cancellation.
///
/// Demuxes the packfile section with [`StreamingSidebandReader`] (no ACK skip;
/// v2 pack sections have no upload-pack ACK preamble) and threads `cancel` into
/// the destination installer.
pub fn install_protocol_v2_fetch_response_from_reader_with_cancel<I, R, C>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: &CancelFlag<C>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    I: RawPackInstaller,
    R: Read,
    C: Cancel,
{
    let header = read_protocol_v2_fetch_response_header(format, reader, sideband_all)?;
    if !header.has_packfile {
        return Ok((header, None));
    }
    let result = install_protocol_v2_packfile_section_with_cancel(
        reader,
        destination,
        false,
        max_input_size,
        cancel,
    )?;
    Ok((header, Some(result)))
}

pub fn install_protocol_v2_fetch_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    R: Read,
{
    install_protocol_v2_fetch_promisor_response_from_reader_with_cancel(
        format,
        reader,
        sideband_all,
        destination,
        max_input_size,
        &CancelFlag::never(),
    )
}

/// Protocol v2 promisor fetch response install with cooperative cancellation.
pub fn install_protocol_v2_fetch_promisor_response_from_reader_with_cancel<R, C>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: &CancelFlag<C>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    R: Read,
    C: Cancel,
{
    let header = read_protocol_v2_fetch_response_header(format, reader, sideband_all)?;
    if !header.has_packfile {
        return Ok((header, None));
    }
    let result = install_protocol_v2_packfile_section_with_cancel(
        reader,
        destination,
        true,
        max_input_size,
        cancel,
    )?;
    Ok((header, Some(result)))
}

pub fn shallow_info_from_protocol_v2_fetch_header(
    header: &ProtocolV2FetchResponseHeader,
) -> Vec<ProtocolV2FetchShallowInfo> {
    let mut shallow_info = Vec::new();
    for section in &header.sections {
        if let ProtocolV2FetchResponseSection::ShallowInfo(entries) = section {
            shallow_info.extend(entries.clone());
        }
    }
    shallow_info
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::{AtomicCancel, CancelFlag, GitError, ObjectFormat};
    use sley_object::{EncodedObject, ObjectType};
    use sley_odb::{FileObjectDatabase, ObjectDatabase, ObjectReader};
    use sley_pack::PackFile;
    use sley_protocol::{
        SideBandChannel, SideBandPacket, UploadPackAcknowledgment, UploadPackRawPackfileResponse,
        encode_sideband_packet, encode_upload_pack_raw_packfile_response, write_pkt_line_payload,
        write_protocol_v2_fetch_response, write_upload_pack_raw_packfile_response,
    };
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn raw_upload_pack_response_stream_installs_pack_without_buffering_response() {
        let root = test_temp_root("sley-fetch-upload-pack-raw-stream-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"raw streamed upload-pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            packfile: pack.pack,
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let result =
            install_upload_pack_raw_response_from_reader(format, &mut reader, &destination, None)
                .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shallow_raw_upload_pack_response_stream_installs_pack_without_buffering_response() {
        let root = test_temp_root("sley-fetch-upload-pack-shallow-raw-stream-install");
        let format = ObjectFormat::Sha1;
        let shallow_oid =
            sley_core::ObjectId::from_hex(format, "1111111111111111111111111111111111111111")
                .expect("test operation should succeed");
        let object =
            EncodedObject::new(ObjectType::Blob, b"shallow streamed upload-pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            packfile: pack.pack,
        };
        let mut encoded = Vec::new();
        write_pkt_line_payload(&mut encoded, format!("shallow {shallow_oid}\n").as_bytes())
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"0000");
        write_upload_pack_raw_packfile_response(&mut encoded, &response)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let (shallow, result) =
            install_upload_pack_shallow_raw_response_from_reader(format, &mut reader, &destination, None)
                .expect("test operation should succeed");

        assert_eq!(
            shallow,
            vec![ProtocolV2FetchShallowInfo::Shallow(shallow_oid)]
        );
        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn raw_upload_pack_response_installs_promisor_pack_sidecar() {
        let root = test_temp_root("sley-fetch-upload-pack-promisor-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"promisor upload-pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let mut encoded = Vec::new();
        write_upload_pack_raw_packfile_response(&mut encoded, &response)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let result = install_upload_pack_raw_promisor_response_from_reader(
            format,
            &mut reader,
            &destination,
            None,
        )
        .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        assert_promisor_sidecar(&root.join("objects"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_response_packfile_demuxes_and_installs_pack() {
        let root = test_temp_root("sley-fetch-v2-response-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"v2 response packfile\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let sections = vec![ProtocolV2FetchResponseSection::Packfile(vec![
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Progress,
                data: b"counting objects\n".to_vec(),
            })
            .expect("test operation should succeed"),
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: pack.pack,
            })
            .expect("test operation should succeed"),
        ])];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let (header, result) = install_protocol_v2_fetch_response_from_reader(
            format,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect("test operation should succeed");
        let result = result.expect("packfile should be installed");

        assert!(header.has_packfile);
        assert!(header.sections.is_empty());
        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_response_packfile_installs_promisor_sidecar() {
        let root = test_temp_root("sley-fetch-v2-response-promisor-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"v2 promisor packfile\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let sections = vec![ProtocolV2FetchResponseSection::Packfile(vec![
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: pack.pack,
            })
            .expect("test operation should succeed"),
        ])];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let (header, result) = install_protocol_v2_fetch_promisor_response_from_reader(
            format,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect("test operation should succeed");
        let result = result.expect("packfile should be installed");

        assert!(header.has_packfile);
        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        assert_promisor_sidecar(&root.join("objects"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_response_without_packfile_installs_nothing() {
        let root = test_temp_root("sley-fetch-v2-response-empty");
        let destination = FileObjectDatabase::new(root.join("objects"), ObjectFormat::Sha1);
        let sections = vec![ProtocolV2FetchResponseSection::Acknowledgments(Vec::new())];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let mut reader = encoded.as_slice();

        let (header, result) = install_protocol_v2_fetch_response_from_reader(
            ObjectFormat::Sha1,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect("test operation should succeed");

        assert!(!header.has_packfile);
        assert!(result.is_none());
        assert!(!root.join("objects").join("pack").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn raw_upload_pack_response_rejects_pack_exceeding_max_input_size() {
        let root = test_temp_root("sley-fetch-upload-pack-max-size");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"oversized fetch pack\n".to_vec());
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();
        let limit = 32u64;

        let err = install_upload_pack_raw_response_from_reader(
            format,
            &mut reader,
            &destination,
            Some(limit),
        )
        .expect_err("oversized pack should be rejected");

        assert!(
            err.to_string().contains("pack exceeds maximum allowed size"),
            "unexpected error: {err}"
        );
        let pack_dir = root.join("objects").join("pack");
        let installed = fs::read_dir(&pack_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry
                            .path()
                            .extension()
                            .and_then(|ext| ext.to_str())
                            == Some("pack")
                    })
                    .count()
            })
            .unwrap_or_default();
        assert_eq!(installed, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn install_helpers_accept_custom_raw_pack_installer() {
        #[derive(Default)]
        struct RecordingInstaller {
            packs: std::cell::RefCell<Vec<Vec<u8>>>,
        }

        impl RawPackInstaller for RecordingInstaller {
            fn install_raw_pack_from_reader_with_options<R>(
                &self,
                reader: &mut R,
                _options: RawPackInstallOptions,
            ) -> Result<RawPackInstallResult>
            where
                R: Read,
            {
                let mut pack_bytes = Vec::new();
                reader.read_to_end(&mut pack_bytes)?;
                self.packs.borrow_mut().push(pack_bytes.to_vec());
                Ok(RawPackInstallResult {
                    object_ids: Vec::new(),
                })
            }
        }

        let installer = RecordingInstaller::default();
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: b"PACKcustom".to_vec(),
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let mut reader = encoded.as_slice();

        let result = install_upload_pack_raw_response_from_reader(
            ObjectFormat::Sha1,
            &mut reader, &installer, None)
        .expect("test operation should succeed");

        assert!(result.object_ids.is_empty());
        assert_eq!(installer.packs.into_inner(), vec![b"PACKcustom".to_vec()]);
    }

    #[test]
    fn raw_upload_pack_response_installs_into_in_memory_database() {
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"in memory fetch pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let destination = ObjectDatabase::new(format);
        let mut reader = encoded.as_slice();

        let result =
            install_upload_pack_raw_response_from_reader(format, &mut reader, &destination, None)
                .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_eq!(
            destination
                .read_object(&oid)
                .expect("test operation should succeed")
                .as_ref(),
            &object
        );
    }

    fn encode_chunked_v2_packfile(pack_bytes: &[u8], chunk_size: usize) -> Vec<u8> {
        let mut sideband = vec![SideBandPacket {
            channel: SideBandChannel::Progress,
            data: b"counting objects\n".to_vec(),
        }];
        for chunk in pack_bytes.chunks(chunk_size.max(1)) {
            sideband.push(SideBandPacket {
                channel: SideBandChannel::Data,
                data: chunk.to_vec(),
            });
        }
        // Trailing progress after the pack data forces drain_to_end to consume
        // remaining frames after the pack trailer is complete.
        sideband.push(SideBandPacket {
            channel: SideBandChannel::Progress,
            data: b"done\n".to_vec(),
        });
        let sections = vec![ProtocolV2FetchResponseSection::Packfile(
            sideband
                .iter()
                .map(|packet| encode_sideband_packet(packet).expect("sideband should encode"))
                .collect(),
        )];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("response should encode");
        encoded
    }

    #[test]
    fn protocol_v2_chunked_sideband_stream_installs_pack_without_buffering_response() {
        let root = test_temp_root("sley-fetch-v2-chunked-stream-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"v2 chunked stream pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        // Chunk the pack across many sideband frames so install cannot rely on a
        // single demuxed buffer.
        let encoded = encode_chunked_v2_packfile(&pack.pack, 32);
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let (header, result) = install_protocol_v2_fetch_response_from_reader(
            format,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect("test operation should succeed");
        let result = result.expect("packfile should be installed");

        assert!(header.has_packfile);
        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        // Full response consumed (trailing progress + flush drained).
        assert!(reader.is_empty(), "stream should be fully drained");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_sideband_cancel_mid_stream() {
        let root = test_temp_root("sley-fetch-v2-sideband-cancel");
        let format = ObjectFormat::Sha1;
        let objects: Vec<EncodedObject> = (0..12)
            .map(|i| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("v2 cancel fixture {i}\n").into_bytes(),
                )
            })
            .collect();
        let pack = PackFile::write_undeltified(&objects, format).expect("pack should encode");
        let encoded = encode_chunked_v2_packfile(&pack.pack, 48);
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let source = AtomicCancel::new();
        let mut reader = encoded.as_slice();
        // Trip cancel from pack-indexer progress after the first object so the
        // public with_cancel path observes mid-stream cooperative cancel.
        struct CancelOnProgressInstaller<'a> {
            inner: &'a FileObjectDatabase,
            source: &'a AtomicCancel,
            saw_object: std::cell::Cell<bool>,
        }

        impl RawPackInstaller for CancelOnProgressInstaller<'_> {
            fn install_raw_pack_from_reader_with_options<R>(
                &self,
                reader: &mut R,
                options: RawPackInstallOptions,
            ) -> Result<RawPackInstallResult>
            where
                R: Read,
            {
                // Route through the trait impl so PackInstallResult is mapped to
                // RawPackInstallResult (FileObjectDatabase inherent methods differ).
                RawPackInstaller::install_raw_pack_from_reader_with_options(
                    self.inner, reader, options,
                )
            }

            fn install_raw_pack_from_reader_with_progress_and_cancel<R, F, C>(
                &self,
                reader: &mut R,
                options: RawPackInstallOptions,
                cancel: &CancelFlag<C>,
                mut progress: F,
            ) -> Result<RawPackInstallResult>
            where
                R: Read,
                F: FnMut(sley_odb::PackStreamProgress),
                C: Cancel,
            {
                RawPackInstaller::install_raw_pack_from_reader_with_progress_and_cancel(
                    self.inner,
                    reader,
                    options,
                    cancel,
                    |p| {
                        if p.received_objects >= 1 {
                            self.saw_object.set(true);
                            self.source.cancel();
                        }
                        progress(p);
                    },
                )
            }
        }

        let installer = CancelOnProgressInstaller {
            inner: &destination,
            source: &source,
            saw_object: std::cell::Cell::new(false),
        };
        let err = install_protocol_v2_fetch_response_from_reader_with_cancel(
            format,
            &mut reader,
            false,
            &installer,
            None,
            &CancelFlag::new(&source),
        )
        .expect_err("mid-stream cancel should fail");

        assert!(
            installer.saw_object.get(),
            "progress should report at least one object before cancel"
        );
        assert_eq!(err, GitError::Cancelled);
        let pack_dir = root.join("objects").join("pack");
        let installed = fs::read_dir(&pack_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack")
                    })
                    .count()
            })
            .unwrap_or_default();
        assert_eq!(installed, 0, "cancelled install must not leave pack files");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_sideband_fatal_maps_to_invalid_format() {
        let root = test_temp_root("sley-fetch-v2-sideband-fatal");
        let format = ObjectFormat::Sha1;
        let sections = vec![ProtocolV2FetchResponseSection::Packfile(vec![
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Fatal,
                data: b"server error\n".to_vec(),
            })
            .expect("test operation should succeed"),
        ])];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let err = install_protocol_v2_fetch_response_from_reader(
            format,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect_err("sideband fatal should fail");

        match err {
            GitError::InvalidFormat(message) => {
                assert!(
                    message.contains("sideband fatal"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn raw_upload_pack_response_cancel_before_install() {
        let root = test_temp_root("sley-fetch-raw-cancel");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"raw cancel fixture\n".to_vec());
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();
        let source = AtomicCancel::new();
        source.cancel();

        let err = install_upload_pack_raw_response_from_reader_with_cancel(
            format,
            &mut reader,
            &destination,
            None,
            &CancelFlag::new(&source),
        )
        .expect_err("pre-canceled install should fail");

        assert_eq!(err, GitError::Cancelled);
        let _ = fs::remove_dir_all(&root);
    }

    fn test_temp_root(prefix: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            TEST_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn assert_pack_install(
        objects_dir: &Path,
        db: &FileObjectDatabase,
        oid: &sley_core::ObjectId,
        object: &EncodedObject,
    ) {
        assert!(
            !db.loose()
                .object_path(oid)
                .expect("test operation should succeed")
                .exists()
        );
        let pack_dir = objects_dir.join("pack");
        let packs = fs::read_dir(&pack_dir)
            .expect("test operation should succeed")
            .map(|entry| entry.expect("test operation should succeed").path())
            .collect::<Vec<_>>();
        assert!(
            packs
                .iter()
                .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("pack"))
        );
        assert!(
            packs
                .iter()
                .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("idx"))
        );
        assert!(db.contains(oid).expect("test operation should succeed"));
        assert_eq!(
            db.read_object(oid)
                .expect("test operation should succeed")
                .as_ref(),
            object
        );
    }

    fn assert_promisor_sidecar(objects_dir: &Path) {
        let pack_dir = objects_dir.join("pack");
        let promisors = fs::read_dir(&pack_dir)
            .expect("test operation should succeed")
            .map(|entry| entry.expect("test operation should succeed").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("promisor"))
            .collect::<Vec<_>>();
        assert_eq!(promisors.len(), 1);
        assert_eq!(
            fs::read(&promisors[0]).expect("test operation should succeed"),
            b""
        );
    }
}
