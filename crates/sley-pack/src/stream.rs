//! Streaming pack reads for index-pack without buffering the whole pack.
//!
//! Split out of `lib.rs` in the W21 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;

pub(crate) fn index_pack_from_reader<R>(
    reader: &mut R,
    format: ObjectFormat,
    pack_len: u64,
) -> Result<PackStreamIndexBuild>
where
    R: Read,
{
    index_pack_from_stream(PackReadStream::new(reader, format, Some(pack_len))?, format)
}

pub(crate) fn index_pack_from_reader_to_trailer<R>(
    reader: &mut R,
    format: ObjectFormat,
) -> Result<PackStreamIndexBuild>
where
    R: Read,
{
    index_pack_from_stream(PackReadStream::new(reader, format, None)?, format)
}

pub(crate) fn index_pack_from_reader_to_trailer_with_progress_and_cancel<R, F, C>(
    reader: &mut R,
    format: ObjectFormat,
    cancel: &CancelFlag<C>,
    progress: F,
) -> Result<PackStreamIndexBuild>
where
    R: Read,
    F: FnMut(PackStreamProgress),
    C: Cancel,
{
    index_pack_from_stream_with_progress(
        PackReadStream::new(reader, format, None)?,
        format,
        cancel,
        progress,
    )
}

pub(crate) fn index_pack_from_stream<R>(
    stream: PackReadStream<'_, R>,
    format: ObjectFormat,
) -> Result<PackStreamIndexBuild>
where
    R: Read,
{
    index_pack_from_stream_with_progress(stream, format, &CancelFlag::never(), |_| {})
}

/// Approximate cadence for progress emission: report at least every this many
/// pack bytes, matching how git paces "Receiving objects" (no per-object churn).
pub(crate) const PROGRESS_BYTE_STEP: u64 = 256 * 1024;

pub(crate) fn index_pack_from_stream_with_progress<R, F, C>(
    mut stream: PackReadStream<'_, R>,
    format: ObjectFormat,
    cancel: &CancelFlag<C>,
    mut progress: F,
) -> Result<PackStreamIndexBuild>
where
    R: Read,
    F: FnMut(PackStreamProgress),
    C: Cancel,
{
    let mut header = [0u8; 12];
    stream.read_pack_bytes(&mut header)?;
    if &header[..4] != b"PACK" {
        return Err(GitError::InvalidFormat("missing PACK signature".into()));
    }
    let version = u32_be(&header[4..8]);
    if version != 2 && version != 3 {
        return Err(GitError::Unsupported(format!("pack version {version}")));
    }
    let count = u32_be(&header[8..12]) as usize;
    let total_objects = count as u64;
    // Emit an initial sample so the consumer learns `total_objects` (the
    // percentage denominator) as soon as the header is parsed.
    progress(PackStreamProgress {
        received_bytes: stream.pack_offset(),
        received_objects: 0,
        total_objects,
    });
    cancel.check()?;
    // Throttle per-object emission: every ~1% of objects or `PROGRESS_BYTE_STEP`
    // bytes, whichever the loop hits first, plus a guaranteed final sample.
    let object_step = (total_objects / 100).max(1);
    let mut last_emit_bytes = stream.pack_offset();
    let mut last_emit_objects = 0u64;
    let mut parsed_entries = Vec::with_capacity(count);
    let mut raw_entries = Vec::with_capacity(count);
    for index in 0..count {
        cancel.check()?;
        let entry_offset = stream.pack_offset();
        let mut entry_crc = crc32fast::Hasher::new();
        let header = parse_entry_header_from_stream(&mut stream, &mut entry_crc)?;
        let base = match header.kind {
            PackObjectKind::OfsDelta => Some(DeltaBase::Offset(
                parse_ofs_delta_base_offset_from_stream(&mut stream, &mut entry_crc, entry_offset)?,
            )),
            PackObjectKind::RefDelta => {
                let mut raw = vec![0u8; format.raw_len()];
                stream.read_entry_bytes(&mut raw, &mut entry_crc)?;
                Some(DeltaBase::Ref(ObjectId::from_raw(format, &raw)?))
            }
            _ => None,
        };
        let (body, consumed) = inflate_entry_from_stream(
            &mut stream,
            &mut entry_crc,
            header.size.min(usize::MAX as u64) as usize,
        )?;
        if body.len() as u64 != header.size {
            return Err(GitError::InvalidObject(format!(
                "pack object declared {} bytes, decoded {}",
                header.size,
                body.len()
            )));
        }
        if consumed == 0 {
            return Err(GitError::InvalidFormat(
                "empty compressed pack entry".into(),
            ));
        }
        raw_entries.push((entry_offset, entry_crc.finalize()));
        if let Some(base) = base {
            parsed_entries.push(ParsedPackEntry::Delta {
                base,
                compressed_size: consumed as u64,
                delta_size: header.size,
                offset: entry_offset,
                delta: body,
            });
        } else {
            let object_type = pack_object_kind_to_object_type(header.kind)?;
            let object = EncodedObject::new(object_type, body);
            let oid = object.object_id(format)?;
            parsed_entries.push(ParsedPackEntry::Resolved(PackObject {
                entry: PackEntry {
                    oid,
                    compressed_size: consumed as u64,
                    uncompressed_size: header.size,
                    offset: entry_offset,
                },
                object,
            }));
        }
        let received_objects = index as u64 + 1;
        let received_bytes = stream.pack_offset();
        if received_objects == total_objects
            || received_objects - last_emit_objects >= object_step
            || received_bytes - last_emit_bytes >= PROGRESS_BYTE_STEP
        {
            last_emit_objects = received_objects;
            last_emit_bytes = received_bytes;
            progress(PackStreamProgress {
                received_bytes,
                received_objects,
                total_objects,
            });
            cancel.check()?;
        }
    }
    if stream.pack_offset() != stream.trailer_pack_offset() {
        return Err(GitError::InvalidFormat(format!(
            "pack has {} trailing bytes before checksum",
            stream.trailer_pack_offset() - stream.pack_offset()
        )));
    }
    let expected = stream.read_trailer_oid()?;
    let pack_checksum = stream.finish_digest()?;
    if pack_checksum != expected {
        return Err(GitError::InvalidFormat(format!(
            "pack checksum mismatch: expected {expected}, got {pack_checksum}"
        )));
    }

    let resolved = resolve_pack_entries(parsed_entries, format, &mut |_| Ok(None))?;
    let entries = resolved
        .iter()
        .zip(raw_entries)
        .map(|(object, (offset, crc32))| PackIndexEntry {
            oid: object.entry.oid,
            crc32,
            offset,
        })
        .collect::<Vec<_>>();
    let objects = resolved
        .iter()
        .map(|object| PackIndexedObject {
            oid: object.entry.oid,
            object_type: object.object.object_type,
            size: object.object.body.len() as u64,
            offset: object.entry.offset,
        })
        .collect::<Vec<_>>();
    let index = PackIndex::write_v2(format, &entries, &pack_checksum)?;
    Ok(PackStreamIndexBuild {
        index,
        pack_checksum,
        entries,
        objects,
    })
}

pub(crate) fn pack_object_kind_to_object_type(kind: PackObjectKind) -> Result<ObjectType> {
    match kind {
        PackObjectKind::Commit => Ok(ObjectType::Commit),
        PackObjectKind::Tree => Ok(ObjectType::Tree),
        PackObjectKind::Blob => Ok(ObjectType::Blob),
        PackObjectKind::Tag => Ok(ObjectType::Tag),
        PackObjectKind::OfsDelta | PackObjectKind::RefDelta => Err(GitError::InvalidFormat(
            "delta entry cannot be used as an object type".into(),
        )),
    }
}

pub(crate) struct PackReadStream<'a, R> {
    reader: &'a mut R,
    position: u64,
    pack_len: Option<u64>,
    trailer_position: Option<u64>,
    digest: StreamingDigest,
    format: ObjectFormat,
    pending: VecDeque<u8>,
}

impl<'a, R> PackReadStream<'a, R>
where
    R: Read,
{
    pub(crate) fn new(
        reader: &'a mut R,
        format: ObjectFormat,
        pack_len: Option<u64>,
    ) -> Result<Self> {
        let trailer_len = format.raw_len() as u64;
        let trailer_position = pack_len
            .map(|pack_len| {
                if pack_len < 12 + trailer_len {
                    return Err(GitError::InvalidFormat("pack file too short".into()));
                }
                Ok(pack_len - trailer_len)
            })
            .transpose()?;
        Ok(Self {
            reader,
            position: 0,
            pack_len,
            trailer_position,
            digest: StreamingDigest::new(format),
            format,
            pending: VecDeque::new(),
        })
    }

    pub(crate) fn pack_offset(&self) -> u64 {
        self.position
    }

    pub(crate) fn trailer_pack_offset(&self) -> u64 {
        self.trailer_position.unwrap_or(self.position)
    }

    pub(crate) fn read_pack_bytes(&mut self, bytes: &mut [u8]) -> Result<()> {
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
        if self
            .trailer_position
            .is_some_and(|trailer_position| end > trailer_position)
        {
            return Err(GitError::InvalidFormat(
                "pack entry extends past checksum".into(),
            ));
        }
        self.read_exact_raw(bytes)?;
        self.position = end;
        self.digest.update(bytes);
        Ok(())
    }

    pub(crate) fn read_exact_raw(&mut self, bytes: &mut [u8]) -> Result<()> {
        let mut written = 0usize;
        while written < bytes.len() {
            if let Some(byte) = self.pending.pop_front() {
                bytes[written] = byte;
                written += 1;
                continue;
            }
            self.reader.read_exact(&mut bytes[written..])?;
            break;
        }
        Ok(())
    }

    pub(crate) fn read_entry_bytes(
        &mut self,
        bytes: &mut [u8],
        crc: &mut crc32fast::Hasher,
    ) -> Result<()> {
        self.read_pack_bytes(bytes)?;
        crc.update(bytes);
        Ok(())
    }

    pub(crate) fn read_entry_byte(&mut self, crc: &mut crc32fast::Hasher) -> Result<u8> {
        let mut byte = [0u8; 1];
        self.read_entry_bytes(&mut byte, crc)?;
        Ok(byte[0])
    }

    pub(crate) fn read_compressed_chunk(&mut self, bytes: &mut [u8]) -> Result<usize> {
        let len = if let Some(trailer_position) = self.trailer_position {
            if self.position >= trailer_position {
                return Ok(0);
            }
            let remaining = trailer_position - self.position;
            if remaining < bytes.len() as u64 {
                remaining as usize
            } else {
                bytes.len()
            }
        } else {
            bytes.len()
        };
        let mut read = 0usize;
        while read < len {
            let Some(byte) = self.pending.pop_front() else {
                break;
            };
            bytes[read] = byte;
            read += 1;
        }
        if read < len {
            read += self.reader.read(&mut bytes[read..len])?;
        }
        self.position = self
            .position
            .checked_add(read as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
        Ok(read)
    }

    pub(crate) fn accept_compressed_bytes(&mut self, bytes: &[u8], crc: &mut crc32fast::Hasher) {
        self.digest.update(bytes);
        crc.update(bytes);
    }

    pub(crate) fn push_back_compressed_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.position = self
            .position
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
        for byte in bytes.iter().rev() {
            self.pending.push_front(*byte);
        }
        Ok(())
    }

    pub(crate) fn read_trailer_oid(&mut self) -> Result<ObjectId> {
        let mut raw = vec![0u8; self.format.raw_len()];
        self.read_exact_raw(&mut raw)?;
        self.position = self
            .position
            .checked_add(raw.len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
        if let Some(pack_len) = self.pack_len
            && self.position != pack_len
        {
            return Err(GitError::InvalidFormat(format!(
                "pack has {} trailing bytes after checksum",
                pack_len - self.position
            )));
        }
        if self.pack_len.is_none() && !self.pending.is_empty() {
            return Err(GitError::InvalidFormat(
                "pack has trailing bytes after checksum".into(),
            ));
        }
        ObjectId::from_raw(self.format, &raw)
    }

    pub(crate) fn finish_digest(self) -> Result<ObjectId> {
        self.digest.finalize()
    }
}

pub(crate) const STREAM_INFLATE_CHUNK: usize = 32 * 1024;

pub(crate) fn inflate_entry_from_stream<R>(
    stream: &mut PackReadStream<'_, R>,
    crc: &mut crc32fast::Hasher,
    size_hint: usize,
) -> Result<(Vec<u8>, usize)>
where
    R: Read,
{
    INFLATE.with(|cell| {
        let mut decompress = cell.borrow_mut();
        decompress.reset(true);
        let mut out = Vec::with_capacity(inflate::bounded_inflate_reserve(
            size_hint,
            STREAM_INFLATE_CHUNK,
        ));
        let mut compressed_total = 0usize;
        let mut input = [0u8; STREAM_INFLATE_CHUNK];
        loop {
            let read = stream.read_compressed_chunk(&mut input)?;
            if read == 0 {
                return Err(GitError::InvalidObject("truncated zlib stream".into()));
            }
            let mut cursor = 0usize;
            while cursor < read {
                if out.len() == out.capacity() {
                    out.reserve(out.len().max(64));
                }
                let before_in = decompress.total_in();
                let before_out = decompress.total_out();
                let status = decompress
                    .decompress_vec(
                        &input[cursor..read],
                        &mut out,
                        flate2::FlushDecompress::None,
                    )
                    .map_err(|err| {
                        GitError::InvalidObject(format!("zlib inflate failed: {err}"))
                    })?;
                let consumed = (decompress.total_in() - before_in) as usize;
                let produced = decompress.total_out() - before_out;
                if consumed > 0 {
                    let consumed_end = cursor + consumed;
                    stream.accept_compressed_bytes(&input[cursor..consumed_end], crc);
                    compressed_total = compressed_total
                        .checked_add(consumed)
                        .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
                    cursor = consumed_end;
                }
                match status {
                    flate2::Status::StreamEnd => {
                        stream.push_back_compressed_bytes(&input[cursor..read])?;
                        return Ok((out, compressed_total));
                    }
                    _ if consumed == 0 && produced == 0 => {
                        return Err(GitError::InvalidObject("truncated zlib stream".into()));
                    }
                    _ => {}
                }
            }
        }
    })
}

pub(crate) fn parse_entry_header_from_stream<R>(
    stream: &mut PackReadStream<'_, R>,
    crc: &mut crc32fast::Hasher,
) -> Result<EntryHeader>
where
    R: Read,
{
    let first = stream.read_entry_byte(crc)?;
    let mut size = u64::from(first & 0x0f);
    let kind = match (first >> 4) & 0x07 {
        1 => PackObjectKind::Commit,
        2 => PackObjectKind::Tree,
        3 => PackObjectKind::Blob,
        4 => PackObjectKind::Tag,
        6 => PackObjectKind::OfsDelta,
        7 => PackObjectKind::RefDelta,
        other => {
            return Err(GitError::InvalidFormat(format!(
                "invalid pack object type {other}"
            )));
        }
    };
    let mut shift = 4;
    let mut byte = first;
    while byte & 0x80 != 0 {
        byte = stream.read_entry_byte(crc)?;
        let part = u64::from(byte & 0x7f);
        size = size
            .checked_add(
                part.checked_shl(shift)
                    .ok_or_else(|| GitError::InvalidFormat("pack size overflow".into()))?,
            )
            .ok_or_else(|| GitError::InvalidFormat("pack size overflow".into()))?;
        shift += 7;
    }
    Ok(EntryHeader { kind, size })
}

pub(crate) fn parse_ofs_delta_base_offset_from_stream<R>(
    stream: &mut PackReadStream<'_, R>,
    crc: &mut crc32fast::Hasher,
    entry_offset: u64,
) -> Result<u64>
where
    R: Read,
{
    let mut byte = stream.read_entry_byte(crc)?;
    let mut relative = u64::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = stream.read_entry_byte(crc)?;
        relative = relative
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add(u64::from(byte & 0x7f)))
            .ok_or_else(|| GitError::InvalidFormat("ofs-delta offset overflow".into()))?;
    }
    entry_offset
        .checked_sub(relative)
        .ok_or_else(|| GitError::InvalidFormat("ofs-delta points before pack start".into()))
}
