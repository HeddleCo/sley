//! Pack generation: options, deltified/undeltified writes, compression, and bitmap output.
//!
//! Split out of `lib.rs` in the W21 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;

/// Default sliding-window size used by [`PackFile::write_packed`].
///
/// Each object is compared against up to this many previously emitted
/// candidates of the same type when searching for a small delta. Matches git's
/// default `pack.window`.
pub const DEFAULT_PACK_WINDOW: usize = 10;

/// Default maximum delta chain depth used by [`PackFile::write_packed`].
///
/// A delta may reference a base that is itself a delta; this bounds how long
/// such chains may grow so that reconstructing any object stays cheap and the
/// reader's recursion stays shallow. Matches git's default `pack.depth`.
pub const DEFAULT_PACK_DEPTH: usize = 50;

/// Object-count threshold before pack payload compression is fanned out across
/// worker threads. Below this, thread setup and extra buffering cost more than
/// they save.
pub(crate) const PACK_PARALLEL_COMPRESSION_MIN_OBJECTS: usize = 64;

/// Keep parallel compression bounded. Git gets much of its wall-clock win from
/// using several cores, but unbounded threads can steal cache from delta
/// planning and inflate peak memory on large packs.
pub(crate) const PACK_PARALLEL_COMPRESSION_MAX_THREADS: usize = 4;

/// Streaming pack writes pre-compress only this many ordered entries at a time.
/// This restores CPU parallelism without holding every compressed payload for a
/// large pack in memory at once.
pub(crate) const PACK_STREAM_COMPRESSION_WINDOW_OBJECTS: usize = 256;

/// Options controlling sliding-window delta selection during pack generation.
///
/// Construct with [`PackWriteOptions::new`] (sensible defaults) and adjust with
/// the builder-style setters, or build one directly. Used by
/// [`PackFile::write_packed_with_options`] and [`PackFile::write_thin`].
#[derive(Debug, Clone)]
pub struct PackWriteOptions {
    /// Number of previous same-type candidates each object is deltified
    /// against. Larger windows find better deltas at higher cost.
    pub window: usize,
    /// Maximum delta chain depth. A value of `0` disables deltification.
    pub depth: usize,
    /// When `true`, in-pack deltas are encoded as ofs-deltas (the default and
    /// git's preference). When `false`, in-pack deltas use ref-deltas. Deltas
    /// against external thin-pack bases always use ref-deltas regardless.
    pub prefer_ofs_delta: bool,
    /// External base objects, keyed by object id, that are *not* written into
    /// the pack but may be used as delta bases. Supplying any entries here
    /// produces a thin pack (see [`PackFile::write_thin`]). Empty by default,
    /// yielding a self-contained pack.
    pub thin_bases: HashMap<ObjectId, EncodedObject>,
    /// When `true` (the default), objects are reordered by type and size for
    /// better delta locality. When `false`, the input order is preserved (the
    /// emitted pack lists objects in the order supplied); deltas then only
    /// reference earlier input objects. Reordering is always skipped when
    /// deltification is disabled (`depth == 0`), since it has no effect there.
    pub reorder: bool,
    /// Zlib compression level for pack entry payloads.
    pub compression_level: u32,
}

impl Default for PackWriteOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl PackWriteOptions {
    /// Options with git-compatible defaults: window
    /// [`DEFAULT_PACK_WINDOW`], depth [`DEFAULT_PACK_DEPTH`], ofs-deltas, and
    /// no external thin bases.
    pub fn new() -> Self {
        Self {
            window: DEFAULT_PACK_WINDOW,
            depth: DEFAULT_PACK_DEPTH,
            prefer_ofs_delta: true,
            thin_bases: HashMap::new(),
            reorder: true,
            compression_level: 6,
        }
    }

    /// Set the sliding-window size.
    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window;
        self
    }

    /// Set the maximum delta chain depth (`0` disables deltas).
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Choose whether in-pack deltas use ofs-delta (`true`) or ref-delta
    /// (`false`) base references.
    pub fn with_prefer_ofs_delta(mut self, prefer_ofs_delta: bool) -> Self {
        self.prefer_ofs_delta = prefer_ofs_delta;
        self
    }

    /// Provide the set of external base objects permitted for a thin pack.
    pub fn with_thin_bases(mut self, thin_bases: HashMap<ObjectId, EncodedObject>) -> Self {
        self.thin_bases = thin_bases;
        self
    }

    /// Choose whether objects may be reordered for delta locality (`true`) or
    /// emitted in input order (`false`).
    pub fn with_reorder(mut self, reorder: bool) -> Self {
        self.reorder = reorder;
        self
    }

    /// Set the zlib compression level used for pack entry payloads.
    pub fn with_compression_level(mut self, level: u32) -> Self {
        self.compression_level = level.min(9);
        self
    }
}

impl PackFile {
    pub fn write_undeltified_sha1<T>(objects: &[T]) -> Result<PackWrite>
    where
        T: Borrow<EncodedObject>,
    {
        Self::write_undeltified(objects, ObjectFormat::Sha1)
    }

    /// Write a pack with every object stored undeltified (no delta entries).
    ///
    /// This is the simple, self-contained encoding; objects appear in the given
    /// order. For smaller output that exploits similarity between objects, use
    /// [`PackFile::write_packed`].
    pub fn write_undeltified<T>(objects: &[T], format: ObjectFormat) -> Result<PackWrite>
    where
        T: Borrow<EncodedObject>,
    {
        let options = PackWriteOptions::new().with_depth(0).with_reorder(false);
        Self::write_packed_impl(objects, format, &options)
    }

    /// Write a pack using sliding-window delta selection with git-compatible
    /// defaults (window [`DEFAULT_PACK_WINDOW`], depth [`DEFAULT_PACK_DEPTH`],
    /// ofs-deltas, self-contained).
    ///
    /// Objects are grouped by type and ordered for good deltas, then each is
    /// compared against a window of previously emitted candidates; the smallest
    /// acceptable delta is kept, otherwise the object is stored undeltified. The
    /// result round-trips through [`PackFile::parse`].
    pub fn write_packed<T>(objects: &[T], format: ObjectFormat) -> Result<PackWrite>
    where
        T: Borrow<EncodedObject>,
    {
        Self::write_packed_with_options(objects, format, &PackWriteOptions::new())
    }

    /// Like [`PackFile::write_packed`] but with caller-supplied
    /// [`PackWriteOptions`] (window, depth, base-reference style, and optional
    /// external thin bases).
    pub fn write_packed_with_options<T>(
        objects: &[T],
        format: ObjectFormat,
        options: &PackWriteOptions,
    ) -> Result<PackWrite>
    where
        T: Borrow<EncodedObject>,
    {
        Self::write_packed_impl(objects, format, options)
    }

    /// Like [`PackFile::write_packed`], but uses caller-supplied object ids
    /// instead of re-hashing each object before pack planning.
    ///
    /// This is intended for object-database paths that reached each object by
    /// its id and already trust that id/object mapping. The function validates
    /// id formats and duplicate ids, but it does not re-hash object bodies; use
    /// [`PackFile::write_packed`] when the ids are not already known to be
    /// canonical.
    pub fn write_packed_with_known_ids(
        inputs: &[PackInput<'_>],
        format: ObjectFormat,
    ) -> Result<PackWrite> {
        Self::write_packed_with_known_ids_and_options(inputs, format, &PackWriteOptions::new())
    }

    /// Like [`PackFile::write_packed_with_known_ids`] but with caller-supplied
    /// [`PackWriteOptions`].
    pub fn write_packed_with_known_ids_and_options(
        inputs: &[PackInput<'_>],
        format: ObjectFormat,
        options: &PackWriteOptions,
    ) -> Result<PackWrite> {
        if inputs.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat("too many pack objects".into()));
        }
        let mut objects = Vec::with_capacity(inputs.len());
        let mut object_ids = Vec::with_capacity(inputs.len());
        for input in inputs {
            if input.oid.format() != format {
                return Err(GitError::InvalidObjectId(format!(
                    "pack object id {} uses {}, pack uses {}",
                    input.oid,
                    input.oid.format().name(),
                    format.name()
                )));
            }
            objects.push(input.object);
            object_ids.push(*input.oid);
        }
        Self::write_packed_from_parts(objects, object_ids, format, options)
    }

    pub fn write_packed_with_known_ids_to_writer<W>(
        inputs: &[PackInput<'_>],
        format: ObjectFormat,
        options: &PackWriteOptions,
        writer: &mut W,
    ) -> Result<PackWriteSummary>
    where
        W: Write,
    {
        if inputs.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat("too many pack objects".into()));
        }
        let mut objects = Vec::with_capacity(inputs.len());
        let mut object_ids = Vec::with_capacity(inputs.len());
        for input in inputs {
            if input.oid.format() != format {
                return Err(GitError::InvalidObjectId(format!(
                    "pack object id {} uses {}, pack uses {}",
                    input.oid,
                    input.oid.format().name(),
                    format.name()
                )));
            }
            objects.push(input.object);
            object_ids.push(*input.oid);
        }
        Self::write_packed_from_parts_to_writer(objects, object_ids, format, options, writer)
    }

    /// Write a thin pack: objects may be deltified against `external_bases`
    /// that are *not* included in the pack, referenced by ref-delta to their
    /// object id.
    ///
    /// The receiver must already have (or otherwise obtain) those base objects
    /// and resolve the pack with [`PackFile::parse_thin`]. Window and depth use
    /// the defaults; pass options via [`PackFile::write_packed_with_options`]
    /// with [`PackWriteOptions::with_thin_bases`] for finer control.
    pub fn write_thin<T>(
        objects: &[T],
        format: ObjectFormat,
        external_bases: HashMap<ObjectId, EncodedObject>,
    ) -> Result<PackWrite>
    where
        T: Borrow<EncodedObject>,
    {
        let options = PackWriteOptions::new().with_thin_bases(external_bases);
        Self::write_packed_impl(objects, format, &options)
    }

    pub(crate) fn write_packed_impl<T>(
        objects: &[T],
        format: ObjectFormat,
        options: &PackWriteOptions,
    ) -> Result<PackWrite>
    where
        T: Borrow<EncodedObject>,
    {
        if objects.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat("too many pack objects".into()));
        }
        let objects: Vec<&EncodedObject> = objects.iter().map(Borrow::borrow).collect();

        // Compute object ids up front; they are needed both for the index and,
        // for ref-deltas, inside the pack entries themselves.
        let mut object_ids: Vec<ObjectId> = Vec::with_capacity(objects.len());
        for object in &objects {
            object_ids.push(object.object_id(format)?);
        }
        Self::write_packed_from_parts(objects, object_ids, format, options)
    }

    pub(crate) fn write_packed_from_parts(
        objects: Vec<&EncodedObject>,
        object_ids: Vec<ObjectId>,
        format: ObjectFormat,
        options: &PackWriteOptions,
    ) -> Result<PackWrite> {
        let mut seen = HashSet::with_capacity(object_ids.len());
        for oid in &object_ids {
            if !seen.insert(oid) {
                return Err(GitError::InvalidFormat(format!(
                    "pack contains duplicate object id {oid}"
                )));
            }
        }

        // Validate external thin bases share the pack's hash format.
        for oid in options.thin_bases.keys() {
            if oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "thin pack base object id format does not match pack format".into(),
                ));
            }
        }

        // Decide, for each object, whether it is stored undeltified or as a
        // delta against another object (in-pack or an external thin base), and
        // obtain the emit order. In-pack deltas only ever reference candidates
        // that appear earlier in `order`, so emitting in `order` guarantees a
        // base is always written before any object that deltas against it.
        let (plan, order) = plan_pack_deltas(&objects, &object_ids, options)?;

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());

        let mut index_entries = Vec::with_capacity(objects.len());
        let mut delta_count = 0u32;
        // Pack offset at which each original object index was written, or
        // `None` until it has been emitted.
        let mut written_offsets: Vec<Option<u64>> = vec![None; objects.len()];

        let compressed_payloads =
            compress_planned_payloads(&objects, &plan, &order, options.compression_level)?;

        for (order_pos, &idx) in order.iter().enumerate() {
            let offset = pack.len() as u64;
            let mut entry_bytes = Vec::new();
            match &plan[idx].base {
                PlannedBase::None => {
                    write_entry_header(
                        &mut entry_bytes,
                        objects[idx].object_type,
                        objects[idx].body.len() as u64,
                    );
                }
                PlannedBase::InPack { base_idx, delta } => {
                    delta_count += 1;
                    let base_offset = written_offsets[*base_idx].ok_or_else(|| {
                        GitError::InvalidFormat(
                            "in-pack delta base emitted after dependent object".into(),
                        )
                    })?;
                    if options.prefer_ofs_delta {
                        write_pack_entry_header_kind(&mut entry_bytes, 6, delta.len() as u64);
                        let relative = offset.checked_sub(base_offset).ok_or_else(|| {
                            GitError::InvalidFormat("ofs-delta base offset is after delta".into())
                        })?;
                        write_ofs_delta_offset(&mut entry_bytes, relative)?;
                    } else {
                        write_pack_entry_header_kind(&mut entry_bytes, 7, delta.len() as u64);
                        entry_bytes.extend_from_slice(object_ids[*base_idx].as_bytes());
                    }
                }
                PlannedBase::External { base_oid, delta } => {
                    delta_count += 1;
                    write_pack_entry_header_kind(&mut entry_bytes, 7, delta.len() as u64);
                    entry_bytes.extend_from_slice(base_oid.as_bytes());
                }
            }
            entry_bytes.extend_from_slice(&compressed_payloads[order_pos]);
            let crc32 = crc32fast::hash(&entry_bytes);
            pack.extend_from_slice(&entry_bytes);
            written_offsets[idx] = Some(offset);
            index_entries.push(PackIndexEntry {
                oid: object_ids[idx].clone(),
                crc32,
                offset,
            });
        }

        let checksum = sley_core::digest_bytes(format, &pack)?;
        pack.extend_from_slice(checksum.as_bytes());
        let index = PackIndex::write_v2(format, &index_entries, &checksum)?;
        Ok(PackWrite {
            pack,
            index,
            checksum,
            entries: index_entries,
            delta_count,
        })
    }

    pub(crate) fn write_packed_from_parts_to_writer<W>(
        objects: Vec<&EncodedObject>,
        object_ids: Vec<ObjectId>,
        format: ObjectFormat,
        options: &PackWriteOptions,
        writer: &mut W,
    ) -> Result<PackWriteSummary>
    where
        W: Write,
    {
        let mut seen = HashSet::with_capacity(object_ids.len());
        for oid in &object_ids {
            if !seen.insert(oid) {
                return Err(GitError::InvalidFormat(format!(
                    "pack contains duplicate object id {oid}"
                )));
            }
        }

        for oid in options.thin_bases.keys() {
            if oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "thin pack base object id format does not match pack format".into(),
                ));
            }
        }

        let (plan, order) = plan_pack_deltas(&objects, &object_ids, options)?;
        let mut output = PackDigestWriter::new(writer, format);
        output.write_pack_bytes(b"PACK")?;
        output.write_pack_bytes(&2u32.to_be_bytes())?;
        output.write_pack_bytes(&(objects.len() as u32).to_be_bytes())?;

        let mut index_entries = Vec::with_capacity(objects.len());
        let mut delta_count = 0u32;
        let mut written_offsets: Vec<Option<u64>> = vec![None; objects.len()];

        for order_window in order.chunks(PACK_STREAM_COMPRESSION_WINDOW_OBJECTS) {
            let compressed_payloads = compress_planned_payloads(
                &objects,
                &plan,
                order_window,
                options.compression_level,
            )?;
            for (&idx, compressed_payload) in order_window.iter().zip(&compressed_payloads) {
                let offset = output.position();
                let mut entry_header = Vec::new();
                match &plan[idx].base {
                    PlannedBase::None => {
                        write_entry_header(
                            &mut entry_header,
                            objects[idx].object_type,
                            objects[idx].body.len() as u64,
                        );
                    }
                    PlannedBase::InPack { base_idx, delta } => {
                        delta_count += 1;
                        let base_offset = written_offsets[*base_idx].ok_or_else(|| {
                            GitError::InvalidFormat(
                                "in-pack delta base emitted after dependent object".into(),
                            )
                        })?;
                        if options.prefer_ofs_delta {
                            write_pack_entry_header_kind(&mut entry_header, 6, delta.len() as u64);
                            let relative = offset.checked_sub(base_offset).ok_or_else(|| {
                                GitError::InvalidFormat(
                                    "ofs-delta base offset is after delta".into(),
                                )
                            })?;
                            write_ofs_delta_offset(&mut entry_header, relative)?;
                        } else {
                            write_pack_entry_header_kind(&mut entry_header, 7, delta.len() as u64);
                            entry_header.extend_from_slice(object_ids[*base_idx].as_bytes());
                        }
                    }
                    PlannedBase::External { base_oid, delta } => {
                        delta_count += 1;
                        write_pack_entry_header_kind(&mut entry_header, 7, delta.len() as u64);
                        entry_header.extend_from_slice(base_oid.as_bytes());
                    }
                }
                let mut crc32 = crc32fast::Hasher::new();
                crc32.update(&entry_header);
                crc32.update(compressed_payload);
                output.write_pack_bytes(&entry_header)?;
                output.write_pack_bytes(compressed_payload)?;
                written_offsets[idx] = Some(offset);
                index_entries.push(PackIndexEntry {
                    oid: object_ids[idx],
                    crc32: crc32.finalize(),
                    offset,
                });
            }
        }

        let (checksum, pack_size) = output.finish()?;
        let index = PackIndex::write_v2(format, &index_entries, &checksum)?;
        Ok(PackWriteSummary {
            index,
            checksum,
            entries: index_entries,
            delta_count,
            pack_size,
        })
    }

    pub fn write_undeltified_from_source_to_writer<W, F>(
        object_ids: &[ObjectId],
        format: ObjectFormat,
        options: &PackWriteOptions,
        mut read_object: F,
        writer: &mut W,
    ) -> Result<PackWriteSummary>
    where
        W: Write,
        F: FnMut(&ObjectId) -> Result<Arc<EncodedObject>>,
    {
        let mut seen = HashSet::with_capacity(object_ids.len());
        for oid in object_ids {
            if oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "pack object id format does not match pack format".into(),
                ));
            }
            if !seen.insert(oid) {
                return Err(GitError::InvalidFormat(format!(
                    "pack contains duplicate object id {oid}"
                )));
            }
        }

        let mut output = PackDigestWriter::new(writer, format);
        output.write_pack_bytes(b"PACK")?;
        output.write_pack_bytes(&2u32.to_be_bytes())?;
        output.write_pack_bytes(&(object_ids.len() as u32).to_be_bytes())?;

        let mut index_entries = Vec::with_capacity(object_ids.len());
        for oid_window in object_ids.chunks(PACK_STREAM_COMPRESSION_WINDOW_OBJECTS) {
            let mut objects = Vec::with_capacity(oid_window.len());
            for oid in oid_window {
                objects.push(read_object(oid)?);
            }
            let compressed_payloads =
                compress_undeltified_payloads(&objects, options.compression_level)?;
            for ((oid, object), compressed_payload) in
                oid_window.iter().zip(&objects).zip(&compressed_payloads)
            {
                let offset = output.position();
                let mut entry_header = Vec::new();
                write_entry_header(
                    &mut entry_header,
                    object.object_type,
                    object.body.len() as u64,
                );
                let mut crc32 = crc32fast::Hasher::new();
                crc32.update(&entry_header);
                crc32.update(compressed_payload);
                output.write_pack_bytes(&entry_header)?;
                output.write_pack_bytes(compressed_payload)?;
                index_entries.push(PackIndexEntry {
                    oid: *oid,
                    crc32: crc32.finalize(),
                    offset,
                });
            }
        }

        let (checksum, pack_size) = output.finish()?;
        let index = PackIndex::write_v2(format, &index_entries, &checksum)?;
        Ok(PackWriteSummary {
            index,
            checksum,
            entries: index_entries,
            delta_count: 0,
            pack_size,
        })
    }

    pub fn write_packed_from_source_to_writer<W, F>(
        object_ids: &[ObjectId],
        format: ObjectFormat,
        options: &PackWriteOptions,
        mut read_object: F,
        writer: &mut W,
    ) -> Result<PackWriteSummary>
    where
        W: Write,
        F: FnMut(&ObjectId) -> Result<Arc<EncodedObject>>,
    {
        if object_ids.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat("too many pack objects".into()));
        }

        let mut seen = HashSet::with_capacity(object_ids.len());
        for oid in object_ids {
            if oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "pack object id format does not match pack format".into(),
                ));
            }
            if !seen.insert(*oid) {
                return Err(GitError::InvalidFormat(format!(
                    "pack contains duplicate object id {oid}"
                )));
            }
        }

        for oid in options.thin_bases.keys() {
            if oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "thin pack base object id format does not match pack format".into(),
                ));
            }
        }

        let mut output = PackDigestWriter::new(writer, format);
        output.write_pack_bytes(b"PACK")?;
        output.write_pack_bytes(&2u32.to_be_bytes())?;
        output.write_pack_bytes(&(object_ids.len() as u32).to_be_bytes())?;

        let mut index_entries = Vec::with_capacity(object_ids.len());
        let mut delta_count = 0u32;
        let mut base_horizon: VecDeque<StreamingDeltaBase> = VecDeque::new();

        for oid_window in object_ids.chunks(PACK_STREAM_COMPRESSION_WINDOW_OBJECTS) {
            let mut objects = Vec::with_capacity(oid_window.len());
            for oid in oid_window {
                objects.push(read_object(oid)?);
            }

            let (plan, order) =
                plan_streaming_window_deltas(&objects, oid_window, &base_horizon, options);
            let compressed_payloads = compress_streaming_planned_payloads(
                &objects,
                &plan,
                &order,
                options.compression_level,
            )?;
            let mut written_offsets: Vec<Option<u64>> = vec![None; objects.len()];

            for (&idx, compressed_payload) in order.iter().zip(&compressed_payloads) {
                let offset = output.position();
                let mut entry_header = Vec::new();
                match &plan[idx].base {
                    StreamingPlannedBase::None => {
                        write_entry_header(
                            &mut entry_header,
                            objects[idx].object_type,
                            objects[idx].body.len() as u64,
                        );
                    }
                    StreamingPlannedBase::Current { base_idx, delta } => {
                        delta_count += 1;
                        let base_offset = written_offsets[*base_idx].ok_or_else(|| {
                            GitError::InvalidFormat(
                                "in-pack delta base emitted after dependent object".into(),
                            )
                        })?;
                        if options.prefer_ofs_delta {
                            write_pack_entry_header_kind(&mut entry_header, 6, delta.len() as u64);
                            let relative = offset.checked_sub(base_offset).ok_or_else(|| {
                                GitError::InvalidFormat(
                                    "ofs-delta base offset is after delta".into(),
                                )
                            })?;
                            write_ofs_delta_offset(&mut entry_header, relative)?;
                        } else {
                            write_pack_entry_header_kind(&mut entry_header, 7, delta.len() as u64);
                            entry_header.extend_from_slice(oid_window[*base_idx].as_bytes());
                        }
                    }
                    StreamingPlannedBase::Previous {
                        base_oid,
                        base_offset,
                        delta,
                    } => {
                        delta_count += 1;
                        if options.prefer_ofs_delta {
                            write_pack_entry_header_kind(&mut entry_header, 6, delta.len() as u64);
                            let relative = offset.checked_sub(*base_offset).ok_or_else(|| {
                                GitError::InvalidFormat(
                                    "ofs-delta base offset is after delta".into(),
                                )
                            })?;
                            write_ofs_delta_offset(&mut entry_header, relative)?;
                        } else {
                            write_pack_entry_header_kind(&mut entry_header, 7, delta.len() as u64);
                            entry_header.extend_from_slice(base_oid.as_bytes());
                        }
                    }
                    StreamingPlannedBase::External { base_oid, delta } => {
                        delta_count += 1;
                        write_pack_entry_header_kind(&mut entry_header, 7, delta.len() as u64);
                        entry_header.extend_from_slice(base_oid.as_bytes());
                    }
                }

                let mut crc32 = crc32fast::Hasher::new();
                crc32.update(&entry_header);
                crc32.update(compressed_payload);
                output.write_pack_bytes(&entry_header)?;
                output.write_pack_bytes(compressed_payload)?;
                written_offsets[idx] = Some(offset);
                index_entries.push(PackIndexEntry {
                    oid: oid_window[idx],
                    crc32: crc32.finalize(),
                    offset,
                });

                if options.depth > 0 && options.window > 0 {
                    base_horizon.push_back(StreamingDeltaBase {
                        oid: oid_window[idx],
                        object: Arc::clone(&objects[idx]),
                        offset,
                        depth: plan[idx].depth,
                    });
                    while base_horizon.len() > options.window {
                        base_horizon.pop_front();
                    }
                }
            }
        }

        let (checksum, pack_size) = output.finish()?;
        let index = PackIndex::write_v2(format, &index_entries, &checksum)?;
        Ok(PackWriteSummary {
            index,
            checksum,
            entries: index_entries,
            delta_count,
            pack_size,
        })
    }
}

pub(crate) struct PackDigestWriter<'a, W> {
    writer: &'a mut W,
    digest: StreamingDigest,
    position: u64,
}

impl<'a, W> PackDigestWriter<'a, W>
where
    W: Write,
{
    pub(crate) fn new(writer: &'a mut W, format: ObjectFormat) -> Self {
        Self {
            writer,
            digest: StreamingDigest::new(format),
            position: 0,
        }
    }

    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    pub(crate) fn write_pack_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.digest.update(bytes);
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(ObjectId, u64)> {
        let checksum = self.digest.finalize()?;
        self.writer.write_all(checksum.as_bytes())?;
        self.position = self
            .position
            .checked_add(checksum.as_bytes().len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
        Ok((checksum, self.position))
    }
}
pub(crate) fn compress_planned_payloads(
    objects: &[&EncodedObject],
    plan: &[PlannedEntry],
    order: &[usize],
    compression_level: u32,
) -> Result<Vec<Vec<u8>>> {
    if order.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(1)
        .min(PACK_PARALLEL_COMPRESSION_MAX_THREADS)
        .min(order.len());
    if worker_count <= 1 || order.len() < PACK_PARALLEL_COMPRESSION_MIN_OBJECTS {
        let mut payloads = Vec::with_capacity(order.len());
        for &idx in order {
            payloads.push(compressed_payload(
                planned_payload(objects, plan, idx),
                compression_level,
            )?);
        }
        return Ok(payloads);
    }

    let chunk_len = order.len().div_ceil(worker_count);
    let mut payloads: Vec<Vec<u8>> = std::iter::repeat_with(Vec::new).take(order.len()).collect();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (chunk_idx, chunk) in order.chunks(chunk_len).enumerate() {
            let chunk_start = chunk_idx * chunk_len;
            handles.push(scope.spawn(move || -> Result<Vec<(usize, Vec<u8>)>> {
                let mut chunk_payloads = Vec::with_capacity(chunk.len());
                for (offset, &idx) in chunk.iter().enumerate() {
                    chunk_payloads.push((
                        chunk_start + offset,
                        compressed_payload(planned_payload(objects, plan, idx), compression_level)?,
                    ));
                }
                Ok(chunk_payloads)
            }));
        }

        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(chunk_payloads)) => {
                    if first_error.is_none() {
                        for (pos, payload) in chunk_payloads {
                            payloads[pos] = payload;
                        }
                    }
                }
                Ok(Err(err)) => {
                    first_error.get_or_insert(err);
                }
                Err(_) => {
                    first_error.get_or_insert_with(|| {
                        GitError::InvalidObject("pack compression worker panicked".into())
                    });
                }
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    })?;
    Ok(payloads)
}

pub(crate) fn compress_streaming_planned_payloads(
    objects: &[Arc<EncodedObject>],
    plan: &[StreamingPlannedEntry],
    order: &[usize],
    compression_level: u32,
) -> Result<Vec<Vec<u8>>> {
    if order.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(1)
        .min(PACK_PARALLEL_COMPRESSION_MAX_THREADS)
        .min(order.len());
    if worker_count <= 1 || order.len() < PACK_PARALLEL_COMPRESSION_MIN_OBJECTS {
        let mut payloads = Vec::with_capacity(order.len());
        for &idx in order {
            payloads.push(compressed_payload(
                streaming_planned_payload(objects, plan, idx),
                compression_level,
            )?);
        }
        return Ok(payloads);
    }

    let chunk_len = order.len().div_ceil(worker_count);
    let mut payloads: Vec<Vec<u8>> = std::iter::repeat_with(Vec::new).take(order.len()).collect();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (chunk_idx, chunk) in order.chunks(chunk_len).enumerate() {
            let chunk_start = chunk_idx * chunk_len;
            handles.push(scope.spawn(move || -> Result<Vec<(usize, Vec<u8>)>> {
                let mut chunk_payloads = Vec::with_capacity(chunk.len());
                for (offset, &idx) in chunk.iter().enumerate() {
                    chunk_payloads.push((
                        chunk_start + offset,
                        compressed_payload(
                            streaming_planned_payload(objects, plan, idx),
                            compression_level,
                        )?,
                    ));
                }
                Ok(chunk_payloads)
            }));
        }

        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(chunk_payloads)) => {
                    if first_error.is_none() {
                        for (pos, payload) in chunk_payloads {
                            payloads[pos] = payload;
                        }
                    }
                }
                Ok(Err(err)) => {
                    first_error.get_or_insert(err);
                }
                Err(_) => {
                    first_error.get_or_insert_with(|| {
                        GitError::InvalidObject("pack compression worker panicked".into())
                    });
                }
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    })?;
    Ok(payloads)
}

pub(crate) fn compress_undeltified_payloads(
    objects: &[Arc<EncodedObject>],
    compression_level: u32,
) -> Result<Vec<Vec<u8>>> {
    if objects.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(1)
        .min(PACK_PARALLEL_COMPRESSION_MAX_THREADS)
        .min(objects.len());
    if worker_count <= 1 || objects.len() < PACK_PARALLEL_COMPRESSION_MIN_OBJECTS {
        let mut payloads = Vec::with_capacity(objects.len());
        for object in objects {
            payloads.push(compressed_payload(&object.body, compression_level)?);
        }
        return Ok(payloads);
    }

    let chunk_len = objects.len().div_ceil(worker_count);
    let mut payloads: Vec<Vec<u8>> = std::iter::repeat_with(Vec::new)
        .take(objects.len())
        .collect();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (chunk_idx, chunk) in objects.chunks(chunk_len).enumerate() {
            let chunk_start = chunk_idx * chunk_len;
            handles.push(scope.spawn(move || -> Result<Vec<(usize, Vec<u8>)>> {
                let mut chunk_payloads = Vec::with_capacity(chunk.len());
                for (offset, object) in chunk.iter().enumerate() {
                    chunk_payloads.push((
                        chunk_start + offset,
                        compressed_payload(&object.body, compression_level)?,
                    ));
                }
                Ok(chunk_payloads)
            }));
        }

        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(chunk_payloads)) => {
                    if first_error.is_none() {
                        for (pos, payload) in chunk_payloads {
                            payloads[pos] = payload;
                        }
                    }
                }
                Ok(Err(err)) => {
                    first_error.get_or_insert(err);
                }
                Err(_) => {
                    first_error.get_or_insert_with(|| {
                        GitError::InvalidObject("pack compression worker panicked".into())
                    });
                }
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    })?;
    Ok(payloads)
}

pub(crate) fn streaming_planned_payload<'a>(
    objects: &'a [Arc<EncodedObject>],
    plan: &'a [StreamingPlannedEntry],
    idx: usize,
) -> &'a [u8] {
    match &plan[idx].base {
        StreamingPlannedBase::None => &objects[idx].body,
        StreamingPlannedBase::Current { delta, .. }
        | StreamingPlannedBase::Previous { delta, .. }
        | StreamingPlannedBase::External { delta, .. } => delta,
    }
}

pub(crate) fn planned_payload<'a>(
    objects: &'a [&'a EncodedObject],
    plan: &'a [PlannedEntry],
    idx: usize,
) -> &'a [u8] {
    match &plan[idx].base {
        PlannedBase::None => &objects[idx].body,
        PlannedBase::InPack { delta, .. } | PlannedBase::External { delta, .. } => delta,
    }
}

pub(crate) fn compressed_payload(body: &[u8], compression_level: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_compressed_payload(&mut out, body, compression_level)?;
    Ok(out)
}
pub(crate) fn write_compressed_payload(
    out: &mut Vec<u8>,
    body: &[u8],
    compression_level: u32,
) -> Result<()> {
    let mut compressor = Compress::new(Compression::new(compression_level.min(9)), true);
    out.reserve(zlib_compress_bound(body.len()));
    let status = compressor
        .compress_vec(body, out, FlushCompress::Finish)
        .map_err(|err| GitError::InvalidObject(format!("zlib compression failed: {err}")))?;
    if status != Status::StreamEnd || compressor.total_in() != body.len() as u64 {
        return Err(GitError::InvalidObject(
            "zlib compression did not finish pack entry".into(),
        ));
    }
    Ok(())
}

pub(crate) fn zlib_compress_bound(len: usize) -> usize {
    len.saturating_add(len >> 12)
        .saturating_add(len >> 14)
        .saturating_add(len >> 25)
        .saturating_add(13)
}

pub(crate) fn write_entry_header(out: &mut Vec<u8>, object_type: ObjectType, size: u64) {
    let type_code = match object_type {
        ObjectType::Commit => 1,
        ObjectType::Tree => 2,
        ObjectType::Blob => 3,
        ObjectType::Tag => 4,
    };
    write_pack_entry_header_kind(out, type_code, size);
}

pub(crate) fn write_pack_entry_header_kind(out: &mut Vec<u8>, type_code: u8, mut size: u64) {
    let mut byte = (type_code << 4) | ((size as u8) & 0x0f);
    size >>= 4;
    if size != 0 {
        byte |= 0x80;
    }
    out.push(byte);
    while size != 0 {
        let mut byte = (size as u8) & 0x7f;
        size >>= 7;
        if size != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

pub(crate) fn write_ofs_delta_offset(out: &mut Vec<u8>, relative: u64) -> Result<()> {
    if relative == 0 {
        return Err(GitError::InvalidFormat(
            "ofs-delta relative offset cannot be zero".into(),
        ));
    }
    let mut value = relative;
    let mut bytes = vec![(value & 0x7f) as u8];
    value >>= 7;
    while value != 0 {
        value -= 1;
        bytes.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    bytes.reverse();
    out.extend_from_slice(&bytes);
    Ok(())
}
/// Builder that assembles a reachability bitmap (`.bitmap`) for a pack.
///
/// The writer is constructed from the object layout of a pack (one
/// [`ObjectType`] per object, in pack order) and the pack's trailing checksum.
/// Callers then register one selected commit per [`add_commit`] call, supplying
/// the set of pack positions reachable from that commit. [`build`]/[`write`]
/// produce a [`PackBitmapIndex`] / serialised `.bitmap` bytes matching git's
/// on-disk format (signature `BITM`, version 1).
///
/// [`add_commit`]: PackBitmapWriter::add_commit
/// [`build`]: PackBitmapWriter::build
/// [`write`]: PackBitmapWriter::write
#[derive(Debug, Clone)]
pub struct PackBitmapWriter {
    format: ObjectFormat,
    pack_checksum: ObjectId,
    object_count: u32,
    commit_positions: Vec<u32>,
    tree_positions: Vec<u32>,
    blob_positions: Vec<u32>,
    tag_positions: Vec<u32>,
    name_hash_cache: Option<Vec<u32>>,
    write_lookup_table: bool,
    selected: Vec<SelectedCommit>,
    pseudo_merges: Vec<PackBitmapPseudoMerge>,
}

#[derive(Debug, Clone)]
pub(crate) struct SelectedCommit {
    /// Oid-sorted `.idx` position (what the on-disk entry records). The
    /// commit's pack-order position lives in `reachable` with the rest of the
    /// bits.
    commit_index_position: u32,
    flags: u8,
    reachable: Vec<u32>,
}

impl PackBitmapWriter {
    /// `OBJ_NONE` selection flag: this commit's bitmap is stored in full (no XOR
    /// compression against a previously selected commit). This is the only flag
    /// value this writer emits.
    pub const FLAG_NONE: u8 = 0;

    /// Creates a writer for a pack whose objects (in pack order) have the given
    /// [`ObjectType`]s and whose trailing checksum is `pack_checksum`.
    ///
    /// Returns an error if the pack contains more than `u32::MAX` objects, if
    /// `pack_checksum`'s format does not match `format`, or if any object type
    /// is not one of the four reachable git object kinds.
    pub fn new(
        format: ObjectFormat,
        pack_checksum: ObjectId,
        object_types: &[ObjectType],
    ) -> Result<Self> {
        if object_types.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat(
                "too many objects for a pack bitmap".into(),
            ));
        }
        if pack_checksum.format() != format {
            return Err(GitError::InvalidObjectId(
                "pack checksum format does not match bitmap format".into(),
            ));
        }
        let object_count = object_types.len() as u32;
        let mut commit_positions = Vec::new();
        let mut tree_positions = Vec::new();
        let mut blob_positions = Vec::new();
        let mut tag_positions = Vec::new();
        for (index, object_type) in object_types.iter().enumerate() {
            let position = index as u32;
            match object_type {
                ObjectType::Commit => commit_positions.push(position),
                ObjectType::Tree => tree_positions.push(position),
                ObjectType::Blob => blob_positions.push(position),
                ObjectType::Tag => tag_positions.push(position),
            }
        }
        Ok(Self {
            format,
            pack_checksum,
            object_count,
            commit_positions,
            tree_positions,
            blob_positions,
            tag_positions,
            name_hash_cache: None,
            write_lookup_table: false,
            selected: Vec::new(),
            pseudo_merges: Vec::new(),
        })
    }

    /// Attaches a name-hash cache (one `u32` per object, in pack order). When
    /// set, the written bitmap advertises [`PackBitmapIndex::OPTION_HASH_CACHE`]
    /// and appends the cache after the bitmap entries, exactly as git does.
    ///
    /// Returns an error if the cache length does not equal the object count.
    pub fn with_name_hash_cache(mut self, cache: Vec<u32>) -> Result<Self> {
        if cache.len() != self.object_count as usize {
            return Err(GitError::InvalidFormat(format!(
                "name hash cache has {} entries but pack has {} objects",
                cache.len(),
                self.object_count
            )));
        }
        self.name_hash_cache = Some(cache);
        Ok(self)
    }

    /// Enable the commit lookup-table extension. Each row is derived from the
    /// selected commit entries when the bitmap is serialised.
    pub fn with_lookup_table(mut self, enabled: bool) -> Self {
        self.write_lookup_table = enabled;
        self
    }

    /// Registers a selected commit and the pack positions reachable from it.
    ///
    /// `commit_position` is the *pack-order* position of the commit itself (the
    /// bit-number space); it must reference a commit object and is implicitly
    /// part of the reachable set. `commit_index_position` is the commit's
    /// position in the *oid-sorted* pack index — this is what the on-disk entry
    /// records (upstream `oid_pos`); bits and entry positions live in different
    /// spaces. `reachable` lists the pack-order positions of every object
    /// reachable from the commit (it may include or omit `commit_position`;
    /// duplicates are fine). All positions must be in range. The commit's full
    /// (non-XORed) bitmap is stored.
    pub fn add_commit(
        &mut self,
        commit_position: u32,
        commit_index_position: u32,
        reachable: &[u32],
    ) -> Result<()> {
        if commit_position >= self.object_count {
            return Err(GitError::InvalidFormat(format!(
                "commit position {commit_position} out of range for {} objects",
                self.object_count
            )));
        }
        if commit_index_position >= self.object_count {
            return Err(GitError::InvalidFormat(format!(
                "commit index position {commit_index_position} out of range for {} objects",
                self.object_count
            )));
        }
        if !self.commit_positions.contains(&commit_position) {
            return Err(GitError::InvalidFormat(format!(
                "bitmap commit position {commit_position} is not a commit object"
            )));
        }
        for &position in reachable {
            if position >= self.object_count {
                return Err(GitError::InvalidFormat(format!(
                    "reachable position {position} out of range for {} objects",
                    self.object_count
                )));
            }
        }
        let mut reachable = reachable.to_vec();
        reachable.push(commit_position);
        self.selected.push(SelectedCommit {
            commit_index_position,
            flags: Self::FLAG_NONE,
            reachable,
        });
        Ok(())
    }

    /// Registers a pseudo-merge bitmap. Both `commits` and `reachable` are
    /// positions in the bitmap's bit-numbering order (pack order for a single
    /// pack, pseudo-pack order for a MIDX). Every commit position must refer to
    /// a commit object; every reachable position must be in range.
    pub fn add_pseudo_merge(&mut self, commits: &[u32], reachable: &[u32]) -> Result<()> {
        if commits.is_empty() {
            return Err(GitError::InvalidFormat(
                "pseudo-merge must contain at least one commit".into(),
            ));
        }
        for &position in commits {
            if position >= self.object_count {
                return Err(GitError::InvalidFormat(format!(
                    "pseudo-merge commit position {position} out of range for {} objects",
                    self.object_count
                )));
            }
            if !self.commit_positions.contains(&position) {
                return Err(GitError::InvalidFormat(format!(
                    "pseudo-merge commit position {position} is not a commit object"
                )));
            }
        }
        for &position in reachable {
            if position >= self.object_count {
                return Err(GitError::InvalidFormat(format!(
                    "pseudo-merge reachable position {position} out of range for {} objects",
                    self.object_count
                )));
            }
        }
        self.pseudo_merges.push(PackBitmapPseudoMerge {
            commits: EwahBitmap::from_positions(self.object_count, commits)?,
            bitmap: EwahBitmap::from_positions(self.object_count, reachable)?,
        });
        Ok(())
    }

    /// Builds the in-memory [`PackBitmapIndex`] without serialising it.
    ///
    /// The resulting index always advertises
    /// [`PackBitmapIndex::OPTION_FULL_DAG`] (the four type bitmaps fully cover
    /// the pack) and, when a name-hash cache was attached,
    /// [`PackBitmapIndex::OPTION_HASH_CACHE`].
    pub fn build(&self) -> Result<PackBitmapIndex> {
        let commits = EwahBitmap::from_positions(self.object_count, &self.commit_positions)?;
        let trees = EwahBitmap::from_positions(self.object_count, &self.tree_positions)?;
        let blobs = EwahBitmap::from_positions(self.object_count, &self.blob_positions)?;
        let tags = EwahBitmap::from_positions(self.object_count, &self.tag_positions)?;

        let mut entries = Vec::with_capacity(self.selected.len());
        for selected in &self.selected {
            let bitmap = EwahBitmap::from_positions(self.object_count, &selected.reachable)?;
            entries.push(PackBitmapEntry {
                object_position: selected.commit_index_position,
                xor_offset: 0,
                flags: selected.flags,
                bitmap,
            });
        }

        let mut options = PackBitmapIndex::OPTION_FULL_DAG;
        if self.name_hash_cache.is_some() {
            options |= PackBitmapIndex::OPTION_HASH_CACHE;
        }
        if !self.pseudo_merges.is_empty() {
            options |= PackBitmapIndex::OPTION_PSEUDO_MERGES;
        }
        if self.write_lookup_table {
            options |= PackBitmapIndex::OPTION_LOOKUP_TABLE;
        }

        // The index checksum is only known once the body is serialised; the
        // dedicated `write` path fills it in. `build` reports a placeholder of
        // the correct format so the struct is self-consistent for callers that
        // only need the decoded bitmaps.
        let placeholder_checksum = ObjectId::null(self.format);
        Ok(PackBitmapIndex {
            version: 1,
            format: self.format,
            options,
            pack_checksum: self.pack_checksum.clone(),
            index_checksum: placeholder_checksum,
            type_bitmaps: PackBitmapTypeBitmaps {
                commits,
                trees,
                blobs,
                tags,
            },
            entries,
            pseudo_merges: self.pseudo_merges.clone(),
            lookup_table: self.write_lookup_table,
            name_hash_cache: self.name_hash_cache.clone(),
        })
    }

    /// Builds and serialises the `.bitmap` file, returning the on-disk bytes
    /// (including the trailing index checksum).
    pub fn write(&self) -> Result<Vec<u8>> {
        self.build()?.write()
    }
}

impl PackBitmapIndex {
    /// Serialises this index into git's on-disk `.bitmap` byte layout.
    ///
    /// This is the exact inverse of [`PackBitmapIndex::parse`]: signature
    /// `BITM`, version (u16 BE), options (u16 BE), entry count (u32 BE), the
    /// pack checksum, the four type bitmaps (commits, trees, blobs, tags), each
    /// commit entry (object position, XOR offset, flags, EWAH bitmap), the
    /// optional pseudo-merge extension, the optional name-hash cache, and
    /// finally the trailing index checksum over everything written so far.
    ///
    /// The `index_checksum` field of `self` is ignored and recomputed from the
    /// serialised body. Returns an error for unsupported versions, mismatched
    /// object-id formats, an oversized entry table, or an inconsistent name-hash
    /// cache.
    pub fn write(&self) -> Result<Vec<u8>> {
        if self.version != 1 {
            return Err(GitError::Unsupported(format!(
                "bitmap index version {}",
                self.version
            )));
        }
        let mut options = self.options;
        if !self.pseudo_merges.is_empty() {
            options |= Self::OPTION_PSEUDO_MERGES;
        }
        if self.lookup_table {
            options |= Self::OPTION_LOOKUP_TABLE;
        }
        let known_options = Self::OPTION_FULL_DAG
            | Self::OPTION_HASH_CACHE
            | Self::OPTION_LOOKUP_TABLE
            | Self::OPTION_PSEUDO_MERGES;
        if options & !known_options != 0 {
            return Err(GitError::Unsupported(format!(
                "bitmap index options {:#06x}",
                options & !known_options
            )));
        }
        if self.pack_checksum.format() != self.format {
            return Err(GitError::InvalidObjectId(
                "bitmap pack checksum format does not match index format".into(),
            ));
        }
        if self.entries.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat(
                "too many bitmap index entries".into(),
            ));
        }
        if options & Self::OPTION_PSEUDO_MERGES != 0 && self.pseudo_merges.is_empty() {
            return Err(GitError::InvalidFormat(
                "OPTION_PSEUDO_MERGES set without pseudo-merge records".into(),
            ));
        }
        let want_cache = options & Self::OPTION_HASH_CACHE != 0;
        match (&self.name_hash_cache, want_cache) {
            (Some(_), false) => {
                return Err(GitError::InvalidFormat(
                    "name hash cache present without OPTION_HASH_CACHE".into(),
                ));
            }
            (None, true) => {
                return Err(GitError::InvalidFormat(
                    "OPTION_HASH_CACHE set without a name hash cache".into(),
                ));
            }
            _ => {}
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"BITM");
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&options.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        out.extend_from_slice(self.pack_checksum.as_bytes());

        self.type_bitmaps.commits.append_bytes(&mut out);
        self.type_bitmaps.trees.append_bytes(&mut out);
        self.type_bitmaps.blobs.append_bytes(&mut out);
        self.type_bitmaps.tags.append_bytes(&mut out);

        let mut entry_offsets = Vec::with_capacity(self.entries.len());
        for (idx, entry) in self.entries.iter().enumerate() {
            if entry.xor_offset as usize > idx {
                return Err(GitError::InvalidFormat(
                    "bitmap index entry has invalid XOR offset".into(),
                ));
            }
            entry_offsets.push(out.len() as u64);
            out.extend_from_slice(&entry.object_position.to_be_bytes());
            out.push(entry.xor_offset);
            out.push(entry.flags);
            entry.bitmap.append_bytes(&mut out);
        }

        if !self.pseudo_merges.is_empty() {
            append_bitmap_pseudo_merges(&mut out, &self.pseudo_merges)?;
        }

        if self.lookup_table {
            append_bitmap_lookup_table(&mut out, &self.entries, &entry_offsets)?;
        }

        if let Some(cache) = &self.name_hash_cache {
            for value in cache {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }

        let checksum = sley_core::digest_bytes(self.format, &out)?;
        out.extend_from_slice(checksum.as_bytes());
        Ok(out)
    }
}

fn append_bitmap_lookup_table(
    out: &mut Vec<u8>,
    entries: &[PackBitmapEntry],
    entry_offsets: &[u64],
) -> Result<()> {
    if entries.len() != entry_offsets.len() {
        return Err(GitError::InvalidFormat(
            "bitmap lookup table offset count mismatch".into(),
        ));
    }
    let mut table: Vec<usize> = (0..entries.len()).collect();
    table.sort_by_key(|&index| entries[index].object_position);
    let mut inverse = vec![0u32; entries.len()];
    for (row, &entry_index) in table.iter().enumerate() {
        inverse[entry_index] = row as u32;
    }
    for &entry_index in &table {
        let entry = &entries[entry_index];
        let xor_row = if entry.xor_offset == 0 {
            u32::MAX
        } else {
            let base = entry_index
                .checked_sub(entry.xor_offset as usize)
                .ok_or_else(|| {
                    GitError::InvalidFormat("bitmap lookup table XOR base underflow".into())
                })?;
            inverse[base]
        };
        out.extend_from_slice(&entry.object_position.to_be_bytes());
        out.extend_from_slice(&entry_offsets[entry_index].to_be_bytes());
        out.extend_from_slice(&xor_row.to_be_bytes());
    }
    Ok(())
}

pub(crate) fn append_bitmap_pseudo_merges(
    out: &mut Vec<u8>,
    pseudo_merges: &[PackBitmapPseudoMerge],
) -> Result<()> {
    if pseudo_merges.len() > u32::MAX as usize {
        return Err(GitError::InvalidFormat(
            "too many pseudo-merge bitmap records".into(),
        ));
    }
    let start = out.len();
    let mut pseudo_offsets = Vec::with_capacity(pseudo_merges.len());
    let mut commit_to_offsets: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for merge in pseudo_merges {
        let offset = u64::try_from(out.len())
            .map_err(|_| GitError::InvalidFormat("bitmap file offset overflow".into()))?;
        pseudo_offsets.push(offset);
        for commit_pos in merge.commits.to_positions()? {
            commit_to_offsets
                .entry(commit_pos)
                .or_default()
                .push(offset);
        }
        merge.commits.append_bytes(out);
        merge.bitmap.append_bytes(out);
    }
    if commit_to_offsets.len() > u32::MAX as usize {
        return Err(GitError::InvalidFormat(
            "too many pseudo-merge commits".into(),
        ));
    }

    let lookup_start = out.len();
    let lookup_len = commit_to_offsets
        .len()
        .checked_mul(12)
        .ok_or_else(|| GitError::InvalidFormat("pseudo-merge lookup overflow".into()))?;
    let mut next_extended = u64::try_from(
        lookup_start
            .checked_add(lookup_len)
            .ok_or_else(|| GitError::InvalidFormat("pseudo-merge lookup overflow".into()))?,
    )
    .map_err(|_| GitError::InvalidFormat("bitmap file offset overflow".into()))?;
    let mut rows = Vec::with_capacity(commit_to_offsets.len());
    for (commit_pos, offsets) in commit_to_offsets {
        let extended_offset = if offsets.len() > 1 {
            if next_extended & (1u64 << 63) != 0 {
                return Err(GitError::InvalidFormat(
                    "pseudo-merge extended offset overflow".into(),
                ));
            }
            let offset = next_extended;
            let ext_len = offsets
                .len()
                .checked_mul(8)
                .and_then(|len| len.checked_add(4))
                .ok_or_else(|| {
                    GitError::InvalidFormat("pseudo-merge extended lookup overflow".into())
                })?;
            next_extended = next_extended.checked_add(ext_len as u64).ok_or_else(|| {
                GitError::InvalidFormat("pseudo-merge extended lookup overflow".into())
            })?;
            Some(offset)
        } else {
            None
        };
        rows.push((commit_pos, offsets, extended_offset));
    }

    for (commit_pos, offsets, extended_offset) in &rows {
        out.extend_from_slice(&commit_pos.to_be_bytes());
        match extended_offset {
            Some(offset) => out.extend_from_slice(&(offset | (1u64 << 63)).to_be_bytes()),
            None => out.extend_from_slice(&offsets[0].to_be_bytes()),
        }
    }

    for (_commit_pos, offsets, extended_offset) in &rows {
        if extended_offset.is_none() {
            continue;
        }
        let count = u32::try_from(offsets.len())
            .map_err(|_| GitError::InvalidFormat("pseudo-merge extended lookup overflow".into()))?;
        out.extend_from_slice(&count.to_be_bytes());
        for offset in offsets {
            out.extend_from_slice(&offset.to_be_bytes());
        }
    }

    for offset in &pseudo_offsets {
        out.extend_from_slice(&offset.to_be_bytes());
    }
    out.extend_from_slice(&(pseudo_merges.len() as u32).to_be_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_be_bytes());
    let lookup_relative = lookup_start
        .checked_sub(start)
        .ok_or_else(|| GitError::InvalidFormat("pseudo-merge lookup underflow".into()))?;
    out.extend_from_slice(&(lookup_relative as u64).to_be_bytes());
    let extension_size = out
        .len()
        .checked_sub(start)
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| GitError::InvalidFormat("pseudo-merge extension overflow".into()))?;
    out.extend_from_slice(&(extension_size as u64).to_be_bytes());
    Ok(())
}

/// Convenience wrapper that builds a `.bitmap` file in one call.
///
/// `object_types` lists the [`ObjectType`] of every pack object in pack order,
/// `pack_checksum` is the pack's trailing checksum, and `commits` carries, per
/// selected commit, `(pack_position, index_position, reachable_pack_positions)`
/// (see [`PackBitmapWriter::add_commit`] for the two position spaces). An
/// optional `name_hash_cache` (one entry per object) may be supplied to emit
/// the hash-cache extension.
pub fn write_bitmap(
    format: ObjectFormat,
    pack_checksum: ObjectId,
    object_types: &[ObjectType],
    commits: &[(u32, u32, Vec<u32>)],
    name_hash_cache: Option<Vec<u32>>,
) -> Result<Vec<u8>> {
    let mut writer = PackBitmapWriter::new(format, pack_checksum, object_types)?;
    if let Some(cache) = name_hash_cache {
        writer = writer.with_name_hash_cache(cache)?;
    }
    for (commit_position, commit_index_position, reachable) in commits {
        writer.add_commit(*commit_position, *commit_index_position, reachable)?;
    }
    writer.write()
}
