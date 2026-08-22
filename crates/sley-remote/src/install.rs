// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_core::{CancelFlag, ObjectFormat, Result};
use sley_odb::{
    FileObjectDatabase, PackInstallProgress, RawPackInstallOptions, RawPackInstallResult,
    RawPackInstaller,
};
use std::cell::RefCell;
use std::io::IsTerminal;

use crate::{ProgressSink, TransferProgress};

/// Wraps a [`RawPackInstaller`] so its receipt and indexing counters are
/// forwarded to a [`ProgressSink`] as [`TransferProgress`]. Passed
/// as the `destination` of the generic `install_upload_pack_*` helpers, it
/// threads live byte/object progress without changing their signatures.
///
/// Cancel is forwarded through
/// [`RawPackInstaller::install_raw_pack_from_reader_with_progress_and_cancel`]
/// so mid-transfer stop reaches both the pack indexer and any
/// [`CancellableRead`](sley_core::CancellableRead) wrapping the transport.
///
/// The [`RawPackInstaller`] method takes `&self`, so the `&mut dyn ProgressSink`
/// is held behind a [`RefCell`]; the borrow is confined to one install call.
pub(crate) struct ProgressInstaller<'a, I> {
    inner: &'a I,
    sink: RefCell<&'a mut dyn ProgressSink>,
}

impl<'a, I> ProgressInstaller<'a, I> {
    pub(crate) fn new(inner: &'a I, sink: &'a mut dyn ProgressSink) -> Self {
        Self {
            inner,
            sink: RefCell::new(sink),
        }
    }

    /// A sideband channel-2 chunk forwarder multiplexing this installer's
    /// sink through its interior [`RefCell`], so remote progress lines and
    /// pack-install transfer counters share the one exclusive borrow.
    pub(crate) fn remote_sideband_forwarder(&self) -> impl FnMut(&[u8]) + '_ {
        move |chunk: &[u8]| {
            emit_remote_sideband_progress(&mut **self.sink.borrow_mut(), chunk);
        }
    }
}

impl<I> RawPackInstaller for ProgressInstaller<'_, I>
where
    I: RawPackInstaller,
{
    fn install_raw_pack_from_reader_with_options<R>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
    {
        self.install_raw_pack_from_reader_with_progress_and_cancel(
            reader,
            options,
            CancelFlag::never(),
            |_| {},
        )
    }

    fn install_raw_pack_from_reader_with_progress_and_cancel<R, F>(
        &self,
        reader: &mut R,
        options: RawPackInstallOptions,
        cancel: CancelFlag<'_>,
        _progress: F,
    ) -> Result<RawPackInstallResult>
    where
        R: Read,
        F: FnMut(PackInstallProgress),
    {
        // External progress F is ignored: ProgressInstaller owns the ProgressSink.
        self.inner
            .install_raw_pack_from_reader_with_progress_and_cancel(
                reader,
                options,
                cancel,
                |progress| {
                    self.sink
                        .borrow_mut()
                        .transfer(transfer_from_pack(progress));
                },
            )
    }
}

pub(crate) fn transfer_from_pack(progress: PackInstallProgress) -> TransferProgress {
    TransferProgress {
        received_bytes: progress.received_bytes,
        received_objects: progress.indexed_objects,
        total_objects: Some(progress.total_objects),
        // The pack engine reports completed objects, but does not split that
        // count into full objects and deltas.
        indexed_deltas: 0,
    }
}

/// A sideband channel-2 chunk forwarder, invoked per progress frame with the
/// raw remote bytes. Built from a [`ProgressSink`] via
/// [`emit_remote_sideband_progress`] or
/// [`ProgressInstaller::remote_sideband_forwarder`].
pub(crate) type RemoteSidebandForward<'a> = &'a mut dyn FnMut(&[u8]);
use sley_protocol::{
    ProtocolV2FetchResponseHeader, ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo,
    StreamingSidebandReader, read_protocol_v2_fetch_response_header,
    read_upload_pack_raw_packfile_response_header,
    read_upload_pack_shallow_info_and_raw_packfile_response_header,
    read_upload_pack_shallow_info_section,
};
use std::io::{Cursor, Read};

fn raw_pack_install_options(promisor: bool, max_input_size: Option<u64>) -> RawPackInstallOptions {
    RawPackInstallOptions {
        promisor,
        max_input_size,
    }
}

pub fn install_upload_pack_raw_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_raw_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        CancelFlag::never(),
    )
}

/// Install an upload-pack packfile response that may use side-band-64k.
///
/// Smart HTTP always delivers the pack inside sideband channel 1 after any
/// leading `ACK`/`NAK` pkt-lines (see `read_upload_pack_packfile_response`).
pub fn install_upload_pack_packfile_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_sideband_response_from_reader(
        format,
        reader,
        destination,
        false,
        max_input_size,
    )
}

pub fn install_upload_pack_packfile_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    R: Read,
{
    install_upload_pack_packfile_promisor_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        CancelFlag::never(),
        None,
    )
}

/// Promisor sideband pack install with cooperative cancellation.
pub fn install_upload_pack_packfile_promisor_response_from_reader_with_cancel<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<RawPackInstallResult>
where
    R: Read,
{
    install_upload_pack_sideband_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        true,
        max_input_size,
        cancel,
        remote_progress,
    )
}

fn install_upload_pack_sideband_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    promisor: bool,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_sideband_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        promisor,
        max_input_size,
        CancelFlag::never(),
        None,
    )
}

/// Sideband pack install with cooperative cancellation.
///
/// Demuxes sideband channel 1 as frames arrive (no full-response buffer) and
/// threads `cancel` into the destination installer. Pair a transport-level
/// cancel source (`AtomicCancel`, etc.) so mid-transfer stop is observed
/// between pack objects and between network reads when the installer wraps the
/// stream in [`CancellableRead`](sley_core::CancellableRead).
pub fn install_upload_pack_packfile_response_from_reader_with_cancel<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_sideband_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        false,
        max_input_size,
        cancel,
        remote_progress,
    )
}

/// Raw (non-sideband) upload-pack pack install with cooperative cancellation.
pub fn install_upload_pack_raw_response_from_reader_with_cancel<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    let header = read_upload_pack_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(false, max_input_size),
        cancel,
        |_| {},
    )
}

/// Forward one sideband channel-2 chunk to the fetch progress sink using
/// git's stderr prefix rules (`sideband.c`): each line gets the `remote: `
/// display prefix, plus the 8-space dumb-terminal suffix when stderr is not a
/// terminal (parity with the CLI's push hook rendering).
pub(crate) fn emit_remote_sideband_progress(sink: &mut dyn ProgressSink, chunk: &[u8]) {
    const DUMB_SUFFIX: &str = "        ";
    let suffix = if std::io::stderr().is_terminal() {
        ""
    } else {
        DUMB_SUFFIX
    };
    let text = String::from_utf8_lossy(chunk);
    for line in text.lines() {
        sink.diagnostic(&format!("remote: {line}{suffix}"));
    }
}fn install_upload_pack_sideband_response_from_reader_with_cancel<I, R>(
    _format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    promisor: bool,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    mut remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    // Stream demux sideband channel-1 bytes as frames arrive so pack install
    // can cancel mid-transfer without buffering the full response. Leading
    // ACK/NAK pkt-lines are skipped to match read_upload_pack_packfile_response
    // semantics. Channel-2 progress lines are forwarded to the fetch progress
    // sink with git's `remote:` prefix; ProgressInstaller reports receipt/index
    // counters from the installer instead.
    //
    // Parity with the old buffer-then-install path:
    // - after channel-1 pack receipt completes, the wire may still carry
    //   trailing progress frames + flush — drain them;
    // - sideband fatal/protocol errors surface as typed payloads
    //   (`GitError::SidebandFatal` / `InvalidFormat`), not bare Io.
    let mut pack_reader = StreamingSidebandReader::new(reader, move |chunk: &[u8]| {
        if let Some(forward) = remote_progress.as_mut() {
            forward(chunk);
        }
    })
    .skip_upload_pack_acks();
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(promisor, max_input_size),
        cancel,
        |_| {},
    );
    // B2: never drain on error/cancel — that would download the rest of the pack.
    let result = match result {
        Ok(result) => {
            pack_reader
                .drain_to_end()
                .map_err(map_sideband_stream_io_error)?;
            Ok(result)
        }
        Err(install_err) => Err(map_sideband_install_error(install_err)),
    }?;
    Ok(if promisor {
        RawPackInstallResult {
            object_ids: result.object_ids,
        }
    } else {
        result
    })
}

/// Map sideband `Read` failures back to their typed [`GitError`] payloads.
///
/// [`StreamingSidebandReader`](sley_protocol::StreamingSidebandReader) carries
/// its errors as `GitError` payloads across the io boundary, so recovery is a
/// downcast — no substring matching. Cancel keeps its dedicated payload
/// marker; anything else from the transport falls through to the structured
/// `IoKind` conversion.
fn map_sideband_stream_io_error(err: std::io::Error) -> sley_core::GitError {
    if sley_core::is_cancelled_io(&err) {
        return sley_core::GitError::Cancelled;
    }
    if let Some(inner) = err.get_ref().and_then(|inner| inner.downcast_ref::<sley_core::GitError>()) {
        return inner.clone();
    }
    sley_core::GitError::from(err)
}

/// Normalize install-loop failures for fetch/clone diagnostics.
///
/// The streaming reader's typed payloads (`SidebandFatal`, `InvalidFormat`)
/// survive the install loop's error conversion, so sideband aborts already
/// arrive classified; only cancellation needs folding onto `Cancelled`.
fn map_sideband_install_error(err: sley_core::GitError) -> sley_core::GitError {
    if err.is_cancelled() {
        sley_core::GitError::Cancelled
    } else {
        err
    }
}

pub fn install_upload_pack_raw_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<RawPackInstallResult>
where
    R: Read,
{
    install_upload_pack_raw_promisor_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        CancelFlag::never(),
    )
}

/// Raw promisor pack install with cooperative cancellation.
pub fn install_upload_pack_raw_promisor_response_from_reader_with_cancel<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
) -> Result<RawPackInstallResult>
where
    R: Read,
{
    let header = read_upload_pack_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(true, max_input_size),
        cancel,
        |_| {},
    )?;
    Ok(RawPackInstallResult {
        object_ids: result.object_ids,
    })
}

pub fn install_upload_pack_shallow_raw_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_shallow_raw_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        CancelFlag::never(),
    )
}

/// Shallow raw pack install with cooperative cancellation.
pub fn install_upload_pack_shallow_raw_response_from_reader_with_cancel<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    let (shallow, header) =
        read_upload_pack_shallow_info_and_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(false, max_input_size),
        cancel,
        |_| {},
    )?;
    Ok((shallow, result))
}

/// Shallow deepen over smart HTTP: shallow-info section then a sideband pack.
pub fn install_upload_pack_shallow_packfile_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_shallow_packfile_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        CancelFlag::never(),
        None,
    )
}

/// Shallow sideband pack install with cooperative cancellation.
pub fn install_upload_pack_shallow_packfile_response_from_reader_with_cancel<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    install_upload_pack_shallow_sideband_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        false,
        max_input_size,
        cancel,
        remote_progress,
    )
}

fn install_upload_pack_shallow_sideband_response_from_reader_with_cancel<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &I,
    promisor: bool,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    I: RawPackInstaller,
    R: Read,
{
    let shallow = read_upload_pack_shallow_info_section(format, reader)?;
    let result = install_upload_pack_sideband_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        promisor,
        max_input_size,
        cancel,
        remote_progress,
    )?;
    Ok((shallow, result))
}

pub fn install_upload_pack_shallow_raw_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    R: Read,
{
    install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        CancelFlag::never(),
    )
}

/// Shallow raw promisor pack install with cooperative cancellation.
pub fn install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    R: Read,
{
    let (shallow, header) =
        read_upload_pack_shallow_info_and_raw_packfile_response_header(format, reader)?;
    let mut pack_reader = Cursor::new(header.pack_prefix).chain(reader);
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(true, max_input_size),
        cancel,
        |_| {},
    )?;
    Ok((
        shallow,
        RawPackInstallResult {
            object_ids: result.object_ids,
        },
    ))
}

pub fn install_upload_pack_shallow_packfile_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    R: Read,
{
    install_upload_pack_shallow_packfile_promisor_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        max_input_size,
        CancelFlag::never(),
        None,
    )
}

/// Shallow promisor sideband pack install with cooperative cancellation.
pub fn install_upload_pack_shallow_packfile_promisor_response_from_reader_with_cancel<R>(
    format: ObjectFormat,
    reader: &mut R,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<(Vec<ProtocolV2FetchShallowInfo>, RawPackInstallResult)>
where
    R: Read,
{
    install_upload_pack_shallow_sideband_response_from_reader_with_cancel(
        format,
        reader,
        destination,
        true,
        max_input_size,
        cancel,
        remote_progress,
    )
}

pub fn install_protocol_v2_fetch_response_from_reader<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &I,
    max_input_size: Option<u64>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    I: RawPackInstaller,
    R: Read,
{
    install_protocol_v2_fetch_response_from_reader_with_cancel(
        format,
        reader,
        sideband_all,
        destination,
        max_input_size,
        CancelFlag::never(),
        None,
    )
}

/// Protocol v2 fetch pack install with cooperative cancellation.
///
/// After the metadata header is consumed, demuxes the packfile section as a
/// pure sideband stream (no ACK skip — the `packfile` section marker was
/// already read). Drains trailing progress frames after install so connection
/// reuse matches the buffered path.
pub fn install_protocol_v2_fetch_response_from_reader_with_cancel<I, R>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &I,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    I: RawPackInstaller,
    R: Read,
{
    let header = read_protocol_v2_fetch_response_header(format, reader, sideband_all)?;
    if !header.has_packfile {
        return Ok((header, None));
    }
    let result = install_protocol_v2_packfile_from_reader_with_cancel(
        reader,
        destination,
        false,
        max_input_size,
        cancel,
        remote_progress,
    )?;
    Ok((header, Some(result)))
}

pub fn install_protocol_v2_fetch_promisor_response_from_reader<R>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    R: Read,
{
    install_protocol_v2_fetch_promisor_response_from_reader_with_cancel(
        format,
        reader,
        sideband_all,
        destination,
        max_input_size,
        CancelFlag::never(),
        None,
    )
}

/// Protocol v2 promisor pack install with cooperative cancellation.
pub fn install_protocol_v2_fetch_promisor_response_from_reader_with_cancel<R>(
    format: ObjectFormat,
    reader: &mut R,
    sideband_all: bool,
    destination: &FileObjectDatabase,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<(ProtocolV2FetchResponseHeader, Option<RawPackInstallResult>)>
where
    R: Read,
{
    let header = read_protocol_v2_fetch_response_header(format, reader, sideband_all)?;
    if !header.has_packfile {
        return Ok((header, None));
    }
    let result = install_protocol_v2_packfile_from_reader_with_cancel(
        reader,
        destination,
        true,
        max_input_size,
        cancel,
        remote_progress,
    )?;
    Ok((header, Some(result)))
}

/// Install the packfile section of a protocol v2 fetch response.
///
/// The section header (`packfile\n`) has already been consumed; remaining
/// frames are pure sideband until flush — do not skip upload-pack ACKs.
pub(crate) fn install_protocol_v2_packfile_from_reader_with_cancel<I, R>(
    reader: &mut R,
    destination: &I,
    promisor: bool,
    max_input_size: Option<u64>,
    cancel: CancelFlag<'_>,
    mut remote_progress: Option<RemoteSidebandForward<'_>>,
) -> Result<RawPackInstallResult>
where
    I: RawPackInstaller,
    R: Read,
{
    let mut pack_reader = StreamingSidebandReader::new(reader, move |chunk: &[u8]| {
        if let Some(forward) = remote_progress.as_mut() {
            forward(chunk);
        }
    });
    let result = destination.install_raw_pack_from_reader_with_progress_and_cancel(
        &mut pack_reader,
        raw_pack_install_options(promisor, max_input_size),
        cancel,
        |_| {},
    );
    // B2: never drain on error/cancel — that would download the rest of the pack.
    let result = match result {
        Ok(result) => {
            pack_reader
                .drain_to_end()
                .map_err(map_sideband_stream_io_error)?;
            Ok(result)
        }
        Err(install_err) => Err(map_sideband_install_error(install_err)),
    }?;
    Ok(if promisor {
        RawPackInstallResult {
            object_ids: result.object_ids,
        }
    } else {
        result
    })
}

pub fn shallow_info_from_protocol_v2_fetch_header(
    header: &ProtocolV2FetchResponseHeader,
) -> Vec<ProtocolV2FetchShallowInfo> {
    let mut shallow_info = Vec::new();
    for section in &header.sections {
        if let ProtocolV2FetchResponseSection::ShallowInfo(entries) = section {
            shallow_info.extend(entries.clone());
        }
    }
    shallow_info
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;
    use sley_core::{AtomicCancel, CancelFlag, GitError};
    use sley_object::{EncodedObject, ObjectType};
    use sley_odb::{FileObjectDatabase, ObjectDatabase, ObjectReader};
    use sley_pack::PackFile;
    use sley_protocol::{
        SideBandChannel, SideBandPacket, StreamingSidebandReader, UploadPackAcknowledgment,
        UploadPackPackfileResponse, UploadPackRawPackfileResponse, encode_sideband_packet,
        encode_upload_pack_raw_packfile_response, write_pkt_line_payload,
        write_protocol_v2_fetch_response, write_upload_pack_packfile_response,
        write_upload_pack_raw_packfile_response,
    };
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn raw_upload_pack_response_stream_installs_pack_without_buffering_response() {
        let root = test_temp_root("sley-remote-install-upload-pack-raw-stream-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"raw streamed upload-pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            packfile: pack.pack,
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let result =
            install_upload_pack_raw_response_from_reader(format, &mut reader, &destination, None)
                .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let _ = fs::remove_dir_all(&root);
    }

    fn encode_sideband_upload_pack_pack(pack_bytes: &[u8], chunk_size: usize) -> Vec<u8> {
        let mut sideband = vec![SideBandPacket {
            channel: SideBandChannel::Progress,
            data: b"counting objects\n".to_vec(),
        }];
        for chunk in pack_bytes.chunks(chunk_size.max(1)) {
            sideband.push(SideBandPacket {
                channel: SideBandChannel::Data,
                data: chunk.to_vec(),
            });
        }
        let response = UploadPackPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            sideband,
        };
        let mut encoded = Vec::new();
        write_upload_pack_packfile_response(&mut encoded, &response)
            .expect("response should encode");
        encoded
    }

    #[test]
    fn sideband_upload_pack_response_stream_installs_pack_without_buffering_response() {
        let root = test_temp_root("sley-remote-install-upload-pack-sideband-stream-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(
            ObjectType::Blob,
            b"sideband streamed upload-pack\n".to_vec(),
        );
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        // Chunk the pack across many sideband frames so install cannot rely on a
        // single demuxed buffer.
        let encoded = encode_sideband_upload_pack_pack(&pack.pack, 32);
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let result = install_upload_pack_packfile_response_from_reader(
            format,
            &mut reader,
            &destination,
            None,
        )
        .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let _ = fs::remove_dir_all(&root);
    }

    /// Records `diagnostic` lines so tests can observe remote progress
    /// forwarding without a terminal.
    #[derive(Default)]
    struct RecordingDiagnostics {
        messages: std::cell::RefCell<Vec<String>>,
    }

    impl crate::ProgressSink for RecordingDiagnostics {
        fn diagnostic(&mut self, message: &str) {
            self.messages.borrow_mut().push(message.to_string());
        }
    }

    /// Sideband channel-2 frames must reach the fetch progress sink with git's
    /// `remote:` display prefix (plus the dumb-terminal suffix when stderr is
    /// not a terminal, as in the test harness).
    #[test]
    fn sideband_upload_pack_response_forwards_channel2_progress_to_sink() {
        let root = test_temp_root("sley-remote-install-sideband-progress-forwarding");
        let format = ObjectFormat::Sha1;
        let object =
            EncodedObject::new(ObjectType::Blob, b"sideband progress forwarding\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let encoded = encode_sideband_upload_pack_pack(&pack.pack, 32);
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let mut progress = RecordingDiagnostics::default();
        let result = {
            let installer = ProgressInstaller::new(&destination, &mut progress);
            install_upload_pack_packfile_response_from_reader_with_cancel(
                format,
                &mut reader,
                &installer,
                None,
                CancelFlag::never(),
                Some(&mut installer.remote_sideband_forwarder()),
            )
            .expect("test operation should succeed")
        };

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let messages = progress.messages.borrow();
        assert!(
            messages
                .iter()
                .any(|message| message.starts_with("remote: counting objects")),
            "channel-2 lines must reach the sink with the remote: prefix, got {messages:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sideband_upload_pack_response_cancel_mid_stream() {
        let root = test_temp_root("sley-remote-install-upload-pack-sideband-cancel");
        let format = ObjectFormat::Sha1;
        let objects: Vec<EncodedObject> = (0..12)
            .map(|i| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("sideband cancel fixture {i}\n").into_bytes(),
                )
            })
            .collect();
        let pack = PackFile::write_undeltified(&objects, format).expect("pack should encode");
        let encoded = encode_sideband_upload_pack_pack(&pack.pack, 48);
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let source = AtomicCancel::new();
        let mut pack_reader =
            StreamingSidebandReader::new(encoded.as_slice(), |_: &[u8]| {}).skip_upload_pack_acks();
        let mut saw_object = false;
        let err = destination
            .install_raw_pack_from_reader_with_progress_and_cancel(
                &mut pack_reader,
                sley_odb::RawPackInstallOptions::default(),
                CancelFlag::new(&source),
                |progress| {
                    if progress.indexed_objects >= 1 {
                        saw_object = true;
                        source.cancel();
                    }
                },
            )
            .expect_err("mid-index cancel should fail");

        assert!(
            saw_object,
            "progress should report at least one object before cancel"
        );
        assert_eq!(err, GitError::Cancelled);
        let pack_dir = root.join("objects").join("pack");
        let installed = fs::read_dir(&pack_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack")
                    })
                    .count()
            })
            .unwrap_or_default();
        assert_eq!(installed, 0, "cancelled install must not leave pack files");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sideband_upload_pack_response_drains_trailing_progress() {
        let root = test_temp_root("sley-remote-install-upload-pack-sideband-drain");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(
            ObjectType::Blob,
            b"sideband drain trailing progress\n".to_vec(),
        );
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        // Progress after data + flush terminator: install stops at pack trailer
        // but drain_to_end must consume remaining sideband frames.
        let response = UploadPackPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            sideband: vec![
                SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: pack.pack,
                },
                SideBandPacket {
                    channel: SideBandChannel::Progress,
                    data: b"Resolving deltas: 100%\n".to_vec(),
                },
            ],
        };
        let mut encoded = Vec::new();
        write_upload_pack_packfile_response(&mut encoded, &response)
            .expect("response should encode");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let result = install_upload_pack_packfile_response_from_reader(
            format,
            &mut reader,
            &destination,
            None,
        )
        .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert!(
            reader.is_empty(),
            "sideband install must drain trailing progress through flush"
        );
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shallow_raw_upload_pack_response_stream_installs_pack_without_buffering_response() {
        let root = test_temp_root("sley-remote-install-upload-pack-shallow-raw-stream-install");
        let format = ObjectFormat::Sha1;
        let shallow_oid =
            sley_core::ObjectId::from_hex(format, "1111111111111111111111111111111111111111")
                .expect("test operation should succeed");
        let object =
            EncodedObject::new(ObjectType::Blob, b"shallow streamed upload-pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            packfile: pack.pack,
        };
        let mut encoded = Vec::new();
        write_pkt_line_payload(&mut encoded, format!("shallow {shallow_oid}\n").as_bytes())
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"0000");
        write_upload_pack_raw_packfile_response(&mut encoded, &response)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let (shallow, result) = install_upload_pack_shallow_raw_response_from_reader(
            format,
            &mut reader,
            &destination,
            None,
        )
        .expect("test operation should succeed");

        assert_eq!(
            shallow,
            vec![ProtocolV2FetchShallowInfo::Shallow(shallow_oid)]
        );
        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn raw_upload_pack_response_installs_promisor_pack_sidecar() {
        let root = test_temp_root("sley-remote-install-upload-pack-promisor-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"promisor upload-pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let mut encoded = Vec::new();
        write_upload_pack_raw_packfile_response(&mut encoded, &response)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let result = install_upload_pack_raw_promisor_response_from_reader(
            format,
            &mut reader,
            &destination,
            None,
        )
        .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        assert_promisor_sidecar(&root.join("objects"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_response_packfile_demuxes_and_installs_pack() {
        let root = test_temp_root("sley-remote-install-v2-response-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"v2 response packfile\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let sections = vec![ProtocolV2FetchResponseSection::Packfile(vec![
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Progress,
                data: b"counting objects\n".to_vec(),
            })
            .expect("test operation should succeed"),
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: pack.pack,
            })
            .expect("test operation should succeed"),
        ])];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let (header, result) = install_protocol_v2_fetch_response_from_reader(
            format,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect("test operation should succeed");
        let result = result.expect("packfile should be installed");

        assert!(header.has_packfile);
        assert!(header.sections.is_empty());
        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        assert!(
            reader.is_empty(),
            "v2 install must drain packfile sideband through flush"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_response_cancel_mid_stream() {
        let root = test_temp_root("sley-remote-install-v2-response-cancel");
        let format = ObjectFormat::Sha1;
        let objects: Vec<EncodedObject> = (0..12)
            .map(|i| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("v2 cancel fixture {i}\n").into_bytes(),
                )
            })
            .collect();
        let pack = PackFile::write_undeltified(&objects, format).expect("pack should encode");
        // Chunk pack data so cancel can fire between sideband frames / objects.
        let mut packfile_lines = Vec::new();
        for chunk in pack.pack.chunks(48) {
            packfile_lines.push(
                encode_sideband_packet(&SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: chunk.to_vec(),
                })
                .expect("sideband encode should succeed"),
            );
        }
        let sections = vec![ProtocolV2FetchResponseSection::Packfile(packfile_lines)];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let source = AtomicCancel::new();
        let mut saw_object = false;
        let mut sink = CancelAfterObjectProgress {
            source: &source,
            saw_object: &mut saw_object,
        };
        let err = {
            let installer = ProgressInstaller::new(&destination, &mut sink);
            let mut reader = encoded.as_slice();
            install_protocol_v2_fetch_response_from_reader_with_cancel(
                format,
                &mut reader,
                false,
                &installer,
                None,
                CancelFlag::new(&source),
                None,
            )
            .expect_err("mid-stream cancel should fail")
        };

        assert!(
            saw_object,
            "progress should report at least one object before cancel"
        );
        assert_eq!(err, GitError::Cancelled);
        let pack_dir = root.join("objects").join("pack");
        let installed = fs::read_dir(&pack_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack")
                    })
                    .count()
            })
            .unwrap_or_default();
        assert_eq!(installed, 0, "cancelled install must not leave pack files");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_response_packfile_installs_promisor_sidecar() {
        let root = test_temp_root("sley-remote-install-v2-response-promisor-install");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"v2 promisor packfile\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let sections = vec![ProtocolV2FetchResponseSection::Packfile(vec![
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: pack.pack,
            })
            .expect("test operation should succeed"),
        ])];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();

        let (header, result) = install_protocol_v2_fetch_promisor_response_from_reader(
            format,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect("test operation should succeed");
        let result = result.expect("packfile should be installed");

        assert!(header.has_packfile);
        assert_eq!(result.object_ids, vec![oid]);
        assert_pack_install(&root.join("objects"), &destination, &oid, &object);
        assert_promisor_sidecar(&root.join("objects"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protocol_v2_fetch_response_without_packfile_installs_nothing() {
        let root = test_temp_root("sley-remote-install-v2-response-empty");
        let destination = FileObjectDatabase::new(root.join("objects"), ObjectFormat::Sha1);
        let sections = vec![ProtocolV2FetchResponseSection::Acknowledgments(Vec::new())];
        let mut encoded = Vec::new();
        write_protocol_v2_fetch_response(&mut encoded, &sections)
            .expect("test operation should succeed");
        let mut reader = encoded.as_slice();

        let (header, result) = install_protocol_v2_fetch_response_from_reader(
            ObjectFormat::Sha1,
            &mut reader,
            false,
            &destination,
            None,
        )
        .expect("test operation should succeed");

        assert!(!header.has_packfile);
        assert!(result.is_none());
        assert!(!root.join("objects").join("pack").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn raw_upload_pack_response_rejects_pack_exceeding_max_input_size() {
        let root = test_temp_root("sley-remote-install-upload-pack-max-size");
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"oversized fetch pack\n".to_vec());
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let destination = FileObjectDatabase::new(root.join("objects"), format);
        let mut reader = encoded.as_slice();
        let limit = 32u64;

        let err = install_upload_pack_raw_response_from_reader(
            format,
            &mut reader,
            &destination,
            Some(limit),
        )
        .expect_err("oversized pack should be rejected");

        assert!(
            err.to_string()
                .contains("pack exceeds maximum allowed size"),
            "unexpected error: {err}"
        );
        let pack_dir = root.join("objects").join("pack");
        let installed = fs::read_dir(&pack_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack")
                    })
                    .count()
            })
            .unwrap_or_default();
        assert_eq!(installed, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn install_helpers_accept_custom_raw_pack_installer() {
        #[derive(Default)]
        struct RecordingInstaller {
            packs: std::cell::RefCell<Vec<Vec<u8>>>,
        }

        impl RawPackInstaller for RecordingInstaller {
            fn install_raw_pack_from_reader_with_options<R>(
                &self,
                reader: &mut R,
                _options: RawPackInstallOptions,
            ) -> Result<RawPackInstallResult>
            where
                R: Read,
            {
                let mut pack_bytes = Vec::new();
                reader.read_to_end(&mut pack_bytes)?;
                self.packs.borrow_mut().push(pack_bytes.to_vec());
                Ok(RawPackInstallResult {
                    object_ids: Vec::new(),
                })
            }
        }

        let installer = RecordingInstaller::default();
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: b"PACKcustom".to_vec(),
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let mut reader = encoded.as_slice();

        let result = install_upload_pack_raw_response_from_reader(
            ObjectFormat::Sha1,
            &mut reader,
            &installer,
            None,
        )
        .expect("test operation should succeed");

        assert!(result.object_ids.is_empty());
        assert_eq!(installer.packs.into_inner(), vec![b"PACKcustom".to_vec()]);
    }

    #[test]
    fn raw_upload_pack_response_installs_into_in_memory_database() {
        let format = ObjectFormat::Sha1;
        let object = EncodedObject::new(ObjectType::Blob, b"in memory fetch pack\n".to_vec());
        let oid = object
            .object_id(format)
            .expect("test operation should succeed");
        let pack = PackFile::write_undeltified(std::slice::from_ref(&object), format)
            .expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        let encoded =
            encode_upload_pack_raw_packfile_response(&response).expect("response should encode");
        let destination = ObjectDatabase::new(format);
        let mut reader = encoded.as_slice();

        let result =
            install_upload_pack_raw_response_from_reader(format, &mut reader, &destination, None)
                .expect("test operation should succeed");

        assert_eq!(result.object_ids, vec![oid]);
        assert_eq!(
            destination
                .read_object(&oid)
                .expect("test operation should succeed")
                .as_ref(),
            &object
        );
    }

    #[derive(Default)]
    struct RecordingProgress {
        samples: Vec<TransferProgress>,
    }

    impl ProgressSink for RecordingProgress {
        fn transfer(&mut self, progress: TransferProgress) {
            self.samples.push(progress);
        }
    }

    /// Progress sink that trips `AtomicCancel` after the first object is seen.
    struct CancelAfterObjectProgress<'a> {
        source: &'a AtomicCancel,
        saw_object: &'a mut bool,
    }

    impl ProgressSink for CancelAfterObjectProgress<'_> {
        fn transfer(&mut self, progress: TransferProgress) {
            if progress.received_objects >= 1 {
                *self.saw_object = true;
                self.source.cancel();
            }
        }
    }

    fn multi_object_raw_pack(format: ObjectFormat, count: usize) -> Vec<u8> {
        let objects: Vec<EncodedObject> = (0..count)
            .map(|i| {
                EncodedObject::new(
                    ObjectType::Blob,
                    format!("sley#146 progress fixture object {i}\n").into_bytes(),
                )
            })
            .collect();
        let pack =
            PackFile::write_undeltified(&objects, format).expect("test operation should succeed");
        let response = UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: pack.pack,
        };
        encode_upload_pack_raw_packfile_response(&response).expect("response should encode")
    }

    #[test]
    fn progress_installer_reports_monotonic_transfer() {
        let root = test_temp_root("sley-remote-install-progress");
        let format = ObjectFormat::Sha1;
        let object_count = 24usize;
        let encoded = multi_object_raw_pack(format, object_count);
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        let mut sink = RecordingProgress::default();
        {
            let installer = ProgressInstaller::new(&destination, &mut sink);
            let mut reader = encoded.as_slice();
            install_upload_pack_raw_response_from_reader(format, &mut reader, &installer, None)
                .expect("test operation should succeed");
        }

        let samples = sink.samples;
        assert!(
            !samples.is_empty(),
            "transfer() should be called at least once"
        );
        let total = object_count as u64;

        // total_objects is announced from the pack header on every sample.
        for sample in &samples {
            assert_eq!(sample.total_objects, Some(total));
        }

        // Monotonically non-decreasing byte and object counters.
        for window in samples.windows(2) {
            assert!(
                window[1].received_bytes >= window[0].received_bytes,
                "received_bytes regressed: {} -> {}",
                window[0].received_bytes,
                window[1].received_bytes
            );
            assert!(
                window[1].received_objects >= window[0].received_objects,
                "received_objects regressed: {} -> {}",
                window[0].received_objects,
                window[1].received_objects
            );
        }

        let first_nonzero = samples
            .iter()
            .find(|sample| sample.received_bytes > 0)
            .expect("at least one sample should have received_bytes > 0");
        let last = samples.last().expect("samples is non-empty");

        // Bytes advanced incrementally, not in one final jump.
        assert!(last.received_bytes > 0);
        assert!(
            last.received_bytes > first_nonzero.received_bytes,
            "received_bytes did not advance: first {} vs last {}",
            first_nonzero.received_bytes,
            last.received_bytes
        );

        // Objects advanced incrementally to the announced total.
        assert!(
            samples
                .iter()
                .any(|sample| sample.received_objects > 0 && sample.received_objects < total),
            "expected at least one intermediate object sample"
        );
        assert_eq!(last.received_objects, total);
        assert_eq!(last.total_objects, Some(total));
        assert_eq!(last.total_objects, Some(last.received_objects));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn silent_progress_install_uses_default_no_op_path() {
        let root = test_temp_root("sley-remote-install-progress-silent");
        let format = ObjectFormat::Sha1;
        let encoded = multi_object_raw_pack(format, 8);
        let destination = FileObjectDatabase::new(root.join("objects"), format);

        // The default `ProgressSink::transfer` is a no-op; installing through a
        // ProgressInstaller wrapping SilentProgress must still succeed.
        let mut sink = crate::SilentProgress;
        let result = {
            let installer = ProgressInstaller::new(&destination, &mut sink);
            let mut reader = encoded.as_slice();
            install_upload_pack_raw_response_from_reader(format, &mut reader, &installer, None)
                .expect("test operation should succeed")
        };
        assert_eq!(result.object_ids.len(), 8);

        // And installing directly (no wrapper at all) via the default trait
        // method is unaffected.
        let plain_root = test_temp_root("sley-remote-install-progress-plain");
        let plain_db = FileObjectDatabase::new(plain_root.join("objects"), format);
        let mut reader = encoded.as_slice();
        let plain =
            install_upload_pack_raw_response_from_reader(format, &mut reader, &plain_db, None)
                .expect("test operation should succeed");
        assert_eq!(plain.object_ids.len(), 8);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&plain_root);
    }

    fn test_temp_root(prefix: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            TEST_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn assert_pack_install(
        objects_dir: &Path,
        db: &FileObjectDatabase,
        oid: &sley_core::ObjectId,
        object: &EncodedObject,
    ) {
        assert!(
            !db.loose()
                .object_path(oid)
                .expect("test operation should succeed")
                .exists()
        );
        let pack_dir = objects_dir.join("pack");
        let packs = fs::read_dir(&pack_dir)
            .expect("test operation should succeed")
            .map(|entry| entry.expect("test operation should succeed").path())
            .collect::<Vec<_>>();
        assert!(
            packs
                .iter()
                .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("pack"))
        );
        assert!(
            packs
                .iter()
                .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("idx"))
        );
        assert!(db.contains(oid).expect("test operation should succeed"));
        assert_eq!(
            db.read_object(oid)
                .expect("test operation should succeed")
                .as_ref(),
            object
        );
    }

    fn assert_promisor_sidecar(objects_dir: &Path) {
        let pack_dir = objects_dir.join("pack");
        let promisors = fs::read_dir(&pack_dir)
            .expect("test operation should succeed")
            .map(|entry| entry.expect("test operation should succeed").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("promisor"))
            .collect::<Vec<_>>();
        assert_eq!(promisors.len(), 1);
        assert_eq!(
            fs::read(&promisors[0]).expect("test operation should succeed"),
            b""
        );
    }
}
