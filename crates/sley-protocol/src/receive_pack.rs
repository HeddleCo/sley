use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use std::io::{Read, Write};

use crate::pktline::{
    PushSourceRef, RefSpec, PktLineFrame, line_from_str, parse_protocol_v2_line_text,
    parse_pkt_line_frames_until_flush_from, read_pkt_line_frames_until_flush, trim_trailing_lf,
    validate_capability_field,
    validate_protocol_v2_token, validate_refspec_endpoint, validate_refspec_shape,
    write_pkt_line_payload,
};
use crate::v0::{RefAdvertisement, encode_capabilities, parse_capabilities};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivePackCommand {
    pub old_id: ObjectId,
    pub new_id: ObjectId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivePackRequest {
    pub shallow: Vec<ObjectId>,
    pub commands: Vec<ReceivePackCommand>,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivePackPushRequest {
    pub commands: ReceivePackRequest,
    pub push_options: Option<Vec<String>>,
    pub packfile: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivePackPushRequestHeader {
    pub commands: ReceivePackRequest,
    pub push_options: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivePackPushRequestOptions {
    pub report_status: bool,
    pub report_status_v2: bool,
    pub atomic: bool,
    pub ofs_delta: bool,
    pub side_band_64k: bool,
    pub quiet: bool,
    pub agent: Option<String>,
    pub object_format: Option<ObjectFormat>,
    pub push_options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivePackFeatures {
    pub report_status: bool,
    pub report_status_v2: bool,
    pub delete_refs: bool,
    pub ofs_delta: bool,
    pub atomic: bool,
    pub push_options: bool,
    pub side_band_64k: bool,
    pub quiet: bool,
    pub no_thin: bool,
    pub agent: Option<String>,
    pub object_format: Option<ObjectFormat>,
    pub unknown: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceivePackUnpackStatus {
    Ok,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceivePackCommandStatus {
    Ok { name: String },
    Ng { name: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivePackReportStatus {
    pub unpack: ReceivePackUnpackStatus,
    pub commands: Vec<ReceivePackCommandStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceivePackCommandStatusV2Options {
    pub refname: Option<String>,
    pub old_oid: Option<ObjectId>,
    pub new_oid: Option<ObjectId>,
    pub forced_update: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceivePackCommandStatusV2 {
    Ok {
        name: String,
        options: ReceivePackCommandStatusV2Options,
    },
    Ng {
        name: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivePackReportStatusV2 {
    pub unpack: ReceivePackUnpackStatus,
    pub commands: Vec<ReceivePackCommandStatusV2>,
}

pub fn plan_push_commands(
    format: ObjectFormat,
    local_refs: &[PushSourceRef],
    remote_refs: &[RefAdvertisement],
    refspecs: &[RefSpec],
) -> Result<Vec<ReceivePackCommand>> {
    let zero = zero_object_id(format)?;
    let mut commands = Vec::new();
    for refspec in refspecs {
        validate_refspec_shape(refspec)?;
        if refspec.negative {
            return Err(GitError::InvalidFormat(
                "push refspec must not be negative".into(),
            ));
        }
        match (refspec.src.as_deref(), refspec.dst.as_deref()) {
            (None, None) => {
                // A bare ":" (matching) refspec pushes only refs the remote
                // already has, by their fully-qualified `refs/...` name. git's
                // matching source set is the local ref advertisement, which never
                // includes `HEAD` or short-name aliases — push those would try to
                // update the remote's `HEAD`, so skip anything not under `refs/`.
                for local in local_refs {
                    if !local.name.starts_with("refs/") {
                        continue;
                    }
                    validate_push_source_ref(format, local)?;
                    if let Some(remote) = remote_ref(remote_refs, &local.name) {
                        commands.push(ReceivePackCommand {
                            old_id: remote.oid,
                            new_id: local.oid,
                            name: local.name.clone(),
                        });
                    }
                }
            }
            (None, Some(dst)) => {
                validate_refspec_endpoint("push destination", dst)?;
                let old_id = remote_ref(remote_refs, dst)
                    .map(|reference| reference.oid)
                    .unwrap_or_else(|| zero.clone());
                commands.push(ReceivePackCommand {
                    old_id,
                    new_id: zero.clone(),
                    name: dst.to_string(),
                });
            }
            (Some(src), dst) if refspec.pattern => {
                let Some((src_prefix, src_suffix)) = src.split_once('*') else {
                    return Err(GitError::InvalidFormat(
                        "pattern push refspec source is missing wildcard".into(),
                    ));
                };
                let dst = dst.ok_or_else(|| {
                    GitError::InvalidFormat("pattern push refspec is missing destination".into())
                })?;
                let (dst_prefix, dst_suffix) = dst.split_once('*').ok_or_else(|| {
                    GitError::InvalidFormat(
                        "pattern push refspec destination is missing wildcard".into(),
                    )
                })?;
                for local in local_refs {
                    validate_push_source_ref(format, local)?;
                    let Some(middle) = local
                        .name
                        .strip_prefix(src_prefix)
                        .and_then(|value| value.strip_suffix(src_suffix))
                    else {
                        continue;
                    };
                    let name = format!("{dst_prefix}{middle}{dst_suffix}");
                    let old_id = remote_ref(remote_refs, &name)
                        .map(|reference| reference.oid)
                        .unwrap_or_else(|| zero.clone());
                    commands.push(ReceivePackCommand {
                        old_id,
                        new_id: local.oid,
                        name,
                    });
                }
            }
            (Some(src), dst) => {
                validate_refspec_endpoint("push source", src)?;
                let local = local_ref(local_refs, src)
                    .ok_or_else(|| GitError::reference_not_found(format!("local ref {src}")))?;
                validate_push_source_ref(format, local)?;
                let name = dst.unwrap_or(src);
                validate_refspec_endpoint("push destination", name)?;
                let old_id = remote_ref(remote_refs, name)
                    .map(|reference| reference.oid)
                    .unwrap_or_else(|| zero.clone());
                commands.push(ReceivePackCommand {
                    old_id,
                    new_id: local.oid,
                    name: name.to_string(),
                });
            }
        }
    }
    Ok(commands)
}

pub fn build_receive_pack_push_request(
    features: &ReceivePackFeatures,
    commands: Vec<ReceivePackCommand>,
    packfile: Vec<u8>,
    options: ReceivePackPushRequestOptions,
) -> Result<ReceivePackPushRequest> {
    let header = build_receive_pack_push_request_header(features, commands, options)?;
    let request = ReceivePackPushRequest {
        commands: header.commands,
        push_options: header.push_options,
        packfile,
    };
    validate_receive_pack_push_request_features(features, &request)?;
    Ok(request)
}

pub fn build_receive_pack_push_request_header(
    features: &ReceivePackFeatures,
    commands: Vec<ReceivePackCommand>,
    options: ReceivePackPushRequestOptions,
) -> Result<ReceivePackPushRequestHeader> {
    let mut capabilities = Vec::new();
    if options.report_status_v2 {
        require_receive_pack_feature(features.report_status_v2, "report-status-v2")?;
        capabilities.push(Capability {
            name: "report-status-v2".into(),
            value: None,
        });
    } else if options.report_status {
        require_receive_pack_feature(features.report_status, "report-status")?;
        capabilities.push(Capability {
            name: "report-status".into(),
            value: None,
        });
    }
    if commands.iter().any(is_receive_pack_delete_command) {
        require_receive_pack_feature(features.delete_refs, "delete-refs")?;
        capabilities.push(Capability {
            name: "delete-refs".into(),
            value: None,
        });
    }
    if options.atomic {
        require_receive_pack_feature(features.atomic, "atomic")?;
        capabilities.push(Capability {
            name: "atomic".into(),
            value: None,
        });
    }
    if options.ofs_delta {
        require_receive_pack_feature(features.ofs_delta, "ofs-delta")?;
        capabilities.push(Capability {
            name: "ofs-delta".into(),
            value: None,
        });
    }
    if options.side_band_64k {
        require_receive_pack_feature(features.side_band_64k, "side-band-64k")?;
        capabilities.push(Capability {
            name: "side-band-64k".into(),
            value: None,
        });
    }
    if options.quiet {
        require_receive_pack_feature(features.quiet, "quiet")?;
        capabilities.push(Capability {
            name: "quiet".into(),
            value: None,
        });
    }
    if let Some(agent) = &options.agent {
        validate_capability_field("receive-pack request agent", agent)?;
        capabilities.push(Capability {
            name: "agent".into(),
            value: Some(agent.clone()),
        });
    }
    if let Some(format) = options.object_format {
        if features.object_format != Some(format) {
            return Err(GitError::InvalidFormat(
                "receive-pack request object-format was not advertised".into(),
            ));
        }
        capabilities.push(Capability {
            name: "object-format".into(),
            value: Some(format.name().into()),
        });
    }
    let push_options = if options.push_options.is_empty() {
        None
    } else {
        require_receive_pack_feature(features.push_options, "push-options")?;
        for option in &options.push_options {
            validate_receive_pack_push_option(option.as_bytes())?;
        }
        capabilities.push(Capability {
            name: "push-options".into(),
            value: None,
        });
        Some(options.push_options)
    };
    let header = ReceivePackPushRequestHeader {
        commands: ReceivePackRequest {
            commands,
            capabilities,
            shallow: Vec::new(),
        },
        push_options,
    };
    validate_receive_pack_push_request_features(
        features,
        &ReceivePackPushRequest {
            commands: header.commands.clone(),
            push_options: header.push_options.clone(),
            packfile: Vec::new(),
        },
    )?;
    Ok(header)
}
pub fn parse_receive_pack_request(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<ReceivePackRequest> {
    let mut request = ReceivePackRequest::default();
    let mut saw_command = false;
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                let payload = trim_trailing_lf(payload);
                if payload.is_empty() {
                    return Err(GitError::InvalidFormat(
                        "receive-pack request line is empty".into(),
                    ));
                }
                if let Some(shallow) = payload.strip_prefix(b"shallow ") {
                    if saw_command {
                        return Err(GitError::InvalidFormat(
                            "receive-pack request has shallow after commands".into(),
                        ));
                    }
                    let shallow = std::str::from_utf8(shallow)
                        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
                    validate_protocol_v2_token("receive-pack shallow", shallow)?;
                    request.shallow.push(ObjectId::from_hex(format, shallow)?);
                    continue;
                }

                let (command, capabilities) = match payload.iter().position(|byte| *byte == 0) {
                    Some(nul) if !saw_command => (
                        &payload[..nul],
                        Some(parse_capabilities(&payload[nul + 1..])?),
                    ),
                    Some(_) => {
                        return Err(GitError::InvalidFormat(
                            "receive-pack capabilities must appear on the first command".into(),
                        ));
                    }
                    None => (payload, None),
                };
                let command = parse_receive_pack_command(format, command)?;
                if let Some(capabilities) = capabilities {
                    request.capabilities = capabilities;
                }
                request.commands.push(command);
                saw_command = true;
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "receive-pack request has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "receive-pack request missing flush".into(),
        ));
    }
    if !request.shallow.is_empty() && request.commands.is_empty() {
        return Err(GitError::InvalidFormat(
            "receive-pack request has shallow lines without commands".into(),
        ));
    }
    Ok(request)
}

pub fn encode_receive_pack_request(request: &ReceivePackRequest) -> Result<Vec<PktLineFrame>> {
    if !request.shallow.is_empty() && request.commands.is_empty() {
        return Err(GitError::InvalidFormat(
            "receive-pack request has shallow lines without commands".into(),
        ));
    }

    let mut frames = Vec::new();
    for oid in &request.shallow {
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "shallow {oid}"
        )))?);
    }
    for (idx, command) in request.commands.iter().enumerate() {
        let mut payload = format_receive_pack_command(command)?;
        if idx == 0 && !request.capabilities.is_empty() {
            payload.push(0);
            payload.extend_from_slice(&encode_capabilities(&request.capabilities)?);
        }
        payload.push(b'\n');
        frames.push(PktLineFrame::data(payload)?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_receive_pack_request(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ReceivePackRequest> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_receive_pack_request(format, &frames)
}

pub fn write_receive_pack_request(
    writer: &mut impl Write,
    request: &ReceivePackRequest,
) -> Result<()> {
    if !request.shallow.is_empty() && request.commands.is_empty() {
        return Err(GitError::InvalidFormat(
            "receive-pack request has shallow lines without commands".into(),
        ));
    }

    for oid in &request.shallow {
        write_pkt_line_payload(writer, &line_from_str(&format!("shallow {oid}")))?;
    }
    for (idx, command) in request.commands.iter().enumerate() {
        let mut payload = format_receive_pack_command(command)?;
        if idx == 0 && !request.capabilities.is_empty() {
            payload.push(0);
            payload.extend_from_slice(&encode_capabilities(&request.capabilities)?);
        }
        payload.push(b'\n');
        write_pkt_line_payload(writer, &payload)?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_receive_pack_push_request(
    format: ObjectFormat,
    input: &[u8],
    has_push_options: bool,
) -> Result<ReceivePackPushRequest> {
    let (command_frames, consumed) = parse_pkt_line_frames_until_flush_from(input)?;
    let commands = parse_receive_pack_request(format, &command_frames)?;
    let mut offset = consumed;
    let push_options = if has_push_options {
        let (push_option_frames, consumed) =
            parse_pkt_line_frames_until_flush_from(&input[offset..])?;
        offset += consumed;
        Some(parse_receive_pack_push_options(&push_option_frames)?)
    } else {
        None
    };
    Ok(ReceivePackPushRequest {
        commands,
        push_options,
        packfile: input[offset..].to_vec(),
    })
}

pub fn encode_receive_pack_push_request(request: &ReceivePackPushRequest) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_receive_pack_request(&mut out, &request.commands)?;
    if let Some(push_options) = &request.push_options {
        write_receive_pack_push_options(&mut out, push_options)?;
    }
    out.extend_from_slice(&request.packfile);
    Ok(out)
}

pub fn read_receive_pack_push_request(
    format: ObjectFormat,
    reader: &mut impl Read,
    has_push_options: bool,
) -> Result<ReceivePackPushRequest> {
    let header = read_receive_pack_push_request_header(format, reader, has_push_options)?;
    let mut packfile = Vec::new();
    reader.read_to_end(&mut packfile)?;
    Ok(ReceivePackPushRequest {
        commands: header.commands,
        push_options: header.push_options,
        packfile,
    })
}

pub fn read_receive_pack_push_request_header(
    format: ObjectFormat,
    reader: &mut impl Read,
    has_push_options: bool,
) -> Result<ReceivePackPushRequestHeader> {
    let commands = read_receive_pack_request(format, reader)?;
    let push_options = if has_push_options {
        Some(read_receive_pack_push_options(reader)?)
    } else {
        None
    };
    Ok(ReceivePackPushRequestHeader {
        commands,
        push_options,
    })
}

pub fn write_receive_pack_push_request(
    writer: &mut impl Write,
    request: &ReceivePackPushRequest,
) -> Result<()> {
    write_receive_pack_push_request_header(
        writer,
        &ReceivePackPushRequestHeader {
            commands: request.commands.clone(),
            push_options: request.push_options.clone(),
        },
    )?;
    writer.write_all(&request.packfile)?;
    Ok(())
}

pub fn write_receive_pack_push_request_header(
    writer: &mut impl Write,
    header: &ReceivePackPushRequestHeader,
) -> Result<()> {
    write_receive_pack_request(writer, &header.commands)?;
    if let Some(push_options) = &header.push_options {
        write_receive_pack_push_options(writer, push_options)?;
    }
    Ok(())
}

pub fn parse_receive_pack_features(capabilities: &[Capability]) -> Result<ReceivePackFeatures> {
    let mut features = ReceivePackFeatures::default();
    for capability in capabilities {
        match capability.name.as_str() {
            "report-status" => {
                reject_capability_value("receive-pack report-status", capability)?;
                if features.report_status {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate report-status capability".into(),
                    ));
                }
                features.report_status = true;
            }
            "report-status-v2" => {
                reject_capability_value("receive-pack report-status-v2", capability)?;
                if features.report_status_v2 {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate report-status-v2 capability".into(),
                    ));
                }
                features.report_status_v2 = true;
            }
            "delete-refs" => {
                reject_capability_value("receive-pack delete-refs", capability)?;
                if features.delete_refs {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate delete-refs capability".into(),
                    ));
                }
                features.delete_refs = true;
            }
            "ofs-delta" => {
                reject_capability_value("receive-pack ofs-delta", capability)?;
                if features.ofs_delta {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate ofs-delta capability".into(),
                    ));
                }
                features.ofs_delta = true;
            }
            "atomic" => {
                reject_capability_value("receive-pack atomic", capability)?;
                if features.atomic {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate atomic capability".into(),
                    ));
                }
                features.atomic = true;
            }
            "push-options" => {
                reject_capability_value("receive-pack push-options", capability)?;
                if features.push_options {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate push-options capability".into(),
                    ));
                }
                features.push_options = true;
            }
            "side-band-64k" => {
                reject_capability_value("receive-pack side-band-64k", capability)?;
                if features.side_band_64k {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate side-band-64k capability".into(),
                    ));
                }
                features.side_band_64k = true;
            }
            "quiet" => {
                reject_capability_value("receive-pack quiet", capability)?;
                if features.quiet {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate quiet capability".into(),
                    ));
                }
                features.quiet = true;
            }
            "no-thin" => {
                reject_capability_value("receive-pack no-thin", capability)?;
                if features.no_thin {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate no-thin capability".into(),
                    ));
                }
                features.no_thin = true;
            }
            "agent" => {
                let Some(agent) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "receive-pack agent capability is missing value".into(),
                    ));
                };
                if features.agent.is_some() {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate agent capability".into(),
                    ));
                }
                validate_capability_field("receive-pack agent", agent)?;
                features.agent = Some(agent.clone());
            }
            "object-format" => {
                let Some(format) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "receive-pack object-format capability is missing value".into(),
                    ));
                };
                if features.object_format.is_some() {
                    return Err(GitError::InvalidFormat(
                        "receive-pack has duplicate object-format capability".into(),
                    ));
                }
                validate_capability_field("receive-pack object-format", format)?;
                features.object_format = Some(format.parse()?);
            }
            _ => {
                encode_capabilities(std::slice::from_ref(capability))?;
                if features
                    .unknown
                    .iter()
                    .any(|known| known.name == capability.name)
                {
                    return Err(GitError::InvalidFormat(format!(
                        "receive-pack has duplicate {} capability",
                        capability.name
                    )));
                }
                features.unknown.push(capability.clone());
            }
        }
    }
    Ok(features)
}

pub fn encode_receive_pack_features(features: &ReceivePackFeatures) -> Result<Vec<Capability>> {
    let mut capabilities = Vec::new();
    if features.report_status {
        capabilities.push(Capability {
            name: "report-status".into(),
            value: None,
        });
    }
    if features.report_status_v2 {
        capabilities.push(Capability {
            name: "report-status-v2".into(),
            value: None,
        });
    }
    if features.delete_refs {
        capabilities.push(Capability {
            name: "delete-refs".into(),
            value: None,
        });
    }
    if features.ofs_delta {
        capabilities.push(Capability {
            name: "ofs-delta".into(),
            value: None,
        });
    }
    if features.atomic {
        capabilities.push(Capability {
            name: "atomic".into(),
            value: None,
        });
    }
    if features.push_options {
        capabilities.push(Capability {
            name: "push-options".into(),
            value: None,
        });
    }
    if features.side_band_64k {
        capabilities.push(Capability {
            name: "side-band-64k".into(),
            value: None,
        });
    }
    if features.quiet {
        capabilities.push(Capability {
            name: "quiet".into(),
            value: None,
        });
    }
    if features.no_thin {
        capabilities.push(Capability {
            name: "no-thin".into(),
            value: None,
        });
    }
    if let Some(agent) = &features.agent {
        validate_capability_field("receive-pack agent", agent)?;
        capabilities.push(Capability {
            name: "agent".into(),
            value: Some(agent.clone()),
        });
    }
    if let Some(format) = features.object_format {
        capabilities.push(Capability {
            name: "object-format".into(),
            value: Some(format.name().into()),
        });
    }
    for capability in &features.unknown {
        if is_known_receive_pack_capability(&capability.name) {
            return Err(GitError::InvalidFormat(format!(
                "receive-pack unknown capability duplicates known capability {}",
                capability.name
            )));
        }
        encode_capabilities(std::slice::from_ref(capability))?;
        capabilities.push(capability.clone());
    }
    Ok(capabilities)
}

pub fn validate_receive_pack_push_request_features(
    features: &ReceivePackFeatures,
    request: &ReceivePackPushRequest,
) -> Result<()> {
    for capability in &request.commands.capabilities {
        if matches!(
            capability.name.as_str(),
            "report-status"
                | "report-status-v2"
                | "delete-refs"
                | "ofs-delta"
                | "atomic"
                | "push-options"
                | "side-band-64k"
                | "quiet"
                | "no-thin"
        ) {
            reject_capability_value("receive-pack request capability", capability)?;
        }
        match capability.name.as_str() {
            "report-status" if !features.report_status => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses report-status without advertised capability".into(),
                ));
            }
            "report-status-v2" if !features.report_status_v2 => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses report-status-v2 without advertised capability"
                        .into(),
                ));
            }
            "delete-refs" if !features.delete_refs => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses delete-refs without advertised capability".into(),
                ));
            }
            "ofs-delta" if !features.ofs_delta => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses ofs-delta without advertised capability".into(),
                ));
            }
            "atomic" if !features.atomic => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses atomic without advertised capability".into(),
                ));
            }
            "push-options" if !features.push_options => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses push-options without advertised capability".into(),
                ));
            }
            "side-band-64k" if !features.side_band_64k => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses side-band-64k without advertised capability".into(),
                ));
            }
            "quiet" if !features.quiet => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request uses quiet without advertised capability".into(),
                ));
            }
            "no-thin" => {
                return Err(GitError::InvalidFormat(
                    "receive-pack request must not request no-thin".into(),
                ));
            }
            "agent" => {
                validate_capability_field(
                    "receive-pack request agent",
                    capability.value.as_deref().unwrap_or_default(),
                )?;
            }
            "object-format" => {
                let Some(value) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "receive-pack request object-format capability is missing value".into(),
                    ));
                };
                let requested_format: ObjectFormat = value.parse()?;
                if features.object_format != Some(requested_format) {
                    return Err(GitError::InvalidFormat(
                        "receive-pack request object-format was not advertised".into(),
                    ));
                }
            }
            name if is_known_receive_pack_capability(name) => {}
            _ => {
                if !features
                    .unknown
                    .iter()
                    .any(|advertised| advertised.name == capability.name)
                {
                    return Err(GitError::InvalidFormat(format!(
                        "receive-pack request uses unadvertised capability {}",
                        capability.name
                    )));
                }
            }
        }
    }

    let requested_push_options = request
        .commands
        .capabilities
        .iter()
        .any(|capability| capability.name == "push-options");
    match (requested_push_options, &request.push_options) {
        (true, Some(_)) => {}
        (true, None) => {
            return Err(GitError::InvalidFormat(
                "receive-pack request uses push-options without push-options section".into(),
            ));
        }
        (false, Some(_)) => {
            return Err(GitError::InvalidFormat(
                "receive-pack request has push-options section without requested capability".into(),
            ));
        }
        (false, None) => {}
    }

    let has_delete = request
        .commands
        .commands
        .iter()
        .any(is_receive_pack_delete_command);
    if has_delete && !features.delete_refs {
        return Err(GitError::InvalidFormat(
            "receive-pack request deletes refs without advertised delete-refs capability".into(),
        ));
    }

    let has_update_or_create = request
        .commands
        .commands
        .iter()
        .any(|command| !is_receive_pack_delete_command(command));
    if !has_update_or_create && !request.packfile.is_empty() {
        return Err(GitError::InvalidFormat(
            "receive-pack delete-only request must not include packfile".into(),
        ));
    }
    Ok(())
}

pub fn apply_receive_pack_push_request<R, I, C, U, D>(
    features: &ReceivePackFeatures,
    request: &ReceivePackPushRequest,
    mut read_ref: R,
    mut install_pack: I,
    mut contains_object: C,
    mut apply_updates: U,
    mut delete_ref: D,
) -> Result<ReceivePackReportStatus>
where
    R: FnMut(&str) -> Result<Option<ObjectId>>,
    I: FnMut(&[u8]) -> Result<()>,
    C: FnMut(&ObjectId) -> Result<bool>,
    U: FnMut(&[ReceivePackCommand]) -> Result<()>,
    D: FnMut(&ReceivePackCommand) -> Result<()>,
{
    validate_receive_pack_push_request_features(features, request)?;

    for command in request
        .commands
        .commands
        .iter()
        .filter(|command| is_receive_pack_delete_command(command))
    {
        if !command.old_id.is_null() && read_ref(&command.name)? != Some(command.old_id.clone()) {
            return Err(GitError::Transaction(format!(
                "expected ref {} to match",
                command.name
            )));
        }
    }

    let updates = request
        .commands
        .commands
        .iter()
        .filter(|command| !is_receive_pack_delete_command(command))
        .cloned()
        .collect::<Vec<_>>();
    if !updates.is_empty() {
        if !request.packfile.is_empty() {
            install_pack(&request.packfile)?;
        }
        for command in &updates {
            if !contains_object(&command.new_id)? {
                return Err(GitError::InvalidObject(format!(
                    "receive-pack packfile did not provide {}",
                    command.new_id
                )));
            }
        }
        apply_updates(&updates)?;
    }

    for command in request
        .commands
        .commands
        .iter()
        .filter(|command| is_receive_pack_delete_command(command))
    {
        delete_ref(command)?;
    }

    Ok(ReceivePackReportStatus {
        unpack: ReceivePackUnpackStatus::Ok,
        commands: request
            .commands
            .commands
            .iter()
            .map(|command| ReceivePackCommandStatus::Ok {
                name: command.name.clone(),
            })
            .collect(),
    })
}

pub fn parse_receive_pack_report_status(
    frames: &[PktLineFrame],
) -> Result<ReceivePackReportStatus> {
    let Some((first, rest)) = frames.split_first() else {
        return Err(GitError::InvalidFormat(
            "receive-pack report-status is empty".into(),
        ));
    };
    let PktLineFrame::Data(payload) = first else {
        return Err(GitError::InvalidFormat(
            "receive-pack report-status must start with unpack status".into(),
        ));
    };
    let unpack = parse_receive_pack_unpack_status(payload)?;

    let mut commands = Vec::new();
    let mut saw_flush = false;
    for (idx, frame) in rest.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                commands.push(parse_receive_pack_command_status(payload)?);
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "receive-pack report-status has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != rest.len() {
                    return Err(GitError::InvalidFormat(
                        "receive-pack report-status has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "receive-pack report-status contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "receive-pack report-status missing flush".into(),
        ));
    }
    Ok(ReceivePackReportStatus { unpack, commands })
}

pub fn encode_receive_pack_report_status(
    report: &ReceivePackReportStatus,
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    frames.push(PktLineFrame::data(line_from_str(
        &format_receive_pack_unpack_status(&report.unpack)?,
    ))?);
    for command in &report.commands {
        frames.push(PktLineFrame::data(line_from_str(
            &format_receive_pack_command_status(command)?,
        ))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_receive_pack_report_status(reader: &mut impl Read) -> Result<ReceivePackReportStatus> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_receive_pack_report_status(&frames)
}

pub fn write_receive_pack_report_status(
    writer: &mut impl Write,
    report: &ReceivePackReportStatus,
) -> Result<()> {
    write_pkt_line_payload(
        writer,
        &line_from_str(&format_receive_pack_unpack_status(&report.unpack)?),
    )?;
    for command in &report.commands {
        write_pkt_line_payload(
            writer,
            &line_from_str(&format_receive_pack_command_status(command)?),
        )?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_receive_pack_report_status_v2(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<ReceivePackReportStatusV2> {
    let Some((first, rest)) = frames.split_first() else {
        return Err(GitError::InvalidFormat(
            "receive-pack report-status-v2 is empty".into(),
        ));
    };
    let PktLineFrame::Data(payload) = first else {
        return Err(GitError::InvalidFormat(
            "receive-pack report-status-v2 must start with unpack status".into(),
        ));
    };
    let unpack = parse_receive_pack_unpack_status(payload)?;

    let mut commands = Vec::new();
    let mut saw_flush = false;
    for (idx, frame) in rest.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                let text =
                    parse_protocol_v2_line_text("receive-pack report-status-v2 line", payload)?;
                if text.starts_with("option ") {
                    let Some(ReceivePackCommandStatusV2::Ok { options, .. }) = commands.last_mut()
                    else {
                        return Err(GitError::InvalidFormat(
                            "receive-pack report-status-v2 option without ok status".into(),
                        ));
                    };
                    parse_receive_pack_report_status_v2_option(format, text, options)?;
                } else {
                    commands.push(parse_receive_pack_command_status_v2(text)?);
                }
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "receive-pack report-status-v2 has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != rest.len() {
                    return Err(GitError::InvalidFormat(
                        "receive-pack report-status-v2 has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "receive-pack report-status-v2 contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "receive-pack report-status-v2 missing flush".into(),
        ));
    }
    Ok(ReceivePackReportStatusV2 { unpack, commands })
}

pub fn encode_receive_pack_report_status_v2(
    report: &ReceivePackReportStatusV2,
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    frames.push(PktLineFrame::data(line_from_str(
        &format_receive_pack_unpack_status(&report.unpack)?,
    ))?);
    for command in &report.commands {
        frames.push(PktLineFrame::data(line_from_str(
            &format_receive_pack_command_status_v2(command)?,
        ))?);
        if let ReceivePackCommandStatusV2::Ok { options, .. } = command {
            for option in format_receive_pack_report_status_v2_options(options)? {
                frames.push(PktLineFrame::data(line_from_str(&option))?);
            }
        }
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_receive_pack_report_status_v2(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ReceivePackReportStatusV2> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_receive_pack_report_status_v2(format, &frames)
}

pub fn write_receive_pack_report_status_v2(
    writer: &mut impl Write,
    report: &ReceivePackReportStatusV2,
) -> Result<()> {
    write_pkt_line_payload(
        writer,
        &line_from_str(&format_receive_pack_unpack_status(&report.unpack)?),
    )?;
    for command in &report.commands {
        write_pkt_line_payload(
            writer,
            &line_from_str(&format_receive_pack_command_status_v2(command)?),
        )?;
        if let ReceivePackCommandStatusV2::Ok { options, .. } = command {
            for option in format_receive_pack_report_status_v2_options(options)? {
                write_pkt_line_payload(writer, &line_from_str(&option))?;
            }
        }
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_receive_pack_push_options(frames: &[PktLineFrame]) -> Result<Vec<String>> {
    let mut options = Vec::new();
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                let option = trim_trailing_lf(payload);
                validate_receive_pack_push_option(option)?;
                options.push(
                    std::str::from_utf8(option)
                        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
                        .to_string(),
                );
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "receive-pack push-options has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "receive-pack push-options has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "receive-pack push-options contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "receive-pack push-options missing flush".into(),
        ));
    }
    Ok(options)
}

pub fn encode_receive_pack_push_options(options: &[String]) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    for option in options {
        validate_receive_pack_push_option(option.as_bytes())?;
        let mut payload = option.as_bytes().to_vec();
        payload.push(b'\n');
        frames.push(PktLineFrame::data(payload)?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_receive_pack_push_options(reader: &mut impl Read) -> Result<Vec<String>> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_receive_pack_push_options(&frames)
}

pub fn write_receive_pack_push_options(writer: &mut impl Write, options: &[String]) -> Result<()> {
    for option in options {
        validate_receive_pack_push_option(option.as_bytes())?;
        let mut payload = option.as_bytes().to_vec();
        payload.push(b'\n');
        write_pkt_line_payload(writer, &payload)?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}
pub(crate) fn zero_object_id(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

fn local_ref<'a>(refs: &'a [PushSourceRef], name: &str) -> Option<&'a PushSourceRef> {
    refs.iter().find(|reference| reference.name == name)
}

fn remote_ref<'a>(refs: &'a [RefAdvertisement], name: &str) -> Option<&'a RefAdvertisement> {
    refs.iter().find(|reference| reference.name == name)
}

fn validate_push_source_ref(format: ObjectFormat, reference: &PushSourceRef) -> Result<()> {
    if reference.oid.format() != format {
        return Err(GitError::InvalidObjectId(
            "push source ref object format does not match repository".into(),
        ));
    }
    validate_refspec_endpoint("push source ref name", &reference.name)
}

fn require_receive_pack_feature(advertised: bool, name: &str) -> Result<()> {
    if advertised {
        Ok(())
    } else {
        Err(GitError::InvalidFormat(format!(
            "receive-pack feature {name} was not advertised"
        )))
    }
}

fn parse_receive_pack_command(format: ObjectFormat, payload: &[u8]) -> Result<ReceivePackCommand> {
    let text =
        std::str::from_utf8(payload).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut fields = text.split(' ');
    let old_id = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("receive-pack command missing old id".into()))?;
    let new_id = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("receive-pack command missing new id".into()))?;
    let name = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("receive-pack command missing ref name".into()))?;
    if fields.next().is_some() {
        return Err(GitError::InvalidFormat(
            "receive-pack command has too many fields".into(),
        ));
    }
    validate_protocol_v2_token("receive-pack old id", old_id)?;
    validate_protocol_v2_token("receive-pack new id", new_id)?;
    validate_protocol_v2_token("receive-pack ref name", name)?;
    Ok(ReceivePackCommand {
        old_id: ObjectId::from_hex(format, old_id)?,
        new_id: ObjectId::from_hex(format, new_id)?,
        name: name.to_string(),
    })
}

fn format_receive_pack_command(command: &ReceivePackCommand) -> Result<Vec<u8>> {
    if command.old_id.format() != command.new_id.format() {
        return Err(GitError::InvalidObjectId(
            "receive-pack command object formats do not match".into(),
        ));
    }
    validate_protocol_v2_token("receive-pack ref name", &command.name)?;
    Ok(format!("{} {} {}", command.old_id, command.new_id, command.name).into_bytes())
}

pub(crate) fn reject_capability_value(label: &str, capability: &Capability) -> Result<()> {
    if capability.value.is_some() {
        return Err(GitError::InvalidFormat(format!(
            "{label} must not have value"
        )));
    }
    Ok(())
}
fn is_known_receive_pack_capability(name: &str) -> bool {
    matches!(
        name,
        "report-status"
            | "report-status-v2"
            | "delete-refs"
            | "ofs-delta"
            | "atomic"
            | "push-options"
            | "side-band-64k"
            | "quiet"
            | "no-thin"
            | "agent"
            | "object-format"
    )
}

fn is_receive_pack_delete_command(command: &ReceivePackCommand) -> bool {
    command.new_id.is_null()
}

fn parse_receive_pack_unpack_status(payload: &[u8]) -> Result<ReceivePackUnpackStatus> {
    let text = parse_protocol_v2_line_text("receive-pack unpack status", payload)?;
    if text == "unpack ok" {
        return Ok(ReceivePackUnpackStatus::Ok);
    }
    let Some(message) = text.strip_prefix("unpack ") else {
        return Err(GitError::InvalidFormat(format!(
            "unsupported receive-pack unpack status {text}"
        )));
    };
    validate_receive_pack_status_message("receive-pack unpack error", message)?;
    Ok(ReceivePackUnpackStatus::Error(message.to_string()))
}

fn format_receive_pack_unpack_status(status: &ReceivePackUnpackStatus) -> Result<String> {
    match status {
        ReceivePackUnpackStatus::Ok => Ok("unpack ok".into()),
        ReceivePackUnpackStatus::Error(message) => {
            validate_receive_pack_status_message("receive-pack unpack error", message)?;
            Ok(format!("unpack {message}"))
        }
    }
}

fn parse_receive_pack_command_status(payload: &[u8]) -> Result<ReceivePackCommandStatus> {
    let text = parse_protocol_v2_line_text("receive-pack command status", payload)?;
    if let Some(name) = text.strip_prefix("ok ") {
        validate_protocol_v2_token("receive-pack status ref name", name)?;
        return Ok(ReceivePackCommandStatus::Ok {
            name: name.to_string(),
        });
    }
    let Some(rest) = text.strip_prefix("ng ") else {
        return Err(GitError::InvalidFormat(format!(
            "unsupported receive-pack command status {text}"
        )));
    };
    let (name, message) = rest
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat("receive-pack ng status missing message".into()))?;
    validate_protocol_v2_token("receive-pack status ref name", name)?;
    validate_receive_pack_status_message("receive-pack ng status message", message)?;
    Ok(ReceivePackCommandStatus::Ng {
        name: name.to_string(),
        message: message.to_string(),
    })
}

fn format_receive_pack_command_status(status: &ReceivePackCommandStatus) -> Result<String> {
    match status {
        ReceivePackCommandStatus::Ok { name } => {
            validate_protocol_v2_token("receive-pack status ref name", name)?;
            Ok(format!("ok {name}"))
        }
        ReceivePackCommandStatus::Ng { name, message } => {
            validate_protocol_v2_token("receive-pack status ref name", name)?;
            validate_receive_pack_status_message("receive-pack ng status message", message)?;
            Ok(format!("ng {name} {message}"))
        }
    }
}

fn parse_receive_pack_command_status_v2(text: &str) -> Result<ReceivePackCommandStatusV2> {
    if let Some(name) = text.strip_prefix("ok ") {
        validate_protocol_v2_token("receive-pack status-v2 ref name", name)?;
        return Ok(ReceivePackCommandStatusV2::Ok {
            name: name.to_string(),
            options: ReceivePackCommandStatusV2Options::default(),
        });
    }
    let Some(rest) = text.strip_prefix("ng ") else {
        return Err(GitError::InvalidFormat(format!(
            "unsupported receive-pack report-status-v2 line {text}"
        )));
    };
    let (name, message) = rest.split_once(' ').ok_or_else(|| {
        GitError::InvalidFormat("receive-pack status-v2 ng status missing message".into())
    })?;
    validate_protocol_v2_token("receive-pack status-v2 ref name", name)?;
    validate_receive_pack_status_message("receive-pack status-v2 ng message", message)?;
    Ok(ReceivePackCommandStatusV2::Ng {
        name: name.to_string(),
        message: message.to_string(),
    })
}

fn format_receive_pack_command_status_v2(status: &ReceivePackCommandStatusV2) -> Result<String> {
    match status {
        ReceivePackCommandStatusV2::Ok { name, .. } => {
            validate_protocol_v2_token("receive-pack status-v2 ref name", name)?;
            Ok(format!("ok {name}"))
        }
        ReceivePackCommandStatusV2::Ng { name, message } => {
            validate_protocol_v2_token("receive-pack status-v2 ref name", name)?;
            validate_receive_pack_status_message("receive-pack status-v2 ng message", message)?;
            Ok(format!("ng {name} {message}"))
        }
    }
}

fn parse_receive_pack_report_status_v2_option(
    format: ObjectFormat,
    text: &str,
    options: &mut ReceivePackCommandStatusV2Options,
) -> Result<()> {
    if let Some(refname) = text.strip_prefix("option refname ") {
        if options.refname.is_some() {
            return Err(GitError::InvalidFormat(
                "receive-pack report-status-v2 has duplicate refname option".into(),
            ));
        }
        validate_protocol_v2_token("receive-pack status-v2 option refname", refname)?;
        options.refname = Some(refname.to_string());
    } else if let Some(old_oid) = text.strip_prefix("option old-oid ") {
        if options.old_oid.is_some() {
            return Err(GitError::InvalidFormat(
                "receive-pack report-status-v2 has duplicate old-oid option".into(),
            ));
        }
        validate_protocol_v2_token("receive-pack status-v2 option old-oid", old_oid)?;
        options.old_oid = Some(ObjectId::from_hex(format, old_oid)?);
    } else if let Some(new_oid) = text.strip_prefix("option new-oid ") {
        if options.new_oid.is_some() {
            return Err(GitError::InvalidFormat(
                "receive-pack report-status-v2 has duplicate new-oid option".into(),
            ));
        }
        validate_protocol_v2_token("receive-pack status-v2 option new-oid", new_oid)?;
        options.new_oid = Some(ObjectId::from_hex(format, new_oid)?);
    } else if text == "option forced-update" {
        if options.forced_update {
            return Err(GitError::InvalidFormat(
                "receive-pack report-status-v2 has duplicate forced-update option".into(),
            ));
        }
        options.forced_update = true;
    } else {
        return Err(GitError::InvalidFormat(format!(
            "unsupported receive-pack report-status-v2 option {text}"
        )));
    }
    Ok(())
}

fn format_receive_pack_report_status_v2_options(
    options: &ReceivePackCommandStatusV2Options,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(refname) = &options.refname {
        validate_protocol_v2_token("receive-pack status-v2 option refname", refname)?;
        out.push(format!("option refname {refname}"));
    }
    if let Some(old_oid) = &options.old_oid {
        out.push(format!("option old-oid {old_oid}"));
    }
    if let Some(new_oid) = &options.new_oid {
        out.push(format!("option new-oid {new_oid}"));
    }
    if options.forced_update {
        out.push("option forced-update".into());
    }
    Ok(out)
}

fn validate_receive_pack_status_message(label: &str, message: &str) -> Result<()> {
    if message.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if message
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn validate_receive_pack_push_option(option: &[u8]) -> Result<()> {
    if option.iter().any(|byte| matches!(*byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "receive-pack push-option contains a delimiter byte".into(),
        ));
    }
    Ok(())
}
