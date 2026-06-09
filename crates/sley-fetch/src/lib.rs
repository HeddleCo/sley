use sley_core::Result;
use sley_odb::{FileObjectDatabase, RawPackInstallOptions, RawPackInstallResult, RawPackInstaller};
use sley_protocol::{
    ProtocolV2FetchResponseSection, SideBandDemux, UploadPackRawPackfileResponse,
    demux_protocol_v2_fetch_packfile,
};

pub fn install_upload_pack_raw_response<I: RawPackInstaller>(
    response: &UploadPackRawPackfileResponse,
    destination: &I,
) -> Result<RawPackInstallResult> {
    destination.install_raw_pack(&response.packfile)
}

pub fn install_upload_pack_raw_promisor_response(
    response: &UploadPackRawPackfileResponse,
    destination: &FileObjectDatabase,
) -> Result<RawPackInstallResult> {
    let result = destination.install_raw_pack_with_options(
        &response.packfile,
        RawPackInstallOptions { promisor: true },
    )?;
    Ok(RawPackInstallResult {
        object_ids: result.object_ids,
    })
}

pub fn install_protocol_v2_fetch_packfile<I: RawPackInstaller>(
    packfile: &SideBandDemux,
    destination: &I,
) -> Result<RawPackInstallResult> {
    destination.install_raw_pack(&packfile.data)
}

pub fn install_protocol_v2_fetch_promisor_packfile(
    packfile: &SideBandDemux,
    destination: &FileObjectDatabase,
) -> Result<RawPackInstallResult> {
    let result = destination
        .install_raw_pack_with_options(&packfile.data, RawPackInstallOptions { promisor: true })?;
    Ok(RawPackInstallResult {
        object_ids: result.object_ids,
    })
}

pub fn install_protocol_v2_fetch_response_packfile<I: RawPackInstaller>(
    sections: &[ProtocolV2FetchResponseSection],
    destination: &I,
) -> Result<Option<RawPackInstallResult>> {
    demux_protocol_v2_fetch_packfile(sections)?
        .map(|packfile| install_protocol_v2_fetch_packfile(&packfile, destination))
        .transpose()
}

pub fn install_protocol_v2_fetch_response_promisor_packfile(
    sections: &[ProtocolV2FetchResponseSection],
    destination: &FileObjectDatabase,
) -> Result<Option<RawPackInstallResult>> {
    demux_protocol_v2_fetch_packfile(sections)?
        .map(|packfile| install_protocol_v2_fetch_promisor_packfile(&packfile, destination))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;
    use sley_object::{EncodedObject, ObjectType};
    use sley_odb::{FileObjectDatabase, ObjectDatabase, ObjectReader};
    use sley_pack::PackFile;
    use sley_protocol::{SideBandChannel, SideBandPacket, encode_sideband_packet};
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn raw_upload_pack_response_installs_pack_without_loose_objects() {
        let root = test_temp_root("sley-fetch-upload-pack-raw-install");
        let format = ObjectFormat::Sha256;
        let object = EncodedObject::new(ObjectType::Blob, b"raw upload-pack boundary\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let result = install_upload_pack_raw_response(&response, &destination)
            .expect("test operation should succeed");

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
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let result = install_upload_pack_raw_promisor_response(&response, &destination)
            .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        assert_promisor_sidecar(&root.join("objects"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_packfile_installs_pack_without_loose_objects() {
        let root = test_temp_root("sley-fetch-v2-packfile-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(
            ObjectType::Blob,
            b"protocol v2 packfile boundary\n".to_vec(),
        );
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let packfile = SideBandDemux {
            data: pack.pack,
            progress: vec![b"counting objects\n".to_vec()],
        };
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let result = install_protocol_v2_fetch_packfile(&packfile, &destination)
            .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
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
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let result = install_protocol_v2_fetch_response_packfile(&sections, &destination)
            .expect("test operation should succeed")
            .expect("packfile should be installed");

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
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let result = install_protocol_v2_fetch_response_promisor_packfile(&sections, &destination)
            .expect("test operation should succeed")
            .expect("packfile should be installed");

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

        let result = install_protocol_v2_fetch_response_packfile(&sections, &destination)
            .expect("test operation should succeed");

        assert!(result.is_none());
        assert!(!root.join("objects").join("pack").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn install_helpers_accept_custom_raw_pack_installer() {
        #[derive(Default)]
        struct RecordingInstaller {
            packs: std::cell::RefCell<Vec<Vec<u8>>>,
        }

        impl RawPackInstaller for RecordingInstaller {
            fn install_raw_pack(&self, pack_bytes: &[u8]) -> Result<RawPackInstallResult> {
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

        let result = install_upload_pack_raw_response(&response, &installer)
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
        let destination = std::cell::RefCell::new(ObjectDatabase::new(format));

        let result = install_upload_pack_raw_response(&response, &destination)
            .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_eq!(
            destination
                .borrow()
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
