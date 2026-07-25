use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use std::io::{Read, Write};

use crate::pktline::{
    PktLineFrame, PktLineReadLimits, ProtocolVersion, line, line_from_str, parse_oid_argument,
    parse_protocol_v2_line_text, read_pkt_line_frame, read_pkt_line_frames_until_flush,
    read_pkt_line_frames_until_flush_with_limits, read_pkt_line_frames_until_response_end,
    read_pkt_line_frames_until_response_end_with_limits, trace_packet_read_payload,
    trim_trailing_lf, validate_capability_name, validate_protocol_v2_line,
    validate_protocol_v2_token, write_pkt_line_frame, write_pkt_line_payload,
};
use crate::sideband::{
    SideBandChannel, SideBandDemux, SideBandPacket, encode_sideband_packet,
    parse_and_demux_sideband_packets, parse_sideband_packet, write_sideband_payload,
};
use crate::v0::{RefAdvertisement, RefAdvertisementSet, TransportHandshake};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolV2CommandRequest {
    pub command: String,
    pub capabilities: Vec<Capability>,
    pub arguments: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolV2Request {
    Command(ProtocolV2CommandRequest),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolV2Command {
    LsRefs(ProtocolV2LsRefsRequest),
    Fetch(ProtocolV2FetchRequest),
    ObjectInfo(ProtocolV2ObjectInfoRequest),
    Unknown(ProtocolV2CommandRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolV2SessionRequest {
    Command(ProtocolV2Command),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2CommandOptions {
    pub agent: Option<String>,
    pub object_format: Option<ObjectFormat>,
    pub server_options: Vec<String>,
    pub extra: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2FetchFeatures {
    pub shallow: bool,
    pub wait_for_done: bool,
    pub filter: bool,
    pub ref_in_want: bool,
    pub sideband_all: bool,
    pub packfile_uris: bool,
    pub unknown: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2LsRefsFeatures {
    pub unborn: bool,
    pub unknown: Vec<String>,
}

impl ProtocolV2CommandRequest {
    pub fn new(command: impl Into<String>) -> Result<Self> {
        let command = command.into();
        validate_capability_name(&command)?;
        Ok(Self {
            command,
            capabilities: Vec::new(),
            arguments: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2LsRefsRequest {
    pub peel: bool,
    pub symrefs: bool,
    pub unborn: bool,
    pub ref_prefixes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolV2LsRefsRef {
    pub oid: ObjectId,
    pub name: String,
    pub peeled: Option<ObjectId>,
    pub symref_target: Option<String>,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolV2LsRefsRecord {
    Ref(ProtocolV2LsRefsRef),
    Unborn {
        name: String,
        symref_target: Option<String>,
        attributes: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2FetchRequest {
    pub wants: Vec<ObjectId>,
    pub want_refs: Vec<String>,
    pub haves: Vec<ObjectId>,
    pub shallow: Vec<ObjectId>,
    pub deepen: Option<u32>,
    pub deepen_since: Option<u64>,
    pub deepen_not: Vec<String>,
    pub deepen_relative: bool,
    pub filter: Option<String>,
    pub packfile_uris: Option<String>,
    pub thin_pack: bool,
    pub no_progress: bool,
    pub include_tag: bool,
    pub ofs_delta: bool,
    pub sideband_all: bool,
    pub wait_for_done: bool,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolV2FetchAcknowledgment {
    Nak,
    Ack(ObjectId),
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolV2FetchShallowInfo {
    Shallow(ObjectId),
    Unshallow(ObjectId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolV2FetchWantedRef {
    pub oid: ObjectId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolV2FetchPackfileUri {
    pub pack_hash: ObjectId,
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolV2FetchResponseSection {
    Acknowledgments(Vec<ProtocolV2FetchAcknowledgment>),
    ShallowInfo(Vec<ProtocolV2FetchShallowInfo>),
    WantedRefs(Vec<ProtocolV2FetchWantedRef>),
    PackfileUris(Vec<ProtocolV2FetchPackfileUri>),
    Packfile(Vec<Vec<u8>>),
    Unknown { name: String, lines: Vec<Vec<u8>> },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2FetchSidebandAllResponse {
    pub sections: Vec<ProtocolV2FetchResponseSection>,
    pub progress: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2FetchResponseHeader {
    pub sections: Vec<ProtocolV2FetchResponseSection>,
    pub has_packfile: bool,
}

/// The acknowledgment phase of a multi-round protocol-v2 fetch.
///
/// When `has_following_sections` is true, the delimiter after
/// `acknowledgments` has already been consumed and the reader is positioned at
/// the next response section. Otherwise the response ended with a flush and
/// the client must send another negotiation request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2FetchNegotiationResponse {
    pub acknowledgments: Vec<ProtocolV2FetchAcknowledgment>,
    pub has_following_sections: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2ObjectInfoRequest {
    pub size: bool,
    pub oids: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolV2ObjectInfoRecord {
    pub oid: ObjectId,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolV2ObjectInfoResponse {
    pub size: bool,
    pub records: Vec<ProtocolV2ObjectInfoRecord>,
}

impl ProtocolV2LsRefsRequest {
    pub fn from_command_request(request: &ProtocolV2CommandRequest) -> Result<Self> {
        if request.command != "ls-refs" {
            return Err(GitError::InvalidFormat(format!(
                "expected ls-refs command, got {}",
                request.command
            )));
        }
        let mut out = Self::default();
        for argument in &request.arguments {
            let text = std::str::from_utf8(argument)
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
            match text {
                "peel" => out.peel = true,
                "symrefs" => out.symrefs = true,
                "unborn" => out.unborn = true,
                value if value.starts_with("ref-prefix ") => {
                    let prefix = value
                        .strip_prefix("ref-prefix ")
                        .ok_or_else(|| GitError::InvalidFormat("invalid ref-prefix".into()))?;
                    validate_protocol_v2_token("ls-refs ref-prefix", prefix)?;
                    out.ref_prefixes.push(prefix.to_string());
                }
                other => {
                    return Err(GitError::InvalidFormat(format!(
                        "unsupported ls-refs argument {other}"
                    )));
                }
            }
        }
        Ok(out)
    }

    pub fn to_command_request(&self) -> Result<ProtocolV2CommandRequest> {
        let mut request = ProtocolV2CommandRequest::new("ls-refs")?;
        if self.peel {
            request.arguments.push(b"peel".to_vec());
        }
        if self.symrefs {
            request.arguments.push(b"symrefs".to_vec());
        }
        if self.unborn {
            request.arguments.push(b"unborn".to_vec());
        }
        for prefix in &self.ref_prefixes {
            validate_protocol_v2_token("ls-refs ref-prefix", prefix)?;
            request
                .arguments
                .push(format!("ref-prefix {prefix}").into_bytes());
        }
        Ok(request)
    }
}

impl ProtocolV2FetchRequest {
    pub fn from_command_request(
        format: ObjectFormat,
        request: &ProtocolV2CommandRequest,
    ) -> Result<Self> {
        if request.command != "fetch" {
            return Err(GitError::InvalidFormat(format!(
                "expected fetch command, got {}",
                request.command
            )));
        }
        let mut out = Self::default();
        for argument in &request.arguments {
            let text = std::str::from_utf8(argument)
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
            match text {
                "thin-pack" => out.thin_pack = true,
                "no-progress" => out.no_progress = true,
                "include-tag" => out.include_tag = true,
                "ofs-delta" => out.ofs_delta = true,
                "sideband-all" => out.sideband_all = true,
                "wait-for-done" => out.wait_for_done = true,
                "deepen-relative" => out.deepen_relative = true,
                "done" => out.done = true,
                value if value.starts_with("want ") => {
                    out.wants
                        .push(parse_oid_argument(format, "fetch want", value, "want ")?);
                }
                value if value.starts_with("want-ref ") => {
                    let name = value
                        .strip_prefix("want-ref ")
                        .ok_or_else(|| GitError::InvalidFormat("invalid fetch want-ref".into()))?;
                    validate_protocol_v2_token("fetch want-ref", name)?;
                    out.want_refs.push(name.to_string());
                }
                value if value.starts_with("have ") => {
                    out.haves
                        .push(parse_oid_argument(format, "fetch have", value, "have ")?);
                }
                value if value.starts_with("shallow ") => {
                    out.shallow.push(parse_oid_argument(
                        format,
                        "fetch shallow",
                        value,
                        "shallow ",
                    )?);
                }
                value if value.starts_with("deepen ") => {
                    if out.deepen.is_some() {
                        return Err(GitError::InvalidFormat(
                            "fetch request has duplicate deepen".into(),
                        ));
                    }
                    out.deepen = Some(parse_u32_argument("fetch deepen", value, "deepen ")?);
                }
                value if value.starts_with("deepen-since ") => {
                    if out.deepen_since.is_some() {
                        return Err(GitError::InvalidFormat(
                            "fetch request has duplicate deepen-since".into(),
                        ));
                    }
                    out.deepen_since = Some(parse_u64_argument(
                        "fetch deepen-since",
                        value,
                        "deepen-since ",
                    )?);
                }
                value if value.starts_with("deepen-not ") => {
                    let name = value.strip_prefix("deepen-not ").ok_or_else(|| {
                        GitError::InvalidFormat("invalid fetch deepen-not".into())
                    })?;
                    validate_protocol_v2_token("fetch deepen-not", name)?;
                    out.deepen_not.push(name.to_string());
                }
                value if value.starts_with("filter ") => {
                    if out.filter.is_some() {
                        return Err(GitError::InvalidFormat(
                            "fetch request has duplicate filter".into(),
                        ));
                    }
                    let filter = value
                        .strip_prefix("filter ")
                        .ok_or_else(|| GitError::InvalidFormat("invalid fetch filter".into()))?;
                    validate_protocol_v2_token("fetch filter", filter)?;
                    out.filter = Some(filter.to_string());
                }
                value if value.starts_with("packfile-uris ") => {
                    if out.packfile_uris.is_some() {
                        return Err(GitError::InvalidFormat(
                            "fetch request has duplicate packfile-uris".into(),
                        ));
                    }
                    let protocols = value.strip_prefix("packfile-uris ").ok_or_else(|| {
                        GitError::InvalidFormat("invalid fetch packfile-uris".into())
                    })?;
                    validate_protocol_v2_token("fetch packfile-uris", protocols)?;
                    out.packfile_uris = Some(protocols.to_string());
                }
                other => {
                    return Err(GitError::InvalidFormat(format!(
                        "unsupported fetch argument {other}"
                    )));
                }
            }
        }
        Ok(out)
    }

    pub fn to_command_request(&self) -> Result<ProtocolV2CommandRequest> {
        let mut request = ProtocolV2CommandRequest::new("fetch")?;
        for oid in &self.wants {
            request.arguments.push(format!("want {oid}").into_bytes());
        }
        for name in &self.want_refs {
            validate_protocol_v2_token("fetch want-ref", name)?;
            request
                .arguments
                .push(format!("want-ref {name}").into_bytes());
        }
        for oid in &self.haves {
            request.arguments.push(format!("have {oid}").into_bytes());
        }
        for oid in &self.shallow {
            request
                .arguments
                .push(format!("shallow {oid}").into_bytes());
        }
        if let Some(deepen) = self.deepen {
            if deepen == 0 {
                return Err(GitError::InvalidFormat(
                    "fetch deepen must be positive".into(),
                ));
            }
            request
                .arguments
                .push(format!("deepen {deepen}").into_bytes());
        }
        if let Some(deepen_since) = self.deepen_since {
            request
                .arguments
                .push(format!("deepen-since {deepen_since}").into_bytes());
        }
        for name in &self.deepen_not {
            validate_protocol_v2_token("fetch deepen-not", name)?;
            request
                .arguments
                .push(format!("deepen-not {name}").into_bytes());
        }
        if self.deepen_relative {
            request.arguments.push(b"deepen-relative".to_vec());
        }
        if let Some(filter) = &self.filter {
            validate_protocol_v2_token("fetch filter", filter)?;
            request
                .arguments
                .push(format!("filter {filter}").into_bytes());
        }
        if let Some(protocols) = &self.packfile_uris {
            validate_protocol_v2_token("fetch packfile-uris", protocols)?;
            request
                .arguments
                .push(format!("packfile-uris {protocols}").into_bytes());
        }
        if self.thin_pack {
            request.arguments.push(b"thin-pack".to_vec());
        }
        if self.no_progress {
            request.arguments.push(b"no-progress".to_vec());
        }
        if self.include_tag {
            request.arguments.push(b"include-tag".to_vec());
        }
        if self.ofs_delta {
            request.arguments.push(b"ofs-delta".to_vec());
        }
        if self.sideband_all {
            request.arguments.push(b"sideband-all".to_vec());
        }
        if self.wait_for_done {
            request.arguments.push(b"wait-for-done".to_vec());
        }
        if self.done {
            request.arguments.push(b"done".to_vec());
        }
        Ok(request)
    }
}

impl ProtocolV2ObjectInfoRequest {
    pub fn from_command_request(
        format: ObjectFormat,
        request: &ProtocolV2CommandRequest,
    ) -> Result<Self> {
        if request.command != "object-info" {
            return Err(GitError::InvalidFormat(format!(
                "expected object-info command, got {}",
                request.command
            )));
        }
        let mut out = Self::default();
        for argument in &request.arguments {
            let text = parse_protocol_v2_line_text("object-info request argument", argument)?;
            if text == "size" {
                if out.size {
                    return Err(GitError::InvalidFormat(
                        "object-info request has duplicate size argument".into(),
                    ));
                }
                out.size = true;
            } else if text.starts_with("oid ") {
                out.oids
                    .push(parse_oid_argument(format, "object-info oid", text, "oid ")?);
            } else {
                return Err(GitError::InvalidFormat(format!(
                    "unsupported object-info request argument {text}"
                )));
            }
        }
        if !out.size {
            return Err(GitError::InvalidFormat(
                "object-info request is missing size argument".into(),
            ));
        }
        if out.oids.is_empty() {
            return Err(GitError::InvalidFormat(
                "object-info request is missing object ids".into(),
            ));
        }
        Ok(out)
    }

    pub fn to_command_request(&self) -> Result<ProtocolV2CommandRequest> {
        if !self.size {
            return Err(GitError::InvalidFormat(
                "object-info request is missing size argument".into(),
            ));
        }
        if self.oids.is_empty() {
            return Err(GitError::InvalidFormat(
                "object-info request is missing object ids".into(),
            ));
        }
        let mut request = ProtocolV2CommandRequest::new("object-info")?;
        request.arguments.push(b"size".to_vec());
        for oid in &self.oids {
            request.arguments.push(format!("oid {oid}").into_bytes());
        }
        Ok(request)
    }
}

pub fn parse_protocol_v2_advertisement(frames: &[PktLineFrame]) -> Result<TransportHandshake> {
    let Some((first, rest)) = frames.split_first() else {
        return Err(GitError::InvalidFormat(
            "protocol v2 advertisement is empty".into(),
        ));
    };
    match first {
        PktLineFrame::Data(payload) if trim_trailing_lf(payload) == b"version 2" => {}
        PktLineFrame::Data(_) => {
            return Err(GitError::InvalidFormat(
                "protocol v2 advertisement missing version line".into(),
            ));
        }
        _ => {
            return Err(GitError::InvalidFormat(
                "protocol v2 advertisement must start with a data line".into(),
            ));
        }
    }

    let mut capabilities = Vec::new();
    let mut saw_flush = false;
    for (idx, frame) in rest.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) => {
                if saw_flush {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 advertisement has data after flush".into(),
                    ));
                }
                capabilities.push(parse_protocol_v2_capability_line(payload)?);
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != rest.len() {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 advertisement has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "protocol v2 advertisement contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "protocol v2 advertisement missing flush".into(),
        ));
    }

    Ok(TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities,
    })
}

pub fn encode_protocol_v2_advertisement(
    handshake: &TransportHandshake,
) -> Result<Vec<PktLineFrame>> {
    if handshake.protocol != ProtocolVersion::V2 {
        return Err(GitError::InvalidFormat(
            "protocol v2 advertisement requires a v2 handshake".into(),
        ));
    }
    let mut frames = vec![PktLineFrame::data(line_from_str("version 2"))?];
    for capability in &handshake.capabilities {
        frames.push(PktLineFrame::data(line(encode_protocol_v2_capability(
            capability,
        )?))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_protocol_v2_advertisement(reader: &mut impl Read) -> Result<TransportHandshake> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_protocol_v2_advertisement(&frames)
}

pub fn write_protocol_v2_advertisement(
    writer: &mut impl Write,
    handshake: &TransportHandshake,
) -> Result<()> {
    if handshake.protocol != ProtocolVersion::V2 {
        return Err(GitError::InvalidFormat(
            "protocol v2 advertisement requires a v2 handshake".into(),
        ));
    }
    write_pkt_line_payload(writer, b"version 2\n")?;
    for capability in &handshake.capabilities {
        write_pkt_line_payload(writer, &line(encode_protocol_v2_capability(capability)?))?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

/// Trace a previously discovered v2 advertisement as incoming packets at the
/// current logical protocol consumer. Smart HTTP discovers capabilities in its
/// remote-helper phase, then forwards that handshake to `fetch`; replaying the
/// packet trace at that in-process boundary preserves Git's observable
/// `fetch< version 2` trace without putting a second advertisement on the HTTP
/// RPC wire.
pub fn trace_protocol_v2_advertisement_read(handshake: &TransportHandshake) -> Result<()> {
    for frame in encode_protocol_v2_advertisement(handshake)? {
        match frame {
            PktLineFrame::Data(payload) => trace_packet_read_payload(&payload),
            PktLineFrame::Flush => trace_packet_read_payload(b"0000"),
            PktLineFrame::Delimiter => trace_packet_read_payload(b"0001"),
            PktLineFrame::ResponseEnd => trace_packet_read_payload(b"0002"),
        }
    }
    Ok(())
}

pub fn parse_protocol_v2_command_request(
    frames: &[PktLineFrame],
) -> Result<ProtocolV2CommandRequest> {
    let Some((first, rest)) = frames.split_first() else {
        return Err(GitError::InvalidFormat(
            "protocol v2 command request is empty".into(),
        ));
    };
    let command = match first {
        PktLineFrame::Data(payload) => parse_protocol_v2_command_line(payload)?,
        _ => {
            return Err(GitError::InvalidFormat(
                "protocol v2 command request must start with a command line".into(),
            ));
        }
    };

    let mut capabilities = Vec::new();
    let mut arguments = Vec::new();
    let mut in_arguments = false;
    let mut saw_flush = false;
    for (idx, frame) in rest.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !in_arguments => {
                if saw_flush {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command request has data after flush".into(),
                    ));
                }
                capabilities.push(parse_protocol_v2_capability_line(payload)?);
            }
            PktLineFrame::Data(payload) => {
                if saw_flush {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command request has data after flush".into(),
                    ));
                }
                let argument = trim_trailing_lf(payload);
                if argument.is_empty() {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command argument is empty".into(),
                    ));
                }
                if argument
                    .iter()
                    .any(|byte| matches!(*byte, b'\n' | b'\r' | 0))
                {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command argument contains a delimiter byte".into(),
                    ));
                }
                arguments.push(argument.to_vec());
            }
            PktLineFrame::Delimiter => {
                if in_arguments {
                    return Err(GitError::InvalidFormat(format!(
                        "expected flush after {} arguments",
                        command
                    )));
                }
                if saw_flush {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command request has delimiter after flush".into(),
                    ));
                }
                in_arguments = true;
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != rest.len() {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command request has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "protocol v2 command request contains response-end".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "protocol v2 command request missing flush".into(),
        ));
    }

    Ok(ProtocolV2CommandRequest {
        command,
        capabilities,
        arguments,
    })
}

pub fn encode_protocol_v2_command_request(
    request: &ProtocolV2CommandRequest,
) -> Result<Vec<PktLineFrame>> {
    validate_capability_name(&request.command)?;
    let mut frames = Vec::new();
    frames.push(PktLineFrame::data(line_from_str(&format!(
        "command={}",
        request.command
    )))?);
    for capability in &request.capabilities {
        frames.push(PktLineFrame::data(line(encode_protocol_v2_capability(
            capability,
        )?))?);
    }
    if !request.arguments.is_empty() {
        frames.push(PktLineFrame::Delimiter);
        for argument in &request.arguments {
            validate_protocol_v2_argument(argument)?;
            let mut payload = argument.clone();
            payload.push(b'\n');
            frames.push(PktLineFrame::data(payload)?);
        }
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn parse_protocol_v2_request(frames: &[PktLineFrame]) -> Result<ProtocolV2Request> {
    if matches!(frames, [PktLineFrame::Flush]) {
        return Ok(ProtocolV2Request::Done);
    }
    parse_protocol_v2_command_request(frames).map(ProtocolV2Request::Command)
}

pub fn encode_protocol_v2_request(request: &ProtocolV2Request) -> Result<Vec<PktLineFrame>> {
    match request {
        ProtocolV2Request::Command(command) => encode_protocol_v2_command_request(command),
        ProtocolV2Request::Done => Ok(vec![PktLineFrame::Flush]),
    }
}

pub fn read_protocol_v2_request(reader: &mut impl Read) -> Result<ProtocolV2Request> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_protocol_v2_request(&frames)
}

pub fn write_protocol_v2_request(
    writer: &mut impl Write,
    request: &ProtocolV2Request,
) -> Result<()> {
    match request {
        ProtocolV2Request::Command(command) => write_protocol_v2_command_request(writer, command),
        ProtocolV2Request::Done => {
            writer.write_all(b"0000")?;
            Ok(())
        }
    }
}

pub fn read_protocol_v2_command_request(
    reader: &mut impl Read,
) -> Result<ProtocolV2CommandRequest> {
    let mut frames = Vec::new();
    loop {
        let Some(frame) = read_pkt_line_frame(reader)? else {
            if let Some(command) = frames.first().and_then(|frame| match frame {
                PktLineFrame::Data(payload) => parse_protocol_v2_command_line(payload).ok(),
                _ => None,
            }) && frames
                .iter()
                .any(|frame| matches!(frame, PktLineFrame::Delimiter))
            {
                return Err(GitError::InvalidFormat(format!(
                    "expected flush after {} arguments",
                    command
                )));
            }
            return Err(GitError::InvalidFormat(
                "pkt-line stream ended before control packet".into(),
            ));
        };
        let done = matches!(frame, PktLineFrame::Flush);
        frames.push(frame);
        if done {
            break;
        }
    }
    parse_protocol_v2_command_request(&frames)
}

pub fn write_protocol_v2_command_request(
    writer: &mut impl Write,
    request: &ProtocolV2CommandRequest,
) -> Result<()> {
    validate_capability_name(&request.command)?;
    write_pkt_line_payload(
        writer,
        &line_from_str(&format!("command={}", request.command)),
    )?;
    for capability in &request.capabilities {
        write_pkt_line_payload(writer, &line(encode_protocol_v2_capability(capability)?))?;
    }
    if !request.arguments.is_empty() {
        write_pkt_line_frame(writer, &PktLineFrame::Delimiter)?;
        for argument in &request.arguments {
            validate_protocol_v2_argument(argument)?;
            let mut payload = argument.clone();
            payload.push(b'\n');
            write_pkt_line_payload(writer, &payload)?;
        }
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn read_protocol_v2_ls_refs_request(reader: &mut impl Read) -> Result<ProtocolV2LsRefsRequest> {
    let request = read_protocol_v2_command_request(reader)?;
    ProtocolV2LsRefsRequest::from_command_request(&request)
}

pub fn write_protocol_v2_ls_refs_request(
    writer: &mut impl Write,
    request: &ProtocolV2LsRefsRequest,
) -> Result<()> {
    let command = request.to_command_request()?;
    write_protocol_v2_command_request(writer, &command)
}

pub fn parse_protocol_v2_ls_refs_response(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<Vec<ProtocolV2LsRefsRecord>> {
    let mut records = Vec::new();
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) => {
                if saw_flush {
                    return Err(GitError::InvalidFormat(
                        "ls-refs response has data after flush".into(),
                    ));
                }
                records.push(parse_protocol_v2_ls_refs_line(format, payload)?);
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if !flush_terminates_protocol_v2_response(frames, idx) {
                    return Err(GitError::InvalidFormat(
                        "ls-refs response has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::ResponseEnd if saw_flush && idx + 1 == frames.len() => {}
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "ls-refs response contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "ls-refs response missing flush".into(),
        ));
    }
    Ok(records)
}

pub fn encode_protocol_v2_ls_refs_response(
    records: &[ProtocolV2LsRefsRecord],
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    for record in records {
        frames.push(PktLineFrame::data(line_from_str(
            &format_protocol_v2_ls_refs_record(record)?,
        ))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

fn frames_start_with_protocol_v2_advertisement(frames: &[PktLineFrame]) -> bool {
    matches!(
        frames.first(),
        Some(PktLineFrame::Data(payload)) if trim_trailing_lf(payload) == b"version 2"
    )
}

/// Advance past a leading protocol v2 capability advertisement when present.
/// Returns the first non-advertisement frame when the stream does not begin with
/// `version 2`.
///
/// A capability advertisement (`version 2` … flush) is never sideband-wrapped: it
/// is emitted before the fetch command's response body, and sideband multiplexing
/// only applies to the fetch response itself. The advertisement's leading pkt is a
/// raw `version 2`, whose first byte (`v`, 0x76) can never collide with a sideband
/// channel byte (0x01–0x03), so the advertisement check is unambiguous even under
/// `sideband-all`.
///
/// When `sideband_all` is negotiated and the stream does *not* begin with an
/// advertisement, the first fetch-response frame (a section header such as
/// `acknowledgments`, or a leading channel-2 progress frame) arrives
/// sideband-wrapped. We demux it here so the section-header reader in
/// `read_protocol_v2_fetch_response_header` sees a plain payload rather than a raw
/// control byte.
fn skip_leading_protocol_v2_advertisement_if_present(
    reader: &mut impl Read,
    sideband_all: bool,
) -> Result<Option<PktLineFrame>> {
    let first = read_pkt_line_frame(reader)?.ok_or_else(|| {
        GitError::InvalidFormat("protocol v2 response ended before first pkt-line".into())
    })?;
    let PktLineFrame::Data(payload) = &first else {
        return Ok(Some(first));
    };
    if trim_trailing_lf(payload) != b"version 2" {
        if sideband_all {
            // Not an advertisement: the first fetch-response frame is
            // sideband-wrapped. Demux it, skipping a leading progress frame,
            // so the caller receives the demultiplexed payload.
            let packet = parse_sideband_packet(payload)?;
            let demuxed = match packet.channel {
                SideBandChannel::Data => PktLineFrame::Data(packet.data),
                SideBandChannel::Progress => read_protocol_v2_fetch_metadata_frame(reader, true)?,
                SideBandChannel::Fatal => {
                    let message = String::from_utf8_lossy(&packet.data).into_owned();
                    return Err(GitError::InvalidFormat(format!(
                        "sideband fatal: {message}"
                    )));
                }
            };
            return Ok(Some(demuxed));
        }
        return Ok(Some(first));
    }
    loop {
        match read_pkt_line_frame(reader)? {
            Some(PktLineFrame::Flush) => return Ok(None),
            Some(PktLineFrame::Data(_)) => {}
            Some(_) => {
                return Err(GitError::InvalidFormat(
                    "protocol v2 capability advertisement contains a non-flush control packet"
                        .into(),
                ));
            }
            None => {
                return Err(GitError::InvalidFormat(
                    "protocol v2 capability advertisement missing flush".into(),
                ));
            }
        }
    }
}

/// Read the payload section of a stateless smart-HTTP v2 RPC response, skipping a
/// leading capability advertisement when the server includes one before the
/// command result.
pub fn read_protocol_v2_stateless_rpc_payload_frames(
    reader: &mut impl Read,
) -> Result<Vec<PktLineFrame>> {
    read_protocol_v2_stateless_rpc_payload_frames_with_limits(reader, PktLineReadLimits::CONTROL)
}

/// As [`read_protocol_v2_stateless_rpc_payload_frames`], with an explicit
/// accumulation budget. `fetch` responses may carry packfile bytes in sideband
/// frames and pass [`PktLineReadLimits::PACK_STREAM`]; `ls-refs`,
/// `object-info`, and `bundle-uri` responses keep the default
/// [`PktLineReadLimits::CONTROL`] budget.
pub fn read_protocol_v2_stateless_rpc_payload_frames_with_limits(
    reader: &mut impl Read,
    limits: PktLineReadLimits,
) -> Result<Vec<PktLineFrame>> {
    // Whether the first flush-terminated stream is the optional capability
    // advertisement or the payload itself is only known after reading it, so
    // both reads use the caller's budget.
    let mut frames = read_pkt_line_frames_until_flush_with_limits(reader, limits)?;
    if frames_start_with_protocol_v2_advertisement(&frames) {
        frames = read_pkt_line_frames_until_flush_with_limits(reader, limits)?;
    }
    Ok(frames)
}

pub fn read_protocol_v2_ls_refs_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<ProtocolV2LsRefsRecord>> {
    let frames = read_protocol_v2_stateless_rpc_payload_frames(reader)?;
    parse_protocol_v2_ls_refs_response(format, &frames)
}

pub fn write_protocol_v2_ls_refs_response(
    writer: &mut impl Write,
    records: &[ProtocolV2LsRefsRecord],
) -> Result<()> {
    for record in records {
        write_pkt_line_payload(
            writer,
            &line_from_str(&format_protocol_v2_ls_refs_record(record)?),
        )?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn read_protocol_v2_ls_refs_response_until_response_end(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<ProtocolV2LsRefsRecord>> {
    let frames = read_pkt_line_frames_until_response_end(reader)?;
    parse_protocol_v2_ls_refs_response(format, &frames)
}

pub fn write_protocol_v2_ls_refs_response_with_response_end(
    writer: &mut impl Write,
    records: &[ProtocolV2LsRefsRecord],
) -> Result<()> {
    write_protocol_v2_ls_refs_response(writer, records)?;
    writer.write_all(b"0002")?;
    Ok(())
}

pub fn exchange_protocol_v2_ls_refs(
    format: ObjectFormat,
    reader: &mut impl Read,
    writer: &mut impl Write,
    request: &ProtocolV2LsRefsRequest,
) -> Result<Vec<ProtocolV2LsRefsRecord>> {
    write_protocol_v2_ls_refs_request(writer, request)?;
    writer.flush()?;
    read_protocol_v2_ls_refs_response(format, reader)
}

/// Bridge a parsed protocol v2 `ls-refs` response into the shared
/// [`RefAdvertisementSet`]/[`RefAdvertisement`] types used by the v0/v1 codecs,
/// so callers can drive v2 clone/fetch through the same ref-advertisement
/// machinery.
///
/// Each [`ProtocolV2LsRefsRecord::Ref`] becomes a [`RefAdvertisement`]. A
/// `peeled:<oid>` attribute is emitted as an additional `<peeled-oid>
/// <name>^{}` advertisement, matching the v0/v1 peeled-tag convention.
/// `symref-target:<target>` attributes are collected as `symref=<name>:<target>`
/// capabilities on the first advertised ref, mirroring how the upload-pack v0/v1
/// advertisement carries symrefs. [`ProtocolV2LsRefsRecord::Unborn`] records have
/// no object id, so they cannot be represented as a [`RefAdvertisement`]; an
/// unborn record carrying a `symref-target` is preserved as a `symref` capability
/// while otherwise being skipped. The returned set always reports
/// [`ProtocolVersion::V2`].
pub fn protocol_v2_ls_refs_records_to_ref_advertisement_set(
    records: &[ProtocolV2LsRefsRecord],
) -> Result<RefAdvertisementSet> {
    let mut refs: Vec<RefAdvertisement> = Vec::new();
    let mut symrefs: Vec<Capability> = Vec::new();
    for record in records {
        match record {
            ProtocolV2LsRefsRecord::Ref(reference) => {
                validate_protocol_v2_token("ls-refs ref name", &reference.name)?;
                refs.push(RefAdvertisement {
                    oid: reference.oid,
                    name: reference.name.clone(),
                    capabilities: Vec::new(),
                });
                if let Some(peeled) = &reference.peeled {
                    refs.push(RefAdvertisement {
                        oid: peeled.clone(),
                        name: format!("{}^{{}}", reference.name),
                        capabilities: Vec::new(),
                    });
                }
                if let Some(target) = &reference.symref_target {
                    symrefs.push(protocol_v2_symref_capability(&reference.name, target)?);
                }
            }
            ProtocolV2LsRefsRecord::Unborn {
                name,
                symref_target,
                ..
            } => {
                validate_protocol_v2_token("ls-refs ref name", name)?;
                if let Some(target) = symref_target {
                    symrefs.push(protocol_v2_symref_capability(name, target)?);
                }
            }
        }
    }
    if !symrefs.is_empty() {
        if let Some(first) = refs.first_mut() {
            first.capabilities = symrefs;
        } else {
            return Err(GitError::InvalidFormat(
                "ls-refs response advertised symrefs without any concrete refs".into(),
            ));
        }
    }
    Ok(RefAdvertisementSet {
        protocol: ProtocolVersion::V2,
        refs,
        shallow: Vec::new(),
    })
}

/// Parse a protocol v2 `ls-refs` response and bridge it into the shared
/// [`RefAdvertisementSet`] type. Convenience wrapper combining
/// [`parse_protocol_v2_ls_refs_response`] and
/// [`protocol_v2_ls_refs_records_to_ref_advertisement_set`].
pub fn parse_protocol_v2_ls_refs_response_as_ref_advertisement_set(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<RefAdvertisementSet> {
    let records = parse_protocol_v2_ls_refs_response(format, frames)?;
    protocol_v2_ls_refs_records_to_ref_advertisement_set(&records)
}

/// Read a protocol v2 `ls-refs` response from `reader` and bridge it into the
/// shared [`RefAdvertisementSet`] type.
pub fn read_protocol_v2_ls_refs_response_as_ref_advertisement_set(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<RefAdvertisementSet> {
    let records = read_protocol_v2_ls_refs_response(format, reader)?;
    protocol_v2_ls_refs_records_to_ref_advertisement_set(&records)
}

fn protocol_v2_symref_capability(name: &str, target: &str) -> Result<Capability> {
    validate_protocol_v2_token("ls-refs symref-target", target)?;
    Ok(Capability {
        name: "symref".into(),
        value: Some(format!("{name}:{target}")),
    })
}

pub fn read_protocol_v2_fetch_request(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ProtocolV2FetchRequest> {
    let request = read_protocol_v2_command_request(reader)?;
    ProtocolV2FetchRequest::from_command_request(format, &request)
}

pub fn write_protocol_v2_fetch_request(
    writer: &mut impl Write,
    request: &ProtocolV2FetchRequest,
) -> Result<()> {
    let command = request.to_command_request()?;
    write_protocol_v2_command_request(writer, &command)
}

pub fn read_protocol_v2_object_info_request(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ProtocolV2ObjectInfoRequest> {
    let request = read_protocol_v2_command_request(reader)?;
    ProtocolV2ObjectInfoRequest::from_command_request(format, &request)
}

pub fn write_protocol_v2_object_info_request(
    writer: &mut impl Write,
    request: &ProtocolV2ObjectInfoRequest,
) -> Result<()> {
    let command = request.to_command_request()?;
    write_protocol_v2_command_request(writer, &command)
}

pub fn parse_protocol_v2_fetch_response(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<Vec<ProtocolV2FetchResponseSection>> {
    let mut sections = Vec::new();
    let mut current: Option<(String, Vec<Vec<u8>>)> = None;
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) => {
                if saw_flush {
                    return Err(GitError::InvalidFormat(
                        "fetch response has data after flush".into(),
                    ));
                }
                if let Some((_name, lines)) = &mut current {
                    lines.push(payload.clone());
                } else {
                    let name = parse_fetch_section_header(payload)?;
                    current = Some((name, Vec::new()));
                }
            }
            PktLineFrame::Delimiter => {
                if saw_flush {
                    return Err(GitError::InvalidFormat(
                        "fetch response has delimiter after flush".into(),
                    ));
                }
                let Some((name, lines)) = current.take() else {
                    return Err(GitError::InvalidFormat(
                        "fetch response has delimiter before section".into(),
                    ));
                };
                sections.push(parse_fetch_section(format, name, lines)?);
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if !flush_terminates_protocol_v2_response(frames, idx) {
                    return Err(GitError::InvalidFormat(
                        "fetch response has frames after flush".into(),
                    ));
                }
                if let Some((name, lines)) = current.take() {
                    sections.push(parse_fetch_section(format, name, lines)?);
                }
            }
            PktLineFrame::ResponseEnd if saw_flush && idx + 1 == frames.len() => {}
            PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "fetch response contains response-end".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "fetch response missing flush".into(),
        ));
    }
    Ok(sections)
}

pub fn encode_protocol_v2_fetch_response(
    sections: &[ProtocolV2FetchResponseSection],
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    for (idx, section) in sections.iter().enumerate() {
        if idx != 0 {
            frames.push(PktLineFrame::Delimiter);
        }
        frames.push(PktLineFrame::data(line_from_str(
            protocol_v2_fetch_section_name(section),
        ))?);
        for line in format_protocol_v2_fetch_section_lines(section)? {
            frames.push(PktLineFrame::data(line)?);
        }
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn parse_protocol_v2_fetch_sideband_all_response(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<ProtocolV2FetchSidebandAllResponse> {
    let mut demuxed = Vec::new();
    let mut progress = Vec::new();
    let mut in_packfile = false;
    for frame in frames {
        match frame {
            PktLineFrame::Data(payload) if in_packfile => {
                demuxed.push(PktLineFrame::Data(payload.clone()));
            }
            PktLineFrame::Data(payload) => {
                let packet = parse_sideband_packet(payload)?;
                match packet.channel {
                    SideBandChannel::Data => {
                        if trim_trailing_lf(&packet.data) == b"packfile" {
                            in_packfile = true;
                        }
                        demuxed.push(PktLineFrame::Data(packet.data));
                    }
                    SideBandChannel::Progress => progress.push(packet.data),
                    SideBandChannel::Fatal => {
                        let message = String::from_utf8_lossy(&packet.data).into_owned();
                        return Err(GitError::InvalidFormat(format!(
                            "sideband fatal: {message}"
                        )));
                    }
                }
            }
            PktLineFrame::Delimiter => {
                in_packfile = false;
                demuxed.push(PktLineFrame::Delimiter);
            }
            PktLineFrame::Flush => {
                in_packfile = false;
                demuxed.push(PktLineFrame::Flush);
            }
            PktLineFrame::ResponseEnd => {
                in_packfile = false;
                demuxed.push(PktLineFrame::ResponseEnd);
            }
        }
    }
    Ok(ProtocolV2FetchSidebandAllResponse {
        sections: parse_protocol_v2_fetch_response(format, &demuxed)?,
        progress,
    })
}

pub fn encode_protocol_v2_fetch_sideband_all_response(
    sections: &[ProtocolV2FetchResponseSection],
) -> Result<Vec<PktLineFrame>> {
    let frames = encode_protocol_v2_fetch_response(sections)?;
    let mut encoded = Vec::new();
    let mut in_packfile = false;
    for frame in frames {
        match frame {
            PktLineFrame::Data(payload) if in_packfile => {
                encoded.push(PktLineFrame::Data(payload));
            }
            PktLineFrame::Data(payload) => {
                if trim_trailing_lf(&payload) == b"packfile" {
                    in_packfile = true;
                }
                encoded.push(PktLineFrame::data(encode_sideband_packet(
                    &SideBandPacket {
                        channel: SideBandChannel::Data,
                        data: payload,
                    },
                )?)?);
            }
            PktLineFrame::Delimiter => {
                in_packfile = false;
                encoded.push(PktLineFrame::Delimiter);
            }
            PktLineFrame::Flush => {
                in_packfile = false;
                encoded.push(PktLineFrame::Flush);
            }
            PktLineFrame::ResponseEnd => {
                in_packfile = false;
                encoded.push(PktLineFrame::ResponseEnd);
            }
        }
    }
    Ok(encoded)
}

pub fn read_protocol_v2_fetch_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<ProtocolV2FetchResponseSection>> {
    let frames = read_protocol_v2_stateless_rpc_payload_frames_with_limits(
        reader,
        PktLineReadLimits::PACK_STREAM,
    )?;
    parse_protocol_v2_fetch_response(format, &frames)
}

pub fn read_protocol_v2_fetch_response_header(
    format: ObjectFormat,
    reader: &mut impl Read,
    sideband_all: bool,
) -> Result<ProtocolV2FetchResponseHeader> {
    let mut pending = skip_leading_protocol_v2_advertisement_if_present(reader, sideband_all)?;
    let mut sections = Vec::new();
    let mut current: Option<(String, Vec<Vec<u8>>)> = None;
    loop {
        let frame = if let Some(frame) = pending.take() {
            frame
        } else {
            read_protocol_v2_fetch_metadata_frame(reader, sideband_all)?
        };
        match frame {
            PktLineFrame::Data(payload) => {
                if let Some((_name, lines)) = &mut current {
                    lines.push(payload);
                } else {
                    let name = parse_fetch_section_header(&payload)?;
                    if name == "packfile" {
                        return Ok(ProtocolV2FetchResponseHeader {
                            sections,
                            has_packfile: true,
                        });
                    }
                    current = Some((name, Vec::new()));
                }
            }
            PktLineFrame::Delimiter => {
                let Some((name, lines)) = current.take() else {
                    return Err(GitError::InvalidFormat(
                        "fetch response has delimiter before section".into(),
                    ));
                };
                sections.push(parse_fetch_section(format, name, lines)?);
            }
            PktLineFrame::Flush => {
                if let Some((name, lines)) = current.take() {
                    sections.push(parse_fetch_section(format, name, lines)?);
                }
                return Ok(ProtocolV2FetchResponseHeader {
                    sections,
                    has_packfile: false,
                });
            }
            PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "fetch response contains response-end".into(),
                ));
            }
        }
    }
}

/// Read and validate the acknowledgment-only prefix of a multi-round fetch.
///
/// A server that says `ready` must delimit the acknowledgment section before
/// sending the pack, except when the client requested `wait-for-done`. A server
/// that has not said `ready` must flush the response so the client can continue
/// negotiation. Keeping this request-dependent validation here preserves the
/// generic response parser's support for valid `ready` + flush intermediate
/// responses.
pub fn read_protocol_v2_fetch_negotiation_response(
    format: ObjectFormat,
    reader: &mut impl Read,
    sideband_all: bool,
    wait_for_done: bool,
) -> Result<ProtocolV2FetchNegotiationResponse> {
    let mut pending = skip_leading_protocol_v2_advertisement_if_present(reader, sideband_all)?;
    let first = if let Some(frame) = pending.take() {
        frame
    } else {
        read_protocol_v2_fetch_metadata_frame(reader, sideband_all)?
    };
    let PktLineFrame::Data(header) = first else {
        return Err(GitError::InvalidFormat(
            "fetch negotiation response is missing acknowledgments".into(),
        ));
    };
    if parse_fetch_section_header(&header)? != "acknowledgments" {
        return Err(GitError::InvalidFormat(
            "fetch negotiation response must begin with acknowledgments".into(),
        ));
    }

    let mut acknowledgments = Vec::new();
    loop {
        match read_protocol_v2_fetch_metadata_frame(reader, sideband_all)? {
            PktLineFrame::Data(line) => {
                acknowledgments.push(parse_fetch_acknowledgment(format, &line)?);
            }
            PktLineFrame::Delimiter => {
                let ready = acknowledgments
                    .iter()
                    .any(|ack| matches!(ack, ProtocolV2FetchAcknowledgment::Ready));
                if !ready {
                    return Err(GitError::InvalidFormat(
                        "expected no other sections to be sent after no 'ready'".into(),
                    ));
                }
                return Ok(ProtocolV2FetchNegotiationResponse {
                    acknowledgments,
                    has_following_sections: true,
                });
            }
            PktLineFrame::Flush => {
                let ready = acknowledgments
                    .iter()
                    .any(|ack| matches!(ack, ProtocolV2FetchAcknowledgment::Ready));
                if ready && !wait_for_done {
                    return Err(GitError::InvalidFormat(
                        "expected packfile to be sent after 'ready'".into(),
                    ));
                }
                return Ok(ProtocolV2FetchNegotiationResponse {
                    acknowledgments,
                    has_following_sections: false,
                });
            }
            PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "fetch negotiation response contains response-end".into(),
                ));
            }
        }
    }
}

fn read_protocol_v2_fetch_metadata_frame(
    reader: &mut impl Read,
    sideband_all: bool,
) -> Result<PktLineFrame> {
    loop {
        let frame = read_pkt_line_frame(reader)?
            .ok_or_else(|| GitError::InvalidFormat("fetch response ended before flush".into()))?;
        if sideband_all && let PktLineFrame::Data(payload) = frame {
            let packet = parse_sideband_packet(&payload)?;
            match packet.channel {
                SideBandChannel::Data => return Ok(PktLineFrame::Data(packet.data)),
                SideBandChannel::Progress => continue,
                SideBandChannel::Fatal => {
                    let message = String::from_utf8_lossy(&packet.data).into_owned();
                    return Err(GitError::InvalidFormat(format!(
                        "sideband fatal: {message}"
                    )));
                }
            }
        }
        return Ok(frame);
    }
}

pub fn write_protocol_v2_fetch_response(
    writer: &mut impl Write,
    sections: &[ProtocolV2FetchResponseSection],
) -> Result<()> {
    write_protocol_v2_fetch_response_inner(writer, sections, false, false)
}

pub fn read_protocol_v2_fetch_sideband_all_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ProtocolV2FetchSidebandAllResponse> {
    let frames = read_protocol_v2_stateless_rpc_payload_frames_with_limits(
        reader,
        PktLineReadLimits::PACK_STREAM,
    )?;
    parse_protocol_v2_fetch_sideband_all_response(format, &frames)
}

pub fn write_protocol_v2_fetch_sideband_all_response(
    writer: &mut impl Write,
    sections: &[ProtocolV2FetchResponseSection],
) -> Result<()> {
    write_protocol_v2_fetch_response_inner(writer, sections, true, false)
}

pub fn read_protocol_v2_fetch_response_until_response_end(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<ProtocolV2FetchResponseSection>> {
    let frames = read_pkt_line_frames_until_response_end_with_limits(
        reader,
        PktLineReadLimits::PACK_STREAM,
    )?;
    parse_protocol_v2_fetch_response(format, &frames)
}

pub fn write_protocol_v2_fetch_response_with_response_end(
    writer: &mut impl Write,
    sections: &[ProtocolV2FetchResponseSection],
) -> Result<()> {
    write_protocol_v2_fetch_response_inner(writer, sections, false, true)
}

pub fn read_protocol_v2_fetch_sideband_all_response_until_response_end(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ProtocolV2FetchSidebandAllResponse> {
    let frames = read_pkt_line_frames_until_response_end_with_limits(
        reader,
        PktLineReadLimits::PACK_STREAM,
    )?;
    parse_protocol_v2_fetch_sideband_all_response(format, &frames)
}

pub fn write_protocol_v2_fetch_sideband_all_response_with_response_end(
    writer: &mut impl Write,
    sections: &[ProtocolV2FetchResponseSection],
) -> Result<()> {
    write_protocol_v2_fetch_response_inner(writer, sections, true, true)
}

fn write_protocol_v2_fetch_response_inner(
    writer: &mut impl Write,
    sections: &[ProtocolV2FetchResponseSection],
    sideband_all: bool,
    response_end: bool,
) -> Result<()> {
    let mut in_packfile = false;
    for (idx, section) in sections.iter().enumerate() {
        if idx != 0 {
            in_packfile = false;
            write_pkt_line_frame(writer, &PktLineFrame::Delimiter)?;
        }
        write_protocol_v2_fetch_payload(
            writer,
            &line_from_str(protocol_v2_fetch_section_name(section)),
            sideband_all,
            &mut in_packfile,
        )?;
        for payload in format_protocol_v2_fetch_section_lines(section)? {
            write_protocol_v2_fetch_payload(writer, &payload, sideband_all, &mut in_packfile)?;
        }
    }
    writer.write_all(b"0000")?;
    if response_end {
        writer.write_all(b"0002")?;
    }
    Ok(())
}

fn write_protocol_v2_fetch_payload(
    writer: &mut impl Write,
    payload: &[u8],
    sideband_all: bool,
    in_packfile: &mut bool,
) -> Result<()> {
    if sideband_all && !*in_packfile {
        if trim_trailing_lf(payload) == b"packfile" {
            *in_packfile = true;
        }
        write_sideband_payload(writer, SideBandChannel::Data, payload)
    } else {
        write_pkt_line_payload(writer, payload)
    }
}

pub fn exchange_protocol_v2_fetch(
    format: ObjectFormat,
    reader: &mut impl Read,
    writer: &mut impl Write,
    request: &ProtocolV2FetchRequest,
) -> Result<Vec<ProtocolV2FetchResponseSection>> {
    write_protocol_v2_fetch_request(writer, request)?;
    writer.flush()?;
    read_protocol_v2_fetch_response(format, reader)
}

pub fn parse_protocol_v2_object_info_response(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<ProtocolV2ObjectInfoResponse> {
    let Some((first, rest)) = frames.split_first() else {
        return Err(GitError::InvalidFormat(
            "object-info response is empty".into(),
        ));
    };
    let PktLineFrame::Data(attrs) = first else {
        return Err(GitError::InvalidFormat(
            "object-info response must start with attributes".into(),
        ));
    };
    let attrs = parse_protocol_v2_line_text("object-info response attributes", attrs)?;
    let mut response = ProtocolV2ObjectInfoResponse::default();
    for attr in attrs.split(' ') {
        validate_protocol_v2_token("object-info response attribute", attr)?;
        match attr {
            "size" => {
                if response.size {
                    return Err(GitError::InvalidFormat(
                        "object-info response has duplicate size attribute".into(),
                    ));
                }
                response.size = true;
            }
            other => {
                return Err(GitError::InvalidFormat(format!(
                    "unsupported object-info response attribute {other}"
                )));
            }
        }
    }
    if !response.size {
        return Err(GitError::InvalidFormat(
            "object-info response is missing size attribute".into(),
        ));
    }

    let mut saw_flush = false;
    for (idx, frame) in rest.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                response
                    .records
                    .push(parse_protocol_v2_object_info_record(format, payload)?);
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "object-info response has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != rest.len() {
                    return Err(GitError::InvalidFormat(
                        "object-info response has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "object-info response contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "object-info response missing flush".into(),
        ));
    }
    Ok(response)
}

pub fn encode_protocol_v2_object_info_response(
    response: &ProtocolV2ObjectInfoResponse,
) -> Result<Vec<PktLineFrame>> {
    if !response.size {
        return Err(GitError::InvalidFormat(
            "object-info response is missing size attribute".into(),
        ));
    }
    let mut frames = Vec::new();
    frames.push(PktLineFrame::data(line_from_str("size"))?);
    for record in &response.records {
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "{} {}",
            record.oid, record.size
        )))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_protocol_v2_object_info_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ProtocolV2ObjectInfoResponse> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_protocol_v2_object_info_response(format, &frames)
}

pub fn write_protocol_v2_object_info_response(
    writer: &mut impl Write,
    response: &ProtocolV2ObjectInfoResponse,
) -> Result<()> {
    if !response.size {
        return Err(GitError::InvalidFormat(
            "object-info response is missing size attribute".into(),
        ));
    }
    write_pkt_line_payload(writer, b"size\n")?;
    for record in &response.records {
        write_pkt_line_payload(
            writer,
            &line_from_str(&format!("{} {}", record.oid, record.size)),
        )?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn exchange_protocol_v2_object_info(
    format: ObjectFormat,
    reader: &mut impl Read,
    writer: &mut impl Write,
    request: &ProtocolV2ObjectInfoRequest,
) -> Result<ProtocolV2ObjectInfoResponse> {
    write_protocol_v2_object_info_request(writer, request)?;
    writer.flush()?;
    read_protocol_v2_object_info_response(format, reader)
}

pub fn demux_protocol_v2_fetch_packfile(
    sections: &[ProtocolV2FetchResponseSection],
) -> Result<Option<SideBandDemux>> {
    let mut packfile = None;
    for section in sections {
        if let ProtocolV2FetchResponseSection::Packfile(lines) = section {
            if packfile.is_some() {
                return Err(GitError::InvalidFormat(
                    "fetch response has duplicate packfile sections".into(),
                ));
            }
            packfile = Some(parse_and_demux_sideband_packets(lines)?);
        }
    }
    Ok(packfile)
}

pub fn protocol_v2_object_format(capabilities: &[Capability]) -> Result<ObjectFormat> {
    let mut format = None;
    for capability in capabilities {
        if capability.name != "object-format" {
            continue;
        }
        if format.is_some() {
            return Err(GitError::InvalidFormat(
                "protocol v2 has duplicate object-format capabilities".into(),
            ));
        }
        let Some(value) = &capability.value else {
            return Err(GitError::InvalidFormat(
                "protocol v2 object-format capability is missing a value".into(),
            ));
        };
        format = Some(value.parse::<ObjectFormat>()?);
    }
    Ok(format.unwrap_or(ObjectFormat::Sha1))
}

pub fn validate_protocol_v2_command_request_capabilities(
    handshake: &TransportHandshake,
    request: &ProtocolV2CommandRequest,
) -> Result<()> {
    if handshake.protocol != ProtocolVersion::V2 {
        return Err(GitError::InvalidFormat(
            "protocol v2 command validation requires a v2 handshake".into(),
        ));
    }
    let advertised =
        protocol_v2_capability(&handshake.capabilities, &request.command).ok_or_else(|| {
            GitError::InvalidFormat(format!("unadvertised command {}", request.command))
        })?;
    if advertised.name.is_empty() {
        return Err(GitError::InvalidFormat(
            "advertised command capability is empty".into(),
        ));
    }
    parse_protocol_v2_command_options(&request.capabilities)?;

    for capability in &request.capabilities {
        let advertised = protocol_v2_capability(&handshake.capabilities, &capability.name)
            .ok_or_else(|| {
                GitError::InvalidFormat(format!(
                    "unadvertised protocol v2 capability {}",
                    capability.name
                ))
            })?;
        if capability.name == "object-format" {
            validate_protocol_v2_object_format_request(advertised, capability)?;
        }
    }
    Ok(())
}

pub fn parse_protocol_v2_command_options(
    capabilities: &[Capability],
) -> Result<ProtocolV2CommandOptions> {
    let mut out = ProtocolV2CommandOptions::default();
    for capability in capabilities {
        match capability.name.as_str() {
            "agent" => {
                if out.agent.is_some() {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command has duplicate agent capabilities".into(),
                    ));
                }
                let Some(value) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 agent capability is missing a value".into(),
                    ));
                };
                validate_protocol_v2_capability_value(value)?;
                out.agent = Some(value.clone());
            }
            "object-format" => {
                if out.object_format.is_some() {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command has duplicate object-format capabilities".into(),
                    ));
                }
                let Some(value) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 object-format capability is missing a value".into(),
                    ));
                };
                out.object_format = Some(value.parse::<ObjectFormat>()?);
            }
            "server-option" => {
                let Some(value) = &capability.value else {
                    return Err(GitError::InvalidFormat(
                        "protocol v2 server-option capability is missing a value".into(),
                    ));
                };
                validate_protocol_v2_capability_value(value)?;
                out.server_options.push(value.clone());
            }
            _ => out.extra.push(capability.clone()),
        }
    }
    Ok(out)
}

pub fn encode_protocol_v2_command_options(
    options: &ProtocolV2CommandOptions,
) -> Result<Vec<Capability>> {
    let mut capabilities = Vec::new();
    if let Some(agent) = &options.agent {
        validate_protocol_v2_capability_value(agent)?;
        capabilities.push(Capability {
            name: "agent".into(),
            value: Some(agent.clone()),
        });
    }
    if let Some(format) = options.object_format {
        capabilities.push(Capability {
            name: "object-format".into(),
            value: Some(format.name().into()),
        });
    }
    for option in &options.server_options {
        validate_protocol_v2_capability_value(option)?;
        capabilities.push(Capability {
            name: "server-option".into(),
            value: Some(option.clone()),
        });
    }
    for capability in &options.extra {
        if matches!(
            capability.name.as_str(),
            "agent" | "object-format" | "server-option"
        ) {
            return Err(GitError::InvalidFormat(format!(
                "protocol v2 extra capability duplicates known capability {}",
                capability.name
            )));
        }
        encode_protocol_v2_capability(capability)?;
        capabilities.push(capability.clone());
    }
    Ok(capabilities)
}

pub fn parse_protocol_v2_ls_refs_features(
    capabilities: &[Capability],
) -> Result<Option<ProtocolV2LsRefsFeatures>> {
    let mut ls_refs = None;
    for capability in capabilities {
        if capability.name != "ls-refs" {
            continue;
        }
        if ls_refs.is_some() {
            return Err(GitError::InvalidFormat(
                "protocol v2 has duplicate ls-refs capabilities".into(),
            ));
        }
        ls_refs = Some(parse_protocol_v2_ls_refs_feature_value(
            capability.value.as_deref(),
        )?);
    }
    Ok(ls_refs)
}

pub fn encode_protocol_v2_ls_refs_capability(
    features: &ProtocolV2LsRefsFeatures,
) -> Result<Capability> {
    let mut values = Vec::new();
    if features.unborn {
        values.push("unborn".to_string());
    }
    for feature in &features.unknown {
        validate_protocol_v2_token("ls-refs feature", feature)?;
        if feature == "unborn" {
            return Err(GitError::InvalidFormat(
                "ls-refs unknown features must not duplicate known feature unborn".into(),
            ));
        }
        values.push(feature.clone());
    }
    Ok(Capability {
        name: "ls-refs".into(),
        value: (!values.is_empty()).then(|| values.join(" ")),
    })
}

pub fn validate_protocol_v2_ls_refs_request_features(
    features: &ProtocolV2LsRefsFeatures,
    request: &ProtocolV2LsRefsRequest,
) -> Result<()> {
    if request.unborn && !features.unborn {
        return Err(GitError::InvalidFormat(
            "ls-refs request uses unborn without advertised unborn feature".into(),
        ));
    }
    Ok(())
}

pub fn validate_protocol_v2_ls_refs_command_request(
    handshake: &TransportHandshake,
    request: &ProtocolV2CommandRequest,
) -> Result<ProtocolV2LsRefsRequest> {
    validate_protocol_v2_command_request_capabilities(handshake, request)?;
    let ls_refs = ProtocolV2LsRefsRequest::from_command_request(request)?;
    let features = parse_protocol_v2_ls_refs_features(&handshake.capabilities)?
        .ok_or_else(|| GitError::InvalidFormat("ls-refs command was not advertised".into()))?;
    validate_protocol_v2_ls_refs_request_features(&features, &ls_refs)?;
    Ok(ls_refs)
}

pub fn parse_protocol_v2_fetch_features(
    capabilities: &[Capability],
) -> Result<Option<ProtocolV2FetchFeatures>> {
    let mut fetch = None;
    for capability in capabilities {
        if capability.name != "fetch" {
            continue;
        }
        if fetch.is_some() {
            return Err(GitError::InvalidFormat(
                "protocol v2 has duplicate fetch capabilities".into(),
            ));
        }
        fetch = Some(parse_protocol_v2_fetch_feature_value(
            capability.value.as_deref(),
        )?);
    }
    Ok(fetch)
}

pub fn encode_protocol_v2_fetch_capability(
    features: &ProtocolV2FetchFeatures,
) -> Result<Capability> {
    let mut values = Vec::new();
    if features.shallow {
        values.push("shallow".to_string());
    }
    if features.wait_for_done {
        values.push("wait-for-done".to_string());
    }
    if features.filter {
        values.push("filter".to_string());
    }
    if features.ref_in_want {
        values.push("ref-in-want".to_string());
    }
    if features.sideband_all {
        values.push("sideband-all".to_string());
    }
    if features.packfile_uris {
        values.push("packfile-uris".to_string());
    }
    for feature in &features.unknown {
        validate_protocol_v2_token("fetch feature", feature)?;
        if matches!(
            feature.as_str(),
            "shallow"
                | "wait-for-done"
                | "filter"
                | "ref-in-want"
                | "sideband-all"
                | "packfile-uris"
        ) {
            return Err(GitError::InvalidFormat(format!(
                "fetch unknown features must not duplicate known feature {feature}"
            )));
        }
        values.push(feature.clone());
    }
    Ok(Capability {
        name: "fetch".into(),
        value: (!values.is_empty()).then(|| values.join(" ")),
    })
}

pub fn validate_protocol_v2_fetch_request_features(
    features: &ProtocolV2FetchFeatures,
    request: &ProtocolV2FetchRequest,
) -> Result<()> {
    if !features.shallow
        && (!request.shallow.is_empty()
            || request.deepen.is_some()
            || request.deepen_since.is_some()
            || !request.deepen_not.is_empty()
            || request.deepen_relative)
    {
        return Err(GitError::InvalidFormat(
            "fetch request uses shallow/deepen arguments without advertised shallow feature".into(),
        ));
    }
    if !features.filter && request.filter.is_some() {
        return Err(GitError::InvalidFormat(
            "fetch request uses filter without advertised filter feature".into(),
        ));
    }
    if !features.ref_in_want && !request.want_refs.is_empty() {
        return Err(GitError::InvalidFormat(
            "fetch request uses want-ref without advertised ref-in-want feature".into(),
        ));
    }
    if !features.sideband_all && request.sideband_all {
        return Err(GitError::InvalidFormat(
            "fetch request uses sideband-all without advertised sideband-all feature".into(),
        ));
    }
    if !features.packfile_uris && request.packfile_uris.is_some() {
        return Err(GitError::InvalidFormat(
            "fetch request uses packfile-uris without advertised packfile-uris feature".into(),
        ));
    }
    if !features.wait_for_done && request.wait_for_done {
        return Err(GitError::InvalidFormat(
            "fetch request uses wait-for-done without advertised wait-for-done feature".into(),
        ));
    }
    Ok(())
}

pub fn validate_protocol_v2_fetch_command_request(
    handshake: &TransportHandshake,
    format: ObjectFormat,
    request: &ProtocolV2CommandRequest,
) -> Result<ProtocolV2FetchRequest> {
    validate_protocol_v2_command_request_capabilities(handshake, request)?;
    let fetch = ProtocolV2FetchRequest::from_command_request(format, request)?;
    let features = parse_protocol_v2_fetch_features(&handshake.capabilities)?
        .ok_or_else(|| GitError::InvalidFormat("fetch command was not advertised".into()))?;
    validate_protocol_v2_fetch_request_features(&features, &fetch)?;
    Ok(fetch)
}

pub fn validate_protocol_v2_object_info_command_request(
    handshake: &TransportHandshake,
    format: ObjectFormat,
    request: &ProtocolV2CommandRequest,
) -> Result<ProtocolV2ObjectInfoRequest> {
    validate_protocol_v2_command_request_capabilities(handshake, request)?;
    let object_info = ProtocolV2ObjectInfoRequest::from_command_request(format, request)?;
    protocol_v2_capability(&handshake.capabilities, "object-info")
        .ok_or_else(|| GitError::InvalidFormat("object-info command was not advertised".into()))?;
    Ok(object_info)
}

pub fn classify_protocol_v2_command_request(
    handshake: &TransportHandshake,
    format: ObjectFormat,
    request: &ProtocolV2CommandRequest,
) -> Result<ProtocolV2Command> {
    match request.command.as_str() {
        "ls-refs" => validate_protocol_v2_ls_refs_command_request(handshake, request)
            .map(ProtocolV2Command::LsRefs),
        "fetch" => validate_protocol_v2_fetch_command_request(handshake, format, request)
            .map(ProtocolV2Command::Fetch),
        "object-info" => {
            validate_protocol_v2_object_info_command_request(handshake, format, request)
                .map(ProtocolV2Command::ObjectInfo)
        }
        _ => {
            validate_protocol_v2_command_request_capabilities(handshake, request)?;
            Ok(ProtocolV2Command::Unknown(request.clone()))
        }
    }
}

pub fn classify_protocol_v2_request(
    handshake: &TransportHandshake,
    format: ObjectFormat,
    request: &ProtocolV2Request,
) -> Result<ProtocolV2SessionRequest> {
    match request {
        ProtocolV2Request::Command(command) => {
            classify_protocol_v2_command_request(handshake, format, command)
                .map(ProtocolV2SessionRequest::Command)
        }
        ProtocolV2Request::Done => Ok(ProtocolV2SessionRequest::Done),
    }
}

pub fn read_protocol_v2_session_request(
    handshake: &TransportHandshake,
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ProtocolV2SessionRequest> {
    let request = read_protocol_v2_request(reader)?;
    classify_protocol_v2_request(handshake, format, &request)
}

fn protocol_v2_capability<'a>(
    capabilities: &'a [Capability],
    name: &str,
) -> Option<&'a Capability> {
    capabilities
        .iter()
        .find(|capability| capability.name == name)
}

fn validate_protocol_v2_object_format_request(
    advertised: &Capability,
    requested: &Capability,
) -> Result<()> {
    let Some(advertised) = &advertised.value else {
        return Err(GitError::InvalidFormat(
            "advertised object-format capability is missing a value".into(),
        ));
    };
    let Some(requested) = &requested.value else {
        return Err(GitError::InvalidFormat(
            "requested object-format capability is missing a value".into(),
        ));
    };
    if advertised != requested {
        return Err(GitError::InvalidFormat(format!(
            "requested object-format {requested} does not match advertised {advertised}"
        )));
    }
    Ok(())
}

fn parse_protocol_v2_ls_refs_feature_value(
    value: Option<&str>,
) -> Result<ProtocolV2LsRefsFeatures> {
    let mut out = ProtocolV2LsRefsFeatures::default();
    let Some(value) = value else {
        return Ok(out);
    };
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "protocol v2 ls-refs capability value is empty".into(),
        ));
    }
    for feature in value.split(' ') {
        validate_protocol_v2_token("ls-refs feature", feature)?;
        match feature {
            "unborn" => out.unborn = true,
            other => out.unknown.push(other.to_string()),
        }
    }
    Ok(out)
}

fn parse_protocol_v2_fetch_feature_value(value: Option<&str>) -> Result<ProtocolV2FetchFeatures> {
    let mut out = ProtocolV2FetchFeatures::default();
    let Some(value) = value else {
        return Ok(out);
    };
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "protocol v2 fetch capability value is empty".into(),
        ));
    }
    for feature in value.split(' ') {
        validate_protocol_v2_token("fetch feature", feature)?;
        match feature {
            "shallow" => out.shallow = true,
            "wait-for-done" => out.wait_for_done = true,
            "filter" => out.filter = true,
            "ref-in-want" => out.ref_in_want = true,
            "sideband-all" => out.sideband_all = true,
            "packfile-uris" => out.packfile_uris = true,
            other => out.unknown.push(other.to_string()),
        }
    }
    Ok(out)
}
pub(crate) fn parse_protocol_v2_capability_line(payload: &[u8]) -> Result<Capability> {
    let payload = trim_trailing_lf(payload);
    if payload.is_empty() {
        return Err(GitError::InvalidFormat(
            "empty protocol v2 capability line".into(),
        ));
    }
    let text =
        std::str::from_utf8(payload).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let (name, value) = text
        .split_once('=')
        .map_or((text, None), |(name, value)| (name, Some(value)));
    validate_capability_name(name)?;
    if let Some(value) = value {
        validate_protocol_v2_capability_value(value)?;
    }
    Ok(Capability {
        name: name.to_string(),
        value: value.map(str::to_string),
    })
}

pub(crate) fn parse_protocol_v2_command_line(payload: &[u8]) -> Result<String> {
    let payload = trim_trailing_lf(payload);
    let text =
        std::str::from_utf8(payload).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let Some(command) = text.strip_prefix("command=") else {
        return Err(GitError::InvalidFormat(
            "protocol v2 command request missing command prefix".into(),
        ));
    };
    validate_capability_name(command)?;
    Ok(command.to_string())
}

fn parse_protocol_v2_ls_refs_line(
    format: ObjectFormat,
    payload: &[u8],
) -> Result<ProtocolV2LsRefsRecord> {
    let payload = trim_trailing_lf(payload);
    if payload.is_empty() {
        return Err(GitError::InvalidFormat(
            "ls-refs response line is empty".into(),
        ));
    }
    let text =
        std::str::from_utf8(payload).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut fields = text.split(' ');
    let first = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("ls-refs response line is empty".into()))?;
    if first == "unborn" {
        let name = fields
            .next()
            .ok_or_else(|| GitError::InvalidFormat("ls-refs unborn line is missing name".into()))?;
        validate_protocol_v2_token("ls-refs ref name", name)?;
        let (symref_target, attributes) = parse_protocol_v2_ls_refs_attributes(format, fields)?;
        return Ok(ProtocolV2LsRefsRecord::Unborn {
            name: name.to_string(),
            symref_target,
            attributes,
        });
    }

    let oid = ObjectId::from_hex(format, first)?;
    let name = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("ls-refs ref line is missing name".into()))?;
    validate_protocol_v2_token("ls-refs ref name", name)?;
    let (peeled, symref_target, attributes) =
        parse_protocol_v2_ls_refs_ref_attributes(format, fields)?;
    Ok(ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
        oid,
        name: name.to_string(),
        peeled,
        symref_target,
        attributes,
    }))
}

fn parse_protocol_v2_ls_refs_ref_attributes<'a>(
    format: ObjectFormat,
    fields: impl Iterator<Item = &'a str>,
) -> Result<(Option<ObjectId>, Option<String>, Vec<String>)> {
    let mut peeled = None;
    let (symref_target, attributes) =
        parse_protocol_v2_ls_refs_attributes_with(format, fields, |attr| {
            if let Some(value) = attr.strip_prefix("peeled:") {
                if peeled.is_some() {
                    return Err(GitError::InvalidFormat(
                        "ls-refs response has duplicate peeled attribute".into(),
                    ));
                }
                peeled = Some(ObjectId::from_hex(format, value)?);
                return Ok(true);
            }
            Ok(false)
        })?;
    Ok((peeled, symref_target, attributes))
}

fn parse_protocol_v2_ls_refs_attributes<'a>(
    format: ObjectFormat,
    fields: impl Iterator<Item = &'a str>,
) -> Result<(Option<String>, Vec<String>)> {
    parse_protocol_v2_ls_refs_attributes_with(format, fields, |attr| {
        if attr.starts_with("peeled:") {
            return Err(GitError::InvalidFormat(
                "ls-refs unborn line has peeled attribute".into(),
            ));
        }
        Ok(false)
    })
}

fn parse_protocol_v2_ls_refs_attributes_with<'a, F>(
    _format: ObjectFormat,
    fields: impl Iterator<Item = &'a str>,
    mut handle_known: F,
) -> Result<(Option<String>, Vec<String>)>
where
    F: FnMut(&str) -> Result<bool>,
{
    let mut symref_target = None;
    let mut attributes = Vec::new();
    for attr in fields {
        validate_protocol_v2_token("ls-refs attribute", attr)?;
        if let Some(value) = attr.strip_prefix("symref-target:") {
            if symref_target.is_some() {
                return Err(GitError::InvalidFormat(
                    "ls-refs response has duplicate symref-target attribute".into(),
                ));
            }
            validate_protocol_v2_token("ls-refs symref-target", value)?;
            symref_target = Some(value.to_string());
        } else if !handle_known(attr)? {
            attributes.push(attr.to_string());
        }
    }
    Ok((symref_target, attributes))
}

fn format_protocol_v2_ls_refs_record(record: &ProtocolV2LsRefsRecord) -> Result<String> {
    let mut out = String::new();
    match record {
        ProtocolV2LsRefsRecord::Ref(reference) => {
            validate_protocol_v2_token("ls-refs ref name", &reference.name)?;
            out.push_str(&reference.oid.to_string());
            out.push(' ');
            out.push_str(&reference.name);
            if let Some(peeled) = &reference.peeled {
                if peeled.format() != reference.oid.format() {
                    return Err(GitError::InvalidObjectId(
                        "ls-refs peeled object format does not match ref object format".into(),
                    ));
                }
                out.push(' ');
                out.push_str("peeled:");
                out.push_str(&peeled.to_string());
            }
            if let Some(target) = &reference.symref_target {
                validate_protocol_v2_token("ls-refs symref-target", target)?;
                out.push(' ');
                out.push_str("symref-target:");
                out.push_str(target);
            }
            append_protocol_v2_ls_refs_attributes(&mut out, &reference.attributes)?;
        }
        ProtocolV2LsRefsRecord::Unborn {
            name,
            symref_target,
            attributes,
        } => {
            validate_protocol_v2_token("ls-refs ref name", name)?;
            out.push_str("unborn ");
            out.push_str(name);
            if let Some(target) = symref_target {
                validate_protocol_v2_token("ls-refs symref-target", target)?;
                out.push(' ');
                out.push_str("symref-target:");
                out.push_str(target);
            }
            append_protocol_v2_ls_refs_attributes(&mut out, attributes)?;
        }
    }
    Ok(out)
}

fn append_protocol_v2_ls_refs_attributes(out: &mut String, attributes: &[String]) -> Result<()> {
    for attr in attributes {
        validate_protocol_v2_token("ls-refs attribute", attr)?;
        if attr.starts_with("peeled:") || attr.starts_with("symref-target:") {
            return Err(GitError::InvalidFormat(
                "ls-refs generic attributes must not duplicate known attributes".into(),
            ));
        }
        out.push(' ');
        out.push_str(attr);
    }
    Ok(())
}

fn parse_fetch_section_header(payload: &[u8]) -> Result<String> {
    let name = parse_protocol_v2_line_text("fetch response section", payload)?;
    validate_capability_name(name)?;
    Ok(name.to_string())
}

fn flush_terminates_protocol_v2_response(frames: &[PktLineFrame], idx: usize) -> bool {
    idx + 1 == frames.len()
        || (idx + 2 == frames.len() && matches!(frames[idx + 1], PktLineFrame::ResponseEnd))
}

fn parse_fetch_section(
    format: ObjectFormat,
    name: String,
    lines: Vec<Vec<u8>>,
) -> Result<ProtocolV2FetchResponseSection> {
    match name.as_str() {
        "acknowledgments" => lines
            .iter()
            .map(|line| parse_fetch_acknowledgment(format, line))
            .collect::<Result<Vec<_>>>()
            .map(ProtocolV2FetchResponseSection::Acknowledgments),
        "shallow-info" => lines
            .iter()
            .map(|line| parse_fetch_shallow_info(format, line))
            .collect::<Result<Vec<_>>>()
            .map(ProtocolV2FetchResponseSection::ShallowInfo),
        "wanted-refs" => lines
            .iter()
            .map(|line| parse_fetch_wanted_ref(format, line))
            .collect::<Result<Vec<_>>>()
            .map(ProtocolV2FetchResponseSection::WantedRefs),
        "packfile-uris" => lines
            .iter()
            .map(|line| parse_fetch_packfile_uri(format, line))
            .collect::<Result<Vec<_>>>()
            .map(ProtocolV2FetchResponseSection::PackfileUris),
        "packfile" => Ok(ProtocolV2FetchResponseSection::Packfile(lines)),
        _ => Ok(ProtocolV2FetchResponseSection::Unknown { name, lines }),
    }
}

fn parse_fetch_acknowledgment(
    format: ObjectFormat,
    line: &[u8],
) -> Result<ProtocolV2FetchAcknowledgment> {
    let text = parse_protocol_v2_line_text("fetch acknowledgment", line)?;
    match text {
        "NAK" => Ok(ProtocolV2FetchAcknowledgment::Nak),
        "ready" => Ok(ProtocolV2FetchAcknowledgment::Ready),
        value if value.starts_with("ACK ") => Ok(ProtocolV2FetchAcknowledgment::Ack(
            parse_oid_argument(format, "fetch ACK", value, "ACK ")?,
        )),
        other => Err(GitError::InvalidFormat(format!(
            "unsupported fetch acknowledgment {other}"
        ))),
    }
}

pub(crate) fn parse_fetch_shallow_info(
    format: ObjectFormat,
    line: &[u8],
) -> Result<ProtocolV2FetchShallowInfo> {
    let text = parse_protocol_v2_line_text("fetch shallow-info", line)?;
    if text.starts_with("shallow ") {
        return Ok(ProtocolV2FetchShallowInfo::Shallow(parse_oid_argument(
            format,
            "fetch shallow",
            text,
            "shallow ",
        )?));
    }
    if text.starts_with("unshallow ") {
        return Ok(ProtocolV2FetchShallowInfo::Unshallow(parse_oid_argument(
            format,
            "fetch unshallow",
            text,
            "unshallow ",
        )?));
    }
    Err(GitError::InvalidFormat(format!(
        "unsupported fetch shallow-info {text}"
    )))
}

fn parse_fetch_wanted_ref(format: ObjectFormat, line: &[u8]) -> Result<ProtocolV2FetchWantedRef> {
    let text = parse_protocol_v2_line_text("fetch wanted-ref", line)?;
    let (oid, name) = text
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat("fetch wanted-ref is missing name".into()))?;
    validate_protocol_v2_token("fetch wanted-ref name", name)?;
    Ok(ProtocolV2FetchWantedRef {
        oid: ObjectId::from_hex(format, oid)?,
        name: name.to_string(),
    })
}

fn parse_fetch_packfile_uri(
    format: ObjectFormat,
    line: &[u8],
) -> Result<ProtocolV2FetchPackfileUri> {
    let text = parse_protocol_v2_line_text("fetch packfile-uri", line)?;
    let (pack_hash, uri) = text
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat("fetch packfile-uri is missing uri".into()))?;
    validate_protocol_v2_token("fetch packfile-uri hash", pack_hash)?;
    validate_protocol_v2_token("fetch packfile-uri", uri)?;
    Ok(ProtocolV2FetchPackfileUri {
        pack_hash: ObjectId::from_hex(format, pack_hash)?,
        uri: uri.to_string(),
    })
}

fn protocol_v2_fetch_section_name(section: &ProtocolV2FetchResponseSection) -> &str {
    match section {
        ProtocolV2FetchResponseSection::Acknowledgments(_) => "acknowledgments",
        ProtocolV2FetchResponseSection::ShallowInfo(_) => "shallow-info",
        ProtocolV2FetchResponseSection::WantedRefs(_) => "wanted-refs",
        ProtocolV2FetchResponseSection::PackfileUris(_) => "packfile-uris",
        ProtocolV2FetchResponseSection::Packfile(_) => "packfile",
        ProtocolV2FetchResponseSection::Unknown { name, .. } => name,
    }
}

fn format_protocol_v2_fetch_section_lines(
    section: &ProtocolV2FetchResponseSection,
) -> Result<Vec<Vec<u8>>> {
    match section {
        ProtocolV2FetchResponseSection::Acknowledgments(acks) => acks
            .iter()
            .map(|ack| match ack {
                ProtocolV2FetchAcknowledgment::Nak => Ok(line_from_str("NAK")),
                ProtocolV2FetchAcknowledgment::Ack(oid) => Ok(line_from_str(&format!("ACK {oid}"))),
                ProtocolV2FetchAcknowledgment::Ready => Ok(line_from_str("ready")),
            })
            .collect(),
        ProtocolV2FetchResponseSection::ShallowInfo(entries) => entries
            .iter()
            .map(|entry| match entry {
                // Unlike most protocol-v2 text records, upload-pack writes
                // shallow-info entries without a trailing LF. The pkt-line
                // length is therefore 0034/0036 for SHA-1 and 004c/004e for
                // SHA-256, matching Git's byte-level wire contract.
                ProtocolV2FetchShallowInfo::Shallow(oid) => {
                    Ok(format!("shallow {oid}").into_bytes())
                }
                ProtocolV2FetchShallowInfo::Unshallow(oid) => {
                    Ok(format!("unshallow {oid}").into_bytes())
                }
            })
            .collect(),
        ProtocolV2FetchResponseSection::WantedRefs(refs) => refs
            .iter()
            .map(|wanted| {
                validate_protocol_v2_token("fetch wanted-ref name", &wanted.name)?;
                Ok(line_from_str(&format!("{} {}", wanted.oid, wanted.name)))
            })
            .collect(),
        ProtocolV2FetchResponseSection::PackfileUris(uris) => uris
            .iter()
            .map(|packfile_uri| {
                validate_protocol_v2_token("fetch packfile-uri", &packfile_uri.uri)?;
                Ok(line_from_str(&format!(
                    "{} {}",
                    packfile_uri.pack_hash, packfile_uri.uri
                )))
            })
            .collect(),
        ProtocolV2FetchResponseSection::Packfile(lines) => Ok(lines.clone()),
        ProtocolV2FetchResponseSection::Unknown { name, lines } => {
            validate_capability_name(name)?;
            for line in lines {
                validate_protocol_v2_line("fetch unknown section line", line)?;
            }
            Ok(lines.clone())
        }
    }
}

fn parse_protocol_v2_object_info_record(
    format: ObjectFormat,
    line: &[u8],
) -> Result<ProtocolV2ObjectInfoRecord> {
    let text = parse_protocol_v2_line_text("object-info record", line)?;
    let mut fields = text.split(' ');
    let oid = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("object-info record is missing oid".into()))?;
    let size = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("object-info record is missing size".into()))?;
    if fields.next().is_some() {
        return Err(GitError::InvalidFormat(
            "object-info record has too many fields".into(),
        ));
    }
    validate_protocol_v2_token("object-info oid", oid)?;
    validate_protocol_v2_token("object-info size", size)?;
    let size = size
        .parse::<u64>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    Ok(ProtocolV2ObjectInfoRecord {
        oid: ObjectId::from_hex(format, oid)?,
        size,
    })
}

pub(crate) fn encode_protocol_v2_capability(capability: &Capability) -> Result<Vec<u8>> {
    validate_capability_name(&capability.name)?;
    let mut out = capability.name.as_bytes().to_vec();
    if let Some(value) = &capability.value {
        validate_protocol_v2_capability_value(value)?;
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
    }
    Ok(out)
}

pub(crate) fn validate_protocol_v2_capability_value(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "protocol v2 capability value is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "protocol v2 capability value contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_protocol_v2_argument(value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "protocol v2 command argument is empty".into(),
        ));
    }
    if value.iter().any(|byte| matches!(*byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "protocol v2 command argument contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

pub(crate) fn parse_u32_argument(label: &str, value: &str, prefix: &str) -> Result<u32> {
    let number = value
        .strip_prefix(prefix)
        .ok_or_else(|| GitError::InvalidFormat(format!("invalid {label}")))?;
    validate_protocol_v2_token(label, number)?;
    let parsed = number
        .parse::<u32>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    if parsed == 0 {
        return Err(GitError::InvalidFormat(format!("{label} must be positive")));
    }
    Ok(parsed)
}

pub(crate) fn parse_u64_argument(label: &str, value: &str, prefix: &str) -> Result<u64> {
    let number = value
        .strip_prefix(prefix)
        .ok_or_else(|| GitError::InvalidFormat(format!("invalid {label}")))?;
    validate_protocol_v2_token(label, number)?;
    number
        .parse::<u64>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))
}
