use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use std::io::{ErrorKind, Read, Write};

pub const PKT_LINE_MAX_LEN: usize = 65_520;

pub const PKT_LINE_MAX_PAYLOAD_LEN: usize = PKT_LINE_MAX_LEN - 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersion {
    V0,
    V1,
    V2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PktLine(pub Vec<u8>);

impl PktLine {
    pub fn encode(&self) -> Vec<u8> {
        encode_pkt_line_payload(&self.0)
    }

    pub fn try_encode(&self) -> Result<Vec<u8>> {
        validate_pkt_line_payload(&self.0)?;
        Ok(self.encode())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PktLineFrame {
    Data(Vec<u8>),
    Flush,
    Delimiter,
    ResponseEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolErrorLine {
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitService {
    UploadPack,
    ReceivePack,
    UploadArchive,
}

impl GitService {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
            Self::UploadArchive => "git-upload-archive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSpec {
    pub force: bool,
    pub negative: bool,
    pub src: Option<String>,
    pub dst: Option<String>,
    pub pattern: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchHeadRecord {
    pub oid: ObjectId,
    pub not_for_merge: bool,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRefUpdate {
    pub src: String,
    pub dst: Option<String>,
    pub oid: ObjectId,
    pub not_for_merge: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSourceRef {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideBandChannel {
    Data,
    Progress,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideBandPacket {
    pub channel: SideBandChannel,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SideBandDemux {
    pub data: Vec<u8>,
    pub progress: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadArchiveRequest {
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadArchiveResponse {
    Ack { sideband: Vec<SideBandPacket> },
    Nack { message: String },
}

impl PktLineFrame {
    pub fn data(payload: impl Into<Vec<u8>>) -> Result<Self> {
        let payload = payload.into();
        validate_pkt_line_payload(&payload)?;
        Ok(Self::Data(payload))
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Data(payload) => encode_pkt_line_payload(payload),
            Self::Flush => b"0000".to_vec(),
            Self::Delimiter => b"0001".to_vec(),
            Self::ResponseEnd => b"0002".to_vec(),
        }
    }

    pub fn try_encode(&self) -> Result<Vec<u8>> {
        match self {
            Self::Data(payload) => try_encode_pkt_line_payload(payload),
            Self::Flush | Self::Delimiter | Self::ResponseEnd => Ok(self.encode()),
        }
    }

    pub fn parse(input: &[u8]) -> Result<(Self, usize)> {
        if input.len() < 4 {
            return Err(GitError::InvalidFormat("truncated pkt-line length".into()));
        }
        let len = parse_pkt_len(&input[..4])?;
        match len {
            0 => Ok((Self::Flush, 4)),
            1 => Ok((Self::Delimiter, 4)),
            2 => Ok((Self::ResponseEnd, 4)),
            3 => Err(GitError::InvalidFormat(
                "reserved pkt-line length 0003".into(),
            )),
            4..=PKT_LINE_MAX_LEN => {
                if input.len() < len {
                    return Err(GitError::InvalidFormat(format!(
                        "truncated pkt-line payload: expected {} bytes, got {}",
                        len - 4,
                        input.len().saturating_sub(4)
                    )));
                }
                Ok((Self::Data(input[4..len].to_vec()), len))
            }
            _ => Err(GitError::InvalidFormat(format!(
                "pkt-line length exceeds {PKT_LINE_MAX_LEN}: {len}"
            ))),
        }
    }
}

fn validate_pkt_line_payload(payload: &[u8]) -> Result<()> {
    if payload.len() > PKT_LINE_MAX_PAYLOAD_LEN {
        return Err(GitError::InvalidFormat(format!(
            "pkt-line payload exceeds {PKT_LINE_MAX_PAYLOAD_LEN} bytes"
        )));
    }
    Ok(())
}

fn pkt_line_header(len: usize) -> [u8; 4] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        HEX[(len >> 12) & 0xf],
        HEX[(len >> 8) & 0xf],
        HEX[(len >> 4) & 0xf],
        HEX[len & 0xf],
    ]
}

fn encode_pkt_line_payload(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 4;
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&pkt_line_header(len));
    out.extend_from_slice(payload);
    out
}

fn try_encode_pkt_line_payload(payload: &[u8]) -> Result<Vec<u8>> {
    validate_pkt_line_payload(payload)?;
    Ok(encode_pkt_line_payload(payload))
}

pub fn parse_pkt_line_stream(mut input: &[u8]) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    while !input.is_empty() {
        let (frame, consumed) = PktLineFrame::parse(input)?;
        frames.push(frame);
        input = &input[consumed..];
    }
    Ok(frames)
}

fn parse_pkt_line_frames_until_flush_from(mut input: &[u8]) -> Result<(Vec<PktLineFrame>, usize)> {
    let mut frames = Vec::new();
    let mut total = 0usize;
    loop {
        if input.is_empty() {
            return Err(GitError::InvalidFormat(
                "pkt-line stream ended before flush".into(),
            ));
        }
        let (frame, consumed) = PktLineFrame::parse(input)?;
        total += consumed;
        let done = matches!(frame, PktLineFrame::Flush);
        frames.push(frame);
        input = &input[consumed..];
        if done {
            return Ok((frames, total));
        }
    }
}

pub fn read_pkt_line_frame(reader: &mut impl Read) -> Result<Option<PktLineFrame>> {
    let mut header = [0u8; 4];
    let mut read = 0usize;
    while read < header.len() {
        match reader.read(&mut header[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                return Err(GitError::InvalidFormat("truncated pkt-line length".into()));
            }
            Ok(n) => read += n,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }

    let len = parse_pkt_len(&header)?;
    match len {
        0 => Ok(Some(PktLineFrame::Flush)),
        1 => Ok(Some(PktLineFrame::Delimiter)),
        2 => Ok(Some(PktLineFrame::ResponseEnd)),
        3 => Err(GitError::InvalidFormat(
            "reserved pkt-line length 0003".into(),
        )),
        4..=PKT_LINE_MAX_LEN => {
            let mut payload = vec![0; len - 4];
            reader.read_exact(&mut payload)?;
            Ok(Some(PktLineFrame::Data(payload)))
        }
        _ => Err(GitError::InvalidFormat(format!(
            "pkt-line length exceeds {PKT_LINE_MAX_LEN}: {len}"
        ))),
    }
}

pub fn read_pkt_line_frames(reader: &mut impl Read) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    while let Some(frame) = read_pkt_line_frame(reader)? {
        frames.push(frame);
    }
    Ok(frames)
}

pub fn read_pkt_line_frames_until_flush(reader: &mut impl Read) -> Result<Vec<PktLineFrame>> {
    read_pkt_line_frames_until_control(reader, |frame| matches!(frame, PktLineFrame::Flush))
}

pub fn read_pkt_line_frames_until_response_end(
    reader: &mut impl Read,
) -> Result<Vec<PktLineFrame>> {
    read_pkt_line_frames_until_control(reader, |frame| matches!(frame, PktLineFrame::ResponseEnd))
}

fn read_pkt_line_frames_until_control(
    reader: &mut impl Read,
    stop: impl Fn(&PktLineFrame) -> bool,
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    loop {
        let Some(frame) = read_pkt_line_frame(reader)? else {
            return Err(GitError::InvalidFormat(
                "pkt-line stream ended before control packet".into(),
            ));
        };
        let done = stop(&frame);
        frames.push(frame);
        if done {
            return Ok(frames);
        }
    }
}

pub fn write_pkt_line_frame(writer: &mut impl Write, frame: &PktLineFrame) -> Result<()> {
    match frame {
        PktLineFrame::Data(payload) => write_pkt_line_payload(writer, payload)?,
        PktLineFrame::Flush => writer.write_all(b"0000")?,
        PktLineFrame::Delimiter => writer.write_all(b"0001")?,
        PktLineFrame::ResponseEnd => writer.write_all(b"0002")?,
    }
    Ok(())
}

pub fn write_pkt_line_payload(writer: &mut impl Write, payload: &[u8]) -> Result<()> {
    validate_pkt_line_payload(payload)?;
    let len = payload.len() + 4;
    writer.write_all(&pkt_line_header(len))?;
    writer.write_all(payload)?;
    Ok(())
}

pub fn write_pkt_line_frames(writer: &mut impl Write, frames: &[PktLineFrame]) -> Result<()> {
    for frame in frames {
        write_pkt_line_frame(writer, frame)?;
    }
    Ok(())
}

pub fn parse_error_line(payload: &[u8]) -> Result<ProtocolErrorLine> {
    let text = parse_protocol_v2_line_text("protocol error line", payload)?;
    let Some(message) = text.strip_prefix("ERR ") else {
        return Err(GitError::InvalidFormat(
            "protocol error line must start with ERR".into(),
        ));
    };
    validate_protocol_error_message(message)?;
    Ok(ProtocolErrorLine {
        message: message.to_string(),
    })
}

pub fn encode_error_line(error: &ProtocolErrorLine) -> Result<Vec<u8>> {
    validate_protocol_error_message(&error.message)?;
    Ok(line_from_str(&format!("ERR {}", error.message)))
}

pub fn parse_error_frame(frame: &PktLineFrame) -> Result<Option<ProtocolErrorLine>> {
    match frame {
        PktLineFrame::Data(payload) if trim_trailing_lf(payload).starts_with(b"ERR ") => {
            parse_error_line(payload).map(Some)
        }
        PktLineFrame::Data(_)
        | PktLineFrame::Flush
        | PktLineFrame::Delimiter
        | PktLineFrame::ResponseEnd => Ok(None),
    }
}

pub fn read_error_line(reader: &mut impl Read) -> Result<ProtocolErrorLine> {
    let Some(frame) = read_pkt_line_frame(reader)? else {
        return Err(GitError::InvalidFormat(
            "pkt-line stream ended before protocol error line".into(),
        ));
    };
    match frame {
        PktLineFrame::Data(payload) => parse_error_line(&payload),
        _ => Err(GitError::InvalidFormat(
            "protocol error line must be a data packet".into(),
        )),
    }
}

pub fn write_error_line(writer: &mut impl Write, error: &ProtocolErrorLine) -> Result<()> {
    write_pkt_line_frame(writer, &PktLineFrame::data(encode_error_line(error)?)?)
}

pub fn parse_git_service(value: &str) -> Result<GitService> {
    match value {
        "git-upload-pack" => Ok(GitService::UploadPack),
        "git-receive-pack" => Ok(GitService::ReceivePack),
        "git-upload-archive" => Ok(GitService::UploadArchive),
        other => Err(GitError::InvalidFormat(format!(
            "unsupported git service {other}"
        ))),
    }
}

pub fn parse_refspec(value: &str) -> Result<RefSpec> {
    validate_refspec_value(value)?;
    let (force, value) = value
        .strip_prefix('+')
        .map_or((false, value), |value| (true, value));
    let (negative, value) = value
        .strip_prefix('^')
        .map_or((false, value), |value| (true, value));
    if force && negative {
        return Err(GitError::InvalidFormat(
            "negative refspec must not be forced".into(),
        ));
    }
    let (src, dst) = if negative {
        if value.contains(':') {
            return Err(GitError::InvalidFormat(
                "negative refspec must not have a destination".into(),
            ));
        }
        (Some(value), None)
    } else if let Some((src, dst)) = value.split_once(':') {
        (non_empty(src), non_empty(dst))
    } else {
        (Some(value), None)
    };
    if src.is_none() && dst.is_none() && value != ":" {
        return Err(GitError::InvalidFormat(
            "refspec must include a source or destination".into(),
        ));
    }
    if negative && src.is_none() {
        return Err(GitError::InvalidFormat(
            "negative refspec is missing a source".into(),
        ));
    }
    if let Some(src) = src {
        validate_refspec_endpoint("refspec source", src)?;
    }
    if let Some(dst) = dst {
        validate_refspec_endpoint("refspec destination", dst)?;
    }
    let src_pattern_count = src.map(count_refspec_wildcards).unwrap_or(0);
    let dst_pattern_count = dst.map(count_refspec_wildcards).unwrap_or(0);
    if src_pattern_count > 1 || dst_pattern_count > 1 {
        return Err(GitError::InvalidFormat(
            "refspec endpoint has too many wildcards".into(),
        ));
    }
    if dst.is_some() && (src_pattern_count == 1) != (dst_pattern_count == 1) {
        return Err(GitError::InvalidFormat(
            "refspec wildcard must appear in both source and destination".into(),
        ));
    }
    Ok(RefSpec {
        force,
        negative,
        src: src.map(str::to_string),
        dst: dst.map(str::to_string),
        pattern: src_pattern_count == 1 || dst_pattern_count == 1,
    })
}

pub fn encode_refspec(refspec: &RefSpec) -> Result<String> {
    validate_refspec_shape(refspec)?;
    let mut out = String::new();
    if refspec.force {
        out.push('+');
    }
    if refspec.negative {
        out.push('^');
    }
    if let Some(src) = &refspec.src {
        out.push_str(src);
    }
    if !refspec.negative && refspec.src.is_none() && refspec.dst.is_none() {
        out.push(':');
    } else if !refspec.negative && refspec.dst.is_some() {
        out.push(':');
        if let Some(dst) = &refspec.dst {
            out.push_str(dst);
        }
    }
    Ok(out)
}

pub fn refspec_matches_source(refspec: &RefSpec, source: &str) -> Result<bool> {
    Ok(refspec_map_source(refspec, source)?.is_some())
}

pub fn refspec_map_source(refspec: &RefSpec, source: &str) -> Result<Option<String>> {
    validate_refspec_shape(refspec)?;
    validate_refspec_endpoint("refspec match source", source)?;
    let Some(src) = refspec.src.as_deref() else {
        return Ok(None);
    };
    if refspec.pattern {
        let Some((src_prefix, src_suffix)) = src.split_once('*') else {
            return Ok(None);
        };
        let Some(middle) = source
            .strip_prefix(src_prefix)
            .and_then(|value| value.strip_suffix(src_suffix))
        else {
            return Ok(None);
        };
        if let Some(dst) = refspec.dst.as_deref() {
            let (dst_prefix, dst_suffix) = dst.split_once('*').ok_or_else(|| {
                GitError::InvalidFormat("pattern refspec destination is missing wildcard".into())
            })?;
            return Ok(Some(format!("{dst_prefix}{middle}{dst_suffix}")));
        }
        return Ok(Some(source.to_string()));
    }
    if src == source {
        return Ok(Some(
            refspec.dst.clone().unwrap_or_else(|| source.to_string()),
        ));
    }
    Ok(None)
}

pub fn fetch_head_ref_description(refname: &str) -> Result<String> {
    validate_fetch_head_description_field(refname)?;
    if let Some(branch) = refname.strip_prefix("refs/heads/") {
        Ok(format!("branch '{branch}'"))
    } else if let Some(tag) = refname.strip_prefix("refs/tags/") {
        Ok(format!("tag '{tag}'"))
    } else {
        Ok(refname.to_string())
    }
}

pub fn fetch_head_remote_description(refname: &str, remote: &str) -> Result<String> {
    validate_fetch_head_description_field(remote)?;
    Ok(format!(
        "{} of {remote}",
        fetch_head_ref_description(refname)?
    ))
}

pub fn parse_fetch_head(format: ObjectFormat, input: &[u8]) -> Result<Vec<FetchHeadRecord>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| parse_fetch_head_record(format, line))
        .collect()
}

pub fn encode_fetch_head(records: &[FetchHeadRecord]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for record in records {
        validate_fetch_head_description_field(&record.description)?;
        out.extend_from_slice(record.oid.to_string().as_bytes());
        out.push(b'\t');
        if record.not_for_merge {
            out.extend_from_slice(b"not-for-merge");
        }
        out.push(b'\t');
        out.extend_from_slice(record.description.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

pub fn read_fetch_head(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<FetchHeadRecord>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_fetch_head(format, &input)
}

pub fn write_fetch_head(writer: &mut impl Write, records: &[FetchHeadRecord]) -> Result<()> {
    for record in records {
        validate_fetch_head_description_field(&record.description)?;
        writer.write_all(record.oid.to_string().as_bytes())?;
        writer.write_all(b"\t")?;
        if record.not_for_merge {
            writer.write_all(b"not-for-merge")?;
        }
        writer.write_all(b"\t")?;
        writer.write_all(record.description.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

pub fn plan_fetch_ref_updates(
    refs: &[RefAdvertisement],
    refspecs: &[RefSpec],
    auto_follow_tags: bool,
) -> Result<Vec<FetchRefUpdate>> {
    let negative = refspecs
        .iter()
        .filter(|refspec| refspec.negative)
        .collect::<Vec<_>>();
    let mut updates = Vec::new();
    for refspec in refspecs.iter().filter(|refspec| !refspec.negative) {
        validate_refspec_shape(refspec)?;
        if refspec.src.is_none() {
            return Err(GitError::InvalidFormat(
                "fetch refspec is missing a source".into(),
            ));
        }
        if refspec.pattern {
            for reference in refs {
                if refspec_is_excluded(&negative, &reference.name)? {
                    continue;
                }
                if let Some(dst) = refspec_map_source(refspec, &reference.name)? {
                    updates.push(FetchRefUpdate {
                        src: reference.name.clone(),
                        dst: Some(dst),
                        oid: reference.oid,
                        not_for_merge: false,
                    });
                }
            }
            continue;
        }
        let src = refspec
            .src
            .as_deref()
            .expect("validated positive fetch refspec source");
        if refspec_is_excluded(&negative, src)? {
            continue;
        }
        let Some(reference) = refs.iter().find(|reference| reference.name == src) else {
            return Err(GitError::reference_not_found(format!("remote ref {src}")));
        };
        updates.push(FetchRefUpdate {
            src: reference.name.clone(),
            dst: refspec.dst.clone(),
            oid: reference.oid,
            not_for_merge: false,
        });
    }
    if auto_follow_tags && updates.iter().any(|update| update.dst.is_some()) {
        let fetched_oids = updates
            .iter()
            .map(|update| update.oid)
            .collect::<Vec<_>>();
        let fetched_srcs = updates
            .iter()
            .map(|update| update.src.clone())
            .collect::<Vec<_>>();
        for reference in refs {
            if reference.name.starts_with("refs/tags/")
                && fetched_oids.iter().any(|oid| oid == &reference.oid)
                && !fetched_srcs.contains(&reference.name)
                && !refspec_is_excluded(&negative, &reference.name)?
            {
                updates.push(FetchRefUpdate {
                    src: reference.name.clone(),
                    dst: Some(reference.name.clone()),
                    oid: reference.oid,
                    not_for_merge: true,
                });
            }
        }
    }
    Ok(updates)
}

pub fn fetch_ref_updates_to_fetch_head(
    updates: &[FetchRefUpdate],
    remote: &str,
) -> Result<Vec<FetchHeadRecord>> {
    updates
        .iter()
        .map(|update| {
            Ok(FetchHeadRecord {
                oid: update.oid,
                not_for_merge: update.not_for_merge,
                description: fetch_head_remote_description(&update.src, remote)?,
            })
        })
        .collect()
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
                for local in local_refs {
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
                let remote = remote_ref(remote_refs, dst)
                    .ok_or_else(|| GitError::reference_not_found(format!("remote ref {dst}")))?;
                commands.push(ReceivePackCommand {
                    old_id: remote.oid,
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
    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands,
            capabilities,
            shallow: Vec::new(),
        },
        push_options,
        packfile,
    };
    validate_receive_pack_push_request_features(features, &request)?;
    Ok(request)
}

pub fn smart_http_info_refs_path(repository_path: &str, service: GitService) -> Result<String> {
    validate_smart_http_service(service)?;
    let repository_path = normalize_http_repository_path(repository_path)?;
    Ok(format!(
        "{repository_path}/info/refs?service={}",
        service.as_str()
    ))
}

pub fn smart_http_rpc_path(repository_path: &str, service: GitService) -> Result<String> {
    validate_smart_http_service(service)?;
    let repository_path = normalize_http_repository_path(repository_path)?;
    Ok(format!("{repository_path}/{}", service.as_str()))
}

pub fn dumb_http_info_refs_path(repository_path: &str) -> Result<String> {
    let repository_path = normalize_http_repository_path(repository_path)?;
    Ok(format!("{repository_path}/info/refs"))
}

pub fn dumb_http_alternates_path(repository_path: &str) -> Result<String> {
    let repository_path = normalize_http_repository_path(repository_path)?;
    Ok(format!("{repository_path}/objects/info/http-alternates"))
}

pub fn dumb_http_packs_path(repository_path: &str) -> Result<String> {
    let repository_path = normalize_http_repository_path(repository_path)?;
    Ok(format!("{repository_path}/objects/info/packs"))
}

pub fn dumb_http_loose_object_path(repository_path: &str, oid: &ObjectId) -> Result<String> {
    let repository_path = normalize_http_repository_path(repository_path)?;
    let oid = oid.to_string();
    let (directory, file) = oid.split_at(2);
    Ok(format!("{repository_path}/objects/{directory}/{file}"))
}

pub fn dumb_http_pack_file_path(repository_path: &str, hash: &ObjectId) -> Result<String> {
    dumb_http_pack_resource_path(repository_path, hash, "pack")
}

pub fn dumb_http_pack_index_path(repository_path: &str, hash: &ObjectId) -> Result<String> {
    dumb_http_pack_resource_path(repository_path, hash, "idx")
}

pub fn smart_http_advertisement_content_type(service: GitService) -> Result<String> {
    validate_smart_http_service(service)?;
    Ok(format!("application/x-{}-advertisement", service.as_str()))
}

pub fn smart_http_rpc_request_content_type(service: GitService) -> Result<String> {
    validate_smart_http_service(service)?;
    Ok(format!("application/x-{}-request", service.as_str()))
}

pub fn smart_http_rpc_result_content_type(service: GitService) -> Result<String> {
    validate_smart_http_service(service)?;
    Ok(format!("application/x-{}-result", service.as_str()))
}

pub fn parse_smart_http_advertisement_content_type(value: &str) -> Result<GitService> {
    parse_smart_http_content_type(value, "-advertisement")
}

pub fn parse_smart_http_rpc_request_content_type(value: &str) -> Result<GitService> {
    parse_smart_http_content_type(value, "-request")
}

pub fn parse_smart_http_rpc_result_content_type(value: &str) -> Result<GitService> {
    parse_smart_http_content_type(value, "-result")
}

pub fn parse_sideband_packet(payload: &[u8]) -> Result<SideBandPacket> {
    let Some((&channel, data)) = payload.split_first() else {
        return Err(GitError::InvalidFormat("sideband packet is empty".into()));
    };
    let channel = match channel {
        1 => SideBandChannel::Data,
        2 => SideBandChannel::Progress,
        3 => SideBandChannel::Fatal,
        other => {
            return Err(GitError::InvalidFormat(format!(
                "invalid sideband channel {other}"
            )));
        }
    };
    Ok(SideBandPacket {
        channel,
        data: data.to_vec(),
    })
}

pub fn encode_sideband_packet(packet: &SideBandPacket) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(packet.data.len() + 1);
    out.push(match packet.channel {
        SideBandChannel::Data => 1,
        SideBandChannel::Progress => 2,
        SideBandChannel::Fatal => 3,
    });
    out.extend_from_slice(&packet.data);
    if out.len() > PKT_LINE_MAX_PAYLOAD_LEN {
        return Err(GitError::InvalidFormat(format!(
            "sideband packet exceeds {PKT_LINE_MAX_PAYLOAD_LEN} bytes"
        )));
    }
    Ok(out)
}

pub fn write_sideband_packet(writer: &mut impl Write, packet: &SideBandPacket) -> Result<()> {
    write_sideband_payload(writer, packet.channel, &packet.data)
}

fn write_sideband_payload(
    writer: &mut impl Write,
    channel: SideBandChannel,
    data: &[u8],
) -> Result<()> {
    let payload_len = data
        .len()
        .checked_add(1)
        .ok_or_else(|| GitError::InvalidFormat("sideband packet length overflow".into()))?;
    if payload_len > PKT_LINE_MAX_PAYLOAD_LEN {
        return Err(GitError::InvalidFormat(format!(
            "sideband packet exceeds {PKT_LINE_MAX_PAYLOAD_LEN} bytes"
        )));
    }
    writer.write_all(&pkt_line_header(payload_len + 4))?;
    writer.write_all(&[match channel {
        SideBandChannel::Data => 1,
        SideBandChannel::Progress => 2,
        SideBandChannel::Fatal => 3,
    }])?;
    writer.write_all(data)?;
    Ok(())
}

pub fn parse_sideband_packets(payloads: &[Vec<u8>]) -> Result<Vec<SideBandPacket>> {
    payloads
        .iter()
        .map(|payload| parse_sideband_packet(payload))
        .collect()
}

pub fn encode_sideband_packets(packets: &[SideBandPacket]) -> Result<Vec<Vec<u8>>> {
    packets.iter().map(encode_sideband_packet).collect()
}

pub fn parse_sideband_stream(frames: &[PktLineFrame]) -> Result<Vec<SideBandPacket>> {
    let mut packets = Vec::new();
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                packets.push(parse_sideband_packet(payload)?);
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "sideband stream has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "sideband stream has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "sideband stream contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "sideband stream missing flush".into(),
        ));
    }
    Ok(packets)
}

pub fn encode_sideband_stream(packets: &[SideBandPacket]) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    for packet in packets {
        frames.push(PktLineFrame::data(encode_sideband_packet(packet)?)?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_sideband_stream(reader: &mut impl Read) -> Result<Vec<SideBandPacket>> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_sideband_stream(&frames)
}

pub fn write_sideband_stream(writer: &mut impl Write, packets: &[SideBandPacket]) -> Result<()> {
    for packet in packets {
        write_sideband_packet(writer, packet)?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn demux_sideband_packets(packets: &[SideBandPacket]) -> Result<SideBandDemux> {
    let mut out = SideBandDemux::default();
    for packet in packets {
        match packet.channel {
            SideBandChannel::Data => out.data.extend_from_slice(&packet.data),
            SideBandChannel::Progress => out.progress.push(packet.data.clone()),
            SideBandChannel::Fatal => {
                let message = String::from_utf8_lossy(&packet.data).into_owned();
                return Err(GitError::InvalidFormat(format!(
                    "sideband fatal: {message}"
                )));
            }
        }
    }
    Ok(out)
}

pub fn parse_and_demux_sideband_packets(payloads: &[Vec<u8>]) -> Result<SideBandDemux> {
    let packets = parse_sideband_packets(payloads)?;
    demux_sideband_packets(&packets)
}

pub fn demux_sideband_stream(frames: &[PktLineFrame]) -> Result<SideBandDemux> {
    let packets = parse_sideband_stream(frames)?;
    demux_sideband_packets(&packets)
}

pub fn read_and_demux_sideband_stream(reader: &mut impl Read) -> Result<SideBandDemux> {
    let packets = read_sideband_stream(reader)?;
    demux_sideband_packets(&packets)
}

pub fn parse_upload_archive_request(frames: &[PktLineFrame]) -> Result<UploadArchiveRequest> {
    let mut request = UploadArchiveRequest::default();
    let mut saw_flush = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                let text = parse_protocol_v2_line_text("upload-archive request argument", payload)?;
                let argument = text.strip_prefix("argument ").ok_or_else(|| {
                    GitError::InvalidFormat("upload-archive request line must be argument".into())
                })?;
                validate_upload_archive_argument(argument)?;
                request.arguments.push(argument.to_string());
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "upload-archive request has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "upload-archive request has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "upload-archive request contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "upload-archive request missing flush".into(),
        ));
    }
    if request.arguments.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-archive request is missing arguments".into(),
        ));
    }
    Ok(request)
}

pub fn encode_upload_archive_request(request: &UploadArchiveRequest) -> Result<Vec<PktLineFrame>> {
    if request.arguments.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-archive request is missing arguments".into(),
        ));
    }
    let mut frames = Vec::new();
    for argument in &request.arguments {
        validate_upload_archive_argument(argument)?;
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "argument {argument}"
        )))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_upload_archive_request(reader: &mut impl Read) -> Result<UploadArchiveRequest> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_upload_archive_request(&frames)
}

pub fn write_upload_archive_request(
    writer: &mut impl Write,
    request: &UploadArchiveRequest,
) -> Result<()> {
    if request.arguments.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-archive request is missing arguments".into(),
        ));
    }
    for argument in &request.arguments {
        validate_upload_archive_argument(argument)?;
        write_pkt_line_payload(writer, &line_from_str(&format!("argument {argument}")))?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_upload_archive_response(frames: &[PktLineFrame]) -> Result<UploadArchiveResponse> {
    let Some((first, rest)) = frames.split_first() else {
        return Err(GitError::InvalidFormat(
            "upload-archive response is empty".into(),
        ));
    };
    let PktLineFrame::Data(payload) = first else {
        return Err(GitError::InvalidFormat(
            "upload-archive response must start with a data packet".into(),
        ));
    };
    let text = parse_protocol_v2_line_text("upload-archive response status", payload)?;
    if text == "ACK" {
        return Ok(UploadArchiveResponse::Ack {
            sideband: parse_sideband_stream(rest)?,
        });
    }
    if let Some(message) = text.strip_prefix("NACK ") {
        validate_upload_archive_status_message(message)?;
        if !matches!(rest, [PktLineFrame::Flush]) {
            return Err(GitError::InvalidFormat(
                "upload-archive NACK response must end with flush".into(),
            ));
        }
        return Ok(UploadArchiveResponse::Nack {
            message: message.to_string(),
        });
    }
    Err(GitError::InvalidFormat(format!(
        "unsupported upload-archive response status {text}"
    )))
}

pub fn encode_upload_archive_response(
    response: &UploadArchiveResponse,
) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    match response {
        UploadArchiveResponse::Ack { sideband } => {
            frames.push(PktLineFrame::data(line_from_str("ACK"))?);
            frames.extend(encode_sideband_stream(sideband)?);
        }
        UploadArchiveResponse::Nack { message } => {
            validate_upload_archive_status_message(message)?;
            frames.push(PktLineFrame::data(line_from_str(&format!(
                "NACK {message}"
            )))?);
            frames.push(PktLineFrame::Flush);
        }
    }
    Ok(frames)
}

pub fn read_upload_archive_response(reader: &mut impl Read) -> Result<UploadArchiveResponse> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_upload_archive_response(&frames)
}

pub fn write_upload_archive_response(
    writer: &mut impl Write,
    response: &UploadArchiveResponse,
) -> Result<()> {
    match response {
        UploadArchiveResponse::Ack { sideband } => {
            write_pkt_line_payload(writer, b"ACK\n")?;
            write_sideband_stream(writer, sideband)?;
        }
        UploadArchiveResponse::Nack { message } => {
            validate_upload_archive_status_message(message)?;
            write_pkt_line_payload(writer, &line_from_str(&format!("NACK {message}")))?;
            writer.write_all(b"0000")?;
        }
    }
    Ok(())
}

pub fn demux_upload_archive_response(response: &UploadArchiveResponse) -> Result<SideBandDemux> {
    match response {
        UploadArchiveResponse::Ack { sideband } => demux_sideband_packets(sideband),
        UploadArchiveResponse::Nack { message } => Err(GitError::InvalidFormat(format!(
            "upload-archive NACK: {message}"
        ))),
    }
}

fn parse_pkt_len(bytes: &[u8]) -> Result<usize> {
    let mut len = 0usize;
    for byte in bytes {
        len = (len << 4) | hex_nibble(*byte)? as usize;
    }
    Ok(len)
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(GitError::InvalidFormat(format!(
            "invalid pkt-line length byte {byte:#04x}"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportHandshake {
    pub protocol: ProtocolVersion,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefAdvertisement {
    pub oid: ObjectId,
    pub name: String,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumbHttpRefRecord {
    pub oid: ObjectId,
    pub name: String,
    pub peeled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumbHttpPackRecord {
    pub hash: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefAdvertisementSet {
    pub protocol: ProtocolVersion,
    pub refs: Vec<RefAdvertisement>,
    pub shallow: Vec<ObjectId>,
}

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
                    return Err(GitError::InvalidFormat(
                        "protocol v2 command request has duplicate delimiter".into(),
                    ));
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
    let frames = read_pkt_line_frames_until_flush(reader)?;
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

pub fn read_protocol_v2_ls_refs_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<ProtocolV2LsRefsRecord>> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
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
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_protocol_v2_fetch_response(format, &frames)
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
    let frames = read_pkt_line_frames_until_flush(reader)?;
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
    let frames = read_pkt_line_frames_until_response_end(reader)?;
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
    let frames = read_pkt_line_frames_until_response_end(reader)?;
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

pub fn parse_capabilities(input: &[u8]) -> Result<Vec<Capability>> {
    let input = trim_trailing_lf(input);
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let text =
        std::str::from_utf8(input).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    text.split(' ')
        .map(parse_capability_token)
        .collect::<Result<Vec<_>>>()
}

pub fn encode_capabilities(capabilities: &[Capability]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (idx, capability) in capabilities.iter().enumerate() {
        validate_capability_field("capability name", &capability.name)?;
        if idx != 0 {
            out.push(b' ');
        }
        out.extend_from_slice(capability.name.as_bytes());
        if let Some(value) = &capability.value {
            validate_capability_field("capability value", value)?;
            out.push(b'=');
            out.extend_from_slice(value.as_bytes());
        }
    }
    Ok(out)
}

pub fn parse_ref_advertisement(format: ObjectFormat, payload: &[u8]) -> Result<RefAdvertisement> {
    let payload = trim_trailing_lf(payload);
    let (reference, capabilities) = match payload.iter().position(|byte| *byte == 0) {
        Some(idx) => (&payload[..idx], parse_capabilities(&payload[idx + 1..])?),
        None => (payload, Vec::new()),
    };
    let text =
        std::str::from_utf8(reference).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let (oid, name) = text
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat("advertised ref is missing name".into()))?;
    if name.is_empty() {
        return Err(GitError::InvalidFormat(
            "advertised ref name is empty".into(),
        ));
    }
    Ok(RefAdvertisement {
        oid: ObjectId::from_hex(format, oid)?,
        name: name.to_string(),
        capabilities,
    })
}

pub fn encode_ref_advertisement(advertisement: &RefAdvertisement) -> Result<Vec<u8>> {
    validate_protocol_v2_token("advertised ref name", &advertisement.name)?;
    let mut out = advertisement.oid.to_string().into_bytes();
    out.push(b' ');
    out.extend_from_slice(advertisement.name.as_bytes());
    if !advertisement.capabilities.is_empty() {
        out.push(0);
        out.extend_from_slice(&encode_capabilities(&advertisement.capabilities)?);
    }
    out.push(b'\n');
    Ok(out)
}

pub fn parse_ref_advertisements(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<Vec<RefAdvertisement>> {
    Ok(parse_ref_advertisement_set(format, frames)?.refs)
}

pub fn parse_ref_advertisement_set(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<RefAdvertisementSet> {
    let mut set = RefAdvertisementSet {
        protocol: ProtocolVersion::V0,
        refs: Vec::new(),
        shallow: Vec::new(),
    };
    let mut saw_flush = false;
    let mut in_shallow = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                let trimmed = trim_trailing_lf(payload);
                if trimmed == b"version 1" {
                    if idx != 0 {
                        return Err(GitError::InvalidFormat(
                            "advertised ref protocol version must be the first line".into(),
                        ));
                    }
                    set.protocol = ProtocolVersion::V1;
                    continue;
                }
                if trimmed.starts_with(b"version ") {
                    return Err(GitError::InvalidFormat(
                        "unsupported advertised ref protocol version".into(),
                    ));
                }
                if trimmed.starts_with(b"shallow ") {
                    if set.refs.is_empty() {
                        return Err(GitError::InvalidFormat(
                            "advertised shallow refs must follow advertised refs".into(),
                        ));
                    }
                    let text = parse_protocol_v2_line_text("advertised shallow ref", payload)?;
                    set.shallow.push(parse_oid_argument(
                        format,
                        "advertised shallow ref",
                        text,
                        "shallow ",
                    )?);
                    in_shallow = true;
                    continue;
                }
                if in_shallow {
                    return Err(GitError::InvalidFormat(
                        "advertised refs must not follow shallow refs".into(),
                    ));
                }
                let advertisement = parse_ref_advertisement(format, payload)?;
                if !set.refs.is_empty() && !advertisement.capabilities.is_empty() {
                    return Err(GitError::InvalidFormat(
                        "advertised ref capabilities must appear on the first ref".into(),
                    ));
                }
                set.refs.push(advertisement);
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "advertised ref stream has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "advertised ref stream has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "advertised ref stream contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "advertised ref stream missing flush".into(),
        ));
    }
    Ok(set)
}

pub fn encode_ref_advertisements(advertisements: &[RefAdvertisement]) -> Result<Vec<PktLineFrame>> {
    encode_ref_advertisement_set(&RefAdvertisementSet {
        protocol: ProtocolVersion::V0,
        refs: advertisements.to_vec(),
        shallow: Vec::new(),
    })
}

pub fn encode_ref_advertisement_set(set: &RefAdvertisementSet) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    match set.protocol {
        ProtocolVersion::V0 => {}
        ProtocolVersion::V1 => frames.push(PktLineFrame::data(line_from_str("version 1"))?),
        ProtocolVersion::V2 => {
            return Err(GitError::InvalidFormat(
                "protocol v2 does not use v0/v1 advertised-ref streams".into(),
            ));
        }
    }
    if set.refs.is_empty() && !set.shallow.is_empty() {
        return Err(GitError::InvalidFormat(
            "advertised shallow refs require advertised refs".into(),
        ));
    }
    for (idx, advertisement) in set.refs.iter().enumerate() {
        if idx != 0 && !advertisement.capabilities.is_empty() {
            return Err(GitError::InvalidFormat(
                "advertised ref capabilities must appear on the first ref".into(),
            ));
        }
        frames.push(PktLineFrame::data(encode_ref_advertisement(
            advertisement,
        )?)?);
    }
    for oid in &set.shallow {
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "shallow {oid}"
        )))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_ref_advertisements(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<RefAdvertisement>> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_ref_advertisements(format, &frames)
}

pub fn read_ref_advertisement_set(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<RefAdvertisementSet> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_ref_advertisement_set(format, &frames)
}

pub fn write_ref_advertisements(
    writer: &mut impl Write,
    advertisements: &[RefAdvertisement],
) -> Result<()> {
    write_ref_advertisement_stream(writer, ProtocolVersion::V0, advertisements, &[])
}

pub fn write_ref_advertisement_set(
    writer: &mut impl Write,
    set: &RefAdvertisementSet,
) -> Result<()> {
    write_ref_advertisement_stream(writer, set.protocol, &set.refs, &set.shallow)
}

fn write_ref_advertisement_stream(
    writer: &mut impl Write,
    protocol: ProtocolVersion,
    refs: &[RefAdvertisement],
    shallow: &[ObjectId],
) -> Result<()> {
    match protocol {
        ProtocolVersion::V0 => {}
        ProtocolVersion::V1 => write_pkt_line_payload(writer, b"version 1\n")?,
        ProtocolVersion::V2 => {
            return Err(GitError::InvalidFormat(
                "protocol v2 does not use v0/v1 advertised-ref streams".into(),
            ));
        }
    }
    if refs.is_empty() && !shallow.is_empty() {
        return Err(GitError::InvalidFormat(
            "advertised shallow refs require advertised refs".into(),
        ));
    }
    for (idx, advertisement) in refs.iter().enumerate() {
        if idx != 0 && !advertisement.capabilities.is_empty() {
            return Err(GitError::InvalidFormat(
                "advertised ref capabilities must appear on the first ref".into(),
            ));
        }
        write_pkt_line_payload(writer, &encode_ref_advertisement(advertisement)?)?;
    }
    for oid in shallow {
        write_pkt_line_payload(writer, &line_from_str(&format!("shallow {oid}")))?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_dumb_http_info_refs(
    format: ObjectFormat,
    input: &[u8],
) -> Result<Vec<DumbHttpRefRecord>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| parse_dumb_http_info_ref_record(format, line))
        .collect()
}

pub fn encode_dumb_http_info_refs(records: &[DumbHttpRefRecord]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for record in records {
        validate_dumb_http_ref_name(&record.name)?;
        out.extend_from_slice(record.oid.to_string().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(record.name.as_bytes());
        if record.peeled {
            out.extend_from_slice(b"^{}");
        }
        out.push(b'\n');
    }
    Ok(out)
}

pub fn read_dumb_http_info_refs(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<DumbHttpRefRecord>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_dumb_http_info_refs(format, &input)
}

pub fn write_dumb_http_info_refs(
    writer: &mut impl Write,
    records: &[DumbHttpRefRecord],
) -> Result<()> {
    for record in records {
        validate_dumb_http_ref_name(&record.name)?;
        writer.write_all(record.oid.to_string().as_bytes())?;
        writer.write_all(b"\t")?;
        writer.write_all(record.name.as_bytes())?;
        if record.peeled {
            writer.write_all(b"^{}")?;
        }
        writer.write_all(b"\n")?;
    }
    Ok(())
}

pub fn parse_dumb_http_alternates(input: &[u8]) -> Result<Vec<String>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(parse_dumb_http_alternate)
        .collect()
}

pub fn encode_dumb_http_alternates(alternates: &[String]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for alternate in alternates {
        validate_dumb_http_alternate(alternate)?;
        out.extend_from_slice(alternate.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

pub fn read_dumb_http_alternates(reader: &mut impl Read) -> Result<Vec<String>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_dumb_http_alternates(&input)
}

pub fn write_dumb_http_alternates(writer: &mut impl Write, alternates: &[String]) -> Result<()> {
    for alternate in alternates {
        validate_dumb_http_alternate(alternate)?;
        writer.write_all(alternate.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

pub fn parse_dumb_http_packs(
    format: ObjectFormat,
    input: &[u8],
) -> Result<Vec<DumbHttpPackRecord>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| parse_dumb_http_pack_record(format, line))
        .collect()
}

pub fn encode_dumb_http_packs(records: &[DumbHttpPackRecord]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(format!("P pack-{}.pack\n", record.hash).as_bytes());
    }
    Ok(out)
}

pub fn read_dumb_http_packs(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<DumbHttpPackRecord>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_dumb_http_packs(format, &input)
}

pub fn write_dumb_http_packs(
    writer: &mut impl Write,
    records: &[DumbHttpPackRecord],
) -> Result<()> {
    for record in records {
        writer.write_all(format!("P pack-{}.pack\n", record.hash).as_bytes())?;
    }
    Ok(())
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
                    return Err(GitError::InvalidFormat(
                        "upload-pack symref capability is missing value".into(),
                    ));
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
    let packfile = build_pack(request.wants, known_haves)?
        .ok_or_else(|| GitError::InvalidObject("upload-pack request produced empty pack".into()))?;
    Ok(UploadPackRawPackfileResponse {
        acknowledgments: vec![UploadPackAcknowledgment::Nak],
        packfile,
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
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_upload_pack_raw_packfile_response(format, &input)
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
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_upload_pack_shallow_info_and_raw_packfile_response(format, &input)
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
    let commands = read_receive_pack_request(format, reader)?;
    let push_options = if has_push_options {
        Some(read_receive_pack_push_options(reader)?)
    } else {
        None
    };
    let mut packfile = Vec::new();
    reader.read_to_end(&mut packfile)?;
    Ok(ReceivePackPushRequest {
        commands,
        push_options,
        packfile,
    })
}

pub fn write_receive_pack_push_request(
    writer: &mut impl Write,
    request: &ReceivePackPushRequest,
) -> Result<()> {
    write_receive_pack_request(writer, &request.commands)?;
    if let Some(push_options) = &request.push_options {
        write_receive_pack_push_options(writer, push_options)?;
    }
    writer.write_all(&request.packfile)?;
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
    if has_update_or_create && request.packfile.is_empty() {
        return Err(GitError::InvalidFormat(
            "receive-pack request with create/update commands is missing packfile".into(),
        ));
    }
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
    D: FnMut(&str) -> Result<()>,
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
        install_pack(&request.packfile)?;
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
        delete_ref(&command.name)?;
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

fn reject_capability_value(label: &str, capability: &Capability) -> Result<()> {
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

fn validate_protocol_error_message(message: &str) -> Result<()> {
    if message.is_empty() {
        return Err(GitError::InvalidFormat(
            "protocol error message is empty".into(),
        ));
    }
    if message
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(
            "protocol error message contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn parse_capability_token(token: &str) -> Result<Capability> {
    if token.is_empty() {
        return Err(GitError::InvalidFormat("empty capability token".into()));
    }
    let (name, value) = token
        .split_once('=')
        .map_or((token, None), |(name, value)| (name, Some(value)));
    validate_capability_field("capability name", name)?;
    if let Some(value) = value {
        validate_capability_field("capability value", value)?;
    }
    Ok(Capability {
        name: name.to_string(),
        value: value.map(str::to_string),
    })
}

fn parse_protocol_v2_capability_line(payload: &[u8]) -> Result<Capability> {
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

fn parse_protocol_v2_command_line(payload: &[u8]) -> Result<String> {
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

fn parse_fetch_shallow_info(
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
                ProtocolV2FetchShallowInfo::Shallow(oid) => {
                    Ok(line_from_str(&format!("shallow {oid}")))
                }
                ProtocolV2FetchShallowInfo::Unshallow(oid) => {
                    Ok(line_from_str(&format!("unshallow {oid}")))
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

fn parse_dumb_http_info_ref_record(format: ObjectFormat, line: &[u8]) -> Result<DumbHttpRefRecord> {
    validate_dumb_http_info_ref_line(line)?;
    let line = trim_trailing_lf(line);
    let tab = line
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| GitError::InvalidFormat("dumb HTTP ref record is missing name".into()))?;
    let (oid, name) = (&line[..tab], &line[tab + 1..]);
    let oid = std::str::from_utf8(oid).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_protocol_v2_token("dumb HTTP ref oid", oid)?;
    let name = std::str::from_utf8(name).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let (name, peeled) = name
        .strip_suffix("^{}")
        .map_or((name, false), |name| (name, true));
    validate_dumb_http_ref_name(name)?;
    Ok(DumbHttpRefRecord {
        oid: ObjectId::from_hex(format, oid)?,
        name: name.to_string(),
        peeled,
    })
}

fn parse_dumb_http_alternate(line: &[u8]) -> Result<String> {
    validate_dumb_http_alternate_line(line)?;
    let line = trim_trailing_lf(line);
    let alternate =
        std::str::from_utf8(line).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_dumb_http_alternate(alternate)?;
    Ok(alternate.to_string())
}

fn parse_dumb_http_pack_record(format: ObjectFormat, line: &[u8]) -> Result<DumbHttpPackRecord> {
    validate_dumb_http_info_ref_line(line)?;
    let line = parse_protocol_v2_line_text("dumb HTTP pack record", line)?;
    let pack_name = line
        .strip_prefix("P ")
        .ok_or_else(|| GitError::InvalidFormat("dumb HTTP pack record must start with P".into()))?;
    let hash = pack_name
        .strip_prefix("pack-")
        .and_then(|value| value.strip_suffix(".pack"))
        .ok_or_else(|| GitError::InvalidFormat("invalid dumb HTTP pack name".into()))?;
    validate_protocol_v2_token("dumb HTTP pack hash", hash)?;
    Ok(DumbHttpPackRecord {
        hash: ObjectId::from_hex(format, hash)?,
    })
}

fn encode_protocol_v2_capability(capability: &Capability) -> Result<Vec<u8>> {
    validate_capability_name(&capability.name)?;
    let mut out = capability.name.as_bytes().to_vec();
    if let Some(value) = &capability.value {
        validate_protocol_v2_capability_value(value)?;
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
    }
    Ok(out)
}

fn validate_capability_field(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value
        .bytes()
        .any(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | 0))
    {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn validate_capability_name(value: &str) -> Result<()> {
    validate_capability_field("capability name", value)?;
    if value.bytes().any(|byte| byte == b'=') {
        return Err(GitError::InvalidFormat(
            "capability name contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_protocol_v2_capability_value(value: &str) -> Result<()> {
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

fn validate_upload_archive_argument(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-archive argument is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "upload-archive argument contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_upload_archive_status_message(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "upload-archive status message is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "upload-archive status message contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn validate_refspec_value(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("refspec is empty".into()));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "refspec contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_refspec_endpoint(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value
        .bytes()
        .any(|byte| matches!(byte, b':' | b' ' | b'\t' | b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn count_refspec_wildcards(value: &str) -> usize {
    value.bytes().filter(|byte| *byte == b'*').count()
}

fn validate_refspec_shape(refspec: &RefSpec) -> Result<()> {
    if refspec.force && refspec.negative {
        return Err(GitError::InvalidFormat(
            "negative refspec must not be forced".into(),
        ));
    }
    if refspec.negative && refspec.dst.is_some() {
        return Err(GitError::InvalidFormat(
            "negative refspec must not have a destination".into(),
        ));
    }
    if refspec.negative && refspec.src.is_none() {
        return Err(GitError::InvalidFormat(
            "negative refspec is missing a source".into(),
        ));
    }
    if refspec.src.is_none() && refspec.dst.is_none() && refspec.negative {
        return Err(GitError::InvalidFormat(
            "refspec must include a source or destination".into(),
        ));
    }
    if let Some(src) = &refspec.src {
        validate_refspec_endpoint("refspec source", src)?;
    }
    if let Some(dst) = &refspec.dst {
        validate_refspec_endpoint("refspec destination", dst)?;
    }
    let src_pattern_count = refspec
        .src
        .as_deref()
        .map(count_refspec_wildcards)
        .unwrap_or(0);
    let dst_pattern_count = refspec
        .dst
        .as_deref()
        .map(count_refspec_wildcards)
        .unwrap_or(0);
    if src_pattern_count > 1 || dst_pattern_count > 1 {
        return Err(GitError::InvalidFormat(
            "refspec endpoint has too many wildcards".into(),
        ));
    }
    if refspec.dst.is_some() && (src_pattern_count == 1) != (dst_pattern_count == 1) {
        return Err(GitError::InvalidFormat(
            "refspec wildcard must appear in both source and destination".into(),
        ));
    }
    if refspec.pattern != (src_pattern_count == 1 || dst_pattern_count == 1) {
        return Err(GitError::InvalidFormat(
            "refspec pattern flag does not match endpoints".into(),
        ));
    }
    Ok(())
}

fn parse_fetch_head_record(format: ObjectFormat, line: &[u8]) -> Result<FetchHeadRecord> {
    validate_fetch_head_line(line)?;
    let line = trim_trailing_lf(line);
    let mut fields = line.splitn(3, |byte| *byte == b'\t');
    let oid = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("FETCH_HEAD record is missing oid".into()))?;
    let merge_marker = fields.next().ok_or_else(|| {
        GitError::InvalidFormat("FETCH_HEAD record is missing merge marker".into())
    })?;
    let description = fields.next().ok_or_else(|| {
        GitError::InvalidFormat("FETCH_HEAD record is missing description".into())
    })?;
    let oid = std::str::from_utf8(oid).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_protocol_v2_token("FETCH_HEAD oid", oid)?;
    let not_for_merge = match merge_marker {
        b"" => false,
        b"not-for-merge" => true,
        _ => {
            return Err(GitError::InvalidFormat(
                "FETCH_HEAD record has invalid merge marker".into(),
            ));
        }
    };
    let description =
        std::str::from_utf8(description).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_fetch_head_description_field(description)?;
    Ok(FetchHeadRecord {
        oid: ObjectId::from_hex(format, oid)?,
        not_for_merge,
        description: description.to_string(),
    })
}

fn validate_fetch_head_line(value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("FETCH_HEAD record is empty".into()));
    }
    if !value.ends_with(b"\n") {
        return Err(GitError::InvalidFormat(
            "FETCH_HEAD record missing LF".into(),
        ));
    }
    if value.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "FETCH_HEAD record contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_fetch_head_description_field(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "FETCH_HEAD description is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "FETCH_HEAD description contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn refspec_is_excluded(negative: &[&RefSpec], source: &str) -> Result<bool> {
    for refspec in negative {
        if refspec_matches_source(refspec, source)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn zero_object_id(format: ObjectFormat) -> Result<ObjectId> {
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

fn validate_smart_http_service(service: GitService) -> Result<()> {
    match service {
        GitService::UploadPack | GitService::ReceivePack => Ok(()),
        GitService::UploadArchive => Err(GitError::InvalidFormat(
            "smart HTTP only supports upload-pack and receive-pack services".into(),
        )),
    }
}

fn normalize_http_repository_path(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(GitError::InvalidFormat(
            "smart HTTP repository path is empty".into(),
        ));
    }
    if !path.starts_with('/') {
        return Err(GitError::InvalidFormat(
            "smart HTTP repository path must start with /".into(),
        ));
    }
    if path
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0 | b'?' | b'#'))
    {
        return Err(GitError::InvalidFormat(
            "smart HTTP repository path contains a delimiter byte".into(),
        ));
    }
    let normalized = path.trim_end_matches('/');
    Ok(if normalized.is_empty() {
        "/".into()
    } else {
        normalized.to_string()
    })
}

fn dumb_http_pack_resource_path(
    repository_path: &str,
    hash: &ObjectId,
    suffix: &str,
) -> Result<String> {
    let repository_path = normalize_http_repository_path(repository_path)?;
    Ok(format!(
        "{repository_path}/objects/pack/pack-{hash}.{suffix}"
    ))
}

fn parse_smart_http_content_type(value: &str, suffix: &str) -> Result<GitService> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "smart HTTP content type is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "smart HTTP content type contains a delimiter byte".into(),
        ));
    }
    let value = value.to_ascii_lowercase();
    let service = value
        .strip_prefix("application/x-")
        .and_then(|value| value.strip_suffix(suffix))
        .ok_or_else(|| GitError::InvalidFormat("invalid smart HTTP content type".into()))?;
    let service = parse_git_service(service)?;
    validate_smart_http_service(service)?;
    Ok(service)
}

fn validate_dumb_http_info_ref_line(value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref record is empty".into(),
        ));
    }
    if !value.ends_with(b"\n") {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref record missing LF".into(),
        ));
    }
    if value.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref record contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_dumb_http_ref_name(value: &str) -> Result<()> {
    validate_protocol_v2_token("dumb HTTP ref name", value)?;
    if value.ends_with("^{}") {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref name must not include peeled suffix".into(),
        ));
    }
    Ok(())
}

fn validate_dumb_http_alternate_line(value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate is empty".into(),
        ));
    }
    if !value.ends_with(b"\n") {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate missing LF".into(),
        ));
    }
    if value.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_dumb_http_alternate(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_protocol_v2_token(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value
        .bytes()
        .any(|byte| matches!(byte, b' ' | b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn validate_protocol_v2_line(label: &str, value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn parse_protocol_v2_line_text<'a>(label: &str, value: &'a [u8]) -> Result<&'a str> {
    validate_protocol_v2_line(label, value)?;
    let value = trim_trailing_lf(value);
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value.iter().any(|byte| matches!(*byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    std::str::from_utf8(value).map_err(|err| GitError::InvalidFormat(err.to_string()))
}

fn parse_oid_argument(
    format: ObjectFormat,
    label: &str,
    value: &str,
    prefix: &str,
) -> Result<ObjectId> {
    let oid = value
        .strip_prefix(prefix)
        .ok_or_else(|| GitError::InvalidFormat(format!("invalid {label}")))?;
    validate_protocol_v2_token(label, oid)?;
    ObjectId::from_hex(format, oid)
}

fn parse_u32_argument(label: &str, value: &str, prefix: &str) -> Result<u32> {
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

fn parse_u64_argument(label: &str, value: &str, prefix: &str) -> Result<u64> {
    let number = value
        .strip_prefix(prefix)
        .ok_or_else(|| GitError::InvalidFormat(format!("invalid {label}")))?;
    validate_protocol_v2_token(label, number)?;
    number
        .parse::<u64>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))
}

fn line(mut payload: Vec<u8>) -> Vec<u8> {
    payload.push(b'\n');
    payload
}

fn line_from_str(payload: &str) -> Vec<u8> {
    line(payload.as_bytes().to_vec())
}

fn trim_trailing_lf(input: &[u8]) -> &[u8] {
    input.strip_suffix(b"\n").unwrap_or(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkt_line_frame_encodes_data_and_control_frames() {
        assert_eq!(PktLine(b"hello\n".to_vec()).encode(), b"000ahello\n");
        assert_eq!(
            PktLineFrame::data(b"hello\n".to_vec())
                .expect("test operation should succeed")
                .encode(),
            b"000ahello\n"
        );
        assert_eq!(PktLineFrame::Flush.encode(), b"0000");
        assert_eq!(PktLineFrame::Delimiter.encode(), b"0001");
        assert_eq!(PktLineFrame::ResponseEnd.encode(), b"0002");
        assert_eq!(
            PktLineFrame::data(b"hello\n".to_vec())
                .expect("test operation should succeed")
                .try_encode()
                .expect("test operation should succeed"),
            b"000ahello\n"
        );
    }

    #[test]
    fn pkt_line_frame_parses_data_and_control_frames() {
        assert_eq!(
            PktLineFrame::parse(b"000ahello\n").expect("test operation should succeed"),
            (PktLineFrame::Data(b"hello\n".to_vec()), 10)
        );
        assert_eq!(
            PktLineFrame::parse(b"0000").expect("test operation should succeed"),
            (PktLineFrame::Flush, 4)
        );
        assert_eq!(
            PktLineFrame::parse(b"0001").expect("test operation should succeed"),
            (PktLineFrame::Delimiter, 4)
        );
        assert_eq!(
            PktLineFrame::parse(b"0002").expect("test operation should succeed"),
            (PktLineFrame::ResponseEnd, 4)
        );
    }

    #[test]
    fn pkt_line_stream_parses_multiple_frames() {
        let frames = parse_pkt_line_stream(b"000eversion 2\n00010009done\n0000")
            .expect("test operation should succeed");
        assert_eq!(
            frames,
            vec![
                PktLineFrame::Data(b"version 2\n".to_vec()),
                PktLineFrame::Delimiter,
                PktLineFrame::Data(b"done\n".to_vec()),
                PktLineFrame::Flush,
            ]
        );
    }

    #[test]
    fn pkt_line_stream_reads_and_writes_incremental_io() {
        let frames = vec![
            PktLineFrame::Data(b"version 2\n".to_vec()),
            PktLineFrame::Delimiter,
            PktLineFrame::Data(b"done\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let mut encoded = Vec::new();
        write_pkt_line_frames(&mut encoded, &frames).expect("test operation should succeed");
        assert_eq!(encoded, b"000eversion 2\n00010009done\n0000");
        assert_eq!(
            read_pkt_line_frames(&mut encoded.as_slice()).expect("test operation should succeed"),
            frames
        );

        let mut empty: &[u8] = b"";
        assert_eq!(
            read_pkt_line_frame(&mut empty).expect("test operation should succeed"),
            None
        );
    }

    #[test]
    fn pkt_line_stream_reads_until_control_packets() {
        let input = b"000eversion 2\n0000trailing";
        let frames = read_pkt_line_frames_until_flush(&mut &input[..])
            .expect("test operation should succeed");
        assert_eq!(
            frames,
            vec![
                PktLineFrame::Data(b"version 2\n".to_vec()),
                PktLineFrame::Flush,
            ]
        );

        let input = b"0009done\n0002next";
        let frames = read_pkt_line_frames_until_response_end(&mut &input[..])
            .expect("test operation should succeed");
        assert_eq!(
            frames,
            vec![
                PktLineFrame::Data(b"done\n".to_vec()),
                PktLineFrame::ResponseEnd,
            ]
        );
        assert!(read_pkt_line_frames_until_flush(&mut &b"0009done\n"[..]).is_err());
    }

    #[test]
    fn pkt_line_rejects_invalid_lengths() {
        assert!(PktLineFrame::parse(b"000").is_err());
        assert!(PktLineFrame::parse(b"0003").is_err());
        assert!(PktLineFrame::parse(b"000ahello").is_err());
        assert!(PktLineFrame::parse(b"zzzz").is_err());
        assert!(read_pkt_line_frame(&mut &b"000"[..]).is_err());
        assert!(read_pkt_line_frame(&mut &b"0003"[..]).is_err());
    }

    #[test]
    fn pkt_line_rejects_oversized_data() {
        let payload = vec![b'x'; PKT_LINE_MAX_PAYLOAD_LEN + 1];
        assert!(PktLineFrame::data(payload.clone()).is_err());
        assert!(PktLine(payload.clone()).try_encode().is_err());
        assert!(PktLineFrame::Data(payload.clone()).try_encode().is_err());
        assert!(write_pkt_line_frame(&mut Vec::new(), &PktLineFrame::Data(payload)).is_err());
        assert!(PktLineFrame::parse(b"fff1").is_err());
    }

    #[test]
    fn protocol_error_lines_parse_encode_and_stream() {
        let error = parse_error_line(b"ERR remote rejected request\n")
            .expect("test operation should succeed");
        assert_eq!(
            error,
            ProtocolErrorLine {
                message: "remote rejected request".into(),
            }
        );
        assert_eq!(
            encode_error_line(&error).expect("test operation should succeed"),
            b"ERR remote rejected request\n"
        );
        assert_eq!(
            parse_error_frame(&PktLineFrame::Data(
                b"ERR remote rejected request\n".to_vec()
            ))
            .expect("test operation should succeed"),
            Some(error.clone())
        );
        assert_eq!(
            parse_error_frame(&PktLineFrame::Data(b"NAK\n".to_vec()))
                .expect("test operation should succeed"),
            None
        );

        let mut encoded = Vec::new();
        write_error_line(&mut encoded, &error).expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_error_line(&mut input).expect("test operation should succeed"),
            error
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_error_lines_reject_malformed_messages() {
        assert!(parse_error_line(b"ERR\n").is_err());
        assert!(parse_error_line(b"ERR \n").is_err());
        assert!(parse_error_line(b"ERR bad\0message\n").is_err());
        assert!(parse_error_line(b"NAK\n").is_err());
        assert!(
            encode_error_line(&ProtocolErrorLine {
                message: "bad\nmessage".into(),
            })
            .is_err()
        );
        assert!(read_error_line(&mut &b"0000"[..]).is_err());
    }

    #[test]
    fn refspec_parser_handles_fetch_push_and_negative_forms() {
        assert_eq!(
            parse_refspec("+refs/heads/*:refs/remotes/origin/*")
                .expect("test operation should succeed"),
            RefSpec {
                force: true,
                negative: false,
                src: Some("refs/heads/*".into()),
                dst: Some("refs/remotes/origin/*".into()),
                pattern: true,
            }
        );
        assert_eq!(
            parse_refspec("refs/heads/main").expect("test operation should succeed"),
            RefSpec {
                force: false,
                negative: false,
                src: Some("refs/heads/main".into()),
                dst: None,
                pattern: false,
            }
        );
        assert_eq!(
            parse_refspec(":refs/heads/topic").expect("test operation should succeed"),
            RefSpec {
                force: false,
                negative: false,
                src: None,
                dst: Some("refs/heads/topic".into()),
                pattern: false,
            }
        );
        assert_eq!(
            parse_refspec(":").expect("test operation should succeed"),
            RefSpec {
                force: false,
                negative: false,
                src: None,
                dst: None,
                pattern: false,
            }
        );
        assert_eq!(
            parse_refspec("^refs/tags/private/*").expect("test operation should succeed"),
            RefSpec {
                force: false,
                negative: true,
                src: Some("refs/tags/private/*".into()),
                dst: None,
                pattern: true,
            }
        );
    }

    #[test]
    fn refspec_encode_and_map_sources() {
        let pattern = parse_refspec("+refs/heads/*:refs/remotes/origin/*")
            .expect("test operation should succeed");
        assert_eq!(
            encode_refspec(&pattern).expect("test operation should succeed"),
            "+refs/heads/*:refs/remotes/origin/*"
        );
        assert!(
            refspec_matches_source(&pattern, "refs/heads/main")
                .expect("test operation should succeed")
        );
        assert_eq!(
            refspec_map_source(&pattern, "refs/heads/main").expect("test operation should succeed"),
            Some("refs/remotes/origin/main".into())
        );
        assert_eq!(
            refspec_map_source(&pattern, "refs/tags/v1").expect("test operation should succeed"),
            None
        );

        let direct = parse_refspec("HEAD:refs/heads/main").expect("test operation should succeed");
        assert_eq!(
            encode_refspec(&direct).expect("test operation should succeed"),
            "HEAD:refs/heads/main"
        );
        assert_eq!(
            refspec_map_source(&direct, "HEAD").expect("test operation should succeed"),
            Some("refs/heads/main".into())
        );

        let delete = parse_refspec(":refs/heads/old").expect("test operation should succeed");
        assert_eq!(
            encode_refspec(&delete).expect("test operation should succeed"),
            ":refs/heads/old"
        );
        assert_eq!(
            refspec_map_source(&delete, "HEAD").expect("test operation should succeed"),
            None
        );

        let matching = parse_refspec(":").expect("test operation should succeed");
        assert_eq!(
            encode_refspec(&matching).expect("test operation should succeed"),
            ":"
        );
    }

    #[test]
    fn refspec_parser_rejects_malformed_values() {
        assert!(parse_refspec("").is_err());
        assert!(parse_refspec("+^refs/heads/main").is_err());
        assert!(parse_refspec("^refs/heads/main:refs/remotes/origin/main").is_err());
        assert!(parse_refspec("refs/heads/*:refs/remotes/origin/main").is_err());
        assert!(parse_refspec("refs/heads/**:refs/remotes/origin/*").is_err());
        assert!(parse_refspec("refs/heads/main:refs/remotes/origin/main:extra").is_err());
        assert!(parse_refspec("refs/heads/main\n").is_err());
        assert!(
            encode_refspec(&RefSpec {
                force: false,
                negative: false,
                src: Some("refs/heads/*".into()),
                dst: Some("refs/remotes/origin/main".into()),
                pattern: true,
            })
            .is_err()
        );
    }

    #[test]
    fn fetch_head_records_parse_encode_and_describe_refs() {
        let first = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let second = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let input = b"1111111111111111111111111111111111111111\t\tbranch 'main' of ../bundle.bdl\n2222222222222222222222222222222222222222\tnot-for-merge\ttag 'v1' of ../bundle.bdl\n";
        let records =
            parse_fetch_head(ObjectFormat::Sha1, input).expect("test operation should succeed");
        assert_eq!(
            records,
            vec![
                FetchHeadRecord {
                    oid: first,
                    not_for_merge: false,
                    description: "branch 'main' of ../bundle.bdl".into(),
                },
                FetchHeadRecord {
                    oid: second,
                    not_for_merge: true,
                    description: "tag 'v1' of ../bundle.bdl".into(),
                },
            ]
        );
        assert_eq!(
            encode_fetch_head(&records).expect("test operation should succeed"),
            input
        );
        assert_eq!(
            parse_fetch_head(ObjectFormat::Sha1, b"").expect("test operation should succeed"),
            Vec::<FetchHeadRecord>::new()
        );
        assert_eq!(
            fetch_head_remote_description("refs/heads/main", "../bundle.bdl")
                .expect("test operation should succeed"),
            "branch 'main' of ../bundle.bdl"
        );
        assert_eq!(
            fetch_head_remote_description("refs/tags/v1", "../bundle.bdl")
                .expect("test operation should succeed"),
            "tag 'v1' of ../bundle.bdl"
        );
        assert_eq!(
            fetch_head_remote_description("HEAD", "../bundle.bdl")
                .expect("test operation should succeed"),
            "HEAD of ../bundle.bdl"
        );
    }

    #[test]
    fn fetch_head_records_streams_round_trip() {
        let records = vec![FetchHeadRecord {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            not_for_merge: false,
            description: "branch 'main' of ../bundle.bdl".into(),
        }];
        let mut encoded = Vec::new();
        write_fetch_head(&mut encoded, &records).expect("test operation should succeed");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_fetch_head(ObjectFormat::Sha1, &mut input).expect("test operation should succeed"),
            records
        );
        assert!(input.is_empty());
    }

    #[test]
    fn fetch_head_records_reject_malformed_lines() {
        assert!(
            parse_fetch_head(
                ObjectFormat::Sha1,
                b"1111111111111111111111111111111111111111\t\tbranch 'main'"
            )
            .is_err()
        );
        assert!(
            parse_fetch_head(
                ObjectFormat::Sha1,
                b"1111111111111111111111111111111111111111\tfor-merge\tbranch 'main'\n"
            )
            .is_err()
        );
        assert!(parse_fetch_head(ObjectFormat::Sha1, b"not-a-hash\t\tbranch 'main'\n").is_err());
        assert!(
            encode_fetch_head(&[FetchHeadRecord {
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111"
                )
                .expect("test operation should succeed"),
                not_for_merge: false,
                description: "bad\ndescription".into(),
            }])
            .is_err()
        );
    }

    #[test]
    fn fetch_planner_maps_direct_pattern_and_negative_refspecs() {
        let main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let next = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let refs = vec![
            RefAdvertisement {
                oid: main.clone(),
                name: "refs/heads/main".into(),
                capabilities: Vec::new(),
            },
            RefAdvertisement {
                oid: next.clone(),
                name: "refs/heads/tmp".into(),
                capabilities: Vec::new(),
            },
        ];
        let refspecs = vec![
            parse_refspec("+refs/heads/*:refs/remotes/origin/*")
                .expect("test operation should succeed"),
            parse_refspec("^refs/heads/tmp").expect("test operation should succeed"),
        ];
        assert_eq!(
            plan_fetch_ref_updates(&refs, &refspecs, false).expect("test operation should succeed"),
            vec![FetchRefUpdate {
                src: "refs/heads/main".into(),
                dst: Some("refs/remotes/origin/main".into()),
                oid: main,
                not_for_merge: false,
            }]
        );
    }

    #[test]
    fn fetch_planner_autofollows_tags_and_builds_fetch_head_records() {
        let commit = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let refs = vec![
            RefAdvertisement {
                oid: commit.clone(),
                name: "refs/heads/main".into(),
                capabilities: Vec::new(),
            },
            RefAdvertisement {
                oid: commit.clone(),
                name: "refs/tags/v1".into(),
                capabilities: Vec::new(),
            },
        ];
        let refspecs = vec![
            parse_refspec("refs/heads/main:refs/heads/main")
                .expect("test operation should succeed"),
        ];
        let updates =
            plan_fetch_ref_updates(&refs, &refspecs, true).expect("test operation should succeed");
        assert_eq!(
            updates,
            vec![
                FetchRefUpdate {
                    src: "refs/heads/main".into(),
                    dst: Some("refs/heads/main".into()),
                    oid: commit.clone(),
                    not_for_merge: false,
                },
                FetchRefUpdate {
                    src: "refs/tags/v1".into(),
                    dst: Some("refs/tags/v1".into()),
                    oid: commit.clone(),
                    not_for_merge: true,
                },
            ]
        );
        assert_eq!(
            fetch_ref_updates_to_fetch_head(&updates, "../bundle.bdl")
                .expect("test operation should succeed"),
            vec![
                FetchHeadRecord {
                    oid: commit.clone(),
                    not_for_merge: false,
                    description: "branch 'main' of ../bundle.bdl".into(),
                },
                FetchHeadRecord {
                    oid: commit,
                    not_for_merge: true,
                    description: "tag 'v1' of ../bundle.bdl".into(),
                },
            ]
        );
    }

    #[test]
    fn fetch_planner_rejects_missing_or_sourceless_refspecs() {
        let refs = vec![RefAdvertisement {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            name: "refs/heads/main".into(),
            capabilities: Vec::new(),
        }];
        assert!(
            plan_fetch_ref_updates(
                &refs,
                &[parse_refspec("refs/heads/missing").expect("test operation should succeed")],
                false
            )
            .is_err()
        );
        assert!(
            plan_fetch_ref_updates(
                &refs,
                &[parse_refspec(":refs/heads/main").expect("test operation should succeed")],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn push_planner_builds_create_update_delete_and_matching_commands() {
        let old = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let new = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
        let local_refs = vec![
            PushSourceRef {
                oid: new.clone(),
                name: "refs/heads/main".into(),
            },
            PushSourceRef {
                oid: new.clone(),
                name: "refs/heads/new".into(),
            },
        ];
        let remote_refs = vec![
            RefAdvertisement {
                oid: old.clone(),
                name: "refs/heads/main".into(),
                capabilities: Vec::new(),
            },
            RefAdvertisement {
                oid: old.clone(),
                name: "refs/heads/old".into(),
                capabilities: Vec::new(),
            },
        ];

        assert_eq!(
            plan_push_commands(
                ObjectFormat::Sha1,
                &local_refs,
                &remote_refs,
                &[parse_refspec("refs/heads/main").expect("test operation should succeed")],
            )
            .expect("test operation should succeed"),
            vec![ReceivePackCommand {
                old_id: old.clone(),
                new_id: new.clone(),
                name: "refs/heads/main".into(),
            }]
        );
        assert_eq!(
            plan_push_commands(
                ObjectFormat::Sha1,
                &local_refs,
                &remote_refs,
                &[parse_refspec("refs/heads/new:refs/heads/new")
                    .expect("test operation should succeed")],
            )
            .expect("test operation should succeed"),
            vec![ReceivePackCommand {
                old_id: zero.clone(),
                new_id: new.clone(),
                name: "refs/heads/new".into(),
            }]
        );
        assert_eq!(
            plan_push_commands(
                ObjectFormat::Sha1,
                &local_refs,
                &remote_refs,
                &[parse_refspec(":refs/heads/old").expect("test operation should succeed")],
            )
            .expect("test operation should succeed"),
            vec![ReceivePackCommand {
                old_id: old.clone(),
                new_id: zero,
                name: "refs/heads/old".into(),
            }]
        );
        assert_eq!(
            plan_push_commands(
                ObjectFormat::Sha1,
                &local_refs,
                &remote_refs,
                &[parse_refspec(":").expect("test operation should succeed")],
            )
            .expect("test operation should succeed"),
            vec![ReceivePackCommand {
                old_id: old,
                new_id: new,
                name: "refs/heads/main".into(),
            }]
        );
    }

    #[test]
    fn push_planner_builds_wildcard_commands_and_rejects_bad_refspecs() {
        let new = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
        let local_refs = vec![PushSourceRef {
            oid: new.clone(),
            name: "refs/heads/topic".into(),
        }];
        let commands = plan_push_commands(
            ObjectFormat::Sha1,
            &local_refs,
            &[],
            &[parse_refspec("refs/heads/*:refs/heads/review/*")
                .expect("test operation should succeed")],
        )
        .expect("test operation should succeed");
        assert_eq!(
            commands,
            vec![ReceivePackCommand {
                old_id: zero,
                new_id: new,
                name: "refs/heads/review/topic".into(),
            }]
        );
        assert!(
            plan_push_commands(
                ObjectFormat::Sha1,
                &local_refs,
                &[],
                &[parse_refspec("^refs/heads/topic").expect("test operation should succeed")],
            )
            .is_err()
        );
        assert!(
            plan_push_commands(
                ObjectFormat::Sha1,
                &local_refs,
                &[],
                &[parse_refspec(":refs/heads/missing").expect("test operation should succeed")],
            )
            .is_err()
        );
    }

    #[test]
    fn receive_pack_push_request_builder_negotiates_capabilities() {
        let old_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let new_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let features = ReceivePackFeatures {
            report_status_v2: true,
            atomic: true,
            ofs_delta: true,
            push_options: true,
            side_band_64k: true,
            quiet: true,
            object_format: Some(ObjectFormat::Sha1),
            ..ReceivePackFeatures::default()
        };
        let request = build_receive_pack_push_request(
            &features,
            vec![ReceivePackCommand {
                old_id,
                new_id,
                name: "refs/heads/main".into(),
            }],
            b"PACKdata".to_vec(),
            ReceivePackPushRequestOptions {
                report_status_v2: true,
                atomic: true,
                ofs_delta: true,
                side_band_64k: true,
                quiet: true,
                agent: Some("sley/0".into()),
                object_format: Some(ObjectFormat::Sha1),
                push_options: vec!["ci.skip".into()],
                ..ReceivePackPushRequestOptions::default()
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            request.commands.capabilities,
            vec![
                Capability {
                    name: "report-status-v2".into(),
                    value: None,
                },
                Capability {
                    name: "atomic".into(),
                    value: None,
                },
                Capability {
                    name: "ofs-delta".into(),
                    value: None,
                },
                Capability {
                    name: "side-band-64k".into(),
                    value: None,
                },
                Capability {
                    name: "quiet".into(),
                    value: None,
                },
                Capability {
                    name: "agent".into(),
                    value: Some("sley/0".into()),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
                Capability {
                    name: "push-options".into(),
                    value: None,
                },
            ]
        );
        assert_eq!(request.push_options, Some(vec!["ci.skip".into()]));
        validate_receive_pack_push_request_features(&features, &request)
            .expect("test operation should succeed");
    }

    #[test]
    fn receive_pack_push_request_builder_handles_delete_only_and_rejects_unadvertised() {
        let old_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
        let features = ReceivePackFeatures {
            delete_refs: true,
            ..ReceivePackFeatures::default()
        };
        let request = build_receive_pack_push_request(
            &features,
            vec![ReceivePackCommand {
                old_id,
                new_id: zero,
                name: "refs/heads/old".into(),
            }],
            Vec::new(),
            ReceivePackPushRequestOptions::default(),
        )
        .expect("test operation should succeed");
        assert_eq!(
            request.commands.capabilities,
            vec![Capability {
                name: "delete-refs".into(),
                value: None,
            }]
        );
        assert!(request.packfile.is_empty());

        assert!(
            build_receive_pack_push_request(
                &ReceivePackFeatures::default(),
                request.commands.commands.clone(),
                Vec::new(),
                ReceivePackPushRequestOptions::default(),
            )
            .is_err()
        );
        assert!(
            build_receive_pack_push_request(
                &features,
                request.commands.commands,
                b"PACK".to_vec(),
                ReceivePackPushRequestOptions::default(),
            )
            .is_err()
        );
        assert!(
            build_receive_pack_push_request(
                &features,
                Vec::new(),
                Vec::new(),
                ReceivePackPushRequestOptions {
                    push_options: vec!["ci.skip".into()],
                    ..ReceivePackPushRequestOptions::default()
                },
            )
            .is_err()
        );
    }

    #[test]
    fn smart_http_helpers_build_paths_and_content_types() {
        let sha1 = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let sha256 = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "2222222222222222222222222222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        assert_eq!(
            smart_http_info_refs_path("/repo.git/", GitService::UploadPack)
                .expect("test operation should succeed"),
            "/repo.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(
            dumb_http_info_refs_path("/repo.git/").expect("test operation should succeed"),
            "/repo.git/info/refs"
        );
        assert_eq!(
            dumb_http_alternates_path("/repo.git").expect("test operation should succeed"),
            "/repo.git/objects/info/http-alternates"
        );
        assert_eq!(
            dumb_http_packs_path("/repo.git/").expect("test operation should succeed"),
            "/repo.git/objects/info/packs"
        );
        assert_eq!(
            dumb_http_loose_object_path("/repo.git/", &sha1)
                .expect("test operation should succeed"),
            "/repo.git/objects/11/11111111111111111111111111111111111111"
        );
        assert_eq!(
            dumb_http_loose_object_path("/repo.git/", &sha256)
                .expect("test operation should succeed"),
            "/repo.git/objects/22/22222222222222222222222222222222222222222222222222222222222222"
        );
        assert_eq!(
            dumb_http_pack_file_path("/repo.git/", &sha1).expect("test operation should succeed"),
            "/repo.git/objects/pack/pack-1111111111111111111111111111111111111111.pack"
        );
        assert_eq!(
            dumb_http_pack_index_path("/repo.git/", &sha1).expect("test operation should succeed"),
            "/repo.git/objects/pack/pack-1111111111111111111111111111111111111111.idx"
        );
        assert_eq!(
            smart_http_rpc_path("/repo.git", GitService::ReceivePack)
                .expect("test operation should succeed"),
            "/repo.git/git-receive-pack"
        );
        assert_eq!(
            smart_http_advertisement_content_type(GitService::UploadPack)
                .expect("test operation should succeed"),
            "application/x-git-upload-pack-advertisement"
        );
        assert_eq!(
            smart_http_rpc_request_content_type(GitService::UploadPack)
                .expect("test operation should succeed"),
            "application/x-git-upload-pack-request"
        );
        assert_eq!(
            smart_http_rpc_result_content_type(GitService::ReceivePack)
                .expect("test operation should succeed"),
            "application/x-git-receive-pack-result"
        );
        assert_eq!(
            parse_smart_http_advertisement_content_type(
                "Application/X-Git-Upload-Pack-Advertisement"
            )
            .expect("test operation should succeed"),
            GitService::UploadPack
        );
        assert_eq!(
            parse_smart_http_rpc_request_content_type("application/x-git-receive-pack-request")
                .expect("test operation should succeed"),
            GitService::ReceivePack
        );
        assert_eq!(
            parse_smart_http_rpc_result_content_type("application/x-git-upload-pack-result")
                .expect("test operation should succeed"),
            GitService::UploadPack
        );
    }

    #[test]
    fn smart_http_helpers_reject_invalid_services_paths_and_content_types() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        assert!(smart_http_info_refs_path("repo.git", GitService::UploadPack).is_err());
        assert!(smart_http_rpc_path("/repo.git?x=1", GitService::UploadPack).is_err());
        assert!(dumb_http_info_refs_path("repo.git").is_err());
        assert!(dumb_http_alternates_path("/repo.git#fragment").is_err());
        assert!(dumb_http_packs_path("/repo.git?query").is_err());
        assert!(dumb_http_loose_object_path("repo.git", &oid).is_err());
        assert!(dumb_http_pack_file_path("/repo.git#fragment", &oid).is_err());
        assert!(dumb_http_pack_index_path("/repo.git?query", &oid).is_err());
        assert!(smart_http_info_refs_path("/repo.git", GitService::UploadArchive).is_err());
        assert!(smart_http_advertisement_content_type(GitService::UploadArchive).is_err());
        assert!(
            parse_smart_http_advertisement_content_type(
                "application/x-git-upload-archive-advertisement"
            )
            .is_err()
        );
        assert!(
            parse_smart_http_rpc_request_content_type("application/x-git-upload-pack-result")
                .is_err()
        );
        assert!(
            parse_smart_http_rpc_result_content_type(
                "application/x-git-receive-pack-result; charset=utf-8"
            )
            .is_err()
        );
    }

    #[test]
    fn sideband_packets_parse_and_encode_channels() {
        let payloads = vec![
            b"\x01PACK bytes".to_vec(),
            b"\x02counting objects\n".to_vec(),
            b"\x03fatal error\n".to_vec(),
        ];
        let packets = parse_sideband_packets(&payloads).expect("test operation should succeed");
        assert_eq!(
            packets,
            vec![
                SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"PACK bytes".to_vec(),
                },
                SideBandPacket {
                    channel: SideBandChannel::Progress,
                    data: b"counting objects\n".to_vec(),
                },
                SideBandPacket {
                    channel: SideBandChannel::Fatal,
                    data: b"fatal error\n".to_vec(),
                },
            ]
        );
        assert_eq!(
            encode_sideband_packets(&packets).expect("test operation should succeed"),
            payloads
        );
    }

    #[test]
    fn sideband_stream_parses_encodes_and_demuxes_packets() {
        let frames = vec![
            PktLineFrame::Data(vec![1, b'P', b'A']),
            PktLineFrame::Data(vec![2, b'c', b'o', b'u', b'n', b't', b'\n']),
            PktLineFrame::Data(vec![1, b'C', b'K']),
            PktLineFrame::Flush,
        ];
        let packets = parse_sideband_stream(&frames).expect("test operation should succeed");
        assert_eq!(
            packets,
            vec![
                SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"PA".to_vec(),
                },
                SideBandPacket {
                    channel: SideBandChannel::Progress,
                    data: b"count\n".to_vec(),
                },
                SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"CK".to_vec(),
                },
            ]
        );
        assert_eq!(
            encode_sideband_stream(&packets).expect("test operation should succeed"),
            frames
        );
        assert_eq!(
            demux_sideband_stream(&frames).expect("test operation should succeed"),
            SideBandDemux {
                data: b"PACK".to_vec(),
                progress: vec![b"count\n".to_vec()],
            }
        );
    }

    #[test]
    fn sideband_stream_reads_and_writes_until_flush() {
        let packets = vec![
            SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"PACK".to_vec(),
            },
            SideBandPacket {
                channel: SideBandChannel::Progress,
                data: b"done\n".to_vec(),
            },
        ];
        let mut encoded = Vec::new();
        write_sideband_stream(&mut encoded, &packets).expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_sideband_stream(&mut input).expect("test operation should succeed"),
            packets
        );
        assert_eq!(input, b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_and_demux_sideband_stream(&mut input).expect("test operation should succeed"),
            SideBandDemux {
                data: b"PACK".to_vec(),
                progress: vec![b"done\n".to_vec()],
            }
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn sideband_packets_demux_data_and_progress() {
        let payloads = vec![
            b"\x01PACK".to_vec(),
            b"\x02counting objects\n".to_vec(),
            b"\x01 bytes".to_vec(),
            b"\x02done\n".to_vec(),
        ];
        assert_eq!(
            parse_and_demux_sideband_packets(&payloads).expect("test operation should succeed"),
            SideBandDemux {
                data: b"PACK bytes".to_vec(),
                progress: vec![b"counting objects\n".to_vec(), b"done\n".to_vec()],
            }
        );
    }

    #[test]
    fn sideband_packets_reject_bad_channels_and_oversize_payloads() {
        assert!(parse_sideband_packet(b"").is_err());
        assert!(parse_sideband_packet(b"\x04bad").is_err());
        assert!(
            parse_sideband_stream(&[PktLineFrame::Data(vec![1, b'P', b'A', b'C', b'K'])]).is_err()
        );
        assert!(parse_sideband_stream(&[PktLineFrame::Delimiter, PktLineFrame::Flush]).is_err());
        assert!(
            parse_sideband_stream(&[
                PktLineFrame::Data(vec![1, b'P', b'A']),
                PktLineFrame::Flush,
                PktLineFrame::Data(vec![1, b'C', b'K']),
            ])
            .is_err()
        );
        assert!(
            parse_sideband_stream(&[
                PktLineFrame::Data(vec![1, b'P', b'A']),
                PktLineFrame::Data(b"\x04bad".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: vec![0; PKT_LINE_MAX_PAYLOAD_LEN],
            })
            .is_err()
        );
        assert!(
            demux_sideband_packets(&[SideBandPacket {
                channel: SideBandChannel::Fatal,
                data: b"remote died\n".to_vec(),
            }])
            .is_err()
        );
    }

    #[test]
    fn upload_archive_request_parses_and_encodes_arguments() {
        let frames = vec![
            PktLineFrame::Data(b"argument --format=tar\n".to_vec()),
            PktLineFrame::Data(b"argument HEAD:dir with spaces\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let request = parse_upload_archive_request(&frames).expect("test operation should succeed");
        assert_eq!(
            request,
            UploadArchiveRequest {
                arguments: vec!["--format=tar".into(), "HEAD:dir with spaces".into()],
            }
        );
        assert_eq!(
            encode_upload_archive_request(&request).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn upload_archive_request_streams_round_trip() {
        let request = UploadArchiveRequest {
            arguments: vec!["--prefix=src/".into(), "main".into()],
        };
        let mut encoded = Vec::new();
        write_upload_archive_request(&mut encoded, &request)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_archive_request(&mut input).expect("test operation should succeed"),
            request
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn upload_archive_request_rejects_malformed_streams() {
        assert!(parse_upload_archive_request(&[PktLineFrame::Flush]).is_err());
        assert!(
            parse_upload_archive_request(&[
                PktLineFrame::Data(b"--format=tar\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            parse_upload_archive_request(&[
                PktLineFrame::Data(b"argument HEAD\n".to_vec()),
                PktLineFrame::Delimiter,
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            encode_upload_archive_request(&UploadArchiveRequest {
                arguments: vec!["bad\narg".into()],
            })
            .is_err()
        );
    }

    #[test]
    fn upload_archive_response_parses_ack_sideband_and_nack() {
        let ack_frames = vec![
            PktLineFrame::Data(b"ACK\n".to_vec()),
            PktLineFrame::Data(b"\x01tar bytes".to_vec()),
            PktLineFrame::Data(b"\x02progress\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let response =
            parse_upload_archive_response(&ack_frames).expect("test operation should succeed");
        assert_eq!(
            response,
            UploadArchiveResponse::Ack {
                sideband: vec![
                    SideBandPacket {
                        channel: SideBandChannel::Data,
                        data: b"tar bytes".to_vec(),
                    },
                    SideBandPacket {
                        channel: SideBandChannel::Progress,
                        data: b"progress\n".to_vec(),
                    },
                ],
            }
        );
        assert_eq!(
            encode_upload_archive_response(&response).expect("test operation should succeed"),
            ack_frames
        );
        assert_eq!(
            demux_upload_archive_response(&response).expect("test operation should succeed"),
            SideBandDemux {
                data: b"tar bytes".to_vec(),
                progress: vec![b"progress\n".to_vec()],
            }
        );

        let nack = UploadArchiveResponse::Nack {
            message: "unreachable tree".into(),
        };
        let nack_frames = vec![
            PktLineFrame::Data(b"NACK unreachable tree\n".to_vec()),
            PktLineFrame::Flush,
        ];
        assert_eq!(
            parse_upload_archive_response(&nack_frames).expect("test operation should succeed"),
            nack
        );
        assert_eq!(
            encode_upload_archive_response(&nack).expect("test operation should succeed"),
            nack_frames
        );
        assert!(demux_upload_archive_response(&nack).is_err());
    }

    #[test]
    fn upload_archive_response_streams_round_trip() {
        let response = UploadArchiveResponse::Ack {
            sideband: vec![SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"tar bytes".to_vec(),
            }],
        };
        let mut encoded = Vec::new();
        write_upload_archive_response(&mut encoded, &response)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_archive_response(&mut input).expect("test operation should succeed"),
            response
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn upload_archive_response_rejects_malformed_streams() {
        assert!(parse_upload_archive_response(&[]).is_err());
        assert!(
            parse_upload_archive_response(&[
                PktLineFrame::Data(b"ACK\n".to_vec()),
                PktLineFrame::Flush,
                PktLineFrame::Data(b"\x01tail".to_vec()),
            ])
            .is_err()
        );
        assert!(
            parse_upload_archive_response(&[
                PktLineFrame::Data(b"NACK\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            parse_upload_archive_response(&[
                PktLineFrame::Data(b"NACK denied\n".to_vec()),
                PktLineFrame::Data(b"\x02extra\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            encode_upload_archive_response(&UploadArchiveResponse::Nack {
                message: "bad\nmessage".into(),
            })
            .is_err()
        );
    }

    #[test]
    fn capabilities_parse_and_encode_tokens() {
        let capabilities = parse_capabilities(
            b"multi_ack thin-pack agent=git/2.54.0 symref=HEAD:refs/heads/main\n",
        )
        .expect("test operation should succeed");
        assert_eq!(
            capabilities,
            vec![
                Capability {
                    name: "multi_ack".into(),
                    value: None,
                },
                Capability {
                    name: "thin-pack".into(),
                    value: None,
                },
                Capability {
                    name: "agent".into(),
                    value: Some("git/2.54.0".into()),
                },
                Capability {
                    name: "symref".into(),
                    value: Some("HEAD:refs/heads/main".into()),
                },
            ]
        );
        assert_eq!(
            encode_capabilities(&capabilities).expect("test operation should succeed"),
            b"multi_ack thin-pack agent=git/2.54.0 symref=HEAD:refs/heads/main"
        );
    }

    #[test]
    fn capabilities_reject_empty_or_delimited_fields() {
        assert!(parse_capabilities(b"multi_ack  thin-pack").is_err());
        assert!(parse_capabilities(b"agent=").is_err());
        assert!(parse_capabilities(b"symref=HEAD:refs/heads/main\nbad").is_err());
        assert!(
            encode_capabilities(&[Capability {
                name: "bad name".into(),
                value: None,
            }])
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_object_format_uses_capability_or_defaults_to_sha1() {
        assert_eq!(
            protocol_v2_object_format(&[]).expect("test operation should succeed"),
            ObjectFormat::Sha1
        );
        assert_eq!(
            protocol_v2_object_format(&[Capability {
                name: "object-format".into(),
                value: Some("sha256".into()),
            }])
            .expect("test operation should succeed"),
            ObjectFormat::Sha256
        );
        assert!(
            protocol_v2_object_format(&[Capability {
                name: "object-format".into(),
                value: None,
            }])
            .is_err()
        );
        assert!(
            protocol_v2_object_format(&[
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha256".into()),
                },
            ])
            .is_err()
        );
        assert!(
            protocol_v2_object_format(&[Capability {
                name: "object-format".into(),
                value: Some("unknown".into()),
            }])
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_command_request_capabilities_validate_against_handshake() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "fetch".into(),
                    value: Some("shallow filter".into()),
                },
                Capability {
                    name: "agent".into(),
                    value: Some("sley/0".into()),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ],
        };
        validate_protocol_v2_command_request_capabilities(
            &handshake,
            &ProtocolV2CommandRequest {
                command: "fetch".into(),
                capabilities: vec![
                    Capability {
                        name: "agent".into(),
                        value: Some("client/1".into()),
                    },
                    Capability {
                        name: "object-format".into(),
                        value: Some("sha1".into()),
                    },
                ],
                arguments: Vec::new(),
            },
        )
        .expect("test operation should succeed");
        assert!(
            validate_protocol_v2_command_request_capabilities(
                &handshake,
                &ProtocolV2CommandRequest {
                    command: "ls-refs".into(),
                    capabilities: Vec::new(),
                    arguments: Vec::new(),
                },
            )
            .is_err()
        );
        assert!(
            validate_protocol_v2_command_request_capabilities(
                &handshake,
                &ProtocolV2CommandRequest {
                    command: "fetch".into(),
                    capabilities: vec![Capability {
                        name: "server-option".into(),
                        value: None,
                    }],
                    arguments: Vec::new(),
                },
            )
            .is_err()
        );
        assert!(
            validate_protocol_v2_command_request_capabilities(
                &handshake,
                &ProtocolV2CommandRequest {
                    command: "fetch".into(),
                    capabilities: vec![Capability {
                        name: "object-format".into(),
                        value: Some("sha256".into()),
                    }],
                    arguments: Vec::new(),
                },
            )
            .is_err()
        );
        assert!(
            validate_protocol_v2_command_request_capabilities(
                &handshake,
                &ProtocolV2CommandRequest {
                    command: "fetch".into(),
                    capabilities: vec![Capability {
                        name: "agent".into(),
                        value: None,
                    }],
                    arguments: Vec::new(),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_command_options_parse_and_encode_known_capabilities() {
        let capabilities = vec![
            Capability {
                name: "agent".into(),
                value: Some("sley/0".into()),
            },
            Capability {
                name: "object-format".into(),
                value: Some("sha256".into()),
            },
            Capability {
                name: "server-option".into(),
                value: Some("trace=true".into()),
            },
            Capability {
                name: "server-option".into(),
                value: Some("region=west".into()),
            },
            Capability {
                name: "session-id".into(),
                value: Some("abc123".into()),
            },
        ];
        let options = parse_protocol_v2_command_options(&capabilities)
            .expect("test operation should succeed");
        assert_eq!(
            options,
            ProtocolV2CommandOptions {
                agent: Some("sley/0".into()),
                object_format: Some(ObjectFormat::Sha256),
                server_options: vec!["trace=true".into(), "region=west".into()],
                extra: vec![Capability {
                    name: "session-id".into(),
                    value: Some("abc123".into()),
                }],
            }
        );
        assert_eq!(
            encode_protocol_v2_command_options(&options).expect("test operation should succeed"),
            capabilities
        );
    }

    #[test]
    fn protocol_v2_command_options_reject_malformed_known_capabilities() {
        assert!(
            parse_protocol_v2_command_options(&[
                Capability {
                    name: "agent".into(),
                    value: Some("sley/0".into()),
                },
                Capability {
                    name: "agent".into(),
                    value: Some("sley/1".into()),
                },
            ])
            .is_err()
        );
        assert!(
            parse_protocol_v2_command_options(&[Capability {
                name: "object-format".into(),
                value: Some("sha512".into()),
            }])
            .is_err()
        );
        assert!(
            parse_protocol_v2_command_options(&[Capability {
                name: "server-option".into(),
                value: None,
            }])
            .is_err()
        );
        assert!(
            encode_protocol_v2_command_options(&ProtocolV2CommandOptions {
                extra: vec![Capability {
                    name: "server-option".into(),
                    value: Some("trace=true".into()),
                }],
                ..ProtocolV2CommandOptions::default()
            })
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_ls_refs_features_parse_and_encode_advertisement() {
        let capabilities = vec![Capability {
            name: "ls-refs".into(),
            value: Some("unborn custom".into()),
        }];
        let features = parse_protocol_v2_ls_refs_features(&capabilities)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(
            features,
            ProtocolV2LsRefsFeatures {
                unborn: true,
                unknown: vec!["custom".into()],
            }
        );
        assert_eq!(
            encode_protocol_v2_ls_refs_capability(&features)
                .expect("test operation should succeed"),
            capabilities[0]
        );
        assert_eq!(
            parse_protocol_v2_ls_refs_features(&[Capability {
                name: "ls-refs".into(),
                value: None,
            }])
            .expect("test operation should succeed")
            .expect("test operation should succeed"),
            ProtocolV2LsRefsFeatures::default()
        );
        assert!(
            parse_protocol_v2_ls_refs_features(&[Capability {
                name: "fetch".into(),
                value: Some("filter".into()),
            }])
            .expect("test operation should succeed")
            .is_none()
        );
    }

    #[test]
    fn protocol_v2_ls_refs_features_reject_malformed_advertisements() {
        assert!(
            parse_protocol_v2_ls_refs_features(&[
                Capability {
                    name: "ls-refs".into(),
                    value: None,
                },
                Capability {
                    name: "ls-refs".into(),
                    value: None,
                },
            ])
            .is_err()
        );
        assert!(
            parse_protocol_v2_ls_refs_features(&[Capability {
                name: "ls-refs".into(),
                value: Some("unborn  custom".into()),
            }])
            .is_err()
        );
        assert!(
            encode_protocol_v2_ls_refs_capability(&ProtocolV2LsRefsFeatures {
                unknown: vec!["unborn".into()],
                ..ProtocolV2LsRefsFeatures::default()
            })
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_ls_refs_command_request_validates_unborn_feature() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![Capability {
                name: "ls-refs".into(),
                value: Some("unborn".into()),
            }],
        };
        let request = ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: Vec::new(),
            arguments: vec![b"unborn".to_vec(), b"ref-prefix HEAD".to_vec()],
        };
        let parsed = validate_protocol_v2_ls_refs_command_request(&handshake, &request)
            .expect("test operation should succeed");
        assert!(parsed.unborn);
        assert_eq!(parsed.ref_prefixes, vec!["HEAD"]);

        let blocked = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![Capability {
                name: "ls-refs".into(),
                value: None,
            }],
        };
        assert!(validate_protocol_v2_ls_refs_command_request(&blocked, &request).is_err());
    }

    #[test]
    fn protocol_v2_fetch_features_parse_and_encode_advertisement() {
        let capabilities = vec![Capability {
            name: "fetch".into(),
            value: Some(
                "shallow wait-for-done filter ref-in-want sideband-all packfile-uris custom".into(),
            ),
        }];
        let features = parse_protocol_v2_fetch_features(&capabilities)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(
            features,
            ProtocolV2FetchFeatures {
                shallow: true,
                wait_for_done: true,
                filter: true,
                ref_in_want: true,
                sideband_all: true,
                packfile_uris: true,
                unknown: vec!["custom".into()],
            }
        );
        assert_eq!(
            encode_protocol_v2_fetch_capability(&features).expect("test operation should succeed"),
            capabilities[0]
        );
        assert_eq!(
            parse_protocol_v2_fetch_features(&[Capability {
                name: "fetch".into(),
                value: None,
            }])
            .expect("test operation should succeed")
            .expect("test operation should succeed"),
            ProtocolV2FetchFeatures::default()
        );
        assert!(
            parse_protocol_v2_fetch_features(&[])
                .expect("test operation should succeed")
                .is_none()
        );
    }

    #[test]
    fn protocol_v2_fetch_features_reject_malformed_advertisements() {
        assert!(
            parse_protocol_v2_fetch_features(&[
                Capability {
                    name: "fetch".into(),
                    value: None,
                },
                Capability {
                    name: "fetch".into(),
                    value: None,
                },
            ])
            .is_err()
        );
        assert!(
            parse_protocol_v2_fetch_features(&[Capability {
                name: "fetch".into(),
                value: Some("filter  shallow".into()),
            }])
            .is_err()
        );
        assert!(
            encode_protocol_v2_fetch_capability(&ProtocolV2FetchFeatures {
                unknown: vec!["filter".into()],
                ..ProtocolV2FetchFeatures::default()
            })
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_fetch_request_features_validate_feature_gated_arguments() {
        let features = ProtocolV2FetchFeatures {
            shallow: true,
            wait_for_done: true,
            filter: true,
            ref_in_want: true,
            sideband_all: true,
            packfile_uris: true,
            unknown: Vec::new(),
        };
        validate_protocol_v2_fetch_request_features(
            &features,
            &ProtocolV2FetchRequest {
                want_refs: vec!["refs/heads/main".into()],
                shallow: vec![
                    ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                ],
                deepen: Some(1),
                filter: Some("blob:none".into()),
                packfile_uris: Some("https".into()),
                sideband_all: true,
                wait_for_done: true,
                ..ProtocolV2FetchRequest::default()
            },
        )
        .expect("test operation should succeed");

        let request = ProtocolV2FetchRequest {
            want_refs: vec!["refs/heads/main".into()],
            filter: Some("blob:none".into()),
            sideband_all: true,
            ..ProtocolV2FetchRequest::default()
        };
        assert!(
            validate_protocol_v2_fetch_request_features(
                &ProtocolV2FetchFeatures::default(),
                &request,
            )
            .is_err()
        );
        assert!(
            validate_protocol_v2_fetch_request_features(
                &ProtocolV2FetchFeatures {
                    ref_in_want: true,
                    filter: true,
                    ..ProtocolV2FetchFeatures::default()
                },
                &request,
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_fetch_command_request_validates_against_handshake_features() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "fetch".into(),
                    value: Some("filter ref-in-want".into()),
                },
                Capability {
                    name: "agent".into(),
                    value: Some("sley/0".into()),
                },
            ],
        };
        let request = ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: vec![Capability {
                name: "agent".into(),
                value: Some("client/1".into()),
            }],
            arguments: vec![
                b"want-ref refs/heads/main".to_vec(),
                b"filter blob:none".to_vec(),
            ],
        };
        let fetch =
            validate_protocol_v2_fetch_command_request(&handshake, ObjectFormat::Sha1, &request)
                .expect("test operation should succeed");
        assert_eq!(fetch.want_refs, vec!["refs/heads/main"]);
        assert_eq!(fetch.filter.as_deref(), Some("blob:none"));

        let mut bad = request.clone();
        bad.arguments.push(b"sideband-all".to_vec());
        assert!(
            validate_protocol_v2_fetch_command_request(&handshake, ObjectFormat::Sha1, &bad)
                .is_err()
        );
    }

    #[test]
    fn protocol_v2_object_info_request_parses_encodes_and_validates() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let request = ProtocolV2CommandRequest {
            command: "object-info".into(),
            capabilities: Vec::new(),
            arguments: vec![
                b"size".to_vec(),
                b"oid 1111111111111111111111111111111111111111".to_vec(),
            ],
        };
        let parsed =
            ProtocolV2ObjectInfoRequest::from_command_request(ObjectFormat::Sha1, &request)
                .expect("test operation should succeed");
        assert_eq!(
            parsed,
            ProtocolV2ObjectInfoRequest {
                size: true,
                oids: vec![oid],
            }
        );
        assert_eq!(
            parsed
                .to_command_request()
                .expect("test operation should succeed"),
            request
        );

        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![Capability {
                name: "object-info".into(),
                value: None,
            }],
        };
        assert_eq!(
            validate_protocol_v2_object_info_command_request(
                &handshake,
                ObjectFormat::Sha1,
                &request,
            )
            .expect("test operation should succeed"),
            parsed
        );
    }

    #[test]
    fn protocol_v2_object_info_request_streams_round_trip() {
        let request = ProtocolV2ObjectInfoRequest {
            size: true,
            oids: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
            ],
        };
        let mut encoded = Vec::new();
        write_protocol_v2_object_info_request(&mut encoded, &request)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_object_info_request(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            request
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_object_info_request_rejects_malformed_arguments() {
        assert!(
            ProtocolV2ObjectInfoRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "object-info".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"oid 1111111111111111111111111111111111111111".to_vec()],
                },
            )
            .is_err()
        );
        assert!(
            ProtocolV2ObjectInfoRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "object-info".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"size".to_vec(), b"size".to_vec()],
                },
            )
            .is_err()
        );
        assert!(
            ProtocolV2ObjectInfoRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "object-info".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"size".to_vec()],
                },
            )
            .is_err()
        );
        assert!(
            ProtocolV2ObjectInfoRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "object-info".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"size".to_vec(), b"oid not-an-oid".to_vec()],
                },
            )
            .is_err()
        );
        assert!(
            validate_protocol_v2_object_info_command_request(
                &TransportHandshake {
                    protocol: ProtocolVersion::V2,
                    capabilities: Vec::new(),
                },
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "object-info".into(),
                    capabilities: Vec::new(),
                    arguments: vec![
                        b"size".to_vec(),
                        b"oid 1111111111111111111111111111111111111111".to_vec(),
                    ],
                },
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_command_request_classifies_known_and_unknown_commands() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "ls-refs".into(),
                    value: Some("unborn".into()),
                },
                Capability {
                    name: "fetch".into(),
                    value: Some("filter ref-in-want".into()),
                },
                Capability {
                    name: "object-info".into(),
                    value: None,
                },
                Capability {
                    name: "server-option".into(),
                    value: None,
                },
                Capability {
                    name: "server-info".into(),
                    value: Some("custom".into()),
                },
            ],
        };
        assert_eq!(
            classify_protocol_v2_command_request(
                &handshake,
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "ls-refs".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"unborn".to_vec()],
                },
            )
            .expect("test operation should succeed"),
            ProtocolV2Command::LsRefs(ProtocolV2LsRefsRequest {
                unborn: true,
                ..ProtocolV2LsRefsRequest::default()
            })
        );
        assert_eq!(
            classify_protocol_v2_command_request(
                &handshake,
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "fetch".into(),
                    capabilities: Vec::new(),
                    arguments: vec![
                        b"want-ref refs/heads/main".to_vec(),
                        b"filter blob:none".to_vec(),
                    ],
                },
            )
            .expect("test operation should succeed"),
            ProtocolV2Command::Fetch(ProtocolV2FetchRequest {
                want_refs: vec!["refs/heads/main".into()],
                filter: Some("blob:none".into()),
                ..ProtocolV2FetchRequest::default()
            })
        );
        assert_eq!(
            classify_protocol_v2_command_request(
                &handshake,
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "object-info".into(),
                    capabilities: Vec::new(),
                    arguments: vec![
                        b"size".to_vec(),
                        b"oid 1111111111111111111111111111111111111111".to_vec(),
                    ],
                },
            )
            .expect("test operation should succeed"),
            ProtocolV2Command::ObjectInfo(ProtocolV2ObjectInfoRequest {
                size: true,
                oids: vec![
                    ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed")
                ],
            })
        );

        let unknown = ProtocolV2CommandRequest {
            command: "server-info".into(),
            capabilities: vec![Capability {
                name: "server-option".into(),
                value: Some("trace=true".into()),
            }],
            arguments: Vec::new(),
        };
        assert_eq!(
            classify_protocol_v2_command_request(&handshake, ObjectFormat::Sha1, &unknown)
                .expect("test operation should succeed"),
            ProtocolV2Command::Unknown(unknown)
        );
        assert!(
            classify_protocol_v2_command_request(
                &handshake,
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "not-advertised".into(),
                    capabilities: Vec::new(),
                    arguments: Vec::new(),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_session_request_classifies_streamed_command_and_done() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "ls-refs".into(),
                    value: Some("unborn".into()),
                },
                Capability {
                    name: "fetch".into(),
                    value: Some("filter ref-in-want".into()),
                },
            ],
        };
        let command = ProtocolV2Request::Command(ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: Vec::new(),
            arguments: vec![b"unborn".to_vec()],
        });
        assert_eq!(
            classify_protocol_v2_request(&handshake, ObjectFormat::Sha1, &command)
                .expect("test operation should succeed"),
            ProtocolV2SessionRequest::Command(ProtocolV2Command::LsRefs(ProtocolV2LsRefsRequest {
                unborn: true,
                ..ProtocolV2LsRefsRequest::default()
            }))
        );
        assert_eq!(
            classify_protocol_v2_request(&handshake, ObjectFormat::Sha1, &ProtocolV2Request::Done)
                .expect("test operation should succeed"),
            ProtocolV2SessionRequest::Done
        );

        let mut encoded = Vec::new();
        write_protocol_v2_request(&mut encoded, &command).expect("test operation should succeed");
        write_protocol_v2_request(&mut encoded, &ProtocolV2Request::Done)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_session_request(&handshake, ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            ProtocolV2SessionRequest::Command(ProtocolV2Command::LsRefs(ProtocolV2LsRefsRequest {
                unborn: true,
                ..ProtocolV2LsRefsRequest::default()
            }))
        );
        assert_eq!(
            read_protocol_v2_session_request(&handshake, ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            ProtocolV2SessionRequest::Done
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn advertised_ref_parses_first_v0_capability_line() {
        let payload =
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 HEAD\0multi_ack symref=HEAD:refs/heads/main\n";
        let advertisement = parse_ref_advertisement(ObjectFormat::Sha1, payload)
            .expect("test operation should succeed");
        assert_eq!(
            advertisement.oid,
            ObjectId::from_hex(
                ObjectFormat::Sha1,
                "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
            )
            .expect("test operation should succeed")
        );
        assert_eq!(advertisement.name, "HEAD");
        assert_eq!(
            advertisement.capabilities,
            vec![
                Capability {
                    name: "multi_ack".into(),
                    value: None,
                },
                Capability {
                    name: "symref".into(),
                    value: Some("HEAD:refs/heads/main".into()),
                },
            ]
        );
    }

    #[test]
    fn advertised_ref_parses_lines_without_capabilities() {
        let advertisement = parse_ref_advertisement(
            ObjectFormat::Sha1,
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 refs/heads/main\n",
        )
        .expect("test operation should succeed");
        assert_eq!(advertisement.name, "refs/heads/main");
        assert!(advertisement.capabilities.is_empty());
    }

    #[test]
    fn advertised_ref_rejects_malformed_payloads() {
        assert!(
            parse_ref_advertisement(ObjectFormat::Sha1, b"not-an-oid refs/heads/main\n").is_err()
        );
        assert!(
            parse_ref_advertisement(
                ObjectFormat::Sha1,
                b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n"
            )
            .is_err()
        );
    }

    #[test]
    fn advertised_refs_parse_and_encode_stream() {
        let main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let feature = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 HEAD\0multi_ack thin-pack agent=git/2.54.0\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"2222222222222222222222222222222222222222 refs/heads/feature\n".to_vec(),
            ),
            PktLineFrame::Flush,
        ];
        let advertisements = parse_ref_advertisements(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            advertisements,
            vec![
                RefAdvertisement {
                    oid: main,
                    name: "HEAD".into(),
                    capabilities: vec![
                        Capability {
                            name: "multi_ack".into(),
                            value: None,
                        },
                        Capability {
                            name: "thin-pack".into(),
                            value: None,
                        },
                        Capability {
                            name: "agent".into(),
                            value: Some("git/2.54.0".into()),
                        },
                    ],
                },
                RefAdvertisement {
                    oid: feature,
                    name: "refs/heads/feature".into(),
                    capabilities: Vec::new(),
                },
            ]
        );
        assert_eq!(
            encode_ref_advertisements(&advertisements).expect("test operation should succeed"),
            frames
        );
        assert_eq!(
            parse_ref_advertisements(ObjectFormat::Sha1, &[PktLineFrame::Flush])
                .expect("test operation should succeed"),
            Vec::<RefAdvertisement>::new()
        );
    }

    #[test]
    fn advertised_ref_set_parses_v1_version_refs_and_shallow() {
        let main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let feature = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"version 1\n".to_vec()),
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 HEAD\0multi_ack symref=HEAD:refs/heads/main\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"2222222222222222222222222222222222222222 refs/heads/feature\n".to_vec(),
            ),
            PktLineFrame::Data(b"shallow 3333333333333333333333333333333333333333\n".to_vec()),
            PktLineFrame::Flush,
        ];

        let set = parse_ref_advertisement_set(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(set.protocol, ProtocolVersion::V1);
        assert_eq!(set.shallow, vec![shallow]);
        assert_eq!(
            set.refs,
            vec![
                RefAdvertisement {
                    oid: main,
                    name: "HEAD".into(),
                    capabilities: vec![
                        Capability {
                            name: "multi_ack".into(),
                            value: None,
                        },
                        Capability {
                            name: "symref".into(),
                            value: Some("HEAD:refs/heads/main".into()),
                        },
                    ],
                },
                RefAdvertisement {
                    oid: feature,
                    name: "refs/heads/feature".into(),
                    capabilities: Vec::new(),
                },
            ]
        );
        assert_eq!(
            parse_ref_advertisements(ObjectFormat::Sha1, &frames)
                .expect("test operation should succeed"),
            set.refs
        );
        assert_eq!(
            encode_ref_advertisement_set(&set).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn advertised_refs_streams_round_trip() {
        let advertisements = vec![RefAdvertisement {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            name: "HEAD".into(),
            capabilities: vec![Capability {
                name: "symref".into(),
                value: Some("HEAD:refs/heads/main".into()),
            }],
        }];
        let mut encoded = Vec::new();
        write_ref_advertisements(&mut encoded, &advertisements)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_ref_advertisements(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            advertisements
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn advertised_ref_set_streams_round_trip() {
        let set = RefAdvertisementSet {
            protocol: ProtocolVersion::V1,
            refs: vec![RefAdvertisement {
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                name: "HEAD".into(),
                capabilities: vec![Capability {
                    name: "symref".into(),
                    value: Some("HEAD:refs/heads/main".into()),
                }],
            }],
            shallow: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "2222222222222222222222222222222222222222",
                )
                .expect("test operation should succeed"),
            ],
        };
        let mut encoded = Vec::new();
        write_ref_advertisement_set(&mut encoded, &set).expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_ref_advertisement_set(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            set
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn advertised_refs_reject_malformed_streams() {
        assert!(
            parse_ref_advertisements(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),
                )],
            )
            .is_err()
        );
        assert!(
            parse_ref_advertisements(
                ObjectFormat::Sha1,
                &[PktLineFrame::Delimiter, PktLineFrame::Flush],
            )
            .is_err()
        );
        assert!(parse_ref_advertisements(
            ObjectFormat::Sha1,
            &[
                PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
                PktLineFrame::Data(
                    b"2222222222222222222222222222222222222222 refs/heads/main\0thin-pack\n"
                        .to_vec(),
                ),
                PktLineFrame::Flush,
            ],
        )
        .is_err());
        assert!(parse_ref_advertisement_set(
            ObjectFormat::Sha1,
            &[
                PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
                PktLineFrame::Data(b"version 1\n".to_vec()),
                PktLineFrame::Flush,
            ],
        )
        .is_err());
        assert!(
            parse_ref_advertisement_set(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"version 2\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_ref_advertisement_set(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"shallow 1111111111111111111111111111111111111111\n".to_vec()
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(parse_ref_advertisement_set(
            ObjectFormat::Sha1,
            &[
                PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
                PktLineFrame::Data(b"shallow not-an-oid\n".to_vec()),
                PktLineFrame::Flush,
            ],
        )
        .is_err());
        assert!(parse_ref_advertisement_set(
            ObjectFormat::Sha1,
            &[
                PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
                PktLineFrame::Data(b"shallow 2222222222222222222222222222222222222222\n".to_vec()),
                PktLineFrame::Data(
                    b"3333333333333333333333333333333333333333 refs/heads/main\n".to_vec(),
                ),
                PktLineFrame::Flush,
            ],
        )
        .is_err());
        assert!(
            encode_ref_advertisements(&[
                RefAdvertisement {
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    name: "HEAD".into(),
                    capabilities: Vec::new(),
                },
                RefAdvertisement {
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2222222222222222222222222222222222222222",
                    )
                    .expect("test operation should succeed"),
                    name: "refs/heads/main".into(),
                    capabilities: vec![Capability {
                        name: "thin-pack".into(),
                        value: None,
                    }],
                },
            ])
            .is_err()
        );
        assert!(
            encode_ref_advertisement(&RefAdvertisement {
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                name: "bad ref".into(),
                capabilities: Vec::new(),
            })
            .is_err()
        );
        assert!(
            encode_ref_advertisement_set(&RefAdvertisementSet {
                protocol: ProtocolVersion::V2,
                refs: Vec::new(),
                shallow: Vec::new(),
            })
            .is_err()
        );
        assert!(
            encode_ref_advertisement_set(&RefAdvertisementSet {
                protocol: ProtocolVersion::V0,
                refs: Vec::new(),
                shallow: vec![
                    ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed")
                ],
            })
            .is_err()
        );
    }

    #[test]
    fn dumb_http_info_refs_parse_and_encode_records() {
        let main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let tag = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let peeled = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let input = b"1111111111111111111111111111111111111111\trefs/heads/main\n2222222222222222222222222222222222222222\trefs/tags/v1.0\n3333333333333333333333333333333333333333\trefs/tags/v1.0^{}\n";

        let records = parse_dumb_http_info_refs(ObjectFormat::Sha1, input)
            .expect("test operation should succeed");
        assert_eq!(
            records,
            vec![
                DumbHttpRefRecord {
                    oid: main,
                    name: "refs/heads/main".into(),
                    peeled: false,
                },
                DumbHttpRefRecord {
                    oid: tag,
                    name: "refs/tags/v1.0".into(),
                    peeled: false,
                },
                DumbHttpRefRecord {
                    oid: peeled,
                    name: "refs/tags/v1.0".into(),
                    peeled: true,
                },
            ]
        );
        assert_eq!(
            encode_dumb_http_info_refs(&records).expect("test operation should succeed"),
            input
        );
        assert_eq!(
            parse_dumb_http_info_refs(ObjectFormat::Sha1, b"")
                .expect("test operation should succeed"),
            Vec::<DumbHttpRefRecord>::new()
        );
    }

    #[test]
    fn dumb_http_info_refs_streams_round_trip() {
        let records = vec![DumbHttpRefRecord {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            name: "refs/heads/main".into(),
            peeled: false,
        }];
        let mut encoded = Vec::new();
        write_dumb_http_info_refs(&mut encoded, &records).expect("test operation should succeed");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_dumb_http_info_refs(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            records
        );
        assert!(input.is_empty());
    }

    #[test]
    fn dumb_http_info_refs_reject_malformed_records() {
        assert!(
            parse_dumb_http_info_refs(
                ObjectFormat::Sha1,
                b"1111111111111111111111111111111111111111 refs/heads/main\n",
            )
            .is_err()
        );
        assert!(
            parse_dumb_http_info_refs(
                ObjectFormat::Sha1,
                b"1111111111111111111111111111111111111111\trefs/heads/main",
            )
            .is_err()
        );
        assert!(
            parse_dumb_http_info_refs(ObjectFormat::Sha1, b"not-an-oid\trefs/heads/main\n")
                .is_err()
        );
        assert!(
            parse_dumb_http_info_refs(
                ObjectFormat::Sha1,
                b"1111111111111111111111111111111111111111\tbad ref\n",
            )
            .is_err()
        );
        assert!(
            encode_dumb_http_info_refs(&[DumbHttpRefRecord {
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                name: "refs/tags/v1.0^{}".into(),
                peeled: false,
            }])
            .is_err()
        );
    }

    #[test]
    fn dumb_http_alternates_parse_and_encode_locations() {
        let input = b"https://example.com/base.git/objects/\n../other.git/objects/\n";
        let alternates = parse_dumb_http_alternates(input).expect("test operation should succeed");
        assert_eq!(
            alternates,
            vec![
                "https://example.com/base.git/objects/".to_string(),
                "../other.git/objects/".to_string(),
            ]
        );
        assert_eq!(
            encode_dumb_http_alternates(&alternates).expect("test operation should succeed"),
            input
        );
        assert_eq!(
            parse_dumb_http_alternates(b"").expect("test operation should succeed"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn dumb_http_alternates_streams_round_trip() {
        let alternates = vec!["https://example.com/base.git/objects/".to_string()];
        let mut encoded = Vec::new();
        write_dumb_http_alternates(&mut encoded, &alternates)
            .expect("test operation should succeed");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_dumb_http_alternates(&mut input).expect("test operation should succeed"),
            alternates
        );
        assert!(input.is_empty());
    }

    #[test]
    fn dumb_http_alternates_reject_malformed_lines() {
        assert!(parse_dumb_http_alternates(b"https://example.com/base.git/objects/").is_err());
        assert!(parse_dumb_http_alternates(b"\n").is_err());
        assert!(parse_dumb_http_alternates(b"https://example.com/base.git/objects/\r\n").is_err());
        assert!(encode_dumb_http_alternates(&["bad\nalternate".to_string()]).is_err());
    }

    #[test]
    fn dumb_http_packs_parse_and_encode_pack_records() {
        let first = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let second = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let input = b"P pack-1111111111111111111111111111111111111111.pack\nP pack-2222222222222222222222222222222222222222.pack\n";
        let records = parse_dumb_http_packs(ObjectFormat::Sha1, input)
            .expect("test operation should succeed");
        assert_eq!(
            records,
            vec![
                DumbHttpPackRecord { hash: first },
                DumbHttpPackRecord { hash: second },
            ]
        );
        assert_eq!(
            encode_dumb_http_packs(&records).expect("test operation should succeed"),
            input
        );
        assert_eq!(
            parse_dumb_http_packs(ObjectFormat::Sha1, b"").expect("test operation should succeed"),
            Vec::<DumbHttpPackRecord>::new()
        );
    }

    #[test]
    fn dumb_http_packs_streams_round_trip() {
        let records = vec![DumbHttpPackRecord {
            hash: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
        }];
        let mut encoded = Vec::new();
        write_dumb_http_packs(&mut encoded, &records).expect("test operation should succeed");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_dumb_http_packs(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            records
        );
        assert!(input.is_empty());
    }

    #[test]
    fn dumb_http_packs_reject_malformed_records() {
        assert!(
            parse_dumb_http_packs(
                ObjectFormat::Sha1,
                b"P pack-1111111111111111111111111111111111111111.pack",
            )
            .is_err()
        );
        assert!(
            parse_dumb_http_packs(
                ObjectFormat::Sha1,
                b"pack-1111111111111111111111111111111111111111.pack\n",
            )
            .is_err()
        );
        assert!(parse_dumb_http_packs(ObjectFormat::Sha1, b"P pack-not-a-hash.pack\n",).is_err());
        assert!(
            parse_dumb_http_packs(
                ObjectFormat::Sha1,
                b"P pack-1111111111111111111111111111111111111111.idx\n",
            )
            .is_err()
        );
    }

    #[test]
    fn upload_pack_features_parse_encode_and_validate_request() {
        let capabilities = vec![
            Capability {
                name: "multi_ack".into(),
                value: None,
            },
            Capability {
                name: "multi_ack_detailed".into(),
                value: None,
            },
            Capability {
                name: "no-done".into(),
                value: None,
            },
            Capability {
                name: "thin-pack".into(),
                value: None,
            },
            Capability {
                name: "side-band-64k".into(),
                value: None,
            },
            Capability {
                name: "ofs-delta".into(),
                value: None,
            },
            Capability {
                name: "shallow".into(),
                value: None,
            },
            Capability {
                name: "deepen-since".into(),
                value: None,
            },
            Capability {
                name: "deepen-not".into(),
                value: None,
            },
            Capability {
                name: "include-tag".into(),
                value: None,
            },
            Capability {
                name: "no-progress".into(),
                value: None,
            },
            Capability {
                name: "filter".into(),
                value: None,
            },
            Capability {
                name: "agent".into(),
                value: Some("git/2.54.0".into()),
            },
            Capability {
                name: "object-format".into(),
                value: Some("sha256".into()),
            },
            Capability {
                name: "symref".into(),
                value: Some("HEAD:refs/heads/main".into()),
            },
            Capability {
                name: "custom".into(),
                value: Some("value".into()),
            },
        ];
        let features =
            parse_upload_pack_features(&capabilities).expect("test operation should succeed");
        assert_eq!(
            features,
            UploadPackFeatures {
                multi_ack: true,
                multi_ack_detailed: true,
                no_done: true,
                thin_pack: true,
                side_band_64k: true,
                ofs_delta: true,
                shallow: true,
                deepen_since: true,
                deepen_not: true,
                include_tag: true,
                no_progress: true,
                filter: true,
                agent: Some("git/2.54.0".into()),
                object_format: Some(ObjectFormat::Sha256),
                symrefs: vec!["HEAD:refs/heads/main".into()],
                unknown: vec![Capability {
                    name: "custom".into(),
                    value: Some("value".into()),
                }],
                ..UploadPackFeatures::default()
            }
        );
        assert_eq!(
            encode_upload_pack_features(&features).expect("test operation should succeed"),
            capabilities
        );

        let request = UploadPackRequest {
            wants: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
            ],
            capabilities: vec![
                Capability {
                    name: "multi_ack_detailed".into(),
                    value: None,
                },
                Capability {
                    name: "thin-pack".into(),
                    value: None,
                },
                Capability {
                    name: "side-band-64k".into(),
                    value: None,
                },
                Capability {
                    name: "ofs-delta".into(),
                    value: None,
                },
                Capability {
                    name: "include-tag".into(),
                    value: None,
                },
                Capability {
                    name: "agent".into(),
                    value: Some("sley".into()),
                },
            ],
            shallow: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "2222222222222222222222222222222222222222",
                )
                .expect("test operation should succeed"),
            ],
            deepen: Some(5),
            deepen_since: Some(1_710_000_000),
            deepen_not: vec!["refs/tags/base".into()],
            filter: Some("blob:none".into()),
        };
        validate_upload_pack_request_features(&features, &request)
            .expect("test operation should succeed");
    }

    #[test]
    fn upload_pack_features_reject_invalid_requests() {
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let features = UploadPackFeatures {
            thin_pack: true,
            side_band: true,
            ..UploadPackFeatures::default()
        };

        assert!(
            validate_upload_pack_request_features(
                &features,
                &UploadPackRequest {
                    wants: vec![want],
                    capabilities: vec![Capability {
                        name: "ofs-delta".into(),
                        value: None,
                    }],
                    ..UploadPackRequest::default()
                },
            )
            .is_err()
        );
        assert!(
            validate_upload_pack_request_features(
                &features,
                &UploadPackRequest {
                    wants: vec![want],
                    shallow: vec![want],
                    ..UploadPackRequest::default()
                },
            )
            .is_err()
        );
        assert!(
            validate_upload_pack_request_features(
                &features,
                &UploadPackRequest {
                    wants: vec![want],
                    filter: Some("blob:none".into()),
                    ..UploadPackRequest::default()
                },
            )
            .is_err()
        );
        assert!(
            validate_upload_pack_request_features(
                &UploadPackFeatures {
                    side_band: true,
                    side_band_64k: true,
                    ..UploadPackFeatures::default()
                },
                &UploadPackRequest {
                    wants: vec![want],
                    capabilities: vec![
                        Capability {
                            name: "side-band".into(),
                            value: None,
                        },
                        Capability {
                            name: "side-band-64k".into(),
                            value: None,
                        },
                    ],
                    ..UploadPackRequest::default()
                },
            )
            .is_err()
        );

        assert!(
            parse_upload_pack_features(&[
                Capability {
                    name: "thin-pack".into(),
                    value: None,
                },
                Capability {
                    name: "thin-pack".into(),
                    value: None,
                },
            ])
            .is_err()
        );
        assert!(
            encode_upload_pack_features(&UploadPackFeatures {
                unknown: vec![Capability {
                    name: "filter".into(),
                    value: None,
                }],
                ..UploadPackFeatures::default()
            })
            .is_err()
        );
    }

    #[test]
    fn upload_pack_raw_response_builder_filters_unknown_haves_and_builds_pack() {
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let known_have = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let unknown_have = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let existing = std::collections::HashSet::from([want, known_have]);

        let response = build_upload_pack_raw_packfile_response(
            &UploadPackFeatures::default(),
            UploadPackRequest {
                wants: vec![want],
                ..UploadPackRequest::default()
            },
            [known_have, unknown_have],
            |oid| Ok(existing.contains(oid)),
            |wants, haves| {
                assert_eq!(wants, vec![want]);
                assert_eq!(haves, vec![known_have]);
                Ok(Some(b"PACKmock".to_vec()))
            },
        )
        .expect("test operation should succeed");

        assert_eq!(
            response.acknowledgments,
            vec![UploadPackAcknowledgment::Nak]
        );
        assert_eq!(response.packfile, b"PACKmock");
    }

    #[test]
    fn upload_pack_raw_response_builder_rejects_missing_want_and_empty_pack() {
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");

        assert!(
            build_upload_pack_raw_packfile_response(
                &UploadPackFeatures::default(),
                UploadPackRequest {
                    wants: vec![want],
                    ..UploadPackRequest::default()
                },
                Vec::<ObjectId>::new(),
                |_| Ok(false),
                |_, _| Ok(Some(b"PACKmock".to_vec())),
            )
            .is_err()
        );

        assert!(
            build_upload_pack_raw_packfile_response(
                &UploadPackFeatures::default(),
                UploadPackRequest {
                    wants: vec![want],
                    ..UploadPackRequest::default()
                },
                Vec::<ObjectId>::new(),
                |_| Ok(true),
                |_, _| Ok(None),
            )
            .is_err()
        );
    }

    #[test]
    fn upload_pack_request_parses_and_encodes_initial_fetch_request() {
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let second_want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(
                b"want 1111111111111111111111111111111111111111 multi_ack thin-pack agent=git/2.54.0\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(b"want 2222222222222222222222222222222222222222\n".to_vec()),
            PktLineFrame::Data(b"shallow 3333333333333333333333333333333333333333\n".to_vec()),
            PktLineFrame::Data(b"deepen-since 1710000000\n".to_vec()),
            PktLineFrame::Data(b"deepen-not refs/tags/base\n".to_vec()),
            PktLineFrame::Data(b"filter blob:none\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let request = parse_upload_pack_request(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(
            request,
            UploadPackRequest {
                wants: vec![want, second_want],
                capabilities: vec![
                    Capability {
                        name: "multi_ack".into(),
                        value: None,
                    },
                    Capability {
                        name: "thin-pack".into(),
                        value: None,
                    },
                    Capability {
                        name: "agent".into(),
                        value: Some("git/2.54.0".into()),
                    },
                ],
                shallow: vec![shallow],
                deepen: None,
                deepen_since: Some(1_710_000_000),
                deepen_not: vec!["refs/tags/base".into()],
                filter: Some("blob:none".into()),
            }
        );
        assert_eq!(
            encode_upload_pack_request(Some(&request)).expect("test operation should succeed"),
            frames
        );
        assert_eq!(
            parse_upload_pack_request(ObjectFormat::Sha1, &[PktLineFrame::Flush])
                .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            encode_upload_pack_request(None).expect("test operation should succeed"),
            vec![PktLineFrame::Flush]
        );
    }

    #[test]
    fn upload_pack_request_streams_round_trip() {
        let request = UploadPackRequest {
            wants: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
            ],
            capabilities: vec![Capability {
                name: "ofs-delta".into(),
                value: None,
            }],
            deepen: Some(10),
            ..UploadPackRequest::default()
        };
        let mut encoded = Vec::new();
        write_upload_pack_request(&mut encoded, Some(&request))
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_pack_request(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            Some(request)
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn upload_pack_request_rejects_malformed_requests() {
        assert!(
            parse_upload_pack_request(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"want 1111111111111111111111111111111111111111\n".to_vec(),
                )],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_request(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"shallow 1111111111111111111111111111111111111111\n".to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_request(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"want 1111111111111111111111111111111111111111 thin-pack\n".to_vec(),
                    ),
                    PktLineFrame::Data(
                        b"want 2222222222222222222222222222222222222222 ofs-delta\n".to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(parse_upload_pack_request(
            ObjectFormat::Sha1,
            &[
                PktLineFrame::Data(b"want 1111111111111111111111111111111111111111\n".to_vec(),),
                PktLineFrame::Data(b"deepen 1\n".to_vec()),
                PktLineFrame::Data(b"want 2222222222222222222222222222222222222222\n".to_vec()),
                PktLineFrame::Flush,
            ],
        )
        .is_err());
        assert!(parse_upload_pack_request(
            ObjectFormat::Sha1,
            &[
                PktLineFrame::Data(b"want 1111111111111111111111111111111111111111\n".to_vec(),),
                PktLineFrame::Data(b"filter blob:none\n".to_vec()),
                PktLineFrame::Data(b"filter tree:0\n".to_vec()),
                PktLineFrame::Flush,
            ],
        )
        .is_err());
        assert!(encode_upload_pack_request(Some(&UploadPackRequest::default())).is_err());
        assert!(
            encode_upload_pack_request(Some(&UploadPackRequest {
                wants: vec![
                    ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed")
                ],
                deepen: Some(0),
                ..UploadPackRequest::default()
            }))
            .is_err()
        );
    }

    #[test]
    fn upload_pack_shallow_update_parses_and_encodes_records() {
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let unshallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"shallow 1111111111111111111111111111111111111111\n".to_vec()),
            PktLineFrame::Data(b"unshallow 2222222222222222222222222222222222222222\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let entries = parse_upload_pack_shallow_update(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            entries,
            vec![
                ProtocolV2FetchShallowInfo::Shallow(shallow),
                ProtocolV2FetchShallowInfo::Unshallow(unshallow),
            ]
        );
        assert_eq!(
            encode_upload_pack_shallow_update(&entries).expect("test operation should succeed"),
            frames
        );
        assert_eq!(
            parse_upload_pack_shallow_update(ObjectFormat::Sha1, &[PktLineFrame::Flush])
                .expect("test operation should succeed"),
            Vec::<ProtocolV2FetchShallowInfo>::new()
        );
    }

    #[test]
    fn upload_pack_shallow_update_streams_round_trip() {
        let entries = vec![ProtocolV2FetchShallowInfo::Shallow(
            ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
        )];
        let mut encoded = Vec::new();
        write_upload_pack_shallow_update(&mut encoded, &entries)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_pack_shallow_update(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            entries
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn upload_pack_shallow_update_rejects_malformed_records() {
        assert!(
            parse_upload_pack_shallow_update(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"shallow 1111111111111111111111111111111111111111\n".to_vec(),
                )],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_shallow_update(
                ObjectFormat::Sha1,
                &[PktLineFrame::Delimiter, PktLineFrame::Flush],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_shallow_update(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"shallow 1111111111111111111111111111111111111111\n".to_vec(),
                    ),
                    PktLineFrame::Flush,
                    PktLineFrame::Data(
                        b"unshallow 2222222222222222222222222222222222222222\n".to_vec(),
                    ),
                ],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_shallow_update(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"unsupported 1111111111111111111111111111111111111111\n".to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn upload_pack_negotiation_request_parses_flush_and_done_rounds() {
        let have = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let second_have = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let flush_round = vec![
            PktLineFrame::Data(b"have 1111111111111111111111111111111111111111\n".to_vec()),
            PktLineFrame::Data(b"have 2222222222222222222222222222222222222222\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let request = parse_upload_pack_negotiation_request(ObjectFormat::Sha1, &flush_round)
            .expect("test operation should succeed");
        assert_eq!(
            request,
            UploadPackNegotiationRequest {
                haves: vec![have, second_have],
                done: false,
            }
        );
        assert_eq!(
            encode_upload_pack_negotiation_request(&request)
                .expect("test operation should succeed"),
            flush_round
        );

        let done_round = vec![
            PktLineFrame::Data(b"have 1111111111111111111111111111111111111111\n".to_vec()),
            PktLineFrame::Data(b"done\n".to_vec()),
        ];
        let request = parse_upload_pack_negotiation_request(ObjectFormat::Sha1, &done_round)
            .expect("test operation should succeed");
        assert_eq!(
            request,
            UploadPackNegotiationRequest {
                haves: vec![have],
                done: true,
            }
        );
        assert_eq!(
            encode_upload_pack_negotiation_request(&request)
                .expect("test operation should succeed"),
            done_round
        );
    }

    #[test]
    fn upload_pack_negotiation_request_streams_round_trip() {
        let first = UploadPackNegotiationRequest {
            haves: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
            ],
            done: false,
        };
        let second = UploadPackNegotiationRequest {
            haves: Vec::new(),
            done: true,
        };
        let mut encoded = Vec::new();
        write_upload_pack_negotiation_request(&mut encoded, &first)
            .expect("test operation should succeed");
        write_upload_pack_negotiation_request(&mut encoded, &second)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_pack_negotiation_request(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            first
        );
        assert_eq!(
            read_upload_pack_negotiation_request(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            second
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn upload_pack_negotiation_request_rejects_malformed_rounds() {
        assert!(
            parse_upload_pack_negotiation_request(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"have 1111111111111111111111111111111111111111\n".to_vec(),
                )],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_negotiation_request(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"want 1111111111111111111111111111111111111111\n".to_vec(),
                )],
            )
            .is_err()
        );
        assert!(parse_upload_pack_negotiation_request(
            ObjectFormat::Sha1,
            &[
                PktLineFrame::Data(b"done\n".to_vec()),
                PktLineFrame::Data(b"have 1111111111111111111111111111111111111111\n".to_vec(),),
            ],
        )
        .is_err());
        assert!(
            parse_upload_pack_negotiation_request(
                ObjectFormat::Sha1,
                &[PktLineFrame::Delimiter, PktLineFrame::Flush],
            )
            .is_err()
        );
    }

    #[test]
    fn upload_pack_acknowledgments_parse_and_encode_statuses() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        assert_eq!(
            parse_upload_pack_acknowledgment(ObjectFormat::Sha1, b"NAK\n")
                .expect("test operation should succeed"),
            UploadPackAcknowledgment::Nak
        );
        for (payload, status) in [
            (
                b"ACK 1111111111111111111111111111111111111111\n".as_slice(),
                None,
            ),
            (
                b"ACK 1111111111111111111111111111111111111111 continue\n".as_slice(),
                Some(UploadPackAckStatus::Continue),
            ),
            (
                b"ACK 1111111111111111111111111111111111111111 common\n".as_slice(),
                Some(UploadPackAckStatus::Common),
            ),
            (
                b"ACK 1111111111111111111111111111111111111111 ready\n".as_slice(),
                Some(UploadPackAckStatus::Ready),
            ),
        ] {
            let acknowledgment = parse_upload_pack_acknowledgment(ObjectFormat::Sha1, payload)
                .expect("test operation should succeed");
            assert_eq!(
                acknowledgment,
                UploadPackAcknowledgment::Ack {
                    oid,
                    status,
                }
            );
            assert_eq!(
                encode_upload_pack_acknowledgment(&acknowledgment)
                    .expect("test operation should succeed"),
                payload
            );
        }
    }

    #[test]
    fn upload_pack_acknowledgments_stream_round_trip_and_reject_bad_lines() {
        let acknowledgment = UploadPackAcknowledgment::Ack {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            status: Some(UploadPackAckStatus::Ready),
        };
        let mut encoded = Vec::new();
        write_upload_pack_acknowledgment(&mut encoded, &acknowledgment)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_pack_acknowledgment(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            acknowledgment
        );
        assert_eq!(input, b"tail");
        assert!(parse_upload_pack_acknowledgment(ObjectFormat::Sha1, b"ACK not-an-oid\n").is_err());
        assert!(
            parse_upload_pack_acknowledgment(
                ObjectFormat::Sha1,
                b"ACK 1111111111111111111111111111111111111111 unknown\n",
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_acknowledgment(
                ObjectFormat::Sha1,
                b"ACK 1111111111111111111111111111111111111111 ready extra\n",
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_acknowledgment(ObjectFormat::Sha1, b"ERR remote died\n").is_err()
        );
        assert!(read_upload_pack_acknowledgment(ObjectFormat::Sha1, &mut &b"0000"[..]).is_err());
    }

    #[test]
    fn upload_pack_packfile_response_parses_acknowledgments_and_sideband() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"ACK 1111111111111111111111111111111111111111 common\n".to_vec()),
            PktLineFrame::Data(b"NAK\n".to_vec()),
            PktLineFrame::Data(b"\x01PACK".to_vec()),
            PktLineFrame::Data(b"\x02counting objects\n".to_vec()),
            PktLineFrame::Data(b"\x01 bytes".to_vec()),
            PktLineFrame::Flush,
        ];
        let response = parse_upload_pack_packfile_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            response,
            UploadPackPackfileResponse {
                acknowledgments: vec![
                    UploadPackAcknowledgment::Ack {
                        oid,
                        status: Some(UploadPackAckStatus::Common),
                    },
                    UploadPackAcknowledgment::Nak,
                ],
                sideband: vec![
                    SideBandPacket {
                        channel: SideBandChannel::Data,
                        data: b"PACK".to_vec(),
                    },
                    SideBandPacket {
                        channel: SideBandChannel::Progress,
                        data: b"counting objects\n".to_vec(),
                    },
                    SideBandPacket {
                        channel: SideBandChannel::Data,
                        data: b" bytes".to_vec(),
                    },
                ],
            }
        );
        assert_eq!(
            demux_upload_pack_packfile_response(&response).expect("test operation should succeed"),
            SideBandDemux {
                data: b"PACK bytes".to_vec(),
                progress: vec![b"counting objects\n".to_vec()],
            }
        );
        assert_eq!(
            encode_upload_pack_packfile_response(&response).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn upload_pack_packfile_response_streams_round_trip() {
        let response = UploadPackPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            sideband: vec![SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"PACK".to_vec(),
            }],
        };
        let mut encoded = Vec::new();
        write_upload_pack_packfile_response(&mut encoded, &response)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_pack_packfile_response(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            response
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn upload_pack_packfile_response_rejects_malformed_streams() {
        assert!(
            parse_upload_pack_packfile_response(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(b"NAK\n".to_vec())],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_packfile_response(
                ObjectFormat::Sha1,
                &[PktLineFrame::Delimiter, PktLineFrame::Flush],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_packfile_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"\x01PACK".to_vec()),
                    PktLineFrame::Data(
                        b"ACK 1111111111111111111111111111111111111111 common\n".to_vec()
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_packfile_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"NAK\n".to_vec()),
                    PktLineFrame::Flush,
                    PktLineFrame::Data(b"\x01PACK".to_vec()),
                ],
            )
            .is_err()
        );
        assert!(
            parse_upload_pack_packfile_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"NAK\n".to_vec()),
                    PktLineFrame::Data(b"\x04bad".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn upload_pack_raw_packfile_response_parses_acknowledgments_and_raw_pack() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: vec![
                UploadPackAcknowledgment::Ack {
                    oid,
                    status: Some(UploadPackAckStatus::Common),
                },
                UploadPackAcknowledgment::Nak,
            ],
            packfile: b"PACK\x00\x00\x00\x02raw-bytes".to_vec(),
        };
        let encoded = encode_upload_pack_raw_packfile_response(&response)
            .expect("test operation should succeed");
        assert_eq!(
            parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &encoded)
                .expect("test operation should succeed"),
            response
        );
    }

    #[test]
    fn upload_pack_raw_packfile_response_streams_round_trip() {
        let response = UploadPackRawPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            packfile: b"PACK\x00\x00\x00\x02raw-bytes".to_vec(),
        };
        let mut encoded = Vec::new();
        write_upload_pack_raw_packfile_response(&mut encoded, &response)
            .expect("test operation should succeed");
        assert_eq!(
            encoded,
            encode_upload_pack_raw_packfile_response(&response)
                .expect("test operation should succeed")
        );

        let mut input = encoded.as_slice();
        assert_eq!(
            read_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            response
        );
        assert!(input.is_empty());
    }

    #[test]
    fn upload_pack_raw_packfile_response_rejects_malformed_streams() {
        let ack = PktLineFrame::data(b"NAK\n".to_vec())
            .expect("test operation should succeed")
            .try_encode()
            .expect("test operation should succeed");
        let bad_ack = PktLineFrame::data(b"ACK not-an-oid\n".to_vec())
            .expect("test operation should succeed")
            .try_encode()
            .expect("test operation should succeed");
        let non_ack =
            PktLineFrame::data(b"have 1111111111111111111111111111111111111111\n".to_vec())
                .expect("test operation should succeed")
                .try_encode()
                .expect("test operation should succeed");
        let mut garbage_after_ack = ack.clone();
        garbage_after_ack.extend_from_slice(b"garbage");

        assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, b"").is_err());
        assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &ack).is_err());
        assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &bad_ack).is_err());
        assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, b"0000PACK").is_err());
        assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &non_ack).is_err());
        assert!(
            parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &garbage_after_ack)
                .is_err()
        );
        assert!(
            encode_upload_pack_raw_packfile_response(&UploadPackRawPackfileResponse {
                acknowledgments: vec![UploadPackAcknowledgment::Nak],
                packfile: Vec::new(),
            })
            .is_err()
        );
        assert!(
            encode_upload_pack_raw_packfile_response(&UploadPackRawPackfileResponse {
                acknowledgments: Vec::new(),
                packfile: b"not-a-pack".to_vec(),
            })
            .is_err()
        );
    }

    #[test]
    fn upload_pack_request_encodes_deepen_request() {
        // A `--depth 1` clone over smart-HTTP v1: the `want` line carries the
        // capabilities, the client's existing shallow boundary is replayed as a
        // `shallow` line, and `deepen 1` requests the truncation. Built as raw
        // pkt-line bytes so the 4-hex length prefixes are exercised.
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let boundary = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let request = UploadPackRequest {
            wants: vec![want],
            capabilities: vec![Capability {
                name: "shallow".into(),
                value: None,
            }],
            shallow: vec![boundary],
            deepen: Some(1),
            ..UploadPackRequest::default()
        };
        let mut encoded = Vec::new();
        write_upload_pack_request(&mut encoded, Some(&request))
            .expect("test operation should succeed");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"003awant 1111111111111111111111111111111111111111 shallow\n");
        expected.extend_from_slice(b"0035shallow 2222222222222222222222222222222222222222\n");
        expected.extend_from_slice(b"000ddeepen 1\n");
        expected.extend_from_slice(b"0000");
        assert_eq!(encoded, expected);
    }

    #[test]
    fn upload_pack_shallow_info_response_parses_shallow_unshallow_and_pack() {
        // The smart-HTTP v1 deepen response: a shallow-info section (one
        // `shallow` and one `unshallow` line) terminated by a flush, then the
        // NAK and the raw packfile. Hand-built pkt-lines (mind the lengths).
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let unshallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let mut input = Vec::new();
        input.extend_from_slice(b"0035shallow 1111111111111111111111111111111111111111\n");
        input.extend_from_slice(b"0037unshallow 2222222222222222222222222222222222222222\n");
        input.extend_from_slice(b"0000"); // shallow-info terminator
        input.extend_from_slice(b"0008NAK\n");
        input.extend_from_slice(b"PACK\x00\x00\x00\x02raw-bytes");

        let (entries, response) =
            parse_upload_pack_shallow_info_and_raw_packfile_response(ObjectFormat::Sha1, &input)
                .expect("test operation should succeed");
        assert_eq!(
            entries,
            vec![
                ProtocolV2FetchShallowInfo::Shallow(shallow),
                ProtocolV2FetchShallowInfo::Unshallow(unshallow),
            ]
        );
        assert_eq!(
            response,
            UploadPackRawPackfileResponse {
                acknowledgments: vec![UploadPackAcknowledgment::Nak],
                packfile: b"PACK\x00\x00\x00\x02raw-bytes".to_vec(),
            }
        );

        // The reader entry point yields the same result over a stream.
        let mut stream = input.as_slice();
        let (read_entries, read_response) =
            read_upload_pack_shallow_info_and_raw_packfile_response(
                ObjectFormat::Sha1,
                &mut stream,
            )
            .expect("test operation should succeed");
        assert_eq!(read_entries, entries);
        assert_eq!(read_response, response);
    }

    #[test]
    fn upload_pack_shallow_info_response_handles_empty_shallow_section() {
        // A deepen request that creates no boundary change still gets an empty
        // shallow-info section (a bare flush) before the NAK + pack.
        let mut input = Vec::new();
        input.extend_from_slice(b"0000"); // empty shallow-info
        input.extend_from_slice(b"0008NAK\n");
        input.extend_from_slice(b"PACK\x00\x00\x00\x02raw-bytes");

        let (entries, response) =
            parse_upload_pack_shallow_info_and_raw_packfile_response(ObjectFormat::Sha1, &input)
                .expect("test operation should succeed");
        assert!(entries.is_empty());
        assert_eq!(
            response.acknowledgments,
            vec![UploadPackAcknowledgment::Nak]
        );
        assert!(response.packfile.starts_with(b"PACK"));
    }

    #[test]
    fn upload_pack_shallow_info_response_rejects_malformed_sections() {
        // Truncated section (no terminating flush before EOF).
        let truncated = b"0035shallow 1111111111111111111111111111111111111111\n".to_vec();
        assert!(
            parse_upload_pack_shallow_info_and_raw_packfile_response(
                ObjectFormat::Sha1,
                &truncated
            )
            .is_err()
        );
        // A non-flush control packet inside the shallow-info section.
        let mut delimiter_section = Vec::new();
        delimiter_section.extend_from_slice(b"0001"); // delimiter, not a flush
        assert!(
            parse_upload_pack_shallow_info_section(ObjectFormat::Sha1, &delimiter_section).is_err()
        );
        // A non-shallow data line inside the section.
        let mut bad_line = Vec::new();
        bad_line.extend_from_slice(b"0008NAK\n");
        assert!(parse_upload_pack_shallow_info_section(ObjectFormat::Sha1, &bad_line).is_err());
        // Valid shallow-info but a missing packfile afterwards.
        let mut no_pack = Vec::new();
        no_pack.extend_from_slice(b"0000"); // empty shallow-info
        no_pack.extend_from_slice(b"0008NAK\n");
        assert!(
            parse_upload_pack_shallow_info_and_raw_packfile_response(ObjectFormat::Sha1, &no_pack)
                .is_err()
        );
    }

    #[test]
    fn receive_pack_request_parses_and_encodes_commands() {
        let old_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let new_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let delete_old_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let zero = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "0000000000000000000000000000000000000000",
        )
        .expect("test operation should succeed");
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4444444444444444444444444444444444444444",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"shallow 4444444444444444444444444444444444444444\n".to_vec()),
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\0report-status side-band-64k agent=git/2.54.0\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"3333333333333333333333333333333333333333 0000000000000000000000000000000000000000 refs/heads/old\n"
                    .to_vec(),
            ),
            PktLineFrame::Flush,
        ];
        let request = parse_receive_pack_request(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            request,
            ReceivePackRequest {
                shallow: vec![shallow],
                commands: vec![
                    ReceivePackCommand {
                        old_id,
                        new_id,
                        name: "refs/heads/main".into(),
                    },
                    ReceivePackCommand {
                        old_id: delete_old_id,
                        new_id: zero,
                        name: "refs/heads/old".into(),
                    },
                ],
                capabilities: vec![
                    Capability {
                        name: "report-status".into(),
                        value: None,
                    },
                    Capability {
                        name: "side-band-64k".into(),
                        value: None,
                    },
                    Capability {
                        name: "agent".into(),
                        value: Some("git/2.54.0".into()),
                    },
                ],
            }
        );
        assert_eq!(
            encode_receive_pack_request(&request).expect("test operation should succeed"),
            frames
        );
        assert_eq!(
            parse_receive_pack_request(ObjectFormat::Sha1, &[PktLineFrame::Flush])
                .expect("test operation should succeed"),
            ReceivePackRequest::default()
        );
    }

    #[test]
    fn receive_pack_request_streams_round_trip() {
        let request = ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "0000000000000000000000000000000000000000",
                )
                .expect("test operation should succeed"),
                new_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                name: "refs/heads/main".into(),
            }],
            capabilities: vec![Capability {
                name: "report-status".into(),
                value: None,
            }],
            ..ReceivePackRequest::default()
        };
        let mut encoded = Vec::new();
        write_receive_pack_request(&mut encoded, &request).expect("test operation should succeed");
        encoded.extend_from_slice(b"PACK");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_receive_pack_request(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            request
        );
        assert_eq!(input, b"PACK");
    }

    #[test]
    fn receive_pack_request_rejects_malformed_commands() {
        assert!(
            parse_receive_pack_request(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\n"
                        .to_vec(),
                )],
            )
            .is_err()
        );
        assert!(
            parse_receive_pack_request(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\n"
                            .to_vec(),
                    ),
                    PktLineFrame::Data(
                        b"shallow 3333333333333333333333333333333333333333\n".to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_receive_pack_request(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\0report-status\n"
                            .to_vec(),
                    ),
                    PktLineFrame::Data(
                        b"3333333333333333333333333333333333333333 4444444444444444444444444444444444444444 refs/heads/next\0side-band-64k\n"
                            .to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_receive_pack_request(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            encode_receive_pack_request(&ReceivePackRequest {
                shallow: vec![
                    ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed")
                ],
                ..ReceivePackRequest::default()
            })
            .is_err()
        );
        assert!(
            encode_receive_pack_request(&ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    new_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2222222222222222222222222222222222222222",
                    )
                    .expect("test operation should succeed"),
                    name: "bad ref".into(),
                }],
                ..ReceivePackRequest::default()
            })
            .is_err()
        );
    }

    #[test]
    fn receive_pack_features_parse_encode_and_validate_push_request() {
        let capabilities = vec![
            Capability {
                name: "report-status".into(),
                value: None,
            },
            Capability {
                name: "report-status-v2".into(),
                value: None,
            },
            Capability {
                name: "delete-refs".into(),
                value: None,
            },
            Capability {
                name: "ofs-delta".into(),
                value: None,
            },
            Capability {
                name: "atomic".into(),
                value: None,
            },
            Capability {
                name: "push-options".into(),
                value: None,
            },
            Capability {
                name: "side-band-64k".into(),
                value: None,
            },
            Capability {
                name: "quiet".into(),
                value: None,
            },
            Capability {
                name: "no-thin".into(),
                value: None,
            },
            Capability {
                name: "agent".into(),
                value: Some("git/2.54.0".into()),
            },
            Capability {
                name: "object-format".into(),
                value: Some("sha256".into()),
            },
            Capability {
                name: "custom".into(),
                value: Some("value".into()),
            },
        ];
        let features =
            parse_receive_pack_features(&capabilities).expect("test operation should succeed");
        assert_eq!(
            features,
            ReceivePackFeatures {
                report_status: true,
                report_status_v2: true,
                delete_refs: true,
                ofs_delta: true,
                atomic: true,
                push_options: true,
                side_band_64k: true,
                quiet: true,
                no_thin: true,
                agent: Some("git/2.54.0".into()),
                object_format: Some(ObjectFormat::Sha256),
                unknown: vec![Capability {
                    name: "custom".into(),
                    value: Some("value".into()),
                }],
            }
        );
        assert_eq!(
            encode_receive_pack_features(&features).expect("test operation should succeed"),
            capabilities
        );

        let request = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    new_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2222222222222222222222222222222222222222",
                    )
                    .expect("test operation should succeed"),
                    name: "refs/heads/main".into(),
                }],
                capabilities: vec![
                    Capability {
                        name: "report-status".into(),
                        value: None,
                    },
                    Capability {
                        name: "ofs-delta".into(),
                        value: None,
                    },
                    Capability {
                        name: "push-options".into(),
                        value: None,
                    },
                    Capability {
                        name: "side-band-64k".into(),
                        value: None,
                    },
                    Capability {
                        name: "agent".into(),
                        value: Some("sley".into()),
                    },
                ],
                ..ReceivePackRequest::default()
            },
            push_options: Some(vec!["ci.skip".into()]),
            packfile: b"PACKpayload".to_vec(),
        };
        validate_receive_pack_push_request_features(&features, &request)
            .expect("test operation should succeed");
    }

    #[test]
    fn receive_pack_features_reject_invalid_push_requests() {
        let old_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let new_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let zero = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "0000000000000000000000000000000000000000",
        )
        .expect("test operation should succeed");
        let features = ReceivePackFeatures {
            report_status: true,
            push_options: true,
            ..ReceivePackFeatures::default()
        };
        let update = ReceivePackCommand {
            old_id: old_id.clone(),
            new_id: new_id.clone(),
            name: "refs/heads/main".into(),
        };

        assert!(
            validate_receive_pack_push_request_features(
                &features,
                &ReceivePackPushRequest {
                    commands: ReceivePackRequest {
                        commands: vec![update.clone()],
                        capabilities: vec![Capability {
                            name: "push-options".into(),
                            value: None,
                        }],
                        ..ReceivePackRequest::default()
                    },
                    push_options: None,
                    packfile: b"PACKpayload".to_vec(),
                },
            )
            .is_err()
        );
        assert!(
            validate_receive_pack_push_request_features(
                &features,
                &ReceivePackPushRequest {
                    commands: ReceivePackRequest {
                        commands: vec![update.clone()],
                        ..ReceivePackRequest::default()
                    },
                    push_options: Some(Vec::new()),
                    packfile: b"PACKpayload".to_vec(),
                },
            )
            .is_err()
        );
        assert!(
            validate_receive_pack_push_request_features(
                &features,
                &ReceivePackPushRequest {
                    commands: ReceivePackRequest {
                        commands: vec![ReceivePackCommand {
                            old_id: old_id.clone(),
                            new_id: zero.clone(),
                            name: "refs/heads/main".into(),
                        }],
                        ..ReceivePackRequest::default()
                    },
                    push_options: None,
                    packfile: Vec::new(),
                },
            )
            .is_err()
        );
        assert!(
            validate_receive_pack_push_request_features(
                &features,
                &ReceivePackPushRequest {
                    commands: ReceivePackRequest {
                        commands: vec![update.clone()],
                        ..ReceivePackRequest::default()
                    },
                    push_options: None,
                    packfile: Vec::new(),
                },
            )
            .is_err()
        );
        assert!(
            validate_receive_pack_push_request_features(
                &ReceivePackFeatures {
                    delete_refs: true,
                    ..ReceivePackFeatures::default()
                },
                &ReceivePackPushRequest {
                    commands: ReceivePackRequest {
                        commands: vec![ReceivePackCommand {
                            old_id,
                            new_id: zero,
                            name: "refs/heads/main".into(),
                        }],
                        ..ReceivePackRequest::default()
                    },
                    push_options: None,
                    packfile: b"PACKpayload".to_vec(),
                },
            )
            .is_err()
        );
        assert!(
            validate_receive_pack_push_request_features(
                &features,
                &ReceivePackPushRequest {
                    commands: ReceivePackRequest {
                        commands: vec![update],
                        capabilities: vec![Capability {
                            name: "atomic".into(),
                            value: None,
                        }],
                        ..ReceivePackRequest::default()
                    },
                    push_options: None,
                    packfile: b"PACKpayload".to_vec(),
                },
            )
            .is_err()
        );

        assert!(
            parse_receive_pack_features(&[
                Capability {
                    name: "push-options".into(),
                    value: None,
                },
                Capability {
                    name: "push-options".into(),
                    value: None,
                },
            ])
            .is_err()
        );
        assert!(
            encode_receive_pack_features(&ReceivePackFeatures {
                unknown: vec![Capability {
                    name: "atomic".into(),
                    value: None,
                }],
                ..ReceivePackFeatures::default()
            })
            .is_err()
        );
    }

    #[test]
    fn receive_pack_apply_helper_installs_pack_verifies_objects_and_reports_ok() {
        let old_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let new_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let request = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: old_id.clone(),
                    new_id: new_id.clone(),
                    name: "refs/heads/main".into(),
                }],
                ..ReceivePackRequest::default()
            },
            packfile: b"PACKpayload".to_vec(),
            ..ReceivePackPushRequest::default()
        };
        let installed = std::cell::Cell::new(false);
        let applied = std::cell::RefCell::new(Vec::new());

        let report = apply_receive_pack_push_request(
            &ReceivePackFeatures::default(),
            &request,
            |_| unreachable!("update stale-old checks belong to the ref transaction callback"),
            |packfile| {
                assert_eq!(packfile, b"PACKpayload");
                installed.set(true);
                Ok(())
            },
            |oid| Ok(oid == &new_id),
            |commands| {
                applied.borrow_mut().extend_from_slice(commands);
                Ok(())
            },
            |_| unreachable!("no delete command should be applied"),
        )
        .expect("test operation should succeed");

        assert!(installed.get());
        assert_eq!(applied.into_inner(), request.commands.commands);
        assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
        assert_eq!(
            report.commands,
            vec![ReceivePackCommandStatus::Ok {
                name: "refs/heads/main".into(),
            }]
        );
    }

    #[test]
    fn receive_pack_apply_helper_preserves_delete_only_and_stale_delete_rules() {
        let old_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let other_id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
        let request = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: old_id.clone(),
                    new_id: zero,
                    name: "refs/heads/main".into(),
                }],
                ..ReceivePackRequest::default()
            },
            ..ReceivePackPushRequest::default()
        };
        let features = ReceivePackFeatures {
            delete_refs: true,
            ..ReceivePackFeatures::default()
        };
        let installed = std::cell::Cell::new(false);
        let deleted = std::cell::RefCell::new(Vec::new());

        let report = apply_receive_pack_push_request(
            &features,
            &request,
            |_| Ok(Some(old_id.clone())),
            |_| {
                installed.set(true);
                Ok(())
            },
            |_| Ok(false),
            |_| unreachable!("delete-only request should not apply updates"),
            |name| {
                deleted.borrow_mut().push(name.to_string());
                Ok(())
            },
        )
        .expect("test operation should succeed");

        assert!(!installed.get());
        assert_eq!(deleted.into_inner(), vec!["refs/heads/main"]);
        assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
        assert!(
            apply_receive_pack_push_request(
                &features,
                &request,
                |_| Ok(Some(other_id.clone())),
                |_| Ok(()),
                |_| Ok(false),
                |_| Ok(()),
                |_| Ok(()),
            )
            .is_err()
        );
    }

    #[test]
    fn receive_pack_push_request_parses_commands_options_and_packfile() {
        let command = ReceivePackCommand {
            old_id: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            new_id: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "2222222222222222222222222222222222222222",
            )
            .expect("test operation should succeed"),
            name: "refs/heads/main".into(),
        };
        let expected = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![command],
                capabilities: vec![
                    Capability {
                        name: "report-status".into(),
                        value: None,
                    },
                    Capability {
                        name: "push-options".into(),
                        value: None,
                    },
                ],
                ..ReceivePackRequest::default()
            },
            push_options: Some(vec!["ci.skip".into(), "deploy=staging".into()]),
            packfile: b"PACK\x00\x00\x00\x02payload".to_vec(),
        };
        let encoded =
            encode_receive_pack_push_request(&expected).expect("test operation should succeed");

        assert_eq!(
            parse_receive_pack_push_request(ObjectFormat::Sha1, &encoded, true)
                .expect("test operation should succeed"),
            expected
        );
    }

    #[test]
    fn receive_pack_push_request_preserves_packfile_without_push_options() {
        let request = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    new_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2222222222222222222222222222222222222222",
                    )
                    .expect("test operation should succeed"),
                    name: "refs/heads/main".into(),
                }],
                ..ReceivePackRequest::default()
            },
            push_options: None,
            packfile: b"0000PACK-like bytes stay raw".to_vec(),
        };
        let encoded =
            encode_receive_pack_push_request(&request).expect("test operation should succeed");

        assert_eq!(
            parse_receive_pack_push_request(ObjectFormat::Sha1, &encoded, false)
                .expect("test operation should succeed"),
            request
        );
    }

    #[test]
    fn receive_pack_push_request_streams_round_trip() {
        let request = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    new_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2222222222222222222222222222222222222222",
                    )
                    .expect("test operation should succeed"),
                    name: "refs/heads/main".into(),
                }],
                capabilities: vec![Capability {
                    name: "push-options".into(),
                    value: None,
                }],
                ..ReceivePackRequest::default()
            },
            push_options: Some(Vec::new()),
            packfile: b"PACKpayload".to_vec(),
        };
        let mut encoded = Vec::new();
        write_receive_pack_push_request(&mut encoded, &request)
            .expect("test operation should succeed");

        assert_eq!(
            read_receive_pack_push_request(ObjectFormat::Sha1, &mut encoded.as_slice(), true)
                .expect("test operation should succeed"),
            request
        );
    }

    #[test]
    fn receive_pack_push_request_rejects_malformed_sections() {
        assert!(
            parse_receive_pack_push_request(
                ObjectFormat::Sha1,
                b"0014not-a-command\n0000PACK",
                false,
            )
            .is_err()
        );

        let request = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    new_id: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2222222222222222222222222222222222222222",
                    )
                    .expect("test operation should succeed"),
                    name: "refs/heads/main".into(),
                }],
                ..ReceivePackRequest::default()
            },
            push_options: None,
            packfile: b"PACKpayload".to_vec(),
        };
        let encoded =
            encode_receive_pack_push_request(&request).expect("test operation should succeed");
        assert!(parse_receive_pack_push_request(ObjectFormat::Sha1, &encoded, true).is_err());

        assert!(
            encode_receive_pack_push_request(&ReceivePackPushRequest {
                commands: ReceivePackRequest {
                    shallow: vec![
                        ObjectId::from_hex(
                            ObjectFormat::Sha1,
                            "1111111111111111111111111111111111111111",
                        )
                        .expect("test operation should succeed")
                    ],
                    ..ReceivePackRequest::default()
                },
                push_options: None,
                packfile: Vec::new(),
            })
            .is_err()
        );
    }

    #[test]
    fn receive_pack_report_status_parses_and_encodes_status_lines() {
        let frames = vec![
            PktLineFrame::Data(b"unpack ok\n".to_vec()),
            PktLineFrame::Data(b"ok refs/heads/main\n".to_vec()),
            PktLineFrame::Data(b"ng refs/heads/old non-fast-forward\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let report =
            parse_receive_pack_report_status(&frames).expect("test operation should succeed");
        assert_eq!(
            report,
            ReceivePackReportStatus {
                unpack: ReceivePackUnpackStatus::Ok,
                commands: vec![
                    ReceivePackCommandStatus::Ok {
                        name: "refs/heads/main".into(),
                    },
                    ReceivePackCommandStatus::Ng {
                        name: "refs/heads/old".into(),
                        message: "non-fast-forward".into(),
                    },
                ],
            }
        );
        assert_eq!(
            encode_receive_pack_report_status(&report).expect("test operation should succeed"),
            frames
        );

        let frames = vec![
            PktLineFrame::Data(b"unpack pack exceeds maximum size\n".to_vec()),
            PktLineFrame::Flush,
        ];
        assert_eq!(
            parse_receive_pack_report_status(&frames).expect("test operation should succeed"),
            ReceivePackReportStatus {
                unpack: ReceivePackUnpackStatus::Error("pack exceeds maximum size".into()),
                commands: Vec::new(),
            }
        );
    }

    #[test]
    fn receive_pack_report_status_streams_round_trip() {
        let report = ReceivePackReportStatus {
            unpack: ReceivePackUnpackStatus::Ok,
            commands: vec![ReceivePackCommandStatus::Ok {
                name: "refs/heads/main".into(),
            }],
        };
        let mut encoded = Vec::new();
        write_receive_pack_report_status(&mut encoded, &report)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_receive_pack_report_status(&mut input).expect("test operation should succeed"),
            report
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn receive_pack_report_status_rejects_malformed_status_lines() {
        assert!(parse_receive_pack_report_status(&[]).is_err());
        assert!(
            parse_receive_pack_report_status(&[
                PktLineFrame::Data(b"unpack ok\n".to_vec()),
                PktLineFrame::Data(b"ok refs/heads/main\n".to_vec()),
            ])
            .is_err()
        );
        assert!(
            parse_receive_pack_report_status(&[
                PktLineFrame::Flush,
                PktLineFrame::Data(b"ok refs/heads/main\n".to_vec()),
            ])
            .is_err()
        );
        assert!(
            parse_receive_pack_report_status(&[
                PktLineFrame::Data(b"unpack ok\n".to_vec()),
                PktLineFrame::Data(b"bad refs/heads/main\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            parse_receive_pack_report_status(&[
                PktLineFrame::Data(b"unpack ok\n".to_vec()),
                PktLineFrame::Data(b"ng refs/heads/main\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            encode_receive_pack_report_status(&ReceivePackReportStatus {
                unpack: ReceivePackUnpackStatus::Error("".into()),
                commands: Vec::new(),
            })
            .is_err()
        );
        assert!(
            encode_receive_pack_report_status(&ReceivePackReportStatus {
                unpack: ReceivePackUnpackStatus::Ok,
                commands: vec![ReceivePackCommandStatus::Ok {
                    name: "bad ref".into(),
                }],
            })
            .is_err()
        );
    }

    #[test]
    fn receive_pack_report_status_v2_parses_and_encodes_options() {
        let old_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let new_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"unpack ok\n".to_vec()),
            PktLineFrame::Data(b"ok refs/for/main\n".to_vec()),
            PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
            PktLineFrame::Data(
                b"option old-oid 1111111111111111111111111111111111111111\n".to_vec(),
            ),
            PktLineFrame::Data(
                b"option new-oid 2222222222222222222222222222222222222222\n".to_vec(),
            ),
            PktLineFrame::Data(b"option forced-update\n".to_vec()),
            PktLineFrame::Data(b"ng refs/heads/old rejected by hook\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let report = parse_receive_pack_report_status_v2(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            report,
            ReceivePackReportStatusV2 {
                unpack: ReceivePackUnpackStatus::Ok,
                commands: vec![
                    ReceivePackCommandStatusV2::Ok {
                        name: "refs/for/main".into(),
                        options: ReceivePackCommandStatusV2Options {
                            refname: Some("refs/heads/main".into()),
                            old_oid: Some(old_oid),
                            new_oid: Some(new_oid),
                            forced_update: true,
                        },
                    },
                    ReceivePackCommandStatusV2::Ng {
                        name: "refs/heads/old".into(),
                        message: "rejected by hook".into(),
                    },
                ],
            }
        );
        assert_eq!(
            encode_receive_pack_report_status_v2(&report).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn receive_pack_report_status_v2_streams_round_trip() {
        let report = ReceivePackReportStatusV2 {
            unpack: ReceivePackUnpackStatus::Ok,
            commands: vec![ReceivePackCommandStatusV2::Ok {
                name: "refs/for/main".into(),
                options: ReceivePackCommandStatusV2Options {
                    refname: Some("refs/heads/main".into()),
                    old_oid: None,
                    new_oid: None,
                    forced_update: false,
                },
            }],
        };
        let mut encoded = Vec::new();
        write_receive_pack_report_status_v2(&mut encoded, &report)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_receive_pack_report_status_v2(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            report
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn receive_pack_report_status_v2_rejects_malformed_options() {
        assert!(parse_receive_pack_report_status_v2(ObjectFormat::Sha1, &[]).is_err());
        assert!(
            parse_receive_pack_report_status_v2(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"unpack ok\n".to_vec()),
                    PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_receive_pack_report_status_v2(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"unpack ok\n".to_vec()),
                    PktLineFrame::Data(b"ng refs/heads/main rejected\n".to_vec()),
                    PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_receive_pack_report_status_v2(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"unpack ok\n".to_vec()),
                    PktLineFrame::Data(b"ok refs/for/main\n".to_vec()),
                    PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
                    PktLineFrame::Data(b"option refname refs/heads/next\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_receive_pack_report_status_v2(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"unpack ok\n".to_vec()),
                    PktLineFrame::Data(b"ok refs/for/main\n".to_vec()),
                    PktLineFrame::Data(b"option old-oid not-an-oid\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            encode_receive_pack_report_status_v2(&ReceivePackReportStatusV2 {
                unpack: ReceivePackUnpackStatus::Ok,
                commands: vec![ReceivePackCommandStatusV2::Ok {
                    name: "refs/for/main".into(),
                    options: ReceivePackCommandStatusV2Options {
                        refname: Some("bad ref".into()),
                        ..ReceivePackCommandStatusV2Options::default()
                    },
                }],
            })
            .is_err()
        );
    }

    #[test]
    fn receive_pack_push_options_parse_and_encode_options() {
        let frames = vec![
            PktLineFrame::Data(b"ci.skip\n".to_vec()),
            PktLineFrame::Data(b"deploy target=staging\n".to_vec()),
            PktLineFrame::Data(b"\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let options =
            parse_receive_pack_push_options(&frames).expect("test operation should succeed");
        assert_eq!(
            options,
            vec![
                "ci.skip".to_string(),
                "deploy target=staging".to_string(),
                String::new(),
            ]
        );
        assert_eq!(
            encode_receive_pack_push_options(&options).expect("test operation should succeed"),
            frames
        );
        assert_eq!(
            parse_receive_pack_push_options(&[PktLineFrame::Flush])
                .expect("test operation should succeed"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn receive_pack_push_options_streams_round_trip() {
        let options = vec!["ci.skip".to_string(), "reviewer=alice".to_string()];
        let mut encoded = Vec::new();
        write_receive_pack_push_options(&mut encoded, &options)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"PACK");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_receive_pack_push_options(&mut input).expect("test operation should succeed"),
            options
        );
        assert_eq!(input, b"PACK");
    }

    #[test]
    fn receive_pack_push_options_reject_malformed_streams() {
        assert!(
            parse_receive_pack_push_options(&[PktLineFrame::Data(b"ci.skip\n".to_vec())]).is_err()
        );
        assert!(
            parse_receive_pack_push_options(&[PktLineFrame::Delimiter, PktLineFrame::Flush])
                .is_err()
        );
        assert!(
            parse_receive_pack_push_options(&[
                PktLineFrame::Data(b"ci.skip\n".to_vec()),
                PktLineFrame::Flush,
                PktLineFrame::Data(b"after\n".to_vec()),
            ])
            .is_err()
        );
        assert!(
            parse_receive_pack_push_options(&[
                PktLineFrame::Data(b"bad\0option\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(encode_receive_pack_push_options(&["bad\noption".to_string()]).is_err());
    }

    #[test]
    fn protocol_v2_advertisement_parses_version_and_capabilities() {
        let frames = parse_pkt_line_stream(
            b"000eversion 2\n0015agent=git/2.54.0\n0013ls-refs=unborn\n0027fetch=shallow wait-for-done filter\n0012server-option\n0000",
        )
        .expect("test operation should succeed");
        let handshake =
            parse_protocol_v2_advertisement(&frames).expect("test operation should succeed");
        assert_eq!(handshake.protocol, ProtocolVersion::V2);
        assert_eq!(
            handshake.capabilities,
            vec![
                Capability {
                    name: "agent".into(),
                    value: Some("git/2.54.0".into()),
                },
                Capability {
                    name: "ls-refs".into(),
                    value: Some("unborn".into()),
                },
                Capability {
                    name: "fetch".into(),
                    value: Some("shallow wait-for-done filter".into()),
                },
                Capability {
                    name: "server-option".into(),
                    value: None,
                },
            ]
        );
        assert_eq!(
            encode_protocol_v2_advertisement(&handshake).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn protocol_v2_advertisement_reads_until_flush() {
        let mut input = b"000eversion 2\n0013ls-refs=unborn\n0000next-session".as_slice();
        let handshake =
            read_protocol_v2_advertisement(&mut input).expect("test operation should succeed");
        assert_eq!(handshake.protocol, ProtocolVersion::V2);
        assert_eq!(
            handshake.capabilities,
            vec![Capability {
                name: "ls-refs".into(),
                value: Some("unborn".into()),
            }]
        );
        assert_eq!(input, b"next-session");
    }

    #[test]
    fn protocol_v2_advertisement_writes_stream() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "agent".into(),
                    value: Some("sley/0".into()),
                },
                Capability {
                    name: "fetch".into(),
                    value: Some("shallow filter".into()),
                },
            ],
        };
        let mut encoded = Vec::new();
        write_protocol_v2_advertisement(&mut encoded, &handshake)
            .expect("test operation should succeed");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_advertisement(&mut input).expect("test operation should succeed"),
            handshake
        );
        assert!(input.is_empty());
        assert!(
            encode_protocol_v2_advertisement(&TransportHandshake {
                protocol: ProtocolVersion::V1,
                capabilities: Vec::new(),
            })
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_advertisement_rejects_malformed_sequences() {
        assert!(parse_protocol_v2_advertisement(&[]).is_err());
        assert!(
            parse_protocol_v2_advertisement(&[
                PktLineFrame::Data(b"version 1\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            parse_protocol_v2_advertisement(&[PktLineFrame::Data(b"version 2\n".to_vec())])
                .is_err()
        );
        assert!(
            parse_protocol_v2_advertisement(&[
                PktLineFrame::Data(b"version 2\n".to_vec()),
                PktLineFrame::Delimiter,
            ])
            .is_err()
        );
        assert!(
            parse_protocol_v2_advertisement(&[
                PktLineFrame::Data(b"version 2\n".to_vec()),
                PktLineFrame::Data(b"fetch=\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_command_request_parses_and_encodes_sections() {
        let frames = parse_pkt_line_stream(
            b"0014command=ls-refs\n0011agent=sley/0\n0017object-format=sha1\n00010009peel\n000csymrefs\n001bref-prefix refs/heads/\n0000",
        )
        .expect("test operation should succeed");
        let request =
            parse_protocol_v2_command_request(&frames).expect("test operation should succeed");
        assert_eq!(
            request,
            ProtocolV2CommandRequest {
                command: "ls-refs".into(),
                capabilities: vec![
                    Capability {
                        name: "agent".into(),
                        value: Some("sley/0".into()),
                    },
                    Capability {
                        name: "object-format".into(),
                        value: Some("sha1".into()),
                    },
                ],
                arguments: vec![
                    b"peel".to_vec(),
                    b"symrefs".to_vec(),
                    b"ref-prefix refs/heads/".to_vec(),
                ],
            }
        );
        assert_eq!(
            encode_protocol_v2_command_request(&request).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn protocol_v2_command_request_allows_no_argument_section() {
        let frames = parse_pkt_line_stream(b"0012command=fetch\n0000")
            .expect("test operation should succeed");
        let request =
            parse_protocol_v2_command_request(&frames).expect("test operation should succeed");
        assert_eq!(
            request,
            ProtocolV2CommandRequest {
                command: "fetch".into(),
                capabilities: Vec::new(),
                arguments: Vec::new(),
            }
        );
        assert_eq!(
            encode_protocol_v2_command_request(&request).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn protocol_v2_request_parses_commands_and_empty_done() {
        let frames = parse_pkt_line_stream(b"0012command=fetch\n0000")
            .expect("test operation should succeed");
        let command = ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: Vec::new(),
            arguments: Vec::new(),
        };
        assert_eq!(
            parse_protocol_v2_request(&frames).expect("test operation should succeed"),
            ProtocolV2Request::Command(command.clone())
        );
        assert_eq!(
            encode_protocol_v2_request(&ProtocolV2Request::Command(command))
                .expect("test operation should succeed"),
            frames
        );

        assert_eq!(
            parse_protocol_v2_request(&[PktLineFrame::Flush])
                .expect("test operation should succeed"),
            ProtocolV2Request::Done
        );
        assert_eq!(
            encode_protocol_v2_request(&ProtocolV2Request::Done)
                .expect("test operation should succeed"),
            vec![PktLineFrame::Flush]
        );
    }

    #[test]
    fn protocol_v2_request_streams_empty_done() {
        let mut encoded = Vec::new();
        write_protocol_v2_request(&mut encoded, &ProtocolV2Request::Done)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_request(&mut input).expect("test operation should succeed"),
            ProtocolV2Request::Done
        );
        assert_eq!(input, b"tail");
        let mut command_input = encoded.as_slice();
        assert!(read_protocol_v2_command_request(&mut command_input).is_err());
    }

    #[test]
    fn protocol_v2_command_request_streams_round_trip() {
        let request = ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: vec![Capability {
                name: "agent".into(),
                value: Some("sley/0".into()),
            }],
            arguments: vec![b"peel".to_vec(), b"symrefs".to_vec()],
        };
        let mut encoded = Vec::new();
        write_protocol_v2_command_request(&mut encoded, &request)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_command_request(&mut input).expect("test operation should succeed"),
            request
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_command_request_rejects_malformed_sequences() {
        assert!(parse_protocol_v2_command_request(&[]).is_err());
        assert!(
            parse_protocol_v2_command_request(&[
                PktLineFrame::Data(b"agent=sley/0\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            parse_protocol_v2_command_request(&[
                PktLineFrame::Data(b"command=ls-refs\n".to_vec()),
                PktLineFrame::Delimiter,
                PktLineFrame::Delimiter,
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            parse_protocol_v2_command_request(&[
                PktLineFrame::Data(b"command=ls-refs\n".to_vec()),
                PktLineFrame::Delimiter,
                PktLineFrame::Data(b"\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(
            encode_protocol_v2_command_request(&ProtocolV2CommandRequest {
                command: "bad command".into(),
                capabilities: Vec::new(),
                arguments: Vec::new(),
            })
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_ls_refs_request_parses_and_encodes_arguments() {
        let command = ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: Vec::new(),
            arguments: vec![
                b"peel".to_vec(),
                b"symrefs".to_vec(),
                b"unborn".to_vec(),
                b"ref-prefix HEAD".to_vec(),
                b"ref-prefix refs/heads/".to_vec(),
            ],
        };
        let request = ProtocolV2LsRefsRequest::from_command_request(&command)
            .expect("test operation should succeed");
        assert_eq!(
            request,
            ProtocolV2LsRefsRequest {
                peel: true,
                symrefs: true,
                unborn: true,
                ref_prefixes: vec!["HEAD".into(), "refs/heads/".into()],
            }
        );
        assert_eq!(
            request
                .to_command_request()
                .expect("test operation should succeed"),
            command
        );
        assert!(
            ProtocolV2LsRefsRequest::from_command_request(&ProtocolV2CommandRequest {
                command: "fetch".into(),
                capabilities: Vec::new(),
                arguments: Vec::new(),
            })
            .is_err()
        );
        assert!(
            ProtocolV2LsRefsRequest::from_command_request(&ProtocolV2CommandRequest {
                command: "ls-refs".into(),
                capabilities: Vec::new(),
                arguments: vec![b"ref-prefix ".to_vec()],
            })
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_ls_refs_request_streams_round_trip() {
        let request = ProtocolV2LsRefsRequest {
            peel: true,
            symrefs: true,
            unborn: false,
            ref_prefixes: vec!["HEAD".into(), "refs/tags/".into()],
        };
        let mut encoded = Vec::new();
        write_protocol_v2_ls_refs_request(&mut encoded, &request)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_ls_refs_request(&mut input).expect("test operation should succeed"),
            request
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_ls_refs_response_parses_and_encodes_records() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let peeled = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 refs/tags/v1 peeled:2222222222222222222222222222222222222222 symref-target:refs/heads/main custom\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(b"unborn HEAD symref-target:refs/heads/main\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let records = parse_protocol_v2_ls_refs_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            records,
            vec![
                ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
                    oid,
                    name: "refs/tags/v1".into(),
                    peeled: Some(peeled),
                    symref_target: Some("refs/heads/main".into()),
                    attributes: vec!["custom".into()],
                }),
                ProtocolV2LsRefsRecord::Unborn {
                    name: "HEAD".into(),
                    symref_target: Some("refs/heads/main".into()),
                    attributes: Vec::new(),
                },
            ]
        );
        assert_eq!(
            encode_protocol_v2_ls_refs_response(&records).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn protocol_v2_ls_refs_response_streams_round_trip() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let records = vec![ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
            oid,
            name: "refs/heads/main".into(),
            peeled: None,
            symref_target: Some("refs/heads/trunk".into()),
            attributes: vec!["custom".into()],
        })];
        let mut encoded = Vec::new();
        write_protocol_v2_ls_refs_response(&mut encoded, &records)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_ls_refs_response(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            records
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_ls_refs_response_reads_stateless_response_end() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let records = vec![ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
            oid,
            name: "refs/heads/main".into(),
            peeled: None,
            symref_target: None,
            attributes: Vec::new(),
        })];
        let mut encoded = Vec::new();
        write_protocol_v2_ls_refs_response_with_response_end(&mut encoded, &records)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_ls_refs_response_until_response_end(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            records
        );
        assert_eq!(input, b"tail");
        assert!(
            parse_protocol_v2_ls_refs_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec()
                    ),
                    PktLineFrame::ResponseEnd
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_ls_refs_exchange_writes_request_and_reads_response() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let request = ProtocolV2LsRefsRequest {
            peel: true,
            symrefs: true,
            unborn: false,
            ref_prefixes: vec!["refs/heads/".into()],
        };
        let records = vec![ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
            oid,
            name: "refs/heads/main".into(),
            peeled: None,
            symref_target: None,
            attributes: Vec::new(),
        })];
        let mut response = Vec::new();
        write_protocol_v2_ls_refs_response(&mut response, &records)
            .expect("test operation should succeed");

        let mut input = response.as_slice();
        let mut output = Vec::new();
        assert_eq!(
            exchange_protocol_v2_ls_refs(ObjectFormat::Sha1, &mut input, &mut output, &request)
                .expect("test operation should succeed"),
            records
        );
        assert!(input.is_empty());
        let mut output_read = output.as_slice();
        assert_eq!(
            read_protocol_v2_ls_refs_request(&mut output_read)
                .expect("test operation should succeed"),
            request
        );
    }

    #[test]
    fn protocol_v2_ls_refs_response_rejects_malformed_records() {
        assert!(
            parse_protocol_v2_ls_refs_response(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec()
                )],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_ls_refs_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 refs/heads/main peeled:2222222222222222222222222222222222222222 peeled:3333333333333333333333333333333333333333\n"
                            .to_vec()
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_ls_refs_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"unborn HEAD peeled:2222222222222222222222222222222222222222\n".to_vec()
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            encode_protocol_v2_ls_refs_response(&[ProtocolV2LsRefsRecord::Ref(
                ProtocolV2LsRefsRef {
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    name: "refs/heads/main".into(),
                    peeled: None,
                    symref_target: None,
                    attributes: vec!["peeled:2222222222222222222222222222222222222222".into()],
                }
            )])
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_fetch_request_parses_and_encodes_arguments() {
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let have = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let command = ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: Vec::new(),
            arguments: vec![
                b"want 1111111111111111111111111111111111111111".to_vec(),
                b"want-ref refs/heads/main".to_vec(),
                b"have 2222222222222222222222222222222222222222".to_vec(),
                b"shallow 3333333333333333333333333333333333333333".to_vec(),
                b"deepen 10".to_vec(),
                b"deepen-since 123456789".to_vec(),
                b"deepen-not refs/tags/v1".to_vec(),
                b"deepen-relative".to_vec(),
                b"filter blob:none".to_vec(),
                b"packfile-uris http,https".to_vec(),
                b"thin-pack".to_vec(),
                b"no-progress".to_vec(),
                b"include-tag".to_vec(),
                b"ofs-delta".to_vec(),
                b"sideband-all".to_vec(),
                b"wait-for-done".to_vec(),
                b"done".to_vec(),
            ],
        };
        let request = ProtocolV2FetchRequest::from_command_request(ObjectFormat::Sha1, &command)
            .expect("test operation should succeed");
        assert_eq!(
            request,
            ProtocolV2FetchRequest {
                wants: vec![want],
                want_refs: vec!["refs/heads/main".into()],
                haves: vec![have],
                shallow: vec![shallow],
                deepen: Some(10),
                deepen_since: Some(123456789),
                deepen_not: vec!["refs/tags/v1".into()],
                deepen_relative: true,
                filter: Some("blob:none".into()),
                packfile_uris: Some("http,https".into()),
                thin_pack: true,
                no_progress: true,
                include_tag: true,
                ofs_delta: true,
                sideband_all: true,
                wait_for_done: true,
                done: true,
            }
        );
        assert_eq!(
            request
                .to_command_request()
                .expect("test operation should succeed"),
            command
        );
    }

    #[test]
    fn protocol_v2_fetch_request_rejects_malformed_arguments() {
        assert!(
            ProtocolV2FetchRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "ls-refs".into(),
                    capabilities: Vec::new(),
                    arguments: Vec::new(),
                },
            )
            .is_err()
        );
        assert!(
            ProtocolV2FetchRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "fetch".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"want not-an-oid".to_vec()],
                },
            )
            .is_err()
        );
        assert!(
            ProtocolV2FetchRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "fetch".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"deepen 0".to_vec()],
                },
            )
            .is_err()
        );
        assert!(
            ProtocolV2FetchRequest::from_command_request(
                ObjectFormat::Sha1,
                &ProtocolV2CommandRequest {
                    command: "fetch".into(),
                    capabilities: Vec::new(),
                    arguments: vec![b"filter blob:none".to_vec(), b"filter tree:0".to_vec()],
                },
            )
            .is_err()
        );
        assert!(
            ProtocolV2FetchRequest {
                deepen: Some(0),
                ..ProtocolV2FetchRequest::default()
            }
            .to_command_request()
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_fetch_request_streams_round_trip() {
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let have = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let request = ProtocolV2FetchRequest {
            wants: vec![want],
            haves: vec![have],
            deepen: Some(5),
            filter: Some("blob:none".into()),
            thin_pack: true,
            done: true,
            ..ProtocolV2FetchRequest::default()
        };
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_request(&mut encoded, &request)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_fetch_request(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            request
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_fetch_response_parses_and_encodes_sections() {
        let ack = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let wanted = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let pack_hash = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4444444444444444444444444444444444444444",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"acknowledgments\n".to_vec()),
            PktLineFrame::Data(b"ACK 1111111111111111111111111111111111111111\n".to_vec()),
            PktLineFrame::Data(b"ready\n".to_vec()),
            PktLineFrame::Delimiter,
            PktLineFrame::Data(b"shallow-info\n".to_vec()),
            PktLineFrame::Data(b"shallow 2222222222222222222222222222222222222222\n".to_vec()),
            PktLineFrame::Delimiter,
            PktLineFrame::Data(b"wanted-refs\n".to_vec()),
            PktLineFrame::Data(
                b"3333333333333333333333333333333333333333 refs/heads/main\n".to_vec(),
            ),
            PktLineFrame::Delimiter,
            PktLineFrame::Data(b"packfile-uris\n".to_vec()),
            PktLineFrame::Data(
                b"4444444444444444444444444444444444444444 https://example.invalid/pack-a.pack\n"
                    .to_vec(),
            ),
            PktLineFrame::Delimiter,
            PktLineFrame::Data(b"packfile\n".to_vec()),
            PktLineFrame::Data(b"\x01PACK bytes".to_vec()),
            PktLineFrame::Flush,
        ];
        let sections = parse_protocol_v2_fetch_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            sections,
            vec![
                ProtocolV2FetchResponseSection::Acknowledgments(vec![
                    ProtocolV2FetchAcknowledgment::Ack(ack),
                    ProtocolV2FetchAcknowledgment::Ready,
                ]),
                ProtocolV2FetchResponseSection::ShallowInfo(vec![
                    ProtocolV2FetchShallowInfo::Shallow(shallow)
                ]),
                ProtocolV2FetchResponseSection::WantedRefs(vec![ProtocolV2FetchWantedRef {
                    oid: wanted,
                    name: "refs/heads/main".into(),
                }]),
                ProtocolV2FetchResponseSection::PackfileUris(vec![ProtocolV2FetchPackfileUri {
                    pack_hash,
                    uri: "https://example.invalid/pack-a.pack".into(),
                }]),
                ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK bytes".to_vec()]),
            ]
        );
        assert_eq!(
            encode_protocol_v2_fetch_response(&sections).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn protocol_v2_fetch_response_preserves_unknown_sections() {
        let frames = vec![
            PktLineFrame::Data(b"server-feature\n".to_vec()),
            PktLineFrame::Data(b"opaque line\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let sections = parse_protocol_v2_fetch_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            sections,
            vec![ProtocolV2FetchResponseSection::Unknown {
                name: "server-feature".into(),
                lines: vec![b"opaque line\n".to_vec()],
            }]
        );
        assert_eq!(
            encode_protocol_v2_fetch_response(&sections).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn protocol_v2_fetch_response_streams_round_trip() {
        let ack = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let sections = vec![
            ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Ack(ack),
                ProtocolV2FetchAcknowledgment::Ready,
            ]),
            ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK bytes".to_vec()]),
        ];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_fetch_response(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            sections
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_fetch_sideband_all_response_parses_sections_and_progress() {
        let frames = vec![
            PktLineFrame::Data(
                encode_sideband_packet(&SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"acknowledgments\n".to_vec(),
                })
                .expect("test operation should succeed"),
            ),
            PktLineFrame::Data(
                encode_sideband_packet(&SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"NAK\n".to_vec(),
                })
                .expect("test operation should succeed"),
            ),
            PktLineFrame::Data(
                encode_sideband_packet(&SideBandPacket {
                    channel: SideBandChannel::Progress,
                    data: b"keepalive\n".to_vec(),
                })
                .expect("test operation should succeed"),
            ),
            PktLineFrame::Delimiter,
            PktLineFrame::Data(
                encode_sideband_packet(&SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"packfile\n".to_vec(),
                })
                .expect("test operation should succeed"),
            ),
            PktLineFrame::Data(b"\x01PACK".to_vec()),
            PktLineFrame::Data(b"\x02counting objects\n".to_vec()),
            PktLineFrame::Flush,
        ];

        let response = parse_protocol_v2_fetch_sideband_all_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            response,
            ProtocolV2FetchSidebandAllResponse {
                sections: vec![
                    ProtocolV2FetchResponseSection::Acknowledgments(vec![
                        ProtocolV2FetchAcknowledgment::Nak
                    ]),
                    ProtocolV2FetchResponseSection::Packfile(vec![
                        b"\x01PACK".to_vec(),
                        b"\x02counting objects\n".to_vec(),
                    ]),
                ],
                progress: vec![b"keepalive\n".to_vec()],
            }
        );
        assert_eq!(
            demux_protocol_v2_fetch_packfile(&response.sections)
                .expect("test operation should succeed"),
            Some(SideBandDemux {
                data: b"PACK".to_vec(),
                progress: vec![b"counting objects\n".to_vec()],
            })
        );
    }

    #[test]
    fn protocol_v2_fetch_sideband_all_response_streams_round_trip() {
        let sections = vec![
            ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Nak,
            ]),
            ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK bytes".to_vec()]),
        ];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_sideband_all_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_fetch_sideband_all_response(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            ProtocolV2FetchSidebandAllResponse {
                sections: sections.clone(),
                progress: Vec::new(),
            }
        );
        assert_eq!(input, b"tail");

        let mut encoded = Vec::new();
        write_protocol_v2_fetch_sideband_all_response_with_response_end(&mut encoded, &sections)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_fetch_sideband_all_response_until_response_end(
                ObjectFormat::Sha1,
                &mut input,
            )
            .expect("test operation should succeed")
            .sections,
            sections
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_fetch_sideband_all_response_rejects_malformed_sideband() {
        assert!(
            parse_protocol_v2_fetch_sideband_all_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"acknowledgments\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_fetch_sideband_all_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        encode_sideband_packet(&SideBandPacket {
                            channel: SideBandChannel::Fatal,
                            data: b"remote died\n".to_vec(),
                        })
                        .expect("test operation should succeed"),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_object_info_response_parses_and_encodes_size_records() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"size\n".to_vec()),
            PktLineFrame::Data(b"1111111111111111111111111111111111111111 12345\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let response = parse_protocol_v2_object_info_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            response,
            ProtocolV2ObjectInfoResponse {
                size: true,
                records: vec![ProtocolV2ObjectInfoRecord { oid, size: 12345 }],
            }
        );
        assert_eq!(
            encode_protocol_v2_object_info_response(&response)
                .expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn protocol_v2_object_info_response_streams_and_exchanges() {
        let request = ProtocolV2ObjectInfoRequest {
            size: true,
            oids: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
            ],
        };
        let response = ProtocolV2ObjectInfoResponse {
            size: true,
            records: vec![ProtocolV2ObjectInfoRecord {
                oid: request.oids[0].clone(),
                size: 7,
            }],
        };

        let mut encoded = Vec::new();
        write_protocol_v2_object_info_response(&mut encoded, &response)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_object_info_response(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            response
        );
        assert_eq!(input, b"tail");

        let mut response_bytes = Vec::new();
        write_protocol_v2_object_info_response(&mut response_bytes, &response)
            .expect("test operation should succeed");
        let mut input = response_bytes.as_slice();
        let mut output = Vec::new();
        assert_eq!(
            exchange_protocol_v2_object_info(
                ObjectFormat::Sha1,
                &mut input,
                &mut output,
                &request,
            )
            .expect("test operation should succeed"),
            response
        );
        assert!(input.is_empty());
        let mut output_read = output.as_slice();
        assert_eq!(
            read_protocol_v2_object_info_request(ObjectFormat::Sha1, &mut output_read)
                .expect("test operation should succeed"),
            request
        );
    }

    #[test]
    fn protocol_v2_object_info_response_rejects_malformed_records() {
        assert!(parse_protocol_v2_object_info_response(ObjectFormat::Sha1, &[]).is_err());
        assert!(
            parse_protocol_v2_object_info_response(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(b"size\n".to_vec())],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_object_info_response(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(b"type\n".to_vec()), PktLineFrame::Flush,],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_object_info_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"size\n".to_vec()),
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 not-a-size\n".to_vec()
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_object_info_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"size\n".to_vec()),
                    PktLineFrame::Delimiter,
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            encode_protocol_v2_object_info_response(&ProtocolV2ObjectInfoResponse {
                size: false,
                records: Vec::new(),
            })
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_fetch_response_reads_stateless_response_end() {
        let sections = vec![ProtocolV2FetchResponseSection::Acknowledgments(vec![
            ProtocolV2FetchAcknowledgment::Nak,
        ])];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response_with_response_end(&mut encoded, &sections)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_fetch_response_until_response_end(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            sections
        );
        assert_eq!(input, b"tail");
        assert!(
            parse_protocol_v2_fetch_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"acknowledgments\n".to_vec()),
                    PktLineFrame::ResponseEnd,
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_fetch_exchange_writes_request_and_reads_response() {
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let request = ProtocolV2FetchRequest {
            wants: vec![want],
            thin_pack: true,
            done: true,
            ..ProtocolV2FetchRequest::default()
        };
        let sections = vec![ProtocolV2FetchResponseSection::Acknowledgments(vec![
            ProtocolV2FetchAcknowledgment::Nak,
        ])];
        let mut response = Vec::new();
        write_protocol_v2_fetch_response(&mut response, &sections)
            .expect("test operation should succeed");

        let mut input = response.as_slice();
        let mut output = Vec::new();
        assert_eq!(
            exchange_protocol_v2_fetch(ObjectFormat::Sha1, &mut input, &mut output, &request)
                .expect("test operation should succeed"),
            sections
        );
        assert!(input.is_empty());
        let mut output_read = output.as_slice();
        assert_eq!(
            read_protocol_v2_fetch_request(ObjectFormat::Sha1, &mut output_read)
                .expect("test operation should succeed"),
            request
        );
    }

    #[test]
    fn protocol_v2_fetch_packfile_demuxes_sideband_section() {
        let sections = vec![
            ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Nak,
            ]),
            ProtocolV2FetchResponseSection::Packfile(vec![
                b"\x01PACK".to_vec(),
                b"\x02counting objects\n".to_vec(),
                b"\x01 bytes".to_vec(),
                b"\x02done\n".to_vec(),
            ]),
        ];

        assert_eq!(
            demux_protocol_v2_fetch_packfile(&sections).expect("test operation should succeed"),
            Some(SideBandDemux {
                data: b"PACK bytes".to_vec(),
                progress: vec![b"counting objects\n".to_vec(), b"done\n".to_vec()],
            })
        );
        assert_eq!(
            demux_protocol_v2_fetch_packfile(&[ProtocolV2FetchResponseSection::Acknowledgments(
                vec![ProtocolV2FetchAcknowledgment::Nak],
            )])
            .expect("test operation should succeed"),
            None
        );
    }

    #[test]
    fn protocol_v2_fetch_packfile_demux_rejects_duplicate_or_bad_sideband() {
        assert!(
            demux_protocol_v2_fetch_packfile(&[
                ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK".to_vec()]),
                ProtocolV2FetchResponseSection::Packfile(vec![b"\x01more".to_vec()]),
            ])
            .is_err()
        );
        assert!(
            demux_protocol_v2_fetch_packfile(&[ProtocolV2FetchResponseSection::Packfile(vec![
                b"\x03remote died\n".to_vec()
            ])])
            .is_err()
        );
        assert!(
            demux_protocol_v2_fetch_packfile(&[ProtocolV2FetchResponseSection::Packfile(vec![
                b"\x04bad".to_vec()
            ])])
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_fetch_response_rejects_malformed_sections() {
        assert!(
            parse_protocol_v2_fetch_response(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(b"acknowledgments\n".to_vec())],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_fetch_response(
                ObjectFormat::Sha1,
                &[PktLineFrame::Delimiter, PktLineFrame::Flush],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_fetch_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"acknowledgments\n".to_vec()),
                    PktLineFrame::Data(b"ACK not-an-oid\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_fetch_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"packfile-uris\n".to_vec()),
                    PktLineFrame::Data(b"https://example.invalid/pack-a.pack\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_protocol_v2_fetch_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"packfile-uris\n".to_vec()),
                    PktLineFrame::Data(
                        b"not-a-hash https://example.invalid/pack-a.pack\n".to_vec()
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            encode_protocol_v2_fetch_response(&[ProtocolV2FetchResponseSection::WantedRefs(vec![
                ProtocolV2FetchWantedRef {
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "1111111111111111111111111111111111111111",
                    )
                    .expect("test operation should succeed"),
                    name: "bad ref".into(),
                }
            ])])
            .is_err()
        );
    }

    #[test]
    fn protocol_v2_ls_refs_response_bridges_into_ref_advertisement_set() {
        let head = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let tag = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let tag_peeled = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 HEAD symref-target:refs/heads/main\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec(),
            ),
            PktLineFrame::Data(
                b"2222222222222222222222222222222222222222 refs/tags/v1 peeled:3333333333333333333333333333333333333333\n"
                    .to_vec(),
            ),
            PktLineFrame::Flush,
        ];

        let set = parse_protocol_v2_ls_refs_response_as_ref_advertisement_set(
            ObjectFormat::Sha1,
            &frames,
        )
        .expect("test operation should succeed");
        assert_eq!(
            set,
            RefAdvertisementSet {
                protocol: ProtocolVersion::V2,
                refs: vec![
                    RefAdvertisement {
                        oid: head.clone(),
                        name: "HEAD".into(),
                        capabilities: vec![Capability {
                            name: "symref".into(),
                            value: Some("HEAD:refs/heads/main".into()),
                        }],
                    },
                    RefAdvertisement {
                        oid: head,
                        name: "refs/heads/main".into(),
                        capabilities: Vec::new(),
                    },
                    RefAdvertisement {
                        oid: tag,
                        name: "refs/tags/v1".into(),
                        capabilities: Vec::new(),
                    },
                    RefAdvertisement {
                        oid: tag_peeled,
                        name: "refs/tags/v1^{}".into(),
                        capabilities: Vec::new(),
                    },
                ],
                shallow: Vec::new(),
            }
        );

        // The streaming reader path produces the same bridged set.
        let mut encoded = Vec::new();
        write_pkt_line_frames(&mut encoded, &frames).expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");
        let mut input = encoded.as_slice();
        assert_eq!(
            read_protocol_v2_ls_refs_response_as_ref_advertisement_set(
                ObjectFormat::Sha1,
                &mut input,
            )
            .expect("test operation should succeed"),
            set,
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn protocol_v2_ls_refs_records_bridge_unborn_head_symref_and_empty() {
        // An unborn HEAD pointing at an as-yet-uncreated branch carries only a
        // symref capability and has no concrete ref to attach it to.
        let records = vec![ProtocolV2LsRefsRecord::Unborn {
            name: "HEAD".into(),
            symref_target: Some("refs/heads/main".into()),
            attributes: Vec::new(),
        }];
        assert!(protocol_v2_ls_refs_records_to_ref_advertisement_set(&records).is_err());

        // An empty ls-refs response bridges to an empty v2 set.
        assert_eq!(
            protocol_v2_ls_refs_records_to_ref_advertisement_set(&[])
                .expect("test operation should succeed"),
            RefAdvertisementSet {
                protocol: ProtocolVersion::V2,
                refs: Vec::new(),
                shallow: Vec::new(),
            }
        );

        // An unborn HEAD alongside a concrete ref attaches the symref to the
        // first ref, matching the v0/v1 advertisement convention.
        let main = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4444444444444444444444444444444444444444",
        )
        .expect("test operation should succeed");
        let records = vec![
            ProtocolV2LsRefsRecord::Unborn {
                name: "HEAD".into(),
                symref_target: Some("refs/heads/main".into()),
                attributes: Vec::new(),
            },
            ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
                oid: main.clone(),
                name: "refs/heads/main".into(),
                peeled: None,
                symref_target: None,
                attributes: Vec::new(),
            }),
        ];
        let set = protocol_v2_ls_refs_records_to_ref_advertisement_set(&records)
            .expect("test operation should succeed");
        assert_eq!(
            set,
            RefAdvertisementSet {
                protocol: ProtocolVersion::V2,
                refs: vec![RefAdvertisement {
                    oid: main,
                    name: "refs/heads/main".into(),
                    capabilities: vec![Capability {
                        name: "symref".into(),
                        value: Some("HEAD:refs/heads/main".into()),
                    }],
                }],
                shallow: Vec::new(),
            }
        );
    }
}
