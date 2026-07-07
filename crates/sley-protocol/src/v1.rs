use sley_core::Result;
use std::io::Write;

use crate::pktline::{PktLineFrame, line_from_str, trim_trailing_lf, write_pkt_line_payload};

pub(crate) const V1_VERSION_TEXT: &str = "version 1";

pub(crate) fn is_v1_version_payload(payload: &[u8]) -> bool {
    trim_trailing_lf(payload) == V1_VERSION_TEXT.as_bytes()
}

pub(crate) fn encode_v1_version_frame() -> Result<PktLineFrame> {
    PktLineFrame::data(line_from_str(V1_VERSION_TEXT))
}

pub(crate) fn write_v1_version_line(writer: &mut impl Write) -> Result<()> {
    write_pkt_line_payload(writer, b"version 1\n")
}
