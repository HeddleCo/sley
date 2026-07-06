use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use std::io::{ErrorKind, Read, Write};
use std::sync::RwLock;

static PACKET_TRACE_IDENTITY: RwLock<Option<String>> = RwLock::new(None);

/// Set the program identity used in subsequent packet traces (the CLI sets this
/// from the running subcommand). Mirrors `packet_trace_identity(prog)`.
pub fn set_packet_trace_identity(prog: &str) {
    if let Ok(mut guard) = PACKET_TRACE_IDENTITY.write() {
        *guard = Some(prog.to_string());
    }
}

fn packet_trace_prefix() -> String {
    PACKET_TRACE_IDENTITY
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_else(|| "git".to_string())
}

/// The destination for `GIT_TRACE_PACKET`: `1`/`2`/`true` → stderr, an absolute
/// path is opened append+create, `0`/`false`/empty/unset disables. Mirrors
/// git's `get_trace_fd` for the values the tests use.
fn packet_trace_sink() -> Option<Box<dyn Write>> {
    let value = std::env::var("GIT_TRACE_PACKET").ok()?;
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "" | "0" | "false" => None,
        "1" | "2" | "true" => Some(Box::new(std::io::stderr())),
        _ => {
            if std::path::Path::new(&value).is_absolute() {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&value)
                    .ok()
                    .map(|f| Box::new(f) as Box<dyn Write>)
            } else {
                None
            }
        }
    }
}

/// Whether packet tracing is enabled (cheap env probe for the hot-path guard).
fn packet_trace_enabled() -> bool {
    match std::env::var("GIT_TRACE_PACKET") {
        Ok(value) => {
            let lower = value.to_ascii_lowercase();
            !matches!(lower.as_str(), "" | "0" | "false")
        }
        Err(_) => false,
    }
}

/// Emit one packet-trace line for `data` (one pkt-line's framing token or
/// payload). `is_write` selects the `>`/`<` direction. Octal-escapes
/// non-printable bytes and suppresses newlines, exactly like git's
/// `packet_trace`. The pack-data stream is collapsed to `PACK ...` on its first
/// chunk (git does the same so the trace stays human-readable).
fn packet_trace(data: &[u8], is_write: bool) {
    if !packet_trace_enabled() {
        return;
    }
    let Some(mut sink) = packet_trace_sink() else {
        return;
    };
    // Collapse pack data: once a payload starts with "PACK" (raw) or "\1PACK"
    // (sideband channel 1), git emits a single `PACK ...` marker for the human
    // trace rather than the binary stream. We approximate per-call (stateless):
    // any payload beginning with those magic bytes is rendered as `PACK ...`.
    let rendered: Vec<u8> = if data.starts_with(b"PACK") || data.starts_with(b"\x01PACK") {
        b"PACK ...".to_vec()
    } else {
        data.to_vec()
    };

    let mut out = format!(
        "packet: {:>12}{} ",
        packet_trace_prefix(),
        if is_write { '>' } else { '<' }
    );
    for &byte in &rendered {
        if byte == b'\n' {
            continue;
        }
        if (0x20..=0x7e).contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("\\{byte:o}"));
        }
    }
    out.push('\n');
    let _ = sink.write_all(out.as_bytes());
    let _ = sink.flush();
}

pub fn trace_packet_read_payload(payload: &[u8]) {
    packet_trace(payload, false);
}

pub fn trace_packet_write_payload(payload: &[u8]) {
    packet_trace(payload, true);
}

/// Trace a frame on the wire. Flush/delim/response-end map to their 4-byte
/// tokens (`0000`/`0001`/`0002`) like git, data frames to their payload.
fn packet_trace_frame(frame: &PktLineFrame, is_write: bool) {
    if !packet_trace_enabled() {
        return;
    }
    match frame {
        PktLineFrame::Data(payload) => packet_trace(payload, is_write),
        PktLineFrame::Flush => packet_trace(b"0000", is_write),
        PktLineFrame::Delimiter => packet_trace(b"0001", is_write),
        PktLineFrame::ResponseEnd => packet_trace(b"0002", is_write),
    }
}

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
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSourceRef {
    pub name: String,
    pub oid: ObjectId,
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

pub(crate) fn pkt_line_header(len: usize) -> [u8; 4] {
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

pub(crate) fn parse_pkt_line_frames_until_flush_from(mut input: &[u8]) -> Result<(Vec<PktLineFrame>, usize)> {
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
    let frame = match len {
        0 => PktLineFrame::Flush,
        1 => PktLineFrame::Delimiter,
        2 => PktLineFrame::ResponseEnd,
        3 => {
            return Err(GitError::InvalidFormat(
                "reserved pkt-line length 0003".into(),
            ));
        }
        4..=PKT_LINE_MAX_LEN => {
            let mut payload = vec![0; len - 4];
            reader.read_exact(&mut payload)?;
            PktLineFrame::Data(payload)
        }
        _ => {
            return Err(GitError::InvalidFormat(format!(
                "pkt-line length exceeds {PKT_LINE_MAX_LEN}: {len}"
            )));
        }
    };
    packet_trace_frame(&frame, false);
    Ok(Some(frame))
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
        // `write_pkt_line_payload` already traces the data line.
        PktLineFrame::Data(payload) => write_pkt_line_payload(writer, payload)?,
        PktLineFrame::Flush => {
            packet_trace(b"0000", true);
            writer.write_all(b"0000")?;
        }
        PktLineFrame::Delimiter => {
            packet_trace(b"0001", true);
            writer.write_all(b"0001")?;
        }
        PktLineFrame::ResponseEnd => {
            packet_trace(b"0002", true);
            writer.write_all(b"0002")?;
        }
    }
    Ok(())
}

pub fn write_pkt_line_payload(writer: &mut impl Write, payload: &[u8]) -> Result<()> {
    validate_pkt_line_payload(payload)?;
    packet_trace(payload, true);
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
pub(crate) fn parse_pkt_len(bytes: &[u8]) -> Result<usize> {
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

pub(crate) fn validate_capability_field(label: &str, value: &str) -> Result<()> {
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

pub(crate) fn validate_capability_name(value: &str) -> Result<()> {
    validate_capability_field("capability name", value)?;
    if value.bytes().any(|byte| byte == b'=') {
        return Err(GitError::InvalidFormat(
            "capability name contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

pub(crate) fn non_empty(value: &str) -> Option<&str> {
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

pub(crate) fn validate_refspec_endpoint(label: &str, value: &str) -> Result<()> {
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

pub(crate) fn validate_refspec_shape(refspec: &RefSpec) -> Result<()> {
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

pub(crate) fn validate_fetch_head_line(value: &[u8]) -> Result<()> {
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

pub(crate) fn validate_fetch_head_description_field(value: &str) -> Result<()> {
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

pub(crate) fn validate_protocol_v2_token(label: &str, value: &str) -> Result<()> {
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

pub(crate) fn validate_protocol_v2_line(label: &str, value: &[u8]) -> Result<()> {
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

pub(crate) fn parse_protocol_v2_line_text<'a>(label: &str, value: &'a [u8]) -> Result<&'a str> {
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

pub(crate) fn parse_oid_argument(
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

pub(crate) fn line(mut payload: Vec<u8>) -> Vec<u8> {
    payload.push(b'\n');
    payload
}

pub(crate) fn line_from_str(payload: &str) -> Vec<u8> {
    line(payload.as_bytes().to_vec())
}

pub(crate) fn trim_trailing_lf(input: &[u8]) -> &[u8] {
    input.strip_suffix(b"\n").unwrap_or(input)
}
