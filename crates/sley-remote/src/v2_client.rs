//! Shared protocol v2 client pieces for stateful (`ssh://`, `git://`) and
//! stateless (HTTP) upload-pack.
//!
//! Request builders match the HTTP `ls-refs` / `fetch` commands. Stateful
//! transports peek the first pkt-line the way stock git's `discover_version`
//! does: `version 2` continues as v2, `version 1` is consumed and the rest is a
//! v0 advertisement, and any other data line is itself the first v0 ref line.

use std::env;
use std::io::{IsTerminal, Read, Write};

use sley_config::GitConfig;
use sley_core::{
    Capability, GitError, ObjectFormat, ObjectId, Result, UPSTREAM_GIT_COMPAT_VERSION,
};
use sley_protocol::{
    GitService, PktLineFrame, ProtocolV2CommandOptions, ProtocolV2CommandRequest,
    ProtocolV2FetchRequest, ProtocolV2LsRefsRequest, ProtocolVersion, RefAdvertisement,
    RefAdvertisementSet, TransportHandshake, encode_protocol_v2_command_options,
    parse_protocol_v2_advertisement, parse_protocol_v2_fetch_features,
    parse_protocol_v2_ls_refs_features, parse_ref_advertisement_set, parse_upload_pack_features,
    protocol_v2_object_format, read_pkt_line_frame, read_pkt_line_frames_until_flush,
    read_protocol_v2_ls_refs_response_as_ref_advertisement_set, read_ref_advertisement_set,
    validate_protocol_v2_fetch_command_request, validate_protocol_v2_ls_refs_command_request,
    write_protocol_v2_command_request,
};

/// Upload-pack greeting on a stateful connection.
pub(crate) enum StatefulUploadPackGreeting {
    V2(TransportHandshake),
    Legacy(RefAdvertisementSet),
}

/// Upload-pack greeting when the local object format is not known yet.
pub(crate) enum DiscoveredStatefulUploadPack {
    V2 {
        handshake: TransportHandshake,
        object_format: ObjectFormat,
    },
    Legacy {
        set: RefAdvertisementSet,
        object_format: ObjectFormat,
    },
}

/// `version=` token from a `GIT_PROTOCOL` value.
///
/// Colon-separated tokens match `determine_protocol_version_server`: the
/// greatest recognized `version=` wins. `version=0` is recognized so an
/// explicit override can force the v0 client path.
pub(crate) fn protocol_version_from_git_protocol_value(value: &str) -> Option<ProtocolVersion> {
    let mut found = None;
    for token in value.split(':') {
        let version = match token {
            "version=0" => ProtocolVersion::V0,
            "version=1" => ProtocolVersion::V1,
            "version=2" => ProtocolVersion::V2,
            _ => continue,
        };
        found = Some(match found {
            Some(current) if protocol_version_rank(current) >= protocol_version_rank(version) => {
                current
            }
            _ => version,
        });
    }
    found
}

fn protocol_version_rank(version: ProtocolVersion) -> u8 {
    match version {
        ProtocolVersion::V0 => 0,
        ProtocolVersion::V1 => 1,
        ProtocolVersion::V2 => 2,
    }
}

/// Client upload-pack version.
///
/// `GIT_PROTOCOL`'s `version=` token overrides `protocol.version`. An explicit
/// config value of 0, 1, or 2 wins over `GIT_TEST_PROTOCOL_VERSION`. Unknown or
/// absent config falls through to the test variable, then to v2 (stock git's
/// `get_protocol_version_config` default).
pub(crate) fn client_upload_pack_version(config: Option<&GitConfig>) -> ProtocolVersion {
    let git_protocol = env::var("GIT_PROTOCOL").ok();
    let test_protocol = env::var("GIT_TEST_PROTOCOL_VERSION").ok();
    resolve_client_upload_pack_version(git_protocol.as_deref(), config, test_protocol.as_deref())
}

pub(crate) fn resolve_client_upload_pack_version(
    git_protocol: Option<&str>,
    config: Option<&GitConfig>,
    test_protocol: Option<&str>,
) -> ProtocolVersion {
    if let Some(value) = git_protocol
        && let Some(version) = protocol_version_from_git_protocol_value(value)
    {
        return version;
    }
    if let Some(version) = config.and_then(protocol_version_from_config) {
        return version;
    }
    match test_protocol
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some("0") => ProtocolVersion::V0,
        Some("1") => ProtocolVersion::V1,
        Some("2") | None => ProtocolVersion::V2,
        Some(_) => ProtocolVersion::V2,
    }
}

fn protocol_version_from_config(config: &GitConfig) -> Option<ProtocolVersion> {
    match config.get("protocol", None, "version") {
        Some("0") => Some(ProtocolVersion::V0),
        Some("1") => Some(ProtocolVersion::V1),
        Some("2") => Some(ProtocolVersion::V2),
        _ => None,
    }
}

/// Extra `version=` parameter for an upload-pack service request.
///
/// Protocol v0 must not be encoded (`encode_service_request` rejects it).
pub(crate) fn upload_pack_protocol_parameter(version: ProtocolVersion) -> Option<ProtocolVersion> {
    match version {
        ProtocolVersion::V0 => None,
        ProtocolVersion::V1 => Some(ProtocolVersion::V1),
        ProtocolVersion::V2 => Some(ProtocolVersion::V2),
    }
}

pub(crate) fn git_protocol_environment_value(version: ProtocolVersion) -> Option<&'static str> {
    match version {
        ProtocolVersion::V0 => None,
        ProtocolVersion::V1 => Some("version=1"),
        ProtocolVersion::V2 => Some("version=2"),
    }
}

/// Wire version to advertise for `service`.
///
/// Stock git downgrades protocol v2 to v0 for every service other than
/// upload-pack. Protocol v1 is still sent for receive-pack and upload-archive.
pub(crate) fn wire_protocol_for_service(
    service: GitService,
    requested: ProtocolVersion,
) -> Option<ProtocolVersion> {
    match service {
        GitService::UploadPack => upload_pack_protocol_parameter(requested),
        GitService::ReceivePack | GitService::UploadArchive => match requested {
            ProtocolVersion::V0 | ProtocolVersion::V2 => None,
            ProtocolVersion::V1 => Some(ProtocolVersion::V1),
        },
    }
}

pub(crate) fn protocol_v2_ls_refs_command_request(
    format: ObjectFormat,
    handshake: &TransportHandshake,
) -> Result<ProtocolV2CommandRequest> {
    protocol_v2_ls_refs_command_request_with_unborn(format, handshake, false)
}

/// `ls-refs` for a stateful transport.
///
/// Requests `unborn` when the server advertised it, so an empty repository's
/// symbolic HEAD is visible. HTTP keeps [`protocol_v2_ls_refs_command_request`],
/// which does not ask for `unborn`.
pub(crate) fn stateful_v2_ls_refs_command_request(
    format: ObjectFormat,
    handshake: &TransportHandshake,
) -> Result<ProtocolV2CommandRequest> {
    let unborn = parse_protocol_v2_ls_refs_features(&handshake.capabilities)?
        .is_some_and(|features| features.unborn);
    protocol_v2_ls_refs_command_request_with_unborn(format, handshake, unborn)
}

fn protocol_v2_ls_refs_command_request_with_unborn(
    format: ObjectFormat,
    handshake: &TransportHandshake,
    unborn: bool,
) -> Result<ProtocolV2CommandRequest> {
    let ls_refs = ProtocolV2LsRefsRequest {
        peel: true,
        symrefs: true,
        unborn,
        ref_prefixes: vec!["HEAD".into(), "refs/heads/".into(), "refs/tags/".into()],
    };
    let mut command = ls_refs.to_command_request()?;
    command.capabilities = protocol_v2_command_capabilities(format, handshake)?;
    validate_protocol_v2_ls_refs_command_request(handshake, &command)?;
    Ok(command)
}

pub(crate) fn protocol_v2_fetch_command_request(
    format: ObjectFormat,
    handshake: &TransportHandshake,
    fetch: &ProtocolV2FetchRequest,
) -> Result<ProtocolV2CommandRequest> {
    let mut command = fetch.to_command_request()?;
    command.capabilities = protocol_v2_command_capabilities(format, handshake)?;
    validate_protocol_v2_fetch_command_request(handshake, format, &command)?;
    Ok(command)
}

fn protocol_v2_command_capabilities(
    format: ObjectFormat,
    handshake: &TransportHandshake,
) -> Result<Vec<Capability>> {
    let mut options = ProtocolV2CommandOptions::default();
    if handshake
        .capabilities
        .iter()
        .any(|capability| capability.name == "agent")
    {
        options.agent = Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}"));
    }
    if handshake
        .capabilities
        .iter()
        .any(|capability| capability.name == "object-format")
    {
        let advertised_format = protocol_v2_object_format(&handshake.capabilities)?;
        if advertised_format != format {
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                advertised_format.name(),
                format.name()
            )));
        }
        options.object_format = Some(format);
    }
    encode_protocol_v2_command_options(&options)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn protocol_v2_fetch_request_from_upload_pack_semantics(
    wants: Vec<ObjectId>,
    want_refs: Vec<String>,
    haves: Vec<ObjectId>,
    shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    deepen_since: Option<i64>,
    deepen_not: Vec<String>,
    deepen_relative: bool,
    filter: Option<&sley_odb::PackObjectFilter>,
    packfile_uri_protocols: Option<&str>,
    handshake: &TransportHandshake,
) -> Result<ProtocolV2FetchRequest> {
    let v2_features =
        parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
    Ok(ProtocolV2FetchRequest {
        wants,
        want_refs,
        haves,
        shallow,
        deepen,
        deepen_since: deepen_since_u64(deepen_since),
        deepen_not,
        deepen_relative,
        filter: filter.and_then(crate::local::upload_pack_filter_protocol_spec),
        packfile_uris: if v2_features.packfile_uris {
            packfile_uri_protocols.map(str::to_owned)
        } else {
            None
        },
        thin_pack: true,
        include_tag: true,
        ofs_delta: true,
        done: true,
        // Normal fetch negotiation expects the pack immediately after `ready`.
        // `wait-for-done` is reserved for callers such as `--negotiate-only`.
        wait_for_done: false,
        sideband_all: v2_features.sideband_all,
        ..ProtocolV2FetchRequest::default()
    })
}

pub(crate) fn deepen_since_u64(deepen_since: Option<i64>) -> Option<u64> {
    deepen_since.and_then(|value| u64::try_from(value).ok())
}

/// Whether a stateful upload-pack fetch should send `no-progress`.
///
/// Matches `transport.c`: an explicit `--progress` / `--no-progress` wins;
/// otherwise progress is shown only when the command is not quiet and stderr
/// is a terminal.
pub(crate) fn upload_pack_no_progress(quiet: bool, progress: Option<bool>) -> bool {
    match progress {
        Some(enabled) => !enabled,
        None => quiet || !std::io::stderr().is_terminal(),
    }
}

pub(crate) struct StatefulV2FetchParams<'a> {
    pub format: ObjectFormat,
    pub handshake: &'a TransportHandshake,
    pub wants: Vec<ObjectId>,
    pub want_refs: Vec<String>,
    pub haves: Vec<ObjectId>,
    pub shallow: Vec<ObjectId>,
    pub deepen: Option<u32>,
    pub deepen_since: Option<i64>,
    pub deepen_not: Vec<String>,
    pub deepen_relative: bool,
    pub filter: Option<&'a sley_odb::PackObjectFilter>,
    pub packfile_uri_protocols: Option<&'a str>,
    /// Ask the server to omit sideband progress. Stock git sends this unless
    /// `--progress` is set or stderr is a terminal and the command is not quiet.
    pub no_progress: bool,
}

/// Write one stateful `command=fetch` and report whether the request asked for
/// `sideband-all` (the response reader must demux that mode).
pub(crate) fn write_stateful_v2_fetch(
    writer: &mut impl Write,
    params: StatefulV2FetchParams<'_>,
) -> Result<bool> {
    let mut fetch = protocol_v2_fetch_request_from_upload_pack_semantics(
        params.wants,
        params.want_refs,
        params.haves,
        params.shallow,
        params.deepen,
        params.deepen_since,
        params.deepen_not,
        params.deepen_relative,
        params.filter,
        params.packfile_uri_protocols,
        params.handshake,
    )?;
    // The stateful installer indexes the pack as a closed file and does not
    // splice external delta bases. Asking for `thin-pack` makes stock
    // upload-pack deltify against haves, which then fails to index. HTTP keeps
    // the builder's `thin_pack: true` and its own negotiation.
    fetch.thin_pack = false;
    fetch.no_progress = params.no_progress;
    let sideband_all = fetch.sideband_all;
    let command = protocol_v2_fetch_command_request(params.format, params.handshake, &fetch)?;
    write_protocol_v2_command_request(writer, &command)?;
    Ok(sideband_all)
}

pub(crate) fn write_stateful_v2_ls_refs(
    writer: &mut impl Write,
    format: ObjectFormat,
    handshake: &TransportHandshake,
) -> Result<()> {
    let command = stateful_v2_ls_refs_command_request(format, handshake)?;
    write_protocol_v2_command_request(writer, &command)
}

pub(crate) fn read_stateful_v2_ls_refs(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<RefAdvertisementSet> {
    read_protocol_v2_ls_refs_response_as_ref_advertisement_set(format, reader)
}

pub(crate) fn upload_pack_features_from_v2(
    handshake: &TransportHandshake,
    refs: &[RefAdvertisement],
) -> Result<sley_protocol::UploadPackFeatures> {
    let v2 = parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
    let mut features = sley_protocol::UploadPackFeatures {
        object_format: Some(protocol_v2_object_format(&handshake.capabilities)?),
        shallow: v2.shallow,
        deepen_since: v2.shallow,
        deepen_not: v2.shallow,
        filter: v2.filter,
        ..sley_protocol::UploadPackFeatures::default()
    };
    if let Some(first) = refs.first() {
        let bridged = parse_upload_pack_features(&first.capabilities)?;
        features.symrefs = bridged.symrefs;
    }
    Ok(features)
}

pub(crate) fn legacy_upload_pack_features(
    refs: &[RefAdvertisement],
) -> Result<sley_protocol::UploadPackFeatures> {
    Ok(refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default())
}

pub(crate) fn read_stateful_upload_pack_greeting(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<StatefulUploadPackGreeting> {
    match read_first_advertisement_frame(reader)? {
        FirstAdvertisement::Version2(payload) => {
            let handshake = parse_protocol_v2_advertisement(&version2_frames(payload, reader)?)?;
            Ok(StatefulUploadPackGreeting::V2(handshake))
        }
        FirstAdvertisement::Version1 => Ok(StatefulUploadPackGreeting::Legacy(
            read_ref_advertisement_set(format, reader)?,
        )),
        FirstAdvertisement::Legacy(payload) => {
            let mut frames = vec![PktLineFrame::Data(payload)];
            frames.extend(read_pkt_line_frames_until_flush(reader)?);
            Ok(StatefulUploadPackGreeting::Legacy(
                parse_ref_advertisement_set(format, &frames)?,
            ))
        }
    }
}

pub(crate) fn read_discovered_stateful_upload_pack_greeting(
    reader: &mut impl Read,
) -> Result<DiscoveredStatefulUploadPack> {
    match read_first_advertisement_frame(reader)? {
        FirstAdvertisement::Version2(payload) => {
            let handshake = parse_protocol_v2_advertisement(&version2_frames(payload, reader)?)?;
            let object_format = protocol_v2_object_format(&handshake.capabilities)?;
            Ok(DiscoveredStatefulUploadPack::V2 {
                handshake,
                object_format,
            })
        }
        FirstAdvertisement::Version1 => {
            let (set, object_format) = crate::read_discovered_upload_pack_advertisements(reader)?;
            Ok(DiscoveredStatefulUploadPack::Legacy { set, object_format })
        }
        FirstAdvertisement::Legacy(payload) => {
            let mut frames = vec![PktLineFrame::Data(payload)];
            frames.extend(read_pkt_line_frames_until_flush(reader)?);
            let (set, object_format) = discovered_legacy_advertisement(&frames)?;
            Ok(DiscoveredStatefulUploadPack::Legacy { set, object_format })
        }
    }
}

enum FirstAdvertisement {
    Version2(Vec<u8>),
    Version1,
    Legacy(Vec<u8>),
}

fn read_first_advertisement_frame(reader: &mut impl Read) -> Result<FirstAdvertisement> {
    let Some(first) = read_pkt_line_frame(reader)? else {
        return Err(GitError::InvalidFormat(
            "remote unexpectedly hung up before advertisement".into(),
        ));
    };
    match first {
        PktLineFrame::Data(payload) if pkt_line_text(&payload) == b"version 2" => {
            Ok(FirstAdvertisement::Version2(payload))
        }
        PktLineFrame::Data(payload) if pkt_line_text(&payload) == b"version 1" => {
            Ok(FirstAdvertisement::Version1)
        }
        PktLineFrame::Data(payload) => Ok(FirstAdvertisement::Legacy(payload)),
        PktLineFrame::Flush | PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => Err(
            GitError::InvalidFormat("upload-pack advertisement must start with a data line".into()),
        ),
    }
}

fn version2_frames(version_line: Vec<u8>, reader: &mut impl Read) -> Result<Vec<PktLineFrame>> {
    let mut frames = vec![PktLineFrame::Data(version_line)];
    frames.extend(read_pkt_line_frames_until_flush(reader)?);
    Ok(frames)
}

fn pkt_line_text(payload: &[u8]) -> &[u8] {
    payload.strip_suffix(b"\n").unwrap_or(payload)
}

fn discovered_legacy_advertisement(
    frames: &[PktLineFrame],
) -> Result<(RefAdvertisementSet, ObjectFormat)> {
    let sha1 = parse_ref_advertisement_set(ObjectFormat::Sha1, frames);
    let sha256 = parse_ref_advertisement_set(ObjectFormat::Sha256, frames);
    for (format, parsed) in [
        (ObjectFormat::Sha1, sha1.as_ref()),
        (ObjectFormat::Sha256, sha256.as_ref()),
    ] {
        let Ok(set) = parsed else {
            continue;
        };
        let advertised = set
            .refs
            .first()
            .map(|reference| parse_upload_pack_features(&reference.capabilities))
            .transpose()?
            .and_then(|features| features.object_format)
            .unwrap_or(ObjectFormat::Sha1);
        if advertised == format {
            return Ok((set.clone(), format));
        }
    }
    match sha1 {
        Err(err) => Err(err),
        Ok(_) => Err(GitError::InvalidObjectId(
            "advertised object format does not match advertised object ids".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_protocol::{
        ProtocolVersion, RefAdvertisement, RefAdvertisementSet, TransportHandshake,
        write_protocol_v2_advertisement, write_ref_advertisement_set,
    };

    #[test]
    fn git_protocol_value_picks_the_greatest_version() {
        assert_eq!(
            protocol_version_from_git_protocol_value("version=1"),
            Some(ProtocolVersion::V1)
        );
        assert_eq!(
            protocol_version_from_git_protocol_value("version=2:version=1"),
            Some(ProtocolVersion::V2)
        );
        assert_eq!(
            protocol_version_from_git_protocol_value("agent=git/2.0:version=0"),
            Some(ProtocolVersion::V0)
        );
        assert_eq!(
            protocol_version_from_git_protocol_value("agent=git/2.0"),
            None
        );
    }

    #[test]
    fn resolve_prefers_git_protocol_then_config_then_default_v2() {
        let v0 = GitConfig::parse(b"[protocol]\n\tversion = 0\n").expect("config");
        assert_eq!(
            resolve_client_upload_pack_version(Some("version=2"), Some(&v0), Some("0")),
            ProtocolVersion::V2
        );
        assert_eq!(
            resolve_client_upload_pack_version(None, Some(&v0), Some("2")),
            ProtocolVersion::V0
        );
        let unknown = GitConfig::parse(b"[protocol]\n\tversion = future\n").expect("config");
        assert_eq!(
            resolve_client_upload_pack_version(None, Some(&unknown), None),
            ProtocolVersion::V2
        );
        assert_eq!(
            resolve_client_upload_pack_version(None, None, Some("1")),
            ProtocolVersion::V1
        );
        assert_eq!(
            resolve_client_upload_pack_version(None, None, None),
            ProtocolVersion::V2
        );
        let v1 = GitConfig::parse(b"[protocol]\n\tversion = 1\n").expect("config");
        assert_eq!(
            resolve_client_upload_pack_version(Some("version=0"), Some(&v1), None),
            ProtocolVersion::V0
        );
    }

    #[test]
    fn receive_pack_does_not_advertise_protocol_v2() {
        assert_eq!(
            wire_protocol_for_service(GitService::UploadPack, ProtocolVersion::V2),
            Some(ProtocolVersion::V2)
        );
        assert_eq!(
            wire_protocol_for_service(GitService::ReceivePack, ProtocolVersion::V2),
            None
        );
        assert_eq!(
            wire_protocol_for_service(GitService::UploadArchive, ProtocolVersion::V2),
            None
        );
        assert_eq!(
            wire_protocol_for_service(GitService::ReceivePack, ProtocolVersion::V1),
            Some(ProtocolVersion::V1)
        );
        assert_eq!(
            wire_protocol_for_service(GitService::UploadPack, ProtocolVersion::V0),
            None
        );
        assert_eq!(
            upload_pack_protocol_parameter(ProtocolVersion::V1),
            Some(ProtocolVersion::V1)
        );
        assert_eq!(
            git_protocol_environment_value(ProtocolVersion::V2),
            Some("version=2")
        );
        assert_eq!(git_protocol_environment_value(ProtocolVersion::V0), None);
    }

    #[test]
    fn greeting_accepts_v2_and_falls_back_to_v0_refs() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![Capability {
                name: "ls-refs".into(),
                value: Some("unborn".into()),
            }],
        };
        let mut v2 = Vec::new();
        write_protocol_v2_advertisement(&mut v2, &handshake).expect("encode v2");
        match read_stateful_upload_pack_greeting(ObjectFormat::Sha1, &mut v2.as_slice())
            .expect("read v2")
        {
            StatefulUploadPackGreeting::V2(read) => {
                assert_eq!(read.protocol, ProtocolVersion::V2);
            }
            StatefulUploadPackGreeting::Legacy(_) => panic!("v2 greeting was treated as v0"),
        }

        let tip = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("oid");
        let set = RefAdvertisementSet {
            protocol: ProtocolVersion::V0,
            refs: vec![RefAdvertisement {
                oid: tip,
                name: "HEAD".into(),
                capabilities: Vec::new(),
            }],
            shallow: Vec::new(),
        };
        let mut v0 = Vec::new();
        write_ref_advertisement_set(&mut v0, &set).expect("encode v0");
        match read_stateful_upload_pack_greeting(ObjectFormat::Sha1, &mut v0.as_slice())
            .expect("read v0")
        {
            StatefulUploadPackGreeting::Legacy(read) => {
                assert_eq!(read.refs.len(), 1);
                assert_eq!(read.refs[0].name, "HEAD");
                assert_eq!(read.refs[0].oid, tip);
            }
            StatefulUploadPackGreeting::V2(_) => panic!("v0 greeting was treated as v2"),
        }
    }
}
