//! Compatibility entry points for the resolved-object engine.
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

pub(crate) fn build_parallel_index<F, P>(
    pack: &[u8],
    format: ObjectFormat,
    external: &mut F,
    options: PackIndexOptions,
    cancel: CancelFlag<'_>,
    progress: &mut P,
) -> Result<PackIndexBuild>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    P: FnMut(PackIndexProgress),
{
    resolved::resolve_with_base(
        pack,
        format,
        options.threads(),
        ResolveLimits {
            max_delta_depth: options.limits.max_delta_depth,
            ..ResolveLimits::default()
        },
        cancel,
        &|_| Ok::<_, std::convert::Infallible>(()),
        external,
        progress,
    )
    .map_err(resolved::infallible_error)
}

pub(crate) fn parse_parallel_pack<F>(
    pack: &[u8],
    format: ObjectFormat,
    external: &mut F,
    options: PackIndexOptions,
    cancel: CancelFlag<'_>,
) -> Result<PackFile>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    let objects = std::sync::Mutex::new(Vec::new());
    let built = resolved::resolve_with_base(
        pack,
        format,
        options.threads(),
        ResolveLimits {
            max_delta_depth: options.limits.max_delta_depth,
            ..ResolveLimits::default()
        },
        cancel,
        &|object: &ResolvedObject<'_>| {
            objects
                .lock()
                .map_err(|_| GitError::InvalidObject("pack visitor poisoned".into()))?
                .push((
                    object.offset,
                    object.oid,
                    EncodedObject::new(object.object_type, object.body.to_vec()),
                ));
            Ok::<_, GitError>(())
        },
        external,
        &mut |_| {},
    )
    .map_err(|error| match error {
        ResolveError::Pack(error) | ResolveError::Visitor(error) => error,
    })?;
    let mut objects = objects
        .into_inner()
        .map_err(|_| GitError::InvalidObject("pack visitor poisoned".into()))?;
    objects.sort_unstable_by_key(|object| object.0);
    let mut entries = Vec::new();
    for (index, (offset, oid, object)) in objects.into_iter().enumerate() {
        let end = built
            .entries
            .get(index + 1)
            .map_or(pack.len() - format.raw_len(), |next| next.offset as usize);
        let mut cursor = offset as usize;
        let header = parse_entry_header(pack, &mut cursor)?;
        parse_entry_base(pack, &mut cursor, offset, format, header.kind)?;
        entries.push(PackObject {
            entry: PackEntry {
                oid,
                offset,
                compressed_size: (end - cursor) as u64,
                uncompressed_size: object.body.len() as u64,
            },
            object,
        });
    }
    Ok(PackFile {
        version: u32_be(&pack[4..8]),
        entries,
        checksum: built.pack_checksum,
    })
}

pub(crate) fn check_delta_depth(offset: usize, depth: usize, limits: PackReadLimits) -> Result<()> {
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

pub(crate) fn inflate_exact(
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

pub(crate) fn object_type_for_kind(kind: PackObjectKind) -> Result<ObjectType> {
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

pub(crate) fn cancellable_object_id_bytes(
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
