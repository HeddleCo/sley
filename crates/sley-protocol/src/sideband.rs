use sley_core::{GitError, Result};
use std::io::{Read, Write};

use crate::pktline::{
    PktLineFrame, PKT_LINE_MAX_PAYLOAD_LEN, line_from_str, parse_protocol_v2_line_text,
    pkt_line_header, read_pkt_line_frames_until_flush,
    write_pkt_line_payload,
};

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

pub(crate) fn write_sideband_payload(
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

