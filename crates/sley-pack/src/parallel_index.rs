//! Parallel pack discovery, inflate, delta resolution, and index construction.
//!
//! Pack entries do not carry their compressed length. The discovery pass scans
//! possible entry starts in parallel, validates each zlib member without
//! retaining its output, and then follows the unique entry chain from byte 12.
//! Selected entries are inflated once more for materialization, also in
//! parallel. Delta bodies are retained only until their dependency level is
//! resolved; resolved object bodies are retained only when another entry names
//! them as a base (or when a caller requests a fully materialized [`PackFile`]).

use super::*;
use flate2::{Decompress, FlushDecompress};

/// Configuration for the one pack indexing engine.
///
/// `threads` is explicit so callers and tests can prove scheduling-independent
/// output. [`Default`] uses all logical CPUs reported by the operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackIndexOptions {
    pub limits: PackReadLimits,
    threads: usize,
}

impl PackIndexOptions {
    pub fn new(limits: PackReadLimits) -> Self {
        Self {
            limits,
            threads: std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1),
        }
    }

    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    pub const fn threads(self) -> usize {
        self.threads
    }
}

impl Default for PackIndexOptions {
    fn default() -> Self {
        Self::new(PackReadLimits::default())
    }
}

#[derive(Debug, Clone)]
struct EntryDescriptor {
    offset: usize,
    data_offset: usize,
    end_offset: usize,
    header: EntryHeader,
    base: Option<DeltaBase>,
}

#[derive(Debug)]
struct ResolvedEntry {
    oid: ObjectId,
    object_type: ObjectType,
    size: u64,
    crc32: u32,
    depth: usize,
    body: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
enum ReadyBase {
    Internal(usize),
    External(ObjectId),
}

#[derive(Debug, Clone, Copy)]
struct ReadyDelta {
    index: usize,
    base: ReadyBase,
    depth: usize,
}

#[derive(Debug, Clone, Copy)]
struct ResolutionSettings {
    format: ObjectFormat,
    options: PackIndexOptions,
    retain_all: bool,
}

pub(crate) fn build_parallel_index<F, P>(
    pack: &[u8],
    format: ObjectFormat,
    external_base: &mut F,
    options: PackIndexOptions,
    cancel: CancelFlag<'_>,
    progress: &mut P,
) -> Result<PackIndexBuild>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    P: FnMut(PackIndexProgress),
{
    let (pack_checksum, descriptors) = discover_pack_entries(pack, format, options, cancel)?;
    let resolved = resolve_entries_parallel(
        pack,
        &descriptors,
        external_base,
        ResolutionSettings {
            format,
            options,
            retain_all: false,
        },
        cancel,
        progress,
    )?;
    finish_index(pack_checksum, &descriptors, resolved, format)
}

pub(crate) fn parse_parallel_pack<F>(
    pack: &[u8],
    format: ObjectFormat,
    external_base: &mut F,
    options: PackIndexOptions,
    cancel: CancelFlag<'_>,
) -> Result<PackFile>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    let (checksum, descriptors) = discover_pack_entries(pack, format, options, cancel)?;
    let resolved = resolve_entries_parallel(
        pack,
        &descriptors,
        external_base,
        ResolutionSettings {
            format,
            options,
            retain_all: true,
        },
        cancel,
        &mut |_| {},
    )?;
    let mut entries = Vec::with_capacity(resolved.len());
    for (descriptor, resolved) in descriptors.iter().zip(resolved) {
        let body = resolved.body.ok_or_else(|| {
            GitError::InvalidObject("parallel pack parse discarded an object body".into())
        })?;
        entries.push(PackObject {
            entry: PackEntry {
                oid: resolved.oid,
                compressed_size: compressed_size(descriptor)?,
                uncompressed_size: resolved.size,
                offset: descriptor.offset as u64,
            },
            object: EncodedObject::new(resolved.object_type, body),
        });
    }
    Ok(PackFile {
        version: u32_be(&pack[4..8]),
        entries,
        checksum,
    })
}

fn finish_index(
    pack_checksum: ObjectId,
    descriptors: &[EntryDescriptor],
    resolved: Vec<ResolvedEntry>,
    format: ObjectFormat,
) -> Result<PackIndexBuild> {
    let entries = descriptors
        .iter()
        .zip(&resolved)
        .map(|(descriptor, resolved)| PackIndexEntry {
            oid: resolved.oid,
            crc32: resolved.crc32,
            offset: descriptor.offset as u64,
        })
        .collect::<Vec<_>>();
    let objects = descriptors
        .iter()
        .zip(&resolved)
        .map(|(descriptor, resolved)| PackIndexedObject {
            oid: resolved.oid,
            object_type: resolved.object_type,
            size: resolved.size,
            offset: descriptor.offset as u64,
        })
        .collect::<Vec<_>>();
    let index = PackIndex::write_v2(format, &entries, &pack_checksum)?;
    Ok(PackIndexBuild {
        index,
        pack_checksum,
        entries,
        objects,
    })
}

fn discover_pack_entries(
    pack: &[u8],
    format: ObjectFormat,
    options: PackIndexOptions,
    cancel: CancelFlag<'_>,
) -> Result<(ObjectId, Vec<EntryDescriptor>)> {
    cancel.check()?;
    let trailer_len = format.raw_len();
    if pack.len() < 12 + trailer_len {
        return Err(GitError::InvalidFormat("pack file too short".into()));
    }
    if &pack[..4] != b"PACK" {
        return Err(GitError::InvalidFormat("missing PACK signature".into()));
    }
    let version = u32_be(&pack[4..8]);
    if version != 2 && version != 3 {
        return Err(GitError::Unsupported(format!("pack version {version}")));
    }
    let trailer_offset = pack.len() - trailer_len;
    let count = checked_pack_object_count(
        u32_be(&pack[8..12]),
        trailer_offset.saturating_sub(12) as u64,
    )?;
    let pack_checksum = sley_core::digest_bytes(format, &pack[..trailer_offset])?;
    let expected = ObjectId::from_raw(format, &pack[trailer_offset..])?;
    if pack_checksum != expected {
        return Err(GitError::InvalidFormat(format!(
            "pack checksum mismatch: expected {expected}, got {pack_checksum}"
        )));
    }
    if count == 0 {
        if trailer_offset != 12 {
            return Err(GitError::InvalidFormat(format!(
                "empty pack has {} trailing bytes before checksum",
                trailer_offset - 12
            )));
        }
        return Ok((pack_checksum, Vec::new()));
    }

    #[cfg(feature = "fetch-profile")]
    let _inflate_span =
        sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::Inflate);
    let candidates = scan_candidates_parallel(
        pack,
        format,
        trailer_offset,
        options.threads.min(count.max(1)),
        cancel,
    )?;
    #[cfg(feature = "fetch-profile")]
    drop(_inflate_span);

    let mut descriptors = Vec::with_capacity(pack_entry_prealloc(count));
    let mut candidate_index = 0usize;
    let mut offset = 12usize;
    for _ in 0..count {
        while candidate_index < candidates.len() && candidates[candidate_index].offset < offset {
            candidate_index += 1;
        }
        let descriptor = candidates
            .get(candidate_index)
            .filter(|candidate| candidate.offset == offset)
            .ok_or_else(|| {
                GitError::InvalidObject(format!(
                    "pack entry at offset {offset} is not a valid zlib member"
                ))
            })?
            .clone();
        if descriptor.end_offset <= descriptor.offset {
            return Err(GitError::InvalidFormat(
                "empty compressed pack entry".into(),
            ));
        }
        offset = descriptor.end_offset;
        descriptors.push(descriptor);
        candidate_index += 1;
    }
    if offset != trailer_offset {
        let detail = if offset < trailer_offset {
            format!("{} trailing bytes before checksum", trailer_offset - offset)
        } else {
            "entry extends past checksum".into()
        };
        return Err(GitError::InvalidFormat(format!("pack has {detail}")));
    }
    Ok((pack_checksum, descriptors))
}

fn scan_candidates_parallel(
    pack: &[u8],
    format: ObjectFormat,
    trailer_offset: usize,
    requested_threads: usize,
    cancel: CancelFlag<'_>,
) -> Result<Vec<EntryDescriptor>> {
    let possible_starts = trailer_offset.saturating_sub(12);
    let worker_count = requested_threads.max(1).min(possible_starts.max(1));
    let chunk_len = possible_starts.div_ceil(worker_count);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for worker in 0..worker_count {
            let start = 12 + worker * chunk_len;
            let end = (start + chunk_len).min(trailer_offset);
            if start >= end {
                continue;
            }
            handles.push(scope.spawn(move || {
                scan_candidate_range(pack, format, trailer_offset, start, end, cancel)
            }));
        }
        let mut candidates = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok(mut worker_candidates)) => candidates.append(&mut worker_candidates),
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    return Err(GitError::InvalidObject(
                        "parallel pack discovery worker panicked".into(),
                    ));
                }
            }
        }
        Ok(candidates)
    })
}

fn scan_candidate_range(
    pack: &[u8],
    format: ObjectFormat,
    trailer_offset: usize,
    start: usize,
    end: usize,
    cancel: CancelFlag<'_>,
) -> Result<Vec<EntryDescriptor>> {
    let mut candidates = Vec::new();
    let mut decompress = Decompress::new(true);
    let mut output = vec![0u8; 64 * 1024];
    for offset in start..end {
        if offset & 0xfff == 0 {
            cancel.check()?;
        }
        let Some(mut descriptor) = candidate_header(pack, format, trailer_offset, offset) else {
            continue;
        };
        let expected = match usize::try_from(descriptor.header.size) {
            Ok(expected) => expected,
            Err(_) => continue,
        };
        let Some(consumed) = measure_zlib_member(
            &mut decompress,
            &pack[descriptor.data_offset..trailer_offset],
            expected,
            &mut output,
            cancel,
        )?
        else {
            continue;
        };
        let Some(end_offset) = descriptor.data_offset.checked_add(consumed) else {
            continue;
        };
        if consumed == 0 || end_offset > trailer_offset {
            continue;
        }
        descriptor.end_offset = end_offset;
        candidates.push(descriptor);
    }
    cancel.check()?;
    Ok(candidates)
}

fn candidate_header(
    pack: &[u8],
    format: ObjectFormat,
    trailer_offset: usize,
    offset: usize,
) -> Option<EntryDescriptor> {
    let first = *pack.get(offset)?;
    let kind = match (first >> 4) & 0x07 {
        1 => PackObjectKind::Commit,
        2 => PackObjectKind::Tree,
        3 => PackObjectKind::Blob,
        4 => PackObjectKind::Tag,
        6 => PackObjectKind::OfsDelta,
        7 => PackObjectKind::RefDelta,
        _ => return None,
    };
    let mut cursor = offset + 1;
    let mut byte = first;
    let mut size = u64::from(first & 0x0f);
    let mut shift = 4u32;
    while byte & 0x80 != 0 {
        byte = *pack.get(cursor)?;
        cursor = cursor.checked_add(1)?;
        let part = u64::from(byte & 0x7f).checked_shl(shift)?;
        size = size.checked_add(part)?;
        shift = shift.checked_add(7)?;
        if shift > 67 {
            return None;
        }
    }
    let base = match kind {
        PackObjectKind::OfsDelta => {
            let mut base_cursor = cursor;
            let base = parse_ofs_delta_base_offset(pack, &mut base_cursor, offset as u64).ok()?;
            cursor = base_cursor;
            Some(DeltaBase::Offset(base))
        }
        PackObjectKind::RefDelta => {
            let end = cursor.checked_add(format.raw_len())?;
            if end > trailer_offset {
                return None;
            }
            let oid = ObjectId::from_raw(format, pack.get(cursor..end)?).ok()?;
            cursor = end;
            Some(DeltaBase::Ref(oid))
        }
        _ => None,
    };
    if cursor >= trailer_offset || !is_zlib_header(pack.get(cursor..cursor.checked_add(2)?)?) {
        return None;
    }
    Some(EntryDescriptor {
        offset,
        data_offset: cursor,
        end_offset: 0,
        header: EntryHeader { kind, size },
        base,
    })
}

fn is_zlib_header(bytes: &[u8]) -> bool {
    let cmf = bytes[0];
    let flg = bytes[1];
    cmf & 0x0f == 8
        && cmf >> 4 <= 7
        && flg & 0x20 == 0
        && u16::from_be_bytes([cmf, flg]).is_multiple_of(31)
}

fn measure_zlib_member(
    decompress: &mut Decompress,
    compressed: &[u8],
    expected: usize,
    output: &mut [u8],
    cancel: CancelFlag<'_>,
) -> Result<Option<usize>> {
    decompress.reset(true);
    let mut input = compressed;
    loop {
        cancel.check()?;
        let before_in = decompress.total_in();
        let before_out = decompress.total_out();
        let status = match decompress.decompress(input, output, FlushDecompress::None) {
            Ok(status) => status,
            Err(_) => return Ok(None),
        };
        let consumed = (decompress.total_in() - before_in) as usize;
        let produced = (decompress.total_out() - before_out) as usize;
        if decompress.total_out() > expected as u64 {
            return Ok(None);
        }
        input = match input.get(consumed..) {
            Some(remaining) => remaining,
            None => return Ok(None),
        };
        match status {
            flate2::Status::StreamEnd if decompress.total_out() == expected as u64 => {
                return Ok(Some(decompress.total_in() as usize));
            }
            flate2::Status::StreamEnd => return Ok(None),
            _ if consumed == 0 && produced == 0 => return Ok(None),
            _ => {}
        }
    }
}

fn resolve_entries_parallel<F, P>(
    pack: &[u8],
    descriptors: &[EntryDescriptor],
    external_base: &mut F,
    settings: ResolutionSettings,
    cancel: CancelFlag<'_>,
    progress: &mut P,
) -> Result<Vec<ResolvedEntry>>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    P: FnMut(PackIndexProgress),
{
    let mut offset_to_index = HashMap::with_capacity(descriptors.len());
    for (index, descriptor) in descriptors.iter().enumerate() {
        offset_to_index.insert(descriptor.offset as u64, index);
    }
    let mut ofs_bases = HashSet::new();
    let mut ref_bases = HashSet::new();
    for descriptor in descriptors {
        match descriptor.base {
            Some(DeltaBase::Offset(offset)) => {
                let index = offset_to_index.get(&offset).copied().ok_or_else(|| {
                    GitError::InvalidFormat(format!("ofs-delta base offset {offset} not found"))
                })?;
                ofs_bases.insert(index);
            }
            Some(DeltaBase::Ref(oid)) => {
                ref_bases.insert(oid);
            }
            None => {}
        }
    }

    let base_indices = descriptors
        .iter()
        .enumerate()
        .filter_map(|(index, descriptor)| descriptor.base.is_none().then_some(index))
        .collect::<Vec<_>>();
    let mut resolved: Vec<Option<ResolvedEntry>> = std::iter::repeat_with(|| None)
        .take(descriptors.len())
        .collect();

    #[cfg(feature = "fetch-profile")]
    let _inflate_span =
        sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::Inflate);
    let mut completed = 0u64;
    let base_results = parallel_chunks(
        &base_indices,
        settings.options.threads,
        |indices| {
            let mut output = Vec::with_capacity(indices.len());
            for &index in indices {
                let descriptor = &descriptors[index];
                let body = inflate_descriptor(pack, descriptor, cancel)?;
                let object_type = object_type_for_kind(descriptor.header.kind)?;
                let oid = cancellable_object_id_bytes(object_type, &body, settings.format, cancel)?;
                let keep_body =
                    settings.retain_all || ofs_bases.contains(&index) || ref_bases.contains(&oid);
                output.push((
                    index,
                    ResolvedEntry {
                        oid,
                        object_type,
                        size: body.len() as u64,
                        crc32: crc32fast::hash(&pack[descriptor.offset..descriptor.end_offset]),
                        depth: 0,
                        body: keep_body.then_some(body),
                    },
                ));
            }
            Ok(output)
        },
        |batch_len| {
            completed = completed.saturating_add(batch_len as u64);
            progress(PackIndexProgress {
                completed_objects: completed,
                total_objects: descriptors.len() as u64,
            });
            cancel.check()
        },
    )?;
    #[cfg(feature = "fetch-profile")]
    drop(_inflate_span);

    let mut oid_to_index = HashMap::with_capacity(descriptors.len());
    for (index, entry) in base_results {
        oid_to_index.entry(entry.oid).or_insert(index);
        resolved[index] = Some(entry);
    }
    if descriptors.is_empty() {
        progress(PackIndexProgress::default());
        cancel.check()?;
    }

    let mut unresolved = descriptors.len().saturating_sub(base_indices.len());
    let mut external = HashMap::<ObjectId, EncodedObject>::new();
    let mut external_missing = HashSet::<ObjectId>::new();
    while unresolved != 0 {
        cancel.check()?;
        let mut ready = ready_internal_deltas(
            descriptors,
            &resolved,
            &offset_to_index,
            &oid_to_index,
            settings.options.limits,
        )?;
        if ready.is_empty() {
            ready = ready_external_deltas(
                descriptors,
                &resolved,
                &mut external,
                &mut external_missing,
                external_base,
                settings.format,
                settings.options.limits,
            )?;
        }
        if ready.is_empty() {
            return Err(GitError::Unsupported(
                "unresolved, cyclic, or mis-ordered delta base".into(),
            ));
        }

        #[cfg(feature = "fetch-profile")]
        let _delta_span =
            sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::DeltaResolution);
        let batch_results = parallel_chunks(
            &ready,
            settings.options.threads,
            |tasks| {
                let mut output = Vec::with_capacity(tasks.len());
                for task in tasks {
                    output.push((
                        task.index,
                        resolve_delta_entry(
                            pack,
                            settings.format,
                            &descriptors[task.index],
                            *task,
                            &resolved,
                            &external,
                            settings.retain_all,
                            &ofs_bases,
                            &ref_bases,
                            cancel,
                        )?,
                    ));
                }
                Ok(output)
            },
            |batch_len| {
                completed = completed.saturating_add(batch_len as u64);
                progress(PackIndexProgress {
                    completed_objects: completed,
                    total_objects: descriptors.len() as u64,
                });
                cancel.check()
            },
        )?;
        #[cfg(feature = "fetch-profile")]
        {
            sley_core::fetch_profile::add_count(
                sley_core::fetch_profile::Stage::DeltaResolution,
                batch_results.len() as u64,
            );
            drop(_delta_span);
        }
        for (index, entry) in batch_results {
            oid_to_index.entry(entry.oid).or_insert(index);
            resolved[index] = Some(entry);
            unresolved -= 1;
        }
    }

    resolved
        .into_iter()
        .map(|entry| entry.ok_or_else(|| GitError::InvalidFormat("unresolved pack entry".into())))
        .collect()
}

fn ready_internal_deltas(
    descriptors: &[EntryDescriptor],
    resolved: &[Option<ResolvedEntry>],
    offset_to_index: &HashMap<u64, usize>,
    oid_to_index: &HashMap<ObjectId, usize>,
    limits: PackReadLimits,
) -> Result<Vec<ReadyDelta>> {
    let mut ready = Vec::new();
    for (index, descriptor) in descriptors.iter().enumerate() {
        if resolved[index].is_some() {
            continue;
        }
        let base_index = match descriptor.base {
            Some(DeltaBase::Offset(offset)) => offset_to_index.get(&offset).copied(),
            Some(DeltaBase::Ref(oid)) => oid_to_index.get(&oid).copied(),
            None => None,
        };
        let Some(base_index) = base_index else {
            continue;
        };
        let Some(base) = resolved[base_index].as_ref() else {
            continue;
        };
        let depth = base.depth + 1;
        check_delta_depth(descriptor.offset, depth, limits)?;
        ready.push(ReadyDelta {
            index,
            base: ReadyBase::Internal(base_index),
            depth,
        });
    }
    Ok(ready)
}

fn ready_external_deltas<F>(
    descriptors: &[EntryDescriptor],
    resolved: &[Option<ResolvedEntry>],
    external: &mut HashMap<ObjectId, EncodedObject>,
    external_missing: &mut HashSet<ObjectId>,
    external_base: &mut F,
    format: ObjectFormat,
    limits: PackReadLimits,
) -> Result<Vec<ReadyDelta>>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    let mut ready = Vec::new();
    for (index, descriptor) in descriptors.iter().enumerate() {
        if resolved[index].is_some() {
            continue;
        }
        let Some(DeltaBase::Ref(oid)) = descriptor.base else {
            continue;
        };
        if !external.contains_key(&oid) && !external_missing.contains(&oid) {
            match external_base(&oid)? {
                Some(object) => {
                    let actual = object.object_id(format)?;
                    if actual != oid {
                        return Err(GitError::InvalidObject(format!(
                            "external delta base {oid} hashes to {actual}"
                        )));
                    }
                    external.insert(oid, object);
                }
                None => {
                    external_missing.insert(oid);
                }
            }
        }
        if external.contains_key(&oid) {
            check_delta_depth(descriptor.offset, 1, limits)?;
            ready.push(ReadyDelta {
                index,
                base: ReadyBase::External(oid),
                depth: 1,
            });
        }
    }
    Ok(ready)
}

#[allow(clippy::too_many_arguments)]
fn resolve_delta_entry(
    pack: &[u8],
    format: ObjectFormat,
    descriptor: &EntryDescriptor,
    task: ReadyDelta,
    resolved: &[Option<ResolvedEntry>],
    external: &HashMap<ObjectId, EncodedObject>,
    retain_all: bool,
    ofs_bases: &HashSet<usize>,
    ref_bases: &HashSet<ObjectId>,
    cancel: CancelFlag<'_>,
) -> Result<ResolvedEntry> {
    let (base_type, base_body) = match task.base {
        ReadyBase::Internal(index) => {
            let base = resolved[index]
                .as_ref()
                .ok_or_else(|| GitError::InvalidFormat("delta base is not resolved".into()))?;
            let body = base.body.as_deref().ok_or_else(|| {
                GitError::InvalidFormat("delta base body was released before use".into())
            })?;
            (base.object_type, body)
        }
        ReadyBase::External(oid) => {
            let base = external.get(&oid).ok_or_else(|| {
                GitError::InvalidFormat("external delta base is not available".into())
            })?;
            (base.object_type, base.body.as_slice())
        }
    };
    let delta = inflate_descriptor(pack, descriptor, cancel)?;
    if delta.len() as u64 != descriptor.header.size {
        return Err(GitError::InvalidObject(format!(
            "pack delta declared {} bytes, decoded {}",
            descriptor.header.size,
            delta.len()
        )));
    }
    let plan = plan_pack_delta(base_body, &delta)?;
    let result_size = usize::try_from(plan.result_size)
        .map_err(|_| GitError::InvalidObject("delta result size overflows usize".into()))?;
    let mut body = Vec::new();
    body.try_reserve_exact(result_size)
        .map_err(|_| GitError::InvalidObject("could not allocate delta result".into()))?;
    apply_pack_delta_exact(base_body, &delta, plan, &mut body, cancel)?;
    let oid = cancellable_object_id_bytes(base_type, &body, format, cancel)?;
    let keep_body = retain_all || ofs_bases.contains(&task.index) || ref_bases.contains(&oid);
    Ok(ResolvedEntry {
        oid,
        object_type: base_type,
        size: body.len() as u64,
        crc32: crc32fast::hash(&pack[descriptor.offset..descriptor.end_offset]),
        depth: task.depth,
        body: keep_body.then_some(body),
    })
}

fn check_delta_depth(offset: usize, depth: usize, limits: PackReadLimits) -> Result<()> {
    if depth <= limits.max_delta_depth {
        return Ok(());
    }
    Err(GitError::InvalidFormat(format!(
        "pack delta chain at offset {offset} has observed depth {depth}, which exceeds maximum \
         depth (configured limit {}); raise PackReadLimits::max_delta_depth or run `git repack \
         --depth={}`",
        limits.max_delta_depth, limits.max_delta_depth
    )))
}

fn inflate_descriptor(
    pack: &[u8],
    descriptor: &EntryDescriptor,
    cancel: CancelFlag<'_>,
) -> Result<Vec<u8>> {
    let expected = usize::try_from(descriptor.header.size)
        .map_err(|_| GitError::InvalidObject("pack object size overflows usize".into()))?;
    let (body, consumed) = inflate_exact(
        &pack[descriptor.data_offset..descriptor.end_offset],
        expected,
        cancel,
    )?;
    let expected_consumed = descriptor.end_offset - descriptor.data_offset;
    if consumed != expected_consumed {
        return Err(GitError::InvalidObject(format!(
            "pack entry compressed span changed during parallel inflate: expected \
             {expected_consumed}, consumed {consumed}"
        )));
    }
    Ok(body)
}

fn inflate_exact(
    compressed: &[u8],
    expected: usize,
    cancel: CancelFlag<'_>,
) -> Result<(Vec<u8>, usize)> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(expected)
        .map_err(|_| GitError::InvalidObject("could not allocate pack object".into()))?;
    let mut decompress = Decompress::new(true);
    let mut input = compressed;
    let mut overflow = [0u8; 1];
    loop {
        cancel.check()?;
        let before_in = decompress.total_in();
        let before_out = decompress.total_out();
        let checking_overflow = output.len() == expected;
        let status = if checking_overflow {
            decompress.decompress(input, &mut overflow, FlushDecompress::None)
        } else {
            decompress.decompress_vec(input, &mut output, FlushDecompress::None)
        }
        .map_err(|error| GitError::InvalidObject(format!("zlib inflate failed: {error}")))?;
        let consumed = (decompress.total_in() - before_in) as usize;
        let produced = (decompress.total_out() - before_out) as usize;
        if output.len() > expected || (checking_overflow && produced != 0) {
            return Err(GitError::InvalidObject(format!(
                "pack object declared {expected} bytes, decoded more than {expected}"
            )));
        }
        input = input.get(consumed..).ok_or_else(|| {
            GitError::InvalidObject("zlib consumed beyond pack entry input".into())
        })?;
        match status {
            flate2::Status::StreamEnd if output.len() == expected => {
                return Ok((output, decompress.total_in() as usize));
            }
            flate2::Status::StreamEnd => {
                return Err(GitError::InvalidObject(format!(
                    "pack object declared {expected} bytes, decoded {}",
                    output.len()
                )));
            }
            _ if consumed == 0 && produced == 0 => {
                return Err(GitError::InvalidObject("truncated zlib stream".into()));
            }
            _ => {}
        }
    }
}

fn object_type_for_kind(kind: PackObjectKind) -> Result<ObjectType> {
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

fn cancellable_object_id_bytes(
    object_type: ObjectType,
    body: &[u8],
    format: ObjectFormat,
    cancel: CancelFlag<'_>,
) -> Result<ObjectId> {
    cancel.check()?;
    #[cfg(feature = "fetch-profile")]
    let _oid_span = sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::OidHash);
    let mut digest = StreamingDigest::new(format);
    digest.update(object_type.as_str().as_bytes());
    digest.update(b" ");
    digest.update(body.len().to_string().as_bytes());
    digest.update(b"\0");
    for chunk in body.chunks(256 * 1024) {
        cancel.check()?;
        digest.update(chunk);
    }
    let oid = digest.finalize()?;
    #[cfg(feature = "fetch-profile")]
    {
        sley_core::fetch_profile::add_count(sley_core::fetch_profile::Stage::OidHash, 1);
        sley_core::fetch_profile::add_bytes(
            sley_core::fetch_profile::Stage::OidHash,
            body.len() as u64,
        );
        drop(_oid_span);
    }
    Ok(oid)
}

fn compressed_size(descriptor: &EntryDescriptor) -> Result<u64> {
    u64::try_from(descriptor.end_offset - descriptor.data_offset)
        .map_err(|_| GitError::InvalidFormat("compressed pack entry size overflows u64".into()))
}

fn parallel_chunks<T, U, F, P>(
    items: &[T],
    threads: usize,
    work: F,
    mut batch_complete: P,
) -> Result<Vec<U>>
where
    T: Sync,
    U: Send,
    F: Fn(&[T]) -> Result<Vec<U>> + Sync,
    P: FnMut(usize) -> Result<()>,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = threads.max(1).min(items.len());
    let chunk_len = items.len().div_ceil(worker_count);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for chunk in items.chunks(chunk_len) {
            let work = &work;
            handles.push(scope.spawn(move || work(chunk)));
        }
        let mut output = Vec::with_capacity(items.len());
        for handle in handles {
            match handle.join() {
                Ok(Ok(mut values)) => {
                    batch_complete(values.len())?;
                    output.append(&mut values);
                }
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    return Err(GitError::InvalidObject(
                        "parallel pack worker panicked".into(),
                    ));
                }
            }
        }
        Ok(output)
    })
}
