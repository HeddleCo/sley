// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_core::ObjectFormat;
use sley_core::Result;
use sley_odb::{FileObjectDatabase, RawPackInstallOptions, RawPackInstallResult, RawPackInstaller};
use sley_protocol::{
    PktLineFrame, ProtocolV2FetchResponseHeader, ProtocolV2FetchResponseSection,
    ProtocolV2FetchShallowInfo, SideBandChannel, demux_upload_pack_packfile_response,
    parse_sideband_packet, read_pkt_line_frame, read_protocol_v2_fetch_response_header,
    read_upload_pack_packfile_response, read_upload_pack_raw_packfile_response_header,
    read_upload_pack_shallow_info_and_raw_packfile_response_header,
    read_upload_pack_shallow_info_section,
};
use std::io::{Cursor, ErrorKind, Read};

fn raw_pack_install_options(promisor: bool, max_input_size: Option<u64>) -> RawPackInstallOptions {
    RawPackInstallOptions {
        promisor,
        max_input_size,
    }
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
    let header = read_upload_pack_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    destination.install_raw_pack_from_reader_with_options(
        &mut pack_reader,
        raw_pack_install_options(false, max_input_size),
    )
}

/// Install an upload-pack packfile response that may use side-band-64k.
///
/// Smart HTTP always delivers the pack inside sideband channel 1 after any
/// leading `ACK`/`NAK` pkt-lines (see `read_upload_pack_packfile_response`).
pub fn install_upload_pack_packfile_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_sideband_response_from_reader(
        format,
        reader,
        destination,
        false,
        max_input_size,
    )
}

pub fn install_upload_pack_packfile_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    R: Read,
{
    install_upload_pack_sideband_response_from_reader(
        format,
        reader,
        destination,
        true,
        max_input_size,
    )
}

fn install_upload_pack_sideband_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    promisor: bool,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    let response = read_upload_pack_packfile_response(format, reader)?;
    let demuxed = demux_upload_pack_packfile_response(&response)?;
    let mut pack_reader = demuxed.data.as_slice();
    let result = destination.install_raw_pack_from_reader_with_options(
        &mut pack_reader,
        raw_pack_install_options(promisor, max_input_size),
    )?;
    Ok(if promisor {
        RawPackInstallResult {
            object_ids: result.object_ids,
        }
    } else {
        result
    })
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
    let header = read_upload_pack_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_options(
        &mut pack_reader,
        raw_pack_install_options(true, max_input_size),
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
    let (shallow, header) =
        read_upload_pack_shallow_info_and_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_options(
        &mut pack_reader,
        raw_pack_install_options(false, max_input_size),
    )?;
    Ok((shallow, result))
}

/// Shallow deepen over smart HTTP: shallow-info section then a sideband pack.
pub fn install_upload_pack_shallow_packfile_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_shallow_sideband_response_from_reader(
        format,
        reader,
        destination,
        false,
        max_input_size,
    )
}

fn install_upload_pack_shallow_sideband_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    promisor: bool,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    let shallow = read_upload_pack_shallow_info_section(format, reader)?;
    let result = install_upload_pack_sideband_response_from_reader(
        format,
        reader,
        destination,
        promisor,
        max_input_size,
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
    let (shallow, header) =
        read_upload_pack_shallow_info_and_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_options(
        &mut pack_reader,
        raw_pack_install_options(true, max_input_size),
    )?;
    Ok((
        shallow,
        RawPackInstallResult {
            object_ids: result.object_ids,
        },
    ))
}

pub fn install_upload_pack_shallow_packfile_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    R: Read,
{
    install_upload_pack_shallow_sideband_response_from_reader(
        format,
        reader,
        destination,
        true,
        max_input_size,
    )
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
    let header = read_protocol_v2_fetch_response_header(format, reader, sideband_all)?;
    if !header.has_packfile {
        return Ok((header, None));
    }
    let mut pack_reader = ProtocolV2PackfileReader::new(reader);
    let result = destination.install_raw_pack_from_reader_with_options(
        &mut pack_reader,
        raw_pack_install_options(false, max_input_size),
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
    let header = read_protocol_v2_fetch_response_header(format, reader, sideband_all)?;
    if !header.has_packfile {
        return Ok((header, None));
    }
    let mut pack_reader = ProtocolV2PackfileReader::new(reader);
    let result = destination.install_raw_pack_from_reader_with_options(
        &mut pack_reader,
        raw_pack_install_options(true, max_input_size),
    )?;
    Ok((
        header,
        Some(RawPackInstallResult {
            object_ids: result.object_ids,
        }),
    ))
}

struct ProtocolV2PackfileReader<'a, R> {
    reader: &'a mut R,
    pending: Vec<u8>,
    pending_offset: usize,
    done: bool,
}

impl<'a, R> ProtocolV2PackfileReader<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            pending_offset: 0,
            done: false,
        }
    }

    fn drain_pending(&mut self, buf: &mut [u8]) -> usize {
        let available = self.pending.len().saturating_sub(self.pending_offset);
        let to_copy = available.min(buf.len());
        if to_copy == 0 {
            if self.pending_offset >= self.pending.len() {
                self.pending.clear();
                self.pending_offset = 0;
            }
            return 0;
        }
        let end = self.pending_offset + to_copy;
        buf[..to_copy].copy_from_slice(&self.pending[self.pending_offset..end]);
        self.pending_offset = end;
        if self.pending_offset == self.pending.len() {
            self.pending.clear();
            self.pending_offset = 0;
        }
        to_copy
    }
}

impl<R> Read for ProtocolV2PackfileReader<'_, R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut written = 0usize;
        while written < buf.len() {
            let copied = self.drain_pending(&mut buf[written..]);
            if copied > 0 {
                written += copied;
                continue;
            }
            if self.done {
                break;
            }
            let frame = read_pkt_line_frame(self.reader)
                .map_err(git_error_to_io)?
                .ok_or_else(|| {
                    std::io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "protocol v2 packfile ended before flush",
                    )
                })?;
            match frame {
                PktLineFrame::Data(payload) => {
                    let packet = parse_sideband_packet(&payload).map_err(git_error_to_io)?;
                    match packet.channel {
                        SideBandChannel::Data => {
                            self.pending = packet.data;
                            self.pending_offset = 0;
                        }
                        SideBandChannel::Progress => {}
                        SideBandChannel::Fatal => {
                            let message = String::from_utf8_lossy(&packet.data).into_owned();
                            return Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                format!("sideband fatal: {message}"),
                            ));
                        }
                    }
                }
                PktLineFrame::Flush => {
                    self.done = true;
                    break;
                }
                PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "protocol v2 packfile section ended unexpectedly",
                    ));
                }
            }
        }
        Ok(written)
    }
}

fn git_error_to_io(err: sley_core::GitError) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, err.to_string())
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
    use sley_core::ObjectFormat;
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
        let root = test_temp_root("sley-remote-install-upload-pack-raw-stream-install");
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
        let root = test_temp_root("sley-remote-install-upload-pack-shallow-raw-stream-install");
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

        let (shallow, result) = install_upload_pack_shallow_raw_response_from_reader(
            format,
            &mut reader,
            &destination,
            None,
        )
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
        let root = test_temp_root("sley-remote-install-upload-pack-promisor-install");
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
        let root = test_temp_root("sley-remote-install-v2-response-install");
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
        let root = test_temp_root("sley-remote-install-v2-response-promisor-install");
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
        let root = test_temp_root("sley-remote-install-v2-response-empty");
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
        let root = test_temp_root("sley-remote-install-upload-pack-max-size");
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
            err.to_string()
                .contains("pack exceeds maximum allowed size"),
            "unexpected error: {err}"
        );
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
            &mut reader,
            &installer,
            None,
        )
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
