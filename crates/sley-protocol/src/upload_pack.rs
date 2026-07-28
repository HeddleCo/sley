use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use std::io::{Read, Write};

use crate::limits::{TransportLimits, append_to_end_bounded, read_to_end_bounded};
use crate::pktline::{
    PKT_LINE_MAX_LEN, PktLineFrame, line_from_str, parse_oid_argument, parse_pkt_len,
    parse_protocol_v2_line_text, read_pkt_line_frame, read_pkt_line_frames_until_flush,
    trace_packet_read_payload, trim_trailing_lf, validate_capability_field,
    validate_protocol_v2_token, write_pkt_line_payload,
};
use crate::receive_pack::reject_capability_value;
use crate::sideband::{
    SideBandDemux, SideBandPacket, demux_sideband_packets, encode_sideband_packet,
    parse_sideband_packet, write_sideband_packet,
};
use crate::v0::{encode_capabilities, parse_capabilities};
use crate::v2::{
    ProtocolV2FetchShallowInfo, parse_fetch_shallow_info, parse_u32_argument, parse_u64_argument,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadPackRequest {
    pub wants: Vec<ObjectId>,
    pub capabilities: Vec<Capability>,
    pub shallow: Vec<ObjectId>,
    pub deepen: Option<u32>,
    pub deepen_since: Option<u64>,
    pub deepen_not: Vec<String>,
    pub filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadPackFeatures {
    pub multi_ack: bool,
    pub multi_ack_detailed: bool,
    pub no_done: bool,
    pub thin_pack: bool,
    pub side_band: bool,
    pub side_band_64k: bool,
    pub ofs_delta: bool,
    pub shallow: bool,
    pub deepen_since: bool,
    pub deepen_not: bool,
    pub include_tag: bool,
    pub no_progress: bool,
    pub allow_tip_sha1_in_want: bool,
    pub allow_reachable_sha1_in_want: bool,
    pub filter: bool,
    pub agent: Option<String>,
    pub object_format: Option<ObjectFormat>,
    pub symrefs: Vec<String>,
    pub unknown: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadPackNegotiationRequest {
    pub haves: Vec<ObjectId>,
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadPackAckStatus {
    Continue,
    Common,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadPackAcknowledgment {
    Nak,
    Ack {
        oid: ObjectId,
        status: Option<UploadPackAckStatus>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadPackPackfileResponse {
    pub acknowledgments: Vec<UploadPackAcknowledgment>,
    pub sideband: Vec<SideBandPacket>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadPackRawPackfileResponse {
    pub acknowledgments: Vec<UploadPackAcknowledgment>,
    pub packfile: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadPackRawPackfileResponseHeader {
    pub acknowledgments: Vec<UploadPackAcknowledgment>,
    pub pack_prefix: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadPackResponsePlan {
    pub acknowledgments: Vec<UploadPackAcknowledgment>,
    pub wants: Vec<ObjectId>,
    pub known_haves: Vec<ObjectId>,
}
pub fn parse_upload_pack_request(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<Option<UploadPackRequest>> {
    if matches!(frames, [PktLineFrame::Flush]) {
        return Ok(None);
    }

    let mut request = UploadPackRequest::default();
    let mut in_options = false;
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                let text = parse_protocol_v2_line_text("upload-pack request line", payload)?;
                if let Some(value) = text.strip_prefix("want ") {
                    if in_options {
                        return Err(GitError::InvalidFormat(
                            "upload-pack request has want after options".into(),
                        ));
                    }
                    let (oid, capabilities) = if request.wants.is_empty() {
                        value
                            .split_once(' ')
                            .map_or((value, None), |(oid, caps)| (oid, Some(caps.as_bytes())))
                    } else {
                        if value.contains(' ') {
                            return Err(GitError::InvalidFormat(
                                "additional upload-pack want has capabilities".into(),
                            ));
                        }
                        (value, None)
                    };
                    validate_protocol_v2_token("upload-pack want", oid)?;
                    request.wants.push(ObjectId::from_hex(format, oid)?);
                    if let Some(capabilities) = capabilities {
                        request.capabilities = parse_capabilities(capabilities)?;
                    }
                    continue;
                }

                if request.wants.is_empty() {
                    return Err(GitError::InvalidFormat(
                        "upload-pack request must start with want".into(),
                    ));
                }
                in_options = true;
                if text.starts_with("shallow ") {
                    request.shallow.push(parse_oid_argument(
                        format,
                        "upload-pack shallow",
                        text,
                        "shallow ",
                    )?);
                } else if text.starts_with("deepen ") {
                    if request.deepen.is_some() {
                        return Err(GitError::InvalidFormat(
                            "upload-pack request has duplicate deepen".into(),
                        ));
                    }
                    request.deepen =
                        Some(parse_u32_argument("upload-pack deepen", text, "deepen ")?);
                } else if text.starts_with("deepen-since ") {
                    if request.deepen_since.is_some() {
                        return Err(GitError::InvalidFormat(
                            "upload-pack request has duplicate deepen-since".into(),
                        ));
                    }
                    request.deepen_since = Some(parse_u64_argument(
                        "upload-pack deepen-since",
                        text,
                        "deepen-since ",
                    )?);
                } else if let Some(name) = text.strip_prefix("deepen-not ") {
                    validate_protocol_v2_token("upload-pack deepen-not", name)?;
                    request.deepen_not.push(name.to_string());
                } else if let Some(filter) = text.strip_prefix("filter ") {
                    if request.filter.is_some() {
                        return Err(GitError::InvalidFormat(
                            "upload-pack request has duplicate filter".into(),
                        ));
                    }
                    validate_protocol_v2_token("upload-pack filter", filter)?;
                    request.filter = Some(filter.to_string());
                } else {
                    return Err(GitError::InvalidFormat(format!(
                        "unsupported upload-pack request line {text}"
                    )));
                }
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "upload-pack request has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "upload-pack request missing flush".into(),
        ));
    }
    if request.wants.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-pack request missing want".into(),
        ));
    }
    Ok(Some(request))
}

pub fn encode_upload_pack_request(
    request: Option<&UploadPackRequest>,
) -> Result<Vec<PktLineFrame>> {
    let Some(request) = request else {
        return Ok(vec![PktLineFrame::Flush]);
    };
    if request.wants.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-pack request missing want".into(),
        ));
    }

    let mut frames = Vec::new();
    for (idx, oid) in request.wants.iter().enumerate() {
        let mut line = format!("want {oid}");
        if idx == 0 && !request.capabilities.is_empty() {
            line.push(' ');
            line.push_str(
                &String::from_utf8(encode_capabilities(&request.capabilities)?)
                    .map_err(|err| GitError::InvalidFormat(err.to_string()))?,
            );
        }
        frames.push(PktLineFrame::data(line_from_str(&line))?);
    }
    for oid in &request.shallow {
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "shallow {oid}"
        )))?);
    }
    if let Some(deepen) = request.deepen {
        if deepen == 0 {
            return Err(GitError::InvalidFormat(
                "upload-pack deepen must be positive".into(),
            ));
        }
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "deepen {deepen}"
        )))?);
    }
    if let Some(deepen_since) = request.deepen_since {
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "deepen-since {deepen_since}"
        )))?);
    }
    for name in &request.deepen_not {
        validate_protocol_v2_token("upload-pack deepen-not", name)?;
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "deepen-not {name}"
        )))?);
    }
    if let Some(filter) = &request.filter {
        validate_protocol_v2_token("upload-pack filter", filter)?;
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "filter {filter}"
        )))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_upload_pack_request(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Option<UploadPackRequest>> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_upload_pack_request(format, &frames)
}

pub fn write_upload_pack_request(
    writer: &mut impl Write,
    request: Option<&UploadPackRequest>,
) -> Result<()> {
    let Some(request) = request else {
        writer.write_all(b"0000")?;
        return Ok(());
    };
    if request.wants.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-pack request missing want".into(),
        ));
    }

    for (idx, oid) in request.wants.iter().enumerate() {
        let mut line = format!("want {oid}");
        if idx == 0 && !request.capabilities.is_empty() {
            line.push(' ');
            line.push_str(
                &String::from_utf8(encode_capabilities(&request.capabilities)?)
                    .map_err(|err| GitError::InvalidFormat(err.to_string()))?,
            );
        }
        write_pkt_line_payload(writer, &line_from_str(&line))?;
    }
    for oid in &request.shallow {
        write_pkt_line_payload(writer, &line_from_str(&format!("shallow {oid}")))?;
    }
    if let Some(deepen) = request.deepen {
        if deepen == 0 {
            return Err(GitError::InvalidFormat(
                "upload-pack deepen must be positive".into(),
            ));
        }
        write_pkt_line_payload(writer, &line_from_str(&format!("deepen {deepen}")))?;
    }
    if let Some(deepen_since) = request.deepen_since {
        write_pkt_line_payload(
            writer,
            &line_from_str(&format!("deepen-since {deepen_since}")),
        )?;
    }
    for name in &request.deepen_not {
        validate_protocol_v2_token("upload-pack deepen-not", name)?;
        write_pkt_line_payload(writer, &line_from_str(&format!("deepen-not {name}")))?;
    }
    if let Some(filter) = &request.filter {
        validate_protocol_v2_token("upload-pack filter", filter)?;
        write_pkt_line_payload(writer, &line_from_str(&format!("filter {filter}")))?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_upload_pack_features(capabilities: &[Capability]) -> Result<UploadPackFeatures> {
    let mut features = UploadPackFeatures::default();
    for capability in capabilities {
        match capability.name.as_str() {
            "multi_ack" => set_upload_pack_flag(&mut features.multi_ack, capability)?,
            "multi_ack_detailed" => {
                set_upload_pack_flag(&mut features.multi_ack_detailed, capability)?
            }
            "no-done" => set_upload_pack_flag(&mut features.no_done, capability)?,
            "thin-pack" => set_upload_pack_flag(&mut features.thin_pack, capability)?,
            "side-band" => set_upload_pack_flag(&mut features.side_band, capability)?,
            "side-band-64k" => set_upload_pack_flag(&mut features.side_band_64k, capability)?,
            "ofs-delta" => set_upload_pack_flag(&mut features.ofs_delta, capability)?,
            "shallow" => set_upload_pack_flag(&mut features.shallow, capability)?,
            "deepen-since" => set_upload_pack_flag(&mut features.deepen_since, capability)?,
            "deepen-not" => set_upload_pack_flag(&mut features.deepen_not, capability)?,
            "include-tag" => set_upload_pack_flag(&mut features.include_tag, capability)?,
            "no-progress" => set_upload_pack_flag(&mut features.no_progress, capability)?,
            "allow-tip-sha1-in-want" => {
                set_upload_pack_flag(&mut features.allow_tip_sha1_in_want, capability)?
            }
            "allow-reachable-sha1-in-want" => {
                set_upload_pack_flag(&mut features.allow_reachable_sha1_in_want, capability)?
            }
            "filter" => set_upload_pack_flag(&mut features.filter, capability)?,
            "agent" => {
                let Some(agent) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "upload-pack agent capability is missing value".into(),
                    ));
                };
                if features.agent.is_some() {
                    return Err(GitError::InvalidFormat(
                        "upload-pack has duplicate agent capability".into(),
                    ));
                }
                validate_capability_field("upload-pack agent", agent)?;
                features.agent = Some(agent.clone());
            }
            "object-format" => {
                let Some(format) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "upload-pack object-format capability is missing value".into(),
                    ));
                };
                if features.object_format.is_some() {
                    return Err(GitError::InvalidFormat(
                        "upload-pack has duplicate object-format capability".into(),
                    ));
                }
                validate_capability_field("upload-pack object-format", format)?;
                features.object_format = Some(format.parse()?);
            }
            "symref" => {
                let Some(symref) = &capability.value else {
                    continue;
                };
                validate_capability_field("upload-pack symref", symref)?;
                features.symrefs.push(symref.clone());
            }
            _ => {
                encode_capabilities(std::slice::from_ref(capability))?;
                if features
                    .unknown
                    .iter()
                    .any(|known| known.name == capability.name)
                {
                    return Err(GitError::InvalidFormat(format!(
                        "upload-pack has duplicate {} capability",
                        capability.name
                    )));
                }
                features.unknown.push(capability.clone());
            }
        }
    }
    Ok(features)
}

pub fn encode_upload_pack_features(features: &UploadPackFeatures) -> Result<Vec<Capability>> {
    let mut capabilities = Vec::new();
    push_upload_pack_flag(&mut capabilities, "multi_ack", features.multi_ack);
    push_upload_pack_flag(
        &mut capabilities,
        "multi_ack_detailed",
        features.multi_ack_detailed,
    );
    push_upload_pack_flag(&mut capabilities, "no-done", features.no_done);
    push_upload_pack_flag(&mut capabilities, "thin-pack", features.thin_pack);
    push_upload_pack_flag(&mut capabilities, "side-band", features.side_band);
    push_upload_pack_flag(&mut capabilities, "side-band-64k", features.side_band_64k);
    push_upload_pack_flag(&mut capabilities, "ofs-delta", features.ofs_delta);
    push_upload_pack_flag(&mut capabilities, "shallow", features.shallow);
    push_upload_pack_flag(&mut capabilities, "deepen-since", features.deepen_since);
    push_upload_pack_flag(&mut capabilities, "deepen-not", features.deepen_not);
    push_upload_pack_flag(&mut capabilities, "include-tag", features.include_tag);
    push_upload_pack_flag(&mut capabilities, "no-progress", features.no_progress);
    push_upload_pack_flag(
        &mut capabilities,
        "allow-tip-sha1-in-want",
        features.allow_tip_sha1_in_want,
    );
    push_upload_pack_flag(
        &mut capabilities,
        "allow-reachable-sha1-in-want",
        features.allow_reachable_sha1_in_want,
    );
    push_upload_pack_flag(&mut capabilities, "filter", features.filter);
    if let Some(agent) = &features.agent {
        validate_capability_field("upload-pack agent", agent)?;
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
    for symref in &features.symrefs {
        validate_capability_field("upload-pack symref", symref)?;
        capabilities.push(Capability {
            name: "symref".into(),
            value: Some(symref.clone()),
        });
    }
    for capability in &features.unknown {
        if is_known_upload_pack_capability(&capability.name) {
            return Err(GitError::InvalidFormat(format!(
                "upload-pack unknown capability duplicates known capability {}",
                capability.name
            )));
        }
        encode_capabilities(std::slice::from_ref(capability))?;
        capabilities.push(capability.clone());
    }
    Ok(capabilities)
}

pub fn validate_upload_pack_request_features(
    features: &UploadPackFeatures,
    request: &UploadPackRequest,
) -> Result<()> {
    for capability in &request.capabilities {
        if is_upload_pack_flag_capability(&capability.name) {
            reject_capability_value("upload-pack request capability", capability)?;
        }
        match capability.name.as_str() {
            "multi_ack" if !features.multi_ack => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses multi_ack without advertised capability".into(),
                ));
            }
            "multi_ack_detailed" if !features.multi_ack_detailed => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses multi_ack_detailed without advertised capability"
                        .into(),
                ));
            }
            "no-done" if !features.no_done => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses no-done without advertised capability".into(),
                ));
            }
            "thin-pack" if !features.thin_pack => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses thin-pack without advertised capability".into(),
                ));
            }
            "side-band" if !features.side_band => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses side-band without advertised capability".into(),
                ));
            }
            "side-band-64k" if !features.side_band_64k => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses side-band-64k without advertised capability".into(),
                ));
            }
            "ofs-delta" if !features.ofs_delta => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses ofs-delta without advertised capability".into(),
                ));
            }
            "include-tag" if !features.include_tag => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses include-tag without advertised capability".into(),
                ));
            }
            "no-progress" if !features.no_progress => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses no-progress without advertised capability".into(),
                ));
            }
            "allow-tip-sha1-in-want" if !features.allow_tip_sha1_in_want => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses allow-tip-sha1-in-want without advertised capability"
                        .into(),
                ));
            }
            "allow-reachable-sha1-in-want" if !features.allow_reachable_sha1_in_want => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses allow-reachable-sha1-in-want without advertised capability"
                        .into(),
                ));
            }
            "filter" if !features.filter => {
                return Err(GitError::InvalidFormat(
                    "upload-pack request uses filter capability without advertised capability"
                        .into(),
                ));
            }
            "agent" => {
                let Some(agent) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "upload-pack request agent capability is missing value".into(),
                    ));
                };
                validate_capability_field("upload-pack request agent", agent)?;
            }
            "object-format" => {
                let Some(format) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "upload-pack request object-format capability is missing value".into(),
                    ));
                };
                let requested_format: ObjectFormat = format.parse()?;
                if features.object_format != Some(requested_format) {
                    return Err(GitError::InvalidFormat(
                        "upload-pack request object-format was not advertised".into(),
                    ));
                }
            }
            name if is_known_upload_pack_capability(name) => {}
            _ => {
                if !features
                    .unknown
                    .iter()
                    .any(|advertised| advertised.name == capability.name)
                {
                    return Err(GitError::InvalidFormat(format!(
                        "upload-pack request uses unadvertised capability {}",
                        capability.name
                    )));
                }
            }
        }
    }

    let sideband = request
        .capabilities
        .iter()
        .any(|capability| capability.name == "side-band");
    let sideband_64k = request
        .capabilities
        .iter()
        .any(|capability| capability.name == "side-band-64k");
    if sideband && sideband_64k {
        return Err(GitError::InvalidFormat(
            "upload-pack request must not request both side-band and side-band-64k".into(),
        ));
    }

    if !features.shallow && (!request.shallow.is_empty() || request.deepen.is_some()) {
        return Err(GitError::InvalidFormat(
            "upload-pack request uses shallow/deepen without advertised shallow capability".into(),
        ));
    }
    if !features.deepen_since && request.deepen_since.is_some() {
        return Err(GitError::InvalidFormat(
            "upload-pack request uses deepen-since without advertised capability".into(),
        ));
    }
    if !features.deepen_not && !request.deepen_not.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-pack request uses deepen-not without advertised capability".into(),
        ));
    }
    if !features.filter && request.filter.is_some() {
        return Err(GitError::InvalidFormat(
            "upload-pack request uses filter without advertised capability".into(),
        ));
    }
    Ok(())
}

pub fn build_upload_pack_raw_packfile_response<C, B>(
    features: &UploadPackFeatures,
    request: UploadPackRequest,
    haves: impl IntoIterator<Item = ObjectId>,
    mut contains_object: C,
    mut build_pack: B,
) -> Result<UploadPackRawPackfileResponse>
where
    C: FnMut(&ObjectId) -> Result<bool>,
    B: FnMut(Vec<ObjectId>, Vec<ObjectId>) -> Result<Option<Vec<u8>>>,
{
    let plan = prepare_upload_pack_response(features, request, haves, &mut contains_object)?;
    let packfile = build_pack(plan.wants, plan.known_haves)?
        .ok_or_else(|| GitError::InvalidObject("upload-pack request produced empty pack".into()))?;
    Ok(UploadPackRawPackfileResponse {
        acknowledgments: plan.acknowledgments,
        packfile,
    })
}

pub fn prepare_upload_pack_response<C>(
    features: &UploadPackFeatures,
    request: UploadPackRequest,
    haves: impl IntoIterator<Item = ObjectId>,
    mut contains_object: C,
) -> Result<UploadPackResponsePlan>
where
    C: FnMut(&ObjectId) -> Result<bool>,
{
    validate_upload_pack_request_features(features, &request)?;
    for want in &request.wants {
        if !contains_object(want)? {
            return Err(GitError::InvalidObject(format!(
                "upload-pack requested missing object {want}"
            )));
        }
    }
    let known_haves = haves
        .into_iter()
        .filter_map(|oid| match contains_object(&oid) {
            Ok(true) => Some(Ok(oid)),
            Ok(false) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(UploadPackResponsePlan {
        acknowledgments: vec![UploadPackAcknowledgment::Nak],
        wants: request.wants,
        known_haves,
    })
}

pub fn parse_upload_pack_shallow_update(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    let mut entries = Vec::new();
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                entries.push(parse_fetch_shallow_info(format, payload)?);
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "upload-pack shallow update has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "upload-pack shallow update has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "upload-pack shallow update contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "upload-pack shallow update missing flush".into(),
        ));
    }
    Ok(entries)
}

pub fn encode_upload_pack_shallow_update(
    entries: &[ProtocolV2FetchShallowInfo],
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    for entry in entries {
        let line = match entry {
            ProtocolV2FetchShallowInfo::Shallow(oid) => format!("shallow {oid}"),
            ProtocolV2FetchShallowInfo::Unshallow(oid) => format!("unshallow {oid}"),
        };
        frames.push(PktLineFrame::data(line_from_str(&line))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_upload_pack_shallow_update(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_upload_pack_shallow_update(format, &frames)
}

pub fn write_upload_pack_shallow_update(
    writer: &mut impl Write,
    entries: &[ProtocolV2FetchShallowInfo],
) -> Result<()> {
    for entry in entries {
        let line = match entry {
            ProtocolV2FetchShallowInfo::Shallow(oid) => format!("shallow {oid}"),
            ProtocolV2FetchShallowInfo::Unshallow(oid) => format!("unshallow {oid}"),
        };
        write_pkt_line_payload(writer, &line_from_str(&line))?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_upload_pack_negotiation_request(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<UploadPackNegotiationRequest> {
    let mut request = UploadPackNegotiationRequest::default();
    let mut terminated = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !terminated => {
                let text = parse_protocol_v2_line_text("upload-pack negotiation line", payload)?;
                if text == "done" {
                    request.done = true;
                    terminated = true;
                    if idx + 1 != frames.len() {
                        return Err(GitError::InvalidFormat(
                            "upload-pack negotiation has frames after done".into(),
                        ));
                    }
                } else if text.starts_with("have ") {
                    request.haves.push(parse_oid_argument(
                        format,
                        "upload-pack have",
                        text,
                        "have ",
                    )?);
                } else {
                    return Err(GitError::InvalidFormat(format!(
                        "unsupported upload-pack negotiation line {text}"
                    )));
                }
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "upload-pack negotiation has data after terminator".into(),
                ));
            }
            PktLineFrame::Flush => {
                terminated = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "upload-pack negotiation has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "upload-pack negotiation contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !terminated {
        return Err(GitError::InvalidFormat(
            "upload-pack negotiation missing terminator".into(),
        ));
    }
    Ok(request)
}

pub fn encode_upload_pack_negotiation_request(
    request: &UploadPackNegotiationRequest,
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    for oid in &request.haves {
        frames.push(PktLineFrame::data(line_from_str(&format!("have {oid}")))?);
    }
    if request.done {
        frames.push(PktLineFrame::data(line_from_str("done"))?);
    } else {
        frames.push(PktLineFrame::Flush);
    }
    Ok(frames)
}

pub fn read_upload_pack_negotiation_request(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<UploadPackNegotiationRequest> {
    let mut frames = Vec::new();
    loop {
        let Some(frame) = read_pkt_line_frame(reader)? else {
            return Err(GitError::InvalidFormat(
                "pkt-line stream ended before upload-pack negotiation terminator".into(),
            ));
        };
        let done = match &frame {
            PktLineFrame::Flush => true,
            PktLineFrame::Data(payload) => trim_trailing_lf(payload) == b"done",
            _ => false,
        };
        frames.push(frame);
        if done {
            return parse_upload_pack_negotiation_request(format, &frames);
        }
    }
}

pub fn write_upload_pack_negotiation_request(
    writer: &mut impl Write,
    request: &UploadPackNegotiationRequest,
) -> Result<()> {
    for oid in &request.haves {
        write_pkt_line_payload(writer, &line_from_str(&format!("have {oid}")))?;
    }
    if request.done {
        write_pkt_line_payload(writer, b"done\n")?;
    } else {
        writer.write_all(b"0000")?;
    }
    Ok(())
}

pub fn parse_upload_pack_acknowledgment(
    format: ObjectFormat,
    payload: &[u8],
) -> Result<UploadPackAcknowledgment> {
    let text = parse_protocol_v2_line_text("upload-pack acknowledgment", payload)?;
    if text == "NAK" {
        return Ok(UploadPackAcknowledgment::Nak);
    }
    let Some(rest) = text.strip_prefix("ACK ") else {
        return Err(GitError::InvalidFormat(format!(
            "unsupported upload-pack acknowledgment {text}"
        )));
    };
    let mut fields = rest.split(' ');
    let oid = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("upload-pack ACK missing object id".into()))?;
    validate_protocol_v2_token("upload-pack ACK", oid)?;
    let status = match fields.next() {
        None => None,
        Some("continue") => Some(UploadPackAckStatus::Continue),
        Some("common") => Some(UploadPackAckStatus::Common),
        Some("ready") => Some(UploadPackAckStatus::Ready),
        Some(other) => {
            return Err(GitError::InvalidFormat(format!(
                "unsupported upload-pack ACK status {other}"
            )));
        }
    };
    if fields.next().is_some() {
        return Err(GitError::InvalidFormat(
            "upload-pack ACK has too many fields".into(),
        ));
    }
    Ok(UploadPackAcknowledgment::Ack {
        oid: ObjectId::from_hex(format, oid)?,
        status,
    })
}

pub fn encode_upload_pack_acknowledgment(
    acknowledgment: &UploadPackAcknowledgment,
) -> Result<Vec<u8>> {
    let line = match acknowledgment {
        UploadPackAcknowledgment::Nak => "NAK".to_string(),
        UploadPackAcknowledgment::Ack { oid, status } => {
            let mut line = format!("ACK {oid}");
            if let Some(status) = status {
                line.push(' ');
                line.push_str(match status {
                    UploadPackAckStatus::Continue => "continue",
                    UploadPackAckStatus::Common => "common",
                    UploadPackAckStatus::Ready => "ready",
                });
            }
            line
        }
    };
    Ok(line_from_str(&line))
}

pub fn read_upload_pack_acknowledgment(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<UploadPackAcknowledgment> {
    let Some(frame) = read_pkt_line_frame(reader)? else {
        return Err(GitError::InvalidFormat(
            "pkt-line stream ended before upload-pack acknowledgment".into(),
        ));
    };
    match frame {
        PktLineFrame::Data(payload) => parse_upload_pack_acknowledgment(format, &payload),
        _ => Err(GitError::InvalidFormat(
            "upload-pack acknowledgment must be a data packet".into(),
        )),
    }
}

pub fn write_upload_pack_acknowledgment(
    writer: &mut impl Write,
    acknowledgment: &UploadPackAcknowledgment,
) -> Result<()> {
    write_pkt_line_payload(writer, &encode_upload_pack_acknowledgment(acknowledgment)?)
}

pub fn parse_upload_pack_packfile_response(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<UploadPackPackfileResponse> {
    let mut response = UploadPackPackfileResponse::default();
    let mut in_sideband = false;
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                if !in_sideband
                    && (trim_trailing_lf(payload) == b"NAK" || payload.starts_with(b"ACK "))
                {
                    response
                        .acknowledgments
                        .push(parse_upload_pack_acknowledgment(format, payload)?);
                    continue;
                }
                in_sideband = true;
                response.sideband.push(parse_sideband_packet(payload)?);
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "upload-pack packfile response has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "upload-pack packfile response has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "upload-pack packfile response contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "upload-pack packfile response missing flush".into(),
        ));
    }
    Ok(response)
}

pub fn encode_upload_pack_packfile_response(
    response: &UploadPackPackfileResponse,
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    for acknowledgment in &response.acknowledgments {
        frames.push(PktLineFrame::data(encode_upload_pack_acknowledgment(
            acknowledgment,
        )?)?);
    }
    for packet in &response.sideband {
        frames.push(PktLineFrame::data(encode_sideband_packet(packet)?)?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_upload_pack_packfile_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<UploadPackPackfileResponse> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_upload_pack_packfile_response(format, &frames)
}

pub fn write_upload_pack_packfile_response(
    writer: &mut impl Write,
    response: &UploadPackPackfileResponse,
) -> Result<()> {
    for acknowledgment in &response.acknowledgments {
        write_upload_pack_acknowledgment(writer, acknowledgment)?;
    }
    for packet in &response.sideband {
        write_sideband_packet(writer, packet)?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn demux_upload_pack_packfile_response(
    response: &UploadPackPackfileResponse,
) -> Result<SideBandDemux> {
    demux_sideband_packets(&response.sideband)
}

pub fn parse_upload_pack_raw_packfile_response(
    format: ObjectFormat,
    input: &[u8],
) -> Result<UploadPackRawPackfileResponse> {
    let mut response = UploadPackRawPackfileResponse::default();
    let mut offset = 0usize;
    while offset < input.len() {
        match PktLineFrame::parse(&input[offset..]) {
            Ok((PktLineFrame::Data(payload), consumed)) => {
                let trimmed = trim_trailing_lf(&payload);
                if trimmed == b"NAK" || trimmed.starts_with(b"ACK ") {
                    response
                        .acknowledgments
                        .push(parse_upload_pack_acknowledgment(format, &payload)?);
                    offset += consumed;
                    continue;
                }
                return Err(GitError::InvalidFormat(
                    "upload-pack raw packfile response has non-ack pkt-line before packfile".into(),
                ));
            }
            Ok((PktLineFrame::Flush | PktLineFrame::Delimiter | PktLineFrame::ResponseEnd, _)) => {
                return Err(GitError::InvalidFormat(
                    "upload-pack raw packfile response contains a control packet".into(),
                ));
            }
            Err(_) if input[offset..].starts_with(b"PACK") => break,
            Err(err) => return Err(err),
        }
    }
    response.packfile = input[offset..].to_vec();
    if response.packfile.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-pack raw packfile response missing packfile".into(),
        ));
    }
    if !response.packfile.starts_with(b"PACK") {
        return Err(GitError::InvalidFormat(
            "upload-pack raw packfile response packfile must start with PACK".into(),
        ));
    }
    Ok(response)
}

pub fn encode_upload_pack_raw_packfile_response(
    response: &UploadPackRawPackfileResponse,
) -> Result<Vec<u8>> {
    if response.packfile.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-pack raw packfile response missing packfile".into(),
        ));
    }
    if !response.packfile.starts_with(b"PACK") {
        return Err(GitError::InvalidFormat(
            "upload-pack raw packfile response packfile must start with PACK".into(),
        ));
    }
    let mut out = Vec::new();
    for acknowledgment in &response.acknowledgments {
        write_pkt_line_payload(
            &mut out,
            &encode_upload_pack_acknowledgment(acknowledgment)?,
        )?;
    }
    out.extend_from_slice(&response.packfile);
    Ok(out)
}

pub fn read_upload_pack_raw_packfile_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<UploadPackRawPackfileResponse> {
    read_upload_pack_raw_packfile_response_with_limits(format, reader, TransportLimits::default())
}

/// [`read_upload_pack_raw_packfile_response`] with explicit [`TransportLimits`],
/// for embedders that configure the ceiling and for tests that need to exercise
/// the bound without materialising a multi-gigabyte response.
pub fn read_upload_pack_raw_packfile_response_with_limits(
    format: ObjectFormat,
    reader: &mut impl Read,
    limits: TransportLimits,
) -> Result<UploadPackRawPackfileResponse> {
    let input = read_to_end_bounded(reader, limits.packfile_response())?;
    parse_upload_pack_raw_packfile_response(format, &input)
}

pub fn read_upload_pack_raw_packfile_response_header(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<UploadPackRawPackfileResponseHeader> {
    let mut acknowledgments = Vec::new();
    loop {
        let mut header = [0u8; 4];
        reader.read_exact(&mut header)?;
        if &header == b"PACK" {
            return Ok(UploadPackRawPackfileResponseHeader {
                acknowledgments,
                pack_prefix: header.to_vec(),
            });
        }
        let len = parse_pkt_len(&header)?;
        let payload = match len {
            0..=2 => {
                return Err(GitError::InvalidFormat(
                    "upload-pack raw packfile response contains a control packet".into(),
                ));
            }
            3 => {
                return Err(GitError::InvalidFormat(
                    "reserved pkt-line length 0003".into(),
                ));
            }
            4..=PKT_LINE_MAX_LEN => {
                let mut payload = vec![0; len - 4];
                reader.read_exact(&mut payload)?;
                payload
            }
            _ => {
                return Err(GitError::InvalidFormat(format!(
                    "pkt-line length exceeds {PKT_LINE_MAX_LEN}: {len}"
                )));
            }
        };
        trace_packet_read_payload(&payload);
        let trimmed = trim_trailing_lf(&payload);
        if trimmed == b"NAK" || trimmed.starts_with(b"ACK ") {
            acknowledgments.push(parse_upload_pack_acknowledgment(format, &payload)?);
            continue;
        }
        return Err(GitError::InvalidFormat(
            "upload-pack raw packfile response has non-ack pkt-line before packfile".into(),
        ));
    }
}

pub fn read_upload_pack_shallow_info_section(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    let mut entries = Vec::new();
    loop {
        let Some(frame) = read_pkt_line_frame(reader)? else {
            return Err(GitError::InvalidFormat(
                "upload-pack shallow-info section ended before flush".into(),
            ));
        };
        match frame {
            PktLineFrame::Data(payload) => {
                entries.push(parse_fetch_shallow_info(format, &payload)?)
            }
            PktLineFrame::Flush => return Ok(entries),
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "upload-pack shallow-info section contains a non-flush control packet".into(),
                ));
            }
        }
    }
}

pub fn read_upload_pack_shallow_info_and_raw_packfile_response_header(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<(
    Vec<ProtocolV2FetchShallowInfo>,
    UploadPackRawPackfileResponseHeader,
)> {
    let shallow = read_upload_pack_shallow_info_section(format, reader)?;
    let raw = read_upload_pack_raw_packfile_response_header(format, reader)?;
    Ok((shallow, raw))
}

pub fn write_upload_pack_raw_packfile_response(
    writer: &mut impl Write,
    response: &UploadPackRawPackfileResponse,
) -> Result<()> {
    if response.packfile.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-pack raw packfile response missing packfile".into(),
        ));
    }
    if !response.packfile.starts_with(b"PACK") {
        return Err(GitError::InvalidFormat(
            "upload-pack raw packfile response packfile must start with PACK".into(),
        ));
    }
    for acknowledgment in &response.acknowledgments {
        write_upload_pack_acknowledgment(writer, acknowledgment)?;
    }
    writer.write_all(&response.packfile)?;
    Ok(())
}

/// Parse the smart-HTTP/SSH v0 *shallow-info* section that precedes the packfile
/// when the upload-pack request carried `shallow`/`deepen`/`deepen-since`/
/// `deepen-not` arguments.
///
/// The section is zero or more `shallow <oid>` / `unshallow <oid>` pkt-lines
/// terminated by a flush-pkt; git always emits it (even empty, as a bare flush)
/// when the request was a deepen request, and never emits it otherwise. Returns
/// the parsed entries and the number of bytes consumed (through the flush) so the
/// caller can continue parsing the trailing acknowledgments + packfile from
/// `&input[consumed..]` (see [`parse_upload_pack_shallow_info_and_raw_packfile_response`]).
pub fn parse_upload_pack_shallow_info_section(
    format: ObjectFormat,
    input: &[u8],
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, usize)> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    loop {
        let (frame, consumed) = PktLineFrame::parse(&input[offset..])?;
        offset += consumed;
        match frame {
            PktLineFrame::Data(payload) => {
                entries.push(parse_fetch_shallow_info(format, &payload)?)
            }
            PktLineFrame::Flush => return Ok((entries, offset)),
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "upload-pack shallow-info section contains a non-flush control packet".into(),
                ));
            }
        }
    }
}

/// Parse a raw upload-pack response that begins with a *shallow-info* section,
/// i.e. the response to a deepen request.
///
/// This is [`parse_upload_pack_raw_packfile_response`] preceded by the
/// shallow-info section ([`parse_upload_pack_shallow_info_section`]): it returns
/// the `shallow`/`unshallow` entries the server reported alongside the parsed
/// acknowledgments + raw packfile. Use it only when the request carried a
/// `shallow`/`deepen`/`deepen-since`/`deepen-not` argument; for a plain (non-deepen)
/// request the response has no leading shallow-info section and
/// [`parse_upload_pack_raw_packfile_response`] must be used instead.
pub fn parse_upload_pack_shallow_info_and_raw_packfile_response(
    format: ObjectFormat,
    input: &[u8],
) -> Result<(
    Vec<ProtocolV2FetchShallowInfo>,
    UploadPackRawPackfileResponse,
)> {
    let (shallow, consumed) = parse_upload_pack_shallow_info_section(format, input)?;
    let response = parse_upload_pack_raw_packfile_response(format, &input[consumed..])?;
    Ok((shallow, response))
}

/// Read a raw upload-pack response that begins with a *shallow-info* section from
/// `reader`, returning the `shallow`/`unshallow` entries and the parsed
/// acknowledgments + raw packfile.
///
/// The reader counterpart of
/// [`parse_upload_pack_shallow_info_and_raw_packfile_response`]; see it for when
/// this applies.
pub fn read_upload_pack_shallow_info_and_raw_packfile_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<(
    Vec<ProtocolV2FetchShallowInfo>,
    UploadPackRawPackfileResponse,
)> {
    read_upload_pack_shallow_info_and_raw_packfile_response_with_limits(
        format,
        reader,
        TransportLimits::default(),
    )
}

/// [`read_upload_pack_shallow_info_and_raw_packfile_response`] with explicit
/// [`TransportLimits`], for embedders that configure the ceiling and for tests
/// that need to exercise the bound without materialising a multi-gigabyte
/// response.
pub fn read_upload_pack_shallow_info_and_raw_packfile_response_with_limits(
    format: ObjectFormat,
    reader: &mut impl Read,
    limits: TransportLimits,
) -> Result<(
    Vec<ProtocolV2FetchShallowInfo>,
    UploadPackRawPackfileResponse,
)> {
    let (shallow, header) =
        read_upload_pack_shallow_info_and_raw_packfile_response_header(format, reader)?;
    let mut packfile = header.pack_prefix;
    // The already-read prefix counts against the ceiling, so a caller cannot
    // spend the budget twice.
    append_to_end_bounded(&mut *reader, &mut packfile, limits.packfile_response())?;
    Ok((
        shallow,
        UploadPackRawPackfileResponse {
            acknowledgments: header.acknowledgments,
            packfile,
        },
    ))
}
fn set_upload_pack_flag(value: &mut bool, capability: &Capability) -> Result<()> {
    reject_capability_value("upload-pack capability", capability)?;
    if *value {
        return Err(GitError::InvalidFormat(format!(
            "upload-pack has duplicate {} capability",
            capability.name
        )));
    }
    *value = true;
    Ok(())
}

fn push_upload_pack_flag(capabilities: &mut Vec<Capability>, name: &str, enabled: bool) {
    if enabled {
        capabilities.push(Capability {
            name: name.into(),
            value: None,
        });
    }
}

fn is_known_upload_pack_capability(name: &str) -> bool {
    matches!(
        name,
        "multi_ack"
            | "multi_ack_detailed"
            | "no-done"
            | "thin-pack"
            | "side-band"
            | "side-band-64k"
            | "ofs-delta"
            | "shallow"
            | "deepen-since"
            | "deepen-not"
            | "include-tag"
            | "no-progress"
            | "allow-tip-sha1-in-want"
            | "allow-reachable-sha1-in-want"
            | "filter"
            | "agent"
            | "object-format"
            | "symref"
    )
}

fn is_upload_pack_flag_capability(name: &str) -> bool {
    matches!(
        name,
        "multi_ack"
            | "multi_ack_detailed"
            | "no-done"
            | "thin-pack"
            | "side-band"
            | "side-band-64k"
            | "ofs-delta"
            | "shallow"
            | "deepen-since"
            | "deepen-not"
            | "include-tag"
            | "no-progress"
            | "allow-tip-sha1-in-want"
            | "allow-reachable-sha1-in-want"
            | "filter"
    )
}
