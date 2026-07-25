use sley_core::{GitError, Result};
use std::io::{self, ErrorKind, Read, Write};

use crate::pktline::{
    PKT_LINE_MAX_PAYLOAD_LEN, PktLineFrame, PktLineReadLimits, line_from_str,
    parse_protocol_v2_line_text, pkt_line_header, read_pkt_line_frame,
    read_pkt_line_frames_until_flush, read_pkt_line_frames_until_flush_with_limits,
    trim_trailing_lf, write_pkt_line_payload,
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
    // Sideband channel 1 carries packfile/archive bytes, so this buffered
    // reader gets the bulk budget rather than the control one (sley#6).
    let frames =
        read_pkt_line_frames_until_flush_with_limits(reader, PktLineReadLimits::PACK_STREAM)?;
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

/// Streaming demultiplexer for side-band / side-band-64k pkt-line streams.
///
/// Implements [`Read`] over the sideband data channel (channel 1). Progress
/// (channel 2) is delivered via `on_progress`. Fatal (channel 3) and malformed
/// frames surface as `std::io::Error`. The stream ends at the terminating flush
/// pkt-line; subsequent reads return `Ok(0)` once any leftover data-channel
/// bytes have been drained.
///
/// Pair with [`CancellableRead`](sley_core::CancellableRead) as an *outer*
/// wrapper so pack install can observe cooperative cancel between reads without
/// buffering the full demuxed pack.
pub struct StreamingSidebandReader<R, F = fn(&[u8])> {
    reader: R,
    pending: Vec<u8>,
    pending_offset: usize,
    finished: bool,
    /// When set, leading upload-pack `ACK`/`NAK` pkt-lines (before the first
    /// sideband packet) are skipped — matching
    /// [`parse_upload_pack_packfile_response`](crate::parse_upload_pack_packfile_response).
    skip_upload_pack_acks: bool,
    /// Fatal / protocol error deferred until after any already-drained data
    /// bytes have been returned to the caller.
    pending_error: Option<io::Error>,
    on_progress: F,
}

impl<R, F> StreamingSidebandReader<R, F> {
    /// Create a reader that demuxes a pure sideband stream (no ACK preamble).
    pub fn new(reader: R, on_progress: F) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            pending_offset: 0,
            finished: false,
            skip_upload_pack_acks: false,
            pending_error: None,
            on_progress,
        }
    }

    /// Skip leading upload-pack `ACK`/`NAK` pkt-lines before the first sideband
    /// frame. Used for smart-HTTP upload-pack packfile responses.
    pub fn skip_upload_pack_acks(mut self) -> Self {
        self.skip_upload_pack_acks = true;
        self
    }

    /// Borrow the underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Mutably borrow the underlying reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Unwrap the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Whether the terminating flush pkt-line has been observed.
    ///
    /// Pending data-channel bytes may still remain; subsequent reads return
    /// `Ok(0)` only after those bytes are drained.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Consume remaining sideband frames through the terminating flush.
    ///
    /// Pack install may stop reading once the pack trailer is complete while
    /// the wire still has trailing progress frames and the flush pkt-line.
    /// The buffered demux path always consumed the full response; call this
    /// after a successful (or failed) pack install so connection reuse and
    /// protocol state match that behavior.
    pub fn drain_to_end(&mut self) -> io::Result<()>
    where
        R: Read,
        F: FnMut(&[u8]),
    {
        let mut buf = [0u8; 8 * 1024];
        loop {
            match self.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(_) => continue,
                Err(err) => return Err(err),
            }
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

impl<R: Read, F: FnMut(&[u8])> Read for StreamingSidebandReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(err) = self.pending_error.take() {
            return Err(err);
        }
        let mut written = 0usize;
        while written < buf.len() {
            let copied = self.drain_pending(&mut buf[written..]);
            if copied > 0 {
                written += copied;
                continue;
            }
            if self.finished {
                break;
            }
            if let Some(err) = self.pending_error.take() {
                if written > 0 {
                    self.pending_error = Some(err);
                    break;
                }
                return Err(err);
            }
            let frame = match read_pkt_line_frame(&mut self.reader) {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    let err = io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "sideband stream ended before flush",
                    );
                    if written > 0 {
                        self.pending_error = Some(err);
                        break;
                    }
                    return Err(err);
                }
                Err(err) => {
                    let err = sideband_git_error_to_io(err);
                    if written > 0 {
                        self.pending_error = Some(err);
                        break;
                    }
                    return Err(err);
                }
            };
            match frame {
                PktLineFrame::Data(payload) => {
                    if self.skip_upload_pack_acks && is_upload_pack_ack_or_nak_payload(&payload) {
                        continue;
                    }
                    // Once any non-ACK frame is seen, further ACK-shaped lines
                    // (if any) must be treated as sideband.
                    self.skip_upload_pack_acks = false;
                    let packet = match parse_sideband_packet(&payload) {
                        Ok(packet) => packet,
                        Err(err) => {
                            let err = sideband_git_error_to_io(err);
                            if written > 0 {
                                self.pending_error = Some(err);
                                break;
                            }
                            return Err(err);
                        }
                    };
                    match packet.channel {
                        SideBandChannel::Data => {
                            if packet.data.is_empty() {
                                continue;
                            }
                            self.pending = packet.data;
                            self.pending_offset = 0;
                        }
                        SideBandChannel::Progress => {
                            (self.on_progress)(&packet.data);
                        }
                        SideBandChannel::Fatal => {
                            let message = String::from_utf8_lossy(&packet.data).into_owned();
                            let err = io::Error::new(
                                ErrorKind::InvalidData,
                                format!("sideband fatal: {message}"),
                            );
                            if written > 0 {
                                self.pending_error = Some(err);
                                break;
                            }
                            return Err(err);
                        }
                    }
                }
                PktLineFrame::Flush => {
                    self.finished = true;
                    break;
                }
                PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                    let err = io::Error::new(
                        ErrorKind::InvalidData,
                        "sideband stream contains a non-flush control packet",
                    );
                    if written > 0 {
                        self.pending_error = Some(err);
                        break;
                    }
                    return Err(err);
                }
            }
        }
        Ok(written)
    }
}

/// Detect upload-pack acknowledgment pkt-lines that may precede sideband data.
///
/// Matches the preamble rule in
/// [`parse_upload_pack_packfile_response`](crate::parse_upload_pack_packfile_response):
/// `NAK` (with optional trailing LF) or any payload starting with `ACK `.
fn is_upload_pack_ack_or_nak_payload(payload: &[u8]) -> bool {
    trim_trailing_lf(payload) == b"NAK" || payload.starts_with(b"ACK ")
}

fn sideband_git_error_to_io(err: GitError) -> io::Error {
    match err {
        GitError::Io(message) => {
            if message.contains("cancelled") {
                sley_core::cancelled_io_error()
            } else {
                io::Error::other(message)
            }
        }
        GitError::Cancelled => sley_core::cancelled_io_error(),
        other => io::Error::new(ErrorKind::InvalidData, other.to_string()),
    }
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
    // Carries the generated archive in sideband data frames (sley#6).
    let frames =
        read_pkt_line_frames_until_flush_with_limits(reader, PktLineReadLimits::PACK_STREAM)?;
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
