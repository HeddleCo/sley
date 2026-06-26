//! Pack building for push (receive-pack body generation).
//!
//! [`build_push_packfile`] and [`build_receive_pack_body`] lift the pack-planning
//! logic shared by HTTP, SSH, and local push paths so embedders can assemble a
//! receive-pack request without running the full [`crate::push::push`] flow.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use sley_core::{ObjectFormat, ObjectId, Result};
use sley_odb::{
    FileObjectDatabase, ObjectReader, collect_reachable_object_ids, write_reachable_pack_to_writer,
};
use sley_pack::{PackFile, PackInput, PackWriteOptions};
use sley_protocol::{
    ReceivePackCommand, ReceivePackFeatures, ReceivePackPushRequestOptions, RefAdvertisement,
    build_receive_pack_push_request_header, write_receive_pack_push_request_header,
};

/// The advertised tips the local repository already has, deduplicated and
/// excluding the all-zero sentinel — the safe negotiation base for the push pack.
pub fn remote_advertisement_tips_known_to_local(
    local_db: &FileObjectDatabase,
    advertisements: &[RefAdvertisement],
) -> Result<Vec<ObjectId>> {
    let mut tips = Vec::new();
    let mut seen = HashSet::new();
    for advertisement in advertisements {
        if advertisement.oid.is_null() || !seen.insert(advertisement.oid) {
            continue;
        }
        if local_db.contains(&advertisement.oid)? {
            tips.push(advertisement.oid);
        }
    }
    Ok(tips)
}

/// Inputs for building a push packfile or full receive-pack request body.
pub struct PushPackRequest<'a> {
    /// Local object database supplying objects to pack.
    pub local_db: &'a FileObjectDatabase,
    /// Object format of [`PushPackRequest::local_db`].
    pub format: ObjectFormat,
    /// Planned receive-pack ref updates (only non-null `new_id` roots are packed).
    pub commands: &'a [ReceivePackCommand],
    /// Optional explicit pack roots supplied by an embedder-authored push plan.
    /// When empty, non-null command `new_id`s are used.
    pub pack_objects: &'a [ObjectId],
    /// Remote ref advertisements used to exclude objects the remote already has.
    pub remote_advertisements: &'a [RefAdvertisement],
    /// Negotiated receive-pack features (honours [`ReceivePackFeatures::no_thin`]).
    pub features: &'a ReceivePackFeatures,
    /// Receive-pack request options (capabilities, push-options, etc.).
    pub options: ReceivePackPushRequestOptions,
    /// When `true`, omit delta bases the remote is assumed to already have.
    pub thin: bool,
}

/// Build the packfile bytes for a push, excluding objects reachable from remote
/// tips the local repository also holds.
///
/// When [`PushPackRequest::thin`] is `true` and the remote did not advertise
/// `no-thin`, reachable objects are deltified against those remote tips using
/// [`PackWriteOptions::with_thin_bases`].
pub fn build_push_packfile(req: &PushPackRequest<'_>) -> Result<Vec<u8>> {
    let mut packfile = Vec::new();
    write_push_packfile(req, &mut packfile)?;
    Ok(packfile)
}

pub fn write_push_packfile<W>(req: &PushPackRequest<'_>, writer: &mut W) -> Result<()>
where
    W: Write,
{
    let remote_excluded_tips =
        remote_advertisement_tips_known_to_local(req.local_db, req.remote_advertisements)?;
    let remote_excluded =
        collect_reachable_object_ids(req.local_db, req.format, remote_excluded_tips)?;
    let starts = push_pack_roots(req.commands, req.pack_objects);
    if starts.is_empty() {
        return Ok(());
    }

    if req.thin && !req.features.no_thin {
        write_thin_push_packfile(req, starts, &remote_excluded, writer)
    } else {
        if write_reachable_pack_to_writer(
            req.local_db,
            req.format,
            starts,
            &remote_excluded,
            writer,
        )?
        .is_none()
        {
            write_empty_packfile(req.format, writer)?;
        }
        Ok(())
    }
}

fn write_empty_packfile<W>(format: ObjectFormat, writer: &mut W) -> Result<()>
where
    W: Write,
{
    let inputs: Vec<PackInput<'_>> = Vec::new();
    PackFile::write_packed_with_known_ids_to_writer(
        &inputs,
        format,
        &PackWriteOptions::new(),
        writer,
    )?;
    Ok(())
}

pub(crate) fn push_pack_roots(
    commands: &[ReceivePackCommand],
    pack_objects: &[ObjectId],
) -> Vec<ObjectId> {
    if !pack_objects.is_empty() {
        return pack_objects.to_vec();
    }
    commands
        .iter()
        .filter(|command| !command.new_id.is_null())
        .map(|command| command.new_id)
        .collect()
}

fn write_thin_push_packfile<W>(
    req: &PushPackRequest<'_>,
    starts: Vec<sley_core::ObjectId>,
    remote_excluded: &HashSet<sley_core::ObjectId>,
    writer: &mut W,
) -> Result<()>
where
    W: Write,
{
    let reachable = collect_reachable_object_ids(req.local_db, req.format, starts)?;
    let to_send = reachable
        .into_iter()
        .filter(|oid| !remote_excluded.contains(oid))
        .collect::<Vec<_>>();
    if to_send.is_empty() {
        return Ok(());
    }

    let mut thin_bases = HashMap::with_capacity(remote_excluded.len());
    for oid in remote_excluded {
        let object = req.local_db.read_object(oid)?;
        thin_bases.insert(*oid, (*object).clone());
    }

    let mut oids = Vec::with_capacity(to_send.len());
    let mut owned_objects = Vec::with_capacity(to_send.len());
    for oid in to_send {
        let object = req.local_db.read_object(&oid)?;
        owned_objects.push((*object).clone());
        oids.push(oid);
    }
    let inputs = oids
        .iter()
        .zip(&owned_objects)
        .map(|(oid, object)| PackInput { oid, object })
        .collect::<Vec<_>>();

    let options = PackWriteOptions::new()
        .with_thin_bases(thin_bases)
        .with_prefer_ofs_delta(req.options.ofs_delta);
    PackFile::write_packed_with_known_ids_to_writer(&inputs, req.format, &options, writer)?;
    Ok(())
}

/// Build a complete receive-pack push request body: planned commands, negotiated
/// capabilities, optional push-options, and the packfile from [`build_push_packfile`].
pub fn build_receive_pack_body(req: &PushPackRequest<'_>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    write_receive_pack_body(req, &mut body)?;
    Ok(body)
}

pub fn write_receive_pack_body<W>(req: &PushPackRequest<'_>, writer: &mut W) -> Result<()>
where
    W: Write,
{
    let header = build_receive_pack_push_request_header(
        req.features,
        req.commands.to_vec(),
        req.options.clone(),
    )?;
    write_receive_pack_push_request_header(writer, &header)?;
    write_push_packfile(req, writer)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use sley_core::ObjectId;
    use sley_object::{EncodedObject, ObjectType};
    use sley_odb::{FileObjectDatabase, ObjectWriter};
    use sley_protocol::{
        ReceivePackCommand, ReceivePackFeatures, ReceivePackPushRequestOptions, RefAdvertisement,
        parse_receive_pack_push_request,
    };

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_git_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sley-remote-pack-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("objects")).expect("create objects dir");
        dir
    }

    fn write_blob(db: &mut FileObjectDatabase, body: &[u8]) -> ObjectId {
        db.write_object(EncodedObject::new(ObjectType::Blob, body.to_vec()))
            .expect("write blob")
    }

    fn advertisement(oid: &ObjectId, name: &str) -> RefAdvertisement {
        RefAdvertisement {
            oid: *oid,
            name: name.into(),
            capabilities: Vec::new(),
        }
    }

    fn push_command(old_id: &ObjectId, new_id: &ObjectId) -> ReceivePackCommand {
        ReceivePackCommand {
            old_id: old_id.clone(),
            new_id: new_id.clone(),
            name: "refs/heads/main".into(),
        }
    }

    fn default_features() -> ReceivePackFeatures {
        ReceivePackFeatures {
            report_status: true,
            ofs_delta: true,
            ..ReceivePackFeatures::default()
        }
    }

    fn default_options() -> ReceivePackPushRequestOptions {
        ReceivePackPushRequestOptions {
            report_status: true,
            ofs_delta: true,
            ..ReceivePackPushRequestOptions::default()
        }
    }

    #[test]
    fn build_receive_pack_body_round_trips_via_parse_receive_pack_push_request() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let base_oid = write_blob(&mut db, b"shared base payload\n");
        let new_oid = write_blob(&mut db, b"brand new payload for push\n");
        let req = PushPackRequest {
            local_db: &db,
            format,
            commands: &[push_command(&base_oid, &new_oid)],
            pack_objects: &[],
            remote_advertisements: &[advertisement(&base_oid, "refs/heads/main")],
            features: &default_features(),
            options: default_options(),
            thin: false,
        };

        let body = build_receive_pack_body(&req).expect("build receive-pack body");
        let parsed = parse_receive_pack_push_request(format, &body, false).expect("parse body");

        assert_eq!(parsed.commands.commands, req.commands);
        assert!(
            parsed
                .commands
                .capabilities
                .iter()
                .any(|cap| cap.name == "report-status")
        );
        assert!(parsed.packfile.starts_with(b"PACK"));
        assert_eq!(parsed.push_options, None);

        let _ = fs::remove_dir_all(git_dir);
    }

    #[test]
    fn streamed_receive_pack_body_matches_buffered_body() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let base_oid = write_blob(&mut db, b"streamed shared base\n");
        let new_oid = write_blob(&mut db, b"streamed receive-pack body\n");
        let req = PushPackRequest {
            local_db: &db,
            format,
            commands: &[push_command(&base_oid, &new_oid)],
            pack_objects: &[],
            remote_advertisements: &[advertisement(&base_oid, "refs/heads/main")],
            features: &default_features(),
            options: default_options(),
            thin: false,
        };

        let buffered = build_receive_pack_body(&req).expect("buffered body");
        let mut streamed = Vec::new();
        write_receive_pack_body(&req, &mut streamed).expect("streamed body");

        assert_eq!(streamed, buffered);
        let _ = fs::remove_dir_all(git_dir);
    }

    fn pack_request<'a>(
        local_db: &'a FileObjectDatabase,
        format: ObjectFormat,
        commands: &'a [ReceivePackCommand],
        remote_advertisements: &'a [RefAdvertisement],
        features: &'a ReceivePackFeatures,
        thin: bool,
    ) -> PushPackRequest<'a> {
        PushPackRequest {
            local_db,
            format,
            commands,
            pack_objects: &[],
            remote_advertisements,
            features,
            options: default_options(),
            thin,
        }
    }

    #[test]
    fn thin_push_packfile_omits_known_remote_bases_and_round_trips() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let base_oid = write_blob(&mut db, b"0123456789abcdef repeated base\n");
        let similar_oid = write_blob(&mut db, b"0123456789abcdef repeated base with extra tail\n");
        let commands = [push_command(&base_oid, &similar_oid)];
        let remote_advertisements = [advertisement(&base_oid, "refs/heads/main")];
        let features = default_features();

        let thin_pack = build_push_packfile(&pack_request(
            &db,
            format,
            &commands,
            &remote_advertisements,
            &features,
            true,
        ))
        .expect("thin pack");
        let full_pack = build_push_packfile(&pack_request(
            &db,
            format,
            &commands,
            &remote_advertisements,
            &features,
            false,
        ))
        .expect("full pack");
        assert!(thin_pack.starts_with(b"PACK"));
        assert!(full_pack.starts_with(b"PACK"));
        assert!(
            thin_pack.len() <= full_pack.len(),
            "thin pack should not be larger than a self-contained pack"
        );

        let body = build_receive_pack_body(&pack_request(
            &db,
            format,
            &commands,
            &remote_advertisements,
            &features,
            true,
        ))
        .expect("thin receive-pack body");
        let parsed =
            parse_receive_pack_push_request(format, &body, false).expect("parse thin body");
        assert_eq!(parsed.packfile, thin_pack);
        assert_eq!(parsed.commands.commands, commands);

        let _ = fs::remove_dir_all(git_dir);
    }

    #[test]
    fn thin_push_respects_remote_no_thin_capability() {
        let git_dir = temp_git_dir();
        let format = ObjectFormat::Sha1;
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);

        let base_oid = write_blob(&mut db, b"base\n");
        let new_oid = write_blob(&mut db, b"new\n");

        let no_thin_features = ReceivePackFeatures {
            no_thin: true,
            ..default_features()
        };
        let commands = [push_command(&base_oid, &new_oid)];
        let remote_advertisements = [advertisement(&base_oid, "refs/heads/main")];

        let thin_pack = build_push_packfile(&pack_request(
            &db,
            format,
            &commands,
            &remote_advertisements,
            &no_thin_features,
            true,
        ))
        .expect("no-thin fallback pack");
        let full_pack = build_push_packfile(&pack_request(
            &db,
            format,
            &commands,
            &remote_advertisements,
            &no_thin_features,
            false,
        ))
        .expect("full pack");
        assert_eq!(thin_pack, full_pack);

        let _ = fs::remove_dir_all(git_dir);
    }
}
