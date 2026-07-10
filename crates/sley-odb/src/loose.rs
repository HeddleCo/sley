use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::{Decompress, FlushDecompress};
use sley_core::{GitError, MissingObjectContext, ObjectFormat, ObjectId, Result};
use sley_object::{EncodedObject, ObjectType, parse_framed_object};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use crate::{ObjectReader, ObjectWriter, unique_temp_path};

use crate::pack::verify_reads_enabled;
use crate::reachability::{loose_object_id_set, loose_object_ids};
use crate::registry::repository_objects_dir;

pub(crate) fn collect_loose_object_ids(
    objects_dir: &Path,
    format: ObjectFormat,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !objects_dir.exists() {
        return Ok(());
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(fanout) = name.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let name = object_entry.file_name();
            let Some(suffix) = name.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            oids.insert(ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?);
        }
    }
    Ok(())
}

pub(crate) fn collect_loose_fanout_object_ids(
    objects_dir: &Path,
    format: ObjectFormat,
    fanout: u8,
    oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    let fanout_hex = format!("{fanout:02x}");
    let fanout_dir = objects_dir.join(&fanout_hex);
    let entries = match fs::read_dir(&fanout_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let hex_len = format.hex_len();
    for object_entry in entries {
        let object_entry = object_entry?;
        let name = object_entry.file_name();
        let Some(suffix) = name.to_str() else {
            continue;
        };
        if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        oids.insert(ObjectId::from_hex(
            format,
            &format!("{fanout_hex}{suffix}"),
        )?);
    }
    Ok(())
}

/// The set of `objects/XX/` fanout directories that actually exist on disk,
/// learned from a single `read_dir(objects/)`. A freshly cloned or repacked
/// repository has zero loose-object fanout dirs (everything is packed), so this
/// lets a loose-presence probe skip the per-fanout `opendir(objects/XX)` that
/// would otherwise miss with ENOENT on every distinct id prefix — the
/// constant-factor loose-probe floor on packed-repo reads. Returns the present
/// fanout bytes (`0x00..=0xff`); a missing `objects/` dir yields the empty set.
pub(crate) fn present_loose_fanouts(objects_dir: &Path) -> Result<HashSet<u8>> {
    let mut present = HashSet::new();
    let entries = match fs::read_dir(objects_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(present),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() != 2 {
            continue;
        }
        let mut bytes = name.bytes();
        let (Some(hi), Some(lo)) = (bytes.next(), bytes.next()) else {
            continue;
        };
        let (Some(hi), Some(lo)) = ((hi as char).to_digit(16), (lo as char).to_digit(16)) else {
            continue;
        };
        // Only count it as a fanout dir if it really is a directory; `git` keeps
        // non-fanout entries (`pack`, `info`) under `objects/` that happen to be
        // dirs too, but those never collide with a two-hex-char name.
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            present.insert(((hi << 4) | lo) as u8);
        }
    }
    Ok(present)
}

#[derive(Debug, Default)]
pub(crate) struct LoosePresenceCache {
    /// Fanout bytes whose `objects/XX/` listing has been folded into `objects`.
    loaded_fanouts: HashSet<u8>,
    objects: HashSet<ObjectId>,
    /// Which of the 256 `objects/XX/` fanout dirs exist on disk, learned from a
    /// single `read_dir(objects/)`. `None` until first queried. A fanout absent
    /// from this set cannot hold a loose object, so its per-fanout `read_dir`
    /// (which would miss with ENOENT) is skipped entirely.
    pub(crate) present_fanouts: Option<HashSet<u8>>,
}

/// Every object id resolvable through a pack (any `.idx` or the
/// multi-pack-index) under `objects_dir/pack`. Used by `--unpacked`
/// filtering: an object is "unpacked" when absent from this set, regardless
/// of a loose copy also existing.
/// Maximum number of bytes git will inflate when reading a loose object's
/// `"<type> <size>\0"` header (git's `MAX_HEADER_LEN` in object-file.c). The NUL
/// terminator must land within this window, so a header of 32 or more non-NUL
/// bytes is rejected as too long.
const MAX_LOOSE_HEADER_LEN: usize = 32;

/// git's exact `error:`-level diagnostic for a loose object whose header overflows
/// `MAX_LOOSE_HEADER_LEN` (object-file.c: `error(_("header for %s too long, exceeds
/// %d bytes"), ...)`). Shared by the header-only and full-read paths so both surface
/// byte-identical text.
fn loose_header_too_long(oid: &ObjectId) -> GitError {
    GitError::InvalidObject(format!(
        "header for {oid} too long, exceeds {MAX_LOOSE_HEADER_LEN} bytes"
    ))
}

/// git's `error:`-level diagnostic when the loose framing header cannot be inflated at
/// all (object-file.c `loose_object_info`, the `ULHR_BAD` arm: `error(_("unable to
/// unpack %s header"), ...)`).
fn loose_unpack_header_failed(oid: &ObjectId) -> GitError {
    GitError::InvalidObject(format!("unable to unpack {oid} header"))
}

/// git-zlib.c's `error("inflate: %s (%s)", ...)` text for an inflate failure whose
/// cause is identifiable from the zlib stream header. The checks mirror zlib's own
/// `inflate()` HEAD-state validation, in order: the FCHECK checksum over CMF+FLG,
/// the compression method, the window size, and the FDICT preset-dictionary bit
/// (zlib reports `Z_NEED_DICT` with a NULL `msg`, which git renders as
/// "(no message)"). Failures past the stream header return `None`: flate2 does not
/// surface zlib's per-case `msg` strings, so no diagnostic is fabricated for them.
fn inflate_header_diagnostic(input: &[u8]) -> Option<&'static str> {
    let [cmf, flg, ..] = *input else { return None };
    if ((u16::from(cmf) << 8) | u16::from(flg)) % 31 != 0 {
        return Some("inflate: data stream error (incorrect header check)");
    }
    if cmf & 0x0f != 8 {
        return Some("inflate: data stream error (unknown compression method)");
    }
    if cmf >> 4 > 7 {
        return Some("inflate: data stream error (invalid window size)");
    }
    if flg & 0x20 != 0 {
        return Some("inflate: needs dictionary (no message)");
    }
    None
}

/// Print the `error: inflate: ...` line git's zlib wrapper emits the moment
/// `inflate()` fails, when the failure is classifiable from the stream header.
fn emit_inflate_diagnostic(input: &[u8]) {
    if let Some(diagnostic) = inflate_header_diagnostic(input) {
        eprintln!("error: {diagnostic}");
    }
}

/// Integrity verdict for a single loose object file, as classified by
/// [`LooseObjectStore::verify_object`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LooseObjectIntegrity {
    /// Inflated, parsed, and re-hashed to its path-derived oid.
    Ok,
    /// Readable and well-formed, but its content hashes to a different oid
    /// (a loose file stored under the wrong path).
    HashMismatch { actual: ObjectId },
    /// Unreadable: corrupt zlib stream, truncated content, or unparseable header.
    /// The `error:`-level diagnostics were already printed to stderr.
    Corrupt,
}

#[derive(Debug, Clone)]
pub struct LooseObjectStore {
    pub(crate) objects_dir: PathBuf,
    format: ObjectFormat,
    /// Lazily-populated set of loose object ids present on disk, mirroring git's
    /// `loose_objects_cache` (object-file.c). A lookup scans the queried
    /// `objects/XX/` fanout once; afterward misses in that fanout are in-memory
    /// checks instead of failed exact-path opens. Shared across
    /// `FileObjectDatabase` clones via `Arc` so a write through one handle is
    /// visible to reads through another; cleared by `refresh_read_cache` so
    /// objects installed out-of-band (fetch, repack) become visible. Writes
    /// extend the set in place rather than invalidating it.
    pub(crate) loose_cache: Arc<Mutex<LoosePresenceCache>>,
}

impl LooseObjectStore {
    pub fn new(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        Self {
            objects_dir: objects_dir.into(),
            format,
            loose_cache: Arc::new(Mutex::new(LoosePresenceCache::default())),
        }
    }

    /// Whether `oid` is present according to the loose-object cache, populating
    /// the cache on first use. Returns `None` when the lock cannot be trusted or
    /// the scan fails; callers should fall back to an exact filesystem probe in
    /// that case so a cache-building problem cannot change read semantics.
    fn cached_loose_presence(&self, oid: &ObjectId) -> Option<bool> {
        let mut guard = self.loose_cache.lock().ok()?;
        let fanout = oid.as_bytes()[0];
        if !guard.loaded_fanouts.contains(&fanout) {
            // Learn (once) which `objects/XX/` dirs exist via a single
            // `read_dir(objects/)`. If this id's fanout dir is absent, no loose
            // object can live there — skip the per-fanout `read_dir` that would
            // otherwise miss with ENOENT. For an all-packed repo (every fanout
            // absent) this collapses the whole loose-probe cost to one
            // `read_dir(objects/)`.
            if guard.present_fanouts.is_none() {
                guard.present_fanouts = Some(present_loose_fanouts(&self.objects_dir).ok()?);
            }
            let fanout_present = guard
                .present_fanouts
                .as_ref()
                .is_some_and(|present| present.contains(&fanout));
            if fanout_present {
                collect_loose_fanout_object_ids(
                    &self.objects_dir,
                    self.format,
                    fanout,
                    &mut guard.objects,
                )
                .ok()?;
            }
            // Mark the fanout loaded regardless: an absent fanout contributes no
            // ids, and the `present_fanouts` set already proved it empty, so we
            // never need to rescan it (a later loose write into a previously
            // absent fanout goes through `note_loose_write`, which records the
            // id directly, or `invalidate_cache`, which clears `present_fanouts`
            // so the next probe re-learns the dir set).
            guard.loaded_fanouts.insert(fanout);
        }
        Some(guard.objects.contains(oid))
    }

    /// Populate the loose-object cache and return the sorted ids. This mirrors
    /// git's `odb_loose_cache` lazy fill and is reserved for operations that
    /// really need loose-object enumeration.
    fn loose_object_ids_cached(&self) -> Result<Vec<ObjectId>> {
        if let Ok(mut guard) = self.loose_cache.lock() {
            guard.objects = loose_object_id_set(&self.objects_dir, self.format)?;
            guard.loaded_fanouts = (0..=u8::MAX).collect();
            let mut ids = guard.objects.iter().copied().collect::<Vec<_>>();
            ids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            return Ok(ids);
        }
        loose_object_ids(&self.objects_dir, self.format)
    }

    /// Record `oid` as present in loose storage so subsequent reads find it
    /// without a rescan. A no-op when the cache has not been populated yet (the
    /// eventual lazy scan will pick the object up) or the lock is poisoned.
    fn note_loose_write(&self, oid: ObjectId) {
        if let Ok(mut guard) = self.loose_cache.lock() {
            // Keep the present-fanout set coherent: writing this object created
            // (or kept) its `objects/XX/` dir, so a sibling id in the same fanout
            // must be scannable on its next probe rather than short-circuited as
            // an absent fanout.
            let fanout = oid.as_bytes()[0];
            if let Some(present) = guard.present_fanouts.as_mut() {
                present.insert(fanout);
            }
            guard.objects.insert(oid);
        }
    }

    /// Drop the in-memory loose set so the next access rescans the fanout. Called
    /// by `FileObjectDatabase::refresh_read_cache` after out-of-band installs.
    pub(crate) fn invalidate_cache(&self) {
        if let Ok(mut guard) = self.loose_cache.lock() {
            *guard = LoosePresenceCache::default();
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        Self::new(repository_objects_dir(git_dir), format)
    }

    fn validate_oid_format(&self, oid: &ObjectId) -> Result<()> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        Ok(())
    }

    pub fn object_path(&self, oid: &ObjectId) -> Result<PathBuf> {
        self.validate_oid_format(oid)?;
        let hex = oid.to_hex();
        Ok(self.objects_dir.join(&hex[..2]).join(&hex[2..]))
    }

    pub fn exists(&self, oid: &ObjectId) -> Result<bool> {
        self.validate_oid_format(oid)?;
        if self.cached_loose_presence(oid) == Some(false) {
            return Ok(false);
        }
        let path = self.object_path(oid)?;
        Ok(path.exists())
    }

    pub fn disk_size(&self, oid: &ObjectId) -> Result<Option<u64>> {
        self.validate_oid_format(oid)?;
        if self.cached_loose_presence(oid) == Some(false) {
            return Ok(None);
        }
        let path = self.object_path(oid)?;
        match fs::metadata(path) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(GitError::Io(err.to_string())),
        }
    }

    /// The object type and content size of `oid` from loose storage, inflating only
    /// the framing header (`"<type> <size>\0"`) and not the body. Output-limited
    /// reads keep miniz from inflating past the header even for large objects.
    /// Returns `Ok(None)` when the loose object is absent.
    pub fn read_header(&self, oid: &ObjectId) -> Result<Option<(ObjectType, u64)>> {
        self.validate_oid_format(oid)?;
        if self.cached_loose_presence(oid) == Some(false) {
            return Ok(None);
        }
        let path = self.object_path(oid)?;
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        match inflate_loose_header(&compressed)? {
            LooseHeader::Ok(header) => {
                let header = std::str::from_utf8(&header)
                    .map_err(|err| GitError::InvalidObject(err.to_string()))?;
                let (kind, size) = header
                    .split_once(' ')
                    .ok_or_else(|| GitError::InvalidObject("missing object size".into()))?;
                let object_type = kind.parse::<ObjectType>()?;
                let size = size
                    .parse::<u64>()
                    .map_err(|_| GitError::InvalidObject("invalid object size".into()))?;
                Ok(Some((object_type, size)))
            }
            LooseHeader::Bad => {
                // git's ULHR_BAD: the zlib wrapper's `error: inflate: ...` line, then
                // "unable to unpack <oid> header".
                emit_inflate_diagnostic(compressed.get(..2).unwrap_or(&compressed));
                Err(loose_unpack_header_failed(oid))
            }
            LooseHeader::TooLong => {
                // git inflates only the first `MAX_LOOSE_HEADER_LEN` bytes
                // (object-file.c `unpack_loose_header`) and reports ULHR_TOO_LONG when
                // no NUL terminator lands within them — whether the stream simply ends
                // early or overflows the window. Both collapse to the same diagnostic.
                Err(loose_header_too_long(oid))
            }
        }
    }

    /// Loose object ids in this store, sorted by hex.
    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        self.loose_object_ids_cached()
    }

    /// fsck's loose-object integrity probe, mirroring C git's `read_loose_object`
    /// (object-file.c) as called from `fsck_loose` (builtin/fsck.c): inflate and
    /// parse the file at `oid`'s loose path, then re-hash its content against the
    /// path-derived oid. `display_path` appears verbatim in the `error:`-level
    /// diagnostics — the path-form messages of `read_loose_object` ("unable to
    /// unpack header of `<path>`"), unlike the oid-form messages of the normal read
    /// path. Returns `Ok(None)` when no loose file exists for `oid`.
    pub fn verify_object(
        &self,
        oid: &ObjectId,
        display_path: &str,
    ) -> Result<Option<LooseObjectIntegrity>> {
        let path = self.object_path(oid)?;
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut framed = Vec::new();
        if decoder.read_to_end(&mut framed).is_err() {
            emit_inflate_diagnostic(&compressed);
            // git inflates the header first (`unpack_loose_header`), then the body
            // (`unpack_loose_rest`). If the header inflated (its NUL is visible in
            // the partial output) but the body broke, that is a *content*
            // corruption: git's `unpack_loose_rest` prints `corrupt loose object
            // '<oid>'` (status != Z_STREAM_END), then `read_loose_object` adds
            // `unable to unpack contents of <path>`. If inflation died before the
            // header materialized, only the header message fires.
            if framed_loose_header_terminated(&framed) {
                eprintln!("error: corrupt loose object '{oid}'");
                eprintln!("error: unable to unpack contents of {display_path}");
            } else {
                eprintln!("error: unable to unpack header of {display_path}");
            }
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        }
        if !framed_loose_header_terminated(&framed) {
            // ULHR_TOO_LONG collapses into the same path-form message here: C's
            // `read_loose_object` treats every non-OK `unpack_loose_header` alike.
            eprintln!("error: unable to unpack header of {display_path}");
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        }
        // git's `unpack_loose_rest`/`check_stream_oid` reject trailing bytes after
        // the zlib stream: a fully-inflated object whose compressed input was not
        // entirely consumed is `garbage at end of loose object '<oid>'`, then
        // `object corrupt or missing: <path>` from `fsck_loose`. (read_to_end
        // stops at Z_STREAM_END and silently ignores the trailing bytes, so we
        // compare consumed input against the file size ourselves.)
        if (decoder.total_in() as usize) < compressed.len() {
            // git's `unpack_loose_rest` prints `garbage at end of loose object`
            // then returns NULL, so `read_loose_object` also prints `unable to
            // unpack contents of <path>`.
            eprintln!("error: garbage at end of loose object '{oid}'");
            eprintln!("error: unable to unpack contents of {display_path}");
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        }
        // A truncated object can inflate to a clean stream end yet yield fewer
        // body bytes than the header's declared size. git's `unpack_loose_rest`
        // inflates exactly `size` bytes and, finding the stream ends short,
        // prints `corrupt loose object '<oid>'`; `read_loose_object` then adds
        // `unable to unpack contents of <path>`. Detect the short body here so it
        // is not misreported as a header-parse failure.
        if let Some(declared) = loose_header_declared_size(&framed) {
            let nul = framed.iter().position(|&b| b == 0).unwrap_or(framed.len());
            let body_len = framed.len() - (nul + 1).min(framed.len());
            if body_len < declared {
                eprintln!("error: corrupt loose object '{oid}'");
                eprintln!("error: unable to unpack contents of {display_path}");
                return Ok(Some(LooseObjectIntegrity::Corrupt));
            }
        }
        let Ok(object) = parse_framed_object(&framed) else {
            // Distinguish git's two header-parse failures: a structurally valid
            // `"<word> <size>\0"` header whose *type word* is not a known object
            // type yields `unable to parse type from header '<header>'`, while a
            // genuinely malformed header yields `unable to parse header`.
            if let Some(header) = loose_header_with_unknown_type(&framed) {
                eprintln!("error: unable to parse type from header '{header}' of {display_path}");
            } else {
                eprintln!("error: unable to parse header of {display_path}");
            }
            return Ok(Some(LooseObjectIntegrity::Corrupt));
        };
        let actual = object.object_id(self.format)?;
        if &actual != oid {
            return Ok(Some(LooseObjectIntegrity::HashMismatch { actual }));
        }
        Ok(Some(LooseObjectIntegrity::Ok))
    }
}

/// Whether the inflated framing bytes contain the header's NUL terminator within
/// git's `MAX_HEADER_LEN` window (object-file.c `unpack_loose_header`'s success
/// condition).
fn framed_loose_header_terminated(framed: &[u8]) -> bool {
    framed
        .iter()
        .take(MAX_LOOSE_HEADER_LEN)
        .any(|byte| *byte == 0)
}

/// If the framing has a structurally valid `"<word> <size>\0"` header whose body
/// length matches `<size>` but whose `<word>` is not a known object type, return
/// the header string (the bytes before the NUL). Mirrors git's
/// `parse_loose_header` reporting `unable to parse type from header '<header>'`.
fn loose_header_with_unknown_type(framed: &[u8]) -> Option<String> {
    let nul = framed.iter().position(|&b| b == 0)?;
    let header = std::str::from_utf8(&framed[..nul]).ok()?;
    let (kind, size) = header.split_once(' ')?;
    let size: usize = size.parse().ok()?;
    // Body length must match the declared size (otherwise it is a different
    // corruption, handled by the generic path).
    if framed.len() - (nul + 1) != size {
        return None;
    }
    // A known type word would have parsed successfully upstream; only return
    // when the word is genuinely unknown.
    if kind.parse::<ObjectType>().is_ok() {
        return None;
    }
    Some(header.to_string())
}

/// The size declared in a loose object's `"<type> <size>\0"` header, if the
/// header is structurally a `<word> <decimal-size>` pair. Used to detect a body
/// inflated short of its declared length (a truncated object).
fn loose_header_declared_size(framed: &[u8]) -> Option<usize> {
    let nul = framed.iter().position(|&b| b == 0)?;
    let header = std::str::from_utf8(&framed[..nul]).ok()?;
    let (_kind, size) = header.split_once(' ')?;
    size.parse::<usize>().ok()
}

/// Read up to `prefix.len()` bytes from the start of `file`, returning how many
/// were available (short only when the file itself is shorter).
/// Outcome of inflating a loose object's header, mirroring git's
/// `unpack_loose_header` result codes (object-file.c `enum
/// unpack_loose_header_result`).
enum LooseHeader {
    /// ULHR_OK: a NUL-terminated header was found within the window. Carries the
    /// header bytes up to (not including) the NUL.
    Ok(Vec<u8>),
    /// ULHR_BAD: the zlib stream would not inflate (status != Z_OK/Z_STREAM_END).
    Bad,
    /// ULHR_TOO_LONG: the inflated output filled the header window with no NUL.
    TooLong,
}

/// Inflate a loose object's *header* exactly as git's `unpack_loose_header` does
/// (object-file.c): a single bounded inflate into a `MAX_LOOSE_HEADER_LEN`-byte
/// output buffer, then look for the header-terminating NUL in what came out.
///
/// The byte budget is load-bearing for corruption parity: git inflates only up to
/// `MAX_HEADER_LEN` (32) bytes of *output* before stopping, so a `cat-file -s`/`-t`
/// header read detects a zlib data error only when it lands within those first 32
/// inflated bytes (the header plus the start of the body for a small object) — and
/// silently returns the header for corruption buried deeper in the body, which the
/// full-object read path catches instead. A byte-by-byte loop that stopped at the
/// NUL would never inflate into the corrupt region and miss the bit-error case
/// (t1060 "getting type of a corrupt blob fails"); feeding too much output budget
/// would over-detect relative to git. So this matches git's exact window.
fn inflate_loose_header(compressed: &[u8]) -> Result<LooseHeader> {
    let mut out = [0u8; MAX_LOOSE_HEADER_LEN];
    let mut decompress = Decompress::new(true);
    // git feeds the whole mapped file as `avail_in` and inflates once into a
    // 32-byte `avail_out`; zlib stops at the output limit (Z_OK with avail_out==0)
    // or at the stream's end, propagating Z_DATA_ERROR for a corrupt stream.
    let status = decompress.decompress(compressed, &mut out, FlushDecompress::None);
    let produced = decompress.total_out() as usize;
    match status {
        Ok(_) => {
            let window = &out[..produced.min(MAX_LOOSE_HEADER_LEN)];
            match window.iter().position(|&byte| byte == 0) {
                Some(nul) => Ok(LooseHeader::Ok(window[..nul].to_vec())),
                // No NUL within the window: either the stream ended early or the
                // header overflows `MAX_LOOSE_HEADER_LEN`. git collapses both into
                // ULHR_TOO_LONG (object-file.c `unpack_loose_header`).
                None => Ok(LooseHeader::TooLong),
            }
        }
        // Any zlib error before a NUL materializes is git's ULHR_BAD.
        Err(_) => Ok(LooseHeader::Bad),
    }
}

impl ObjectReader for LooseObjectStore {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        self.validate_oid_format(oid)?;
        // Skip the `open()` (and its ENOENT) when an already-built loose cache
        // knows the id is absent. Without a cache, use an exact path probe; a
        // full fanout scan is far more expensive for one-shot packed-object reads.
        if self.cached_loose_presence(oid) == Some(false) {
            return Err(GitError::object_not_found_in(
                *oid,
                MissingObjectContext::Read,
            ));
        }
        let path = self.object_path(oid)?;
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(GitError::object_not_found_in(
                    *oid,
                    MissingObjectContext::Read,
                ));
            }
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut framed = Vec::new();
        if decoder.read_to_end(&mut framed).is_err() {
            emit_inflate_diagnostic(&compressed);
            // A stream that dies before the framing header materializes is git's
            // ULHR_BAD ("unable to unpack <oid> header"); with the header intact,
            // the body is what broke (`unpack_loose_rest`'s "corrupt loose
            // object").
            if !framed_loose_header_terminated(&framed) {
                return Err(loose_unpack_header_failed(oid));
            }
            return Err(GitError::InvalidObject(format!(
                "corrupt loose object '{oid}'"
            )));
        }
        // git only inflates the first `MAX_LOOSE_HEADER_LEN` bytes looking for the
        // header's NUL terminator before parsing the type; an over-long header is
        // rejected here (with git's diagnostic) rather than failing later as an
        // "unknown object type". Mirror that so `cat-file -p` matches upstream.
        if framed
            .iter()
            .take(MAX_LOOSE_HEADER_LEN)
            .all(|byte| *byte != 0)
        {
            return Err(loose_header_too_long(oid));
        }
        let object = parse_framed_object(&framed)?;
        // Trust the loose object's on-disk name rather than re-hashing its full body
        // on every read (see `verify_reads_enabled`); use `validate`/fsck or
        // `SLEY_VERIFY_READS` for an explicit integrity check.
        if verify_reads_enabled() {
            let actual = object.object_id(self.format)?;
            if &actual != oid {
                return Err(GitError::InvalidObject(format!(
                    "loose object {} hashes to {actual}",
                    path.display()
                )));
            }
        }
        Ok(Arc::new(object))
    }
}

impl ObjectWriter for LooseObjectStore {
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        let path = self.object_path(&oid)?;
        if path.exists() {
            self.note_loose_write(oid);
            return Ok(oid);
        }
        let parent = path
            .parent()
            .ok_or_else(|| GitError::InvalidPath("loose object path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let temp_path = unique_temp_path(parent);
        let write_result = (|| -> Result<()> {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&object.framed_bytes())?;
            let compressed = encoder.finish()?;
            {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp_path)?;
                file.write_all(&compressed)?;
                // No fsync: git's default `core.fsync=none` fsyncs nothing on the
                // loose-object write path (object-file.c writes the temp file and
                // renames it without syncing unless `core.fsync` names
                // `loose-object`/`objects`/`all`, which it does not by default).
                // A per-object sync_all() here made `git add` of N files cost N
                // fsyncs — the dominant term in sley#27's 10x `add -u` slowdown —
                // for durability git itself does not provide by default. The
                // create_new temp + atomic rename below still guarantees the
                // object never appears half-written under its final name.
            }
            match fs::rename(&temp_path, &path) {
                Ok(()) => Ok(()),
                Err(_) if path.exists() => {
                    let _ = fs::remove_file(&temp_path);
                    Ok(())
                }
                Err(err) => Err(GitError::Io(err.to_string())),
            }
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result?;
        self.note_loose_write(oid);
        Ok(oid)
    }
}
