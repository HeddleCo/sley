use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{ObjectType, TreeEntries, tree_entry_object_type};
use sley_odb::ObjectReader;
use sley_worktree::TreeAttributes;
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Write;

mod zip;
pub use zip::{
    ZipArchiveOptions, write_zip_archive, write_zip_archive_full, write_zip_archive_with_convert,
};

const TAR_BLOCK_SIZE: usize = 512;
const TAR_RECORD_SIZE: usize = 10 * 1024;

/// Content-conversion context for `git archive` (the smudge / blob -> worktree
/// direction): EOL `LF`->`CRLF` plus any configured `filter.<name>.smudge`
/// driver, applied per `.gitattributes`.
///
/// Mirrors upstream `archive.c::object_file_to_archive`, which runs
/// `convert_to_working_tree` on every regular-file blob. Attributes come from
/// the *archived tree* (upstream sets `GIT_ATTR_INDEX` after unpacking the tree
/// into a scratch index), captured once in [`TreeAttributes`] so the whole
/// archive shares one attribute-source scan.
///
/// Build with [`ArchiveConvert::from_tree`] and pass it to
/// [`write_tar_archive_with_convert`]; the plain [`write_tar_archive`] emits raw
/// blob bytes (no conversion).
///
/// Not yet wired: `export-subst` (`$Format:…$` keyword substitution via
/// `format_subst`) and the `ident` filter (`$Id$` expansion) — both are
/// archive/convert features the underlying engine does not yet implement; see
/// the `TODO(convert)` markers.
pub struct ArchiveConvert<'a> {
    config: &'a GitConfig,
    attributes: TreeAttributes,
    /// `export-subst` keyword expander: given a `$Format:<fmt>$` inner format
    /// string, render it against the archived commit (the same pretty-format
    /// placeholders as `git log --pretty`). `None` for tree-ish archives, where
    /// there is no commit to format against (matching upstream, which only sets
    /// `args->convert` when a commit is present).
    subst: Option<Box<dyn Fn(&[u8]) -> Result<Vec<u8>> + 'a>>,
    /// `diff` userdiff-driver binary tristate for a path (git's
    /// `userdiff_find_by_path(...).binary`): `Some(true)` = forced binary,
    /// `Some(false)` = forced text, `None` = auto-detect via content. Drives the
    /// zip "is text" flag. `None` closure ⇒ always auto-detect.
    diff_binary: Option<Box<dyn Fn(&[u8]) -> Option<bool> + 'a>>,
}

impl<'a> ArchiveConvert<'a> {
    /// Capture the archived tree's attribute chain once. `attr_root` locates the
    /// global config (worktree root for a non-bare repo, git dir for a bare
    /// one), `git_dir` locates `info/attributes`, and `tree_oid` is the tree
    /// being archived (its `.gitattributes` blobs govern conversion, matching
    /// git's index-direction attribute lookup).
    pub fn from_tree(
        attr_root: impl AsRef<std::path::Path>,
        git_dir: impl AsRef<std::path::Path>,
        config: &'a GitConfig,
        db: &sley_odb::FileObjectDatabase,
        format: ObjectFormat,
        tree_oid: &ObjectId,
    ) -> Result<Self> {
        Ok(Self {
            config,
            attributes: TreeAttributes::from_tree(attr_root, git_dir, db, format, tree_oid)?,
            subst: None,
            diff_binary: None,
        })
    }

    /// Install the `export-subst` keyword expander (see [`ArchiveConvert::subst`]).
    /// Pass a closure that renders a `$Format:<fmt>$` inner format against the
    /// archived commit; only call this when archiving a *commit* (git only runs
    /// `format_subst` when a commit is available).
    pub fn with_subst(mut self, subst: impl Fn(&[u8]) -> Result<Vec<u8>> + 'a) -> Self {
        self.subst = Some(Box::new(subst));
        self
    }

    /// Install the `diff` userdiff binary-tristate resolver (see
    /// [`ArchiveConvert::diff_binary`]). The closure maps a tree-relative path to
    /// the path's `diff` driver `binary` flag.
    pub fn with_diff_binary(mut self, resolver: impl Fn(&[u8]) -> Option<bool> + 'a) -> Self {
        self.diff_binary = Some(Box::new(resolver));
        self
    }

    /// git's `entry_is_binary`: the path's `diff` driver `binary` flag when set,
    /// else content auto-detection (`buffer_is_binary`).
    fn is_binary(&self, path: &[u8], body: &[u8]) -> bool {
        if let Some(resolver) = &self.diff_binary
            && let Some(forced) = resolver(path)
        {
            return forced;
        }
        buffer_is_binary(body)
    }

    /// True when tree-relative `path` carries the `export-ignore` attribute and
    /// `git archive` should omit it (and its subtree, for a directory).
    fn export_ignore(&self, path: &[u8]) -> bool {
        self.attributes.export_ignore_for_path(path)
    }

    /// Apply the smudge conversion for a regular-file blob at tree-relative
    /// `path`, then `export-subst` keyword substitution when the path carries the
    /// `export-subst` attribute. Returns the original bytes (borrowed) when
    /// nothing converts.
    fn smudge<'b>(&self, path: &[u8], body: &'b [u8]) -> Result<Cow<'b, [u8]>> {
        let converted = self.attributes.apply_smudge_filter(self.config, path, body)?;
        // `apply_smudge_filter` returns an owned Vec; the borrow-first variant is
        // private to sley-worktree, so compare here to keep the no-op common case
        // (binary blobs, no eol/filter attribute) zero-copy through tar output.
        let smudged = if converted.as_slice() == body {
            Cow::Borrowed(body)
        } else {
            Cow::Owned(converted)
        };
        // export-subst runs after the working-tree conversion, matching
        // `object_file_to_archive` (convert_to_working_tree, then format_subst).
        if let Some(subst) = &self.subst
            && self.attributes.export_subst_for_path(path)
        {
            return Ok(Cow::Owned(format_subst(&smudged, subst)?));
        }
        Ok(smudged)
    }
}

/// Expand every `$Format:<fmt>$` directive in `src`, mirroring upstream
/// `archive.c::format_subst`: locate `$Format:`, read up to the closing `$`,
/// and replace the whole span with the rendered format. Bytes outside a
/// directive (and an unterminated trailing `$Format:`) pass through verbatim.
fn format_subst(src: &[u8], render: &dyn Fn(&[u8]) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    const MARKER: &[u8] = b"$Format:";
    let mut out = Vec::with_capacity(src.len());
    let mut rest = src;
    loop {
        let Some(start) = find_subslice(rest, MARKER) else {
            out.extend_from_slice(rest);
            break;
        };
        let after_marker = &rest[start + MARKER.len()..];
        let Some(close) = after_marker.iter().position(|&b| b == b'$') else {
            // No closing `$`: upstream stops scanning and emits the remainder
            // unchanged (including the dangling `$Format:`).
            out.extend_from_slice(rest);
            break;
        };
        out.extend_from_slice(&rest[..start]);
        let fmt = &after_marker[..close];
        out.extend_from_slice(&render(fmt)?);
        rest = &after_marker[close + 1..];
    }
    Ok(out)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// git's `buffer_is_binary`: a NUL byte within the first 8000 bytes marks the
/// content binary.
pub(crate) fn buffer_is_binary(buffer: &[u8]) -> bool {
    let len = buffer.len().min(8000);
    buffer[..len].contains(&0)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TarArchiveOptions {
    pub prefix: Vec<u8>,
    pub strip_prefix: Vec<u8>,
    pub mtime: u64,
    pub commit_id: Option<ObjectId>,
    pub pathspecs: Vec<Vec<u8>>,
}

/// A single `--add-file` / `--add-virtual-file` entry: the (already
/// prefix-rewritten) output path, its raw content, and its git mode
/// (`0o100644`, `0o100755`, or `0o120000` for a symlink). Emitted after the
/// tree entries, before the archive trailer, matching upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveExtraEntry {
    pub path: Vec<u8>,
    pub content: Vec<u8>,
    pub mode: u32,
}

/// Extra files appended to an archive (`git archive --add-file` /
/// `--add-virtual-file`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveExtras {
    pub files: Vec<ArchiveExtraEntry>,
}

impl ArchiveExtras {
    fn emit_into(&self, sink: &mut dyn ArchiveSink, format: ObjectFormat) -> Result<()> {
        for (index, file) in self.files.iter().enumerate() {
            // git synthesizes a fake oid `put_be64(hash, i+1)` for each extra
            // file (used only by tar's pax fallback naming).
            let oid = fake_extra_file_oid(format, index as u64 + 1);
            if file.mode == 0o120000 {
                sink.emit(ArchiveEntry::Symlink {
                    path: file.path.clone(),
                    target: &file.content,
                    oid,
                })?;
            } else {
                sink.emit(ArchiveEntry::File {
                    path: file.path.clone(),
                    mode: file.mode,
                    body: Cow::Borrowed(&file.content),
                    is_binary: buffer_is_binary(&file.content),
                    oid,
                })?;
            }
        }
        Ok(())
    }
}

/// git's `put_be64(fake_oid.hash, i + 1)`: an object id whose first 8 bytes are
/// the big-endian counter and the rest are zero.
fn fake_extra_file_oid(format: ObjectFormat, counter: u64) -> ObjectId {
    let mut bytes = vec![0u8; format.raw_len()];
    bytes[..8].copy_from_slice(&counter.to_be_bytes());
    ObjectId::from_raw(format, &bytes).expect("hash-length byte slice is a valid oid")
}

/// One emitted archive entry, after prefix/strip-prefix rewriting and (for
/// regular files) content conversion. Shared between the tar and zip backends:
/// the tree walk produces these and a [`ArchiveSink`] serializes them in the
/// target format. `mode` is the raw git tree mode (e.g. `0o100644`,
/// `0o100755`, `0o120000` for symlinks, `0o40000` for directories).
pub(crate) enum ArchiveEntry<'a> {
    Directory {
        path: Vec<u8>,
    },
    File {
        path: Vec<u8>,
        mode: u32,
        body: Cow<'a, [u8]>,
        /// git's `entry_is_binary` classification of the (converted) content,
        /// driven by the path's `diff` userdiff driver. Only the zip backend
        /// consumes it (the central-directory "is text" internal-attribute bit,
        /// which `unzip -a` reads to decide EOL conversion); tar ignores it.
        is_binary: bool,
        /// The blob's object id, used by tar's pax fallback for over-long paths
        /// (the `<oid>.data` placeholder name + `<oid>.paxheader`).
        oid: ObjectId,
    },
    Symlink {
        path: Vec<u8>,
        target: &'a [u8],
        oid: ObjectId,
    },
}

/// Format-specific archive serializer. The tree walk
/// ([`write_archive_entries`]) feeds it the resolved [`ArchiveEntry`] stream;
/// the sink owns the on-disk byte layout (tar headers vs. zip local headers +
/// central directory).
pub(crate) trait ArchiveSink {
    fn emit(&mut self, entry: ArchiveEntry<'_>) -> Result<()>;
}

pub fn write_tar_archive<R, W>(
    writer: &mut W,
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: TarArchiveOptions,
) -> Result<()>
where
    R: ObjectReader,
    W: Write,
{
    write_tar_archive_inner(
        writer,
        reader,
        format,
        tree_oid,
        options,
        None,
        &ArchiveExtras::default(),
    )
}

/// Like [`write_tar_archive`] but applies content conversion (smudge: EOL +
/// `filter.<name>.smudge`) to each regular-file blob per the archived tree's
/// `.gitattributes`, mirroring `git archive`. Symlinks are emitted unconverted,
/// exactly as upstream (`object_file_to_archive` only converts `S_ISREG`).
pub fn write_tar_archive_with_convert<R, W>(
    writer: &mut W,
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: TarArchiveOptions,
    convert: &ArchiveConvert<'_>,
) -> Result<()>
where
    R: ObjectReader,
    W: Write,
{
    write_tar_archive_inner(
        writer,
        reader,
        format,
        tree_oid,
        options,
        Some(convert),
        &ArchiveExtras::default(),
    )
}

/// Like [`write_tar_archive_with_convert`] but also appends `--add-file` /
/// `--add-virtual-file` entries after the tree, before the trailer.
pub fn write_tar_archive_full<W>(
    writer: &mut W,
    reader: &sley_odb::FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: TarArchiveOptions,
    convert: &ArchiveConvert<'_>,
    extra: &ArchiveExtras,
) -> Result<()>
where
    W: Write + ?Sized,
{
    write_tar_archive_inner(writer, reader, format, tree_oid, options, Some(convert), extra)
}

/// Like [`write_tar_archive_full`] but gzip-wrapped (`git archive --format=tgz`
/// / `tar.gz`, the internal-gzip tar filter). The gzip header sets OS = 3
/// (Unix) and mtime = 0, matching git's `git_deflate_init_gzip` for
/// reproducibility.
pub fn write_tar_gz_archive_full<W>(
    writer: &mut W,
    reader: &sley_odb::FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: TarArchiveOptions,
    convert: &ArchiveConvert<'_>,
    extra: &ArchiveExtras,
    compression_level: u32,
) -> Result<()>
where
    W: Write + ?Sized,
{
    let mut tar = Vec::new();
    write_tar_archive_inner(&mut tar, reader, format, tree_oid, options, Some(convert), extra)?;
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(3)
        .write(Vec::new(), flate2::Compression::new(compression_level.min(9)));
    encoder
        .write_all(&tar)
        .map_err(|err| GitError::Io(err.to_string()))?;
    let gz = encoder
        .finish()
        .map_err(|err| GitError::Io(err.to_string()))?;
    writer.write_all(&gz)?;
    Ok(())
}

fn write_tar_archive_inner<R, W>(
    writer: &mut W,
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: TarArchiveOptions,
    convert: Option<&ArchiveConvert<'_>>,
    extra: &ArchiveExtras,
) -> Result<()>
where
    R: ObjectReader,
    W: Write + ?Sized,
{
    let mut writer = CountingWriter::new(writer);
    // The global header writes first and clamps a far-future mtime; every entry
    // uses the clamped value (upstream sets `args->time = USTAR_MAX_MTIME`).
    let entry_mtime =
        write_global_extended_header(&mut writer, options.commit_id.as_ref(), options.mtime)?;
    let prefix = normalize_prefix(&options.prefix)?;
    let strip_prefix = normalize_strip_prefix(&options.strip_prefix)?;
    let mut sink = TarSink {
        writer: &mut writer,
        mtime: entry_mtime,
    };
    write_archive_entries(
        &mut sink,
        reader,
        format,
        tree_oid,
        &prefix,
        &strip_prefix,
        &options.pathspecs,
        convert,
    )?;
    extra.emit_into(&mut sink, format)?;
    writer.write_all(&[0; TAR_BLOCK_SIZE])?;
    writer.write_all(&[0; TAR_BLOCK_SIZE])?;
    write_record_padding(&mut writer)?;
    Ok(())
}

struct TarSink<'a, 'w, W: Write + ?Sized> {
    writer: &'a mut CountingWriter<'w, W>,
    mtime: u64,
}

impl<W: Write + ?Sized> ArchiveSink for TarSink<'_, '_, W> {
    fn emit(&mut self, entry: ArchiveEntry<'_>) -> Result<()> {
        match entry {
            ArchiveEntry::Directory { path } => {
                write_directory_entry(self.writer, &path, self.mtime)
            }
            ArchiveEntry::File {
                path,
                mode,
                body,
                oid,
                ..
            } => {
                let tar_mode = if mode & 0o111 != 0 { 0o775 } else { 0o664 };
                write_file_entry(self.writer, &path, tar_mode, &body, self.mtime, &oid)
            }
            ArchiveEntry::Symlink { path, target, oid } => {
                write_symlink_entry(self.writer, &path, target, self.mtime, &oid)
            }
        }
    }
}

/// Walk `tree_oid` and feed resolved entries (prefix-rewritten, converted) to
/// `sink` in git's deterministic order. Shared by the tar and zip backends.
/// Applies pathspec selection, `--prefix`, `--strip-prefix`, directory
/// synthesis, and (when `convert` is set) smudge conversion — exactly the
/// upstream `write_archive_entries` contract.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_archive_entries<R, S>(
    sink: &mut S,
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &[u8],
    strip_prefix: &[u8],
    pathspecs: &[Vec<u8>],
    convert: Option<&ArchiveConvert<'_>>,
) -> Result<()>
where
    R: ObjectReader,
    S: ArchiveSink + ?Sized,
{
    let pathspecs = normalize_pathspecs(pathspecs)?;
    let mut matched = vec![false; pathspecs.len()];
    mark_archive_pathspec_matches(
        reader,
        format,
        tree_oid,
        b"",
        &pathspecs,
        false,
        &mut matched,
    )?;
    if let Some(pathspec) = pathspecs
        .iter()
        .zip(&matched)
        .find_map(|(pathspec, matched)| (!*matched).then_some(pathspec))
    {
        return Err(GitError::InvalidPath(format!(
            "pathspec '{}' did not match any files",
            String::from_utf8_lossy(pathspec)
        )));
    }
    let mut emitted_directories = HashSet::new();
    if !prefix.is_empty() && prefix.ends_with(b"/") {
        sink.emit(ArchiveEntry::Directory {
            path: prefix.to_vec(),
        })?;
        emitted_directories.insert(prefix.to_vec());
    }
    let context = ArchiveWriteContext {
        reader,
        format,
        prefix,
        strip_prefix,
        pathspecs: &pathspecs,
        convert,
    };
    write_tree_entries(
        sink,
        &context,
        tree_oid,
        b"",
        false,
        &mut matched,
        &mut emitted_directories,
    )
}

fn mark_archive_pathspec_matches<R>(
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    relative_prefix: &[u8],
    pathspecs: &[Vec<u8>],
    force_include: bool,
    matched: &mut [bool],
) -> Result<()>
where
    R: ObjectReader,
{
    if pathspecs.is_empty() {
        return Ok(());
    }
    let object = reader.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let relative_path = join_path(relative_prefix, entry.name);
        match tree_entry_object_type(entry.mode) {
            ObjectType::Tree => {
                let selection = if force_include {
                    ArchiveTreeSelection {
                        descend: true,
                        full_subtree: true,
                    }
                } else {
                    archive_tree_selection(&relative_path, pathspecs)
                };
                if !selection.descend {
                    continue;
                }
                mark_exact_pathspec_matches(&relative_path, pathspecs, matched);
                let relative_directory = ensure_trailing_slash(&relative_path);
                mark_archive_pathspec_matches(
                    reader,
                    format,
                    &entry.oid,
                    &relative_directory,
                    pathspecs,
                    force_include || selection.full_subtree,
                    matched,
                )?;
            }
            ObjectType::Blob => {
                archive_blob_selected(&relative_path, pathspecs, force_include, matched);
            }
            _ => {}
        }
    }
    Ok(())
}

struct ArchiveWriteContext<'a, R> {
    reader: &'a R,
    format: ObjectFormat,
    prefix: &'a [u8],
    strip_prefix: &'a [u8],
    pathspecs: &'a [Vec<u8>],
    /// Smudge conversion, when archiving with `--convert` semantics; `None`
    /// emits raw blob bytes.
    convert: Option<&'a ArchiveConvert<'a>>,
}

fn write_tree_entries<R, S>(
    sink: &mut S,
    context: &ArchiveWriteContext<'_, R>,
    tree_oid: &ObjectId,
    relative_prefix: &[u8],
    force_include: bool,
    matched: &mut [bool],
    emitted_directories: &mut HashSet<Vec<u8>>,
) -> Result<()>
where
    R: ObjectReader,
    S: ArchiveSink + ?Sized,
{
    let object = context.reader.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(context.format, &object.body) {
        let entry = entry?;
        let relative_path = join_path(relative_prefix, entry.name);
        match tree_entry_object_type(entry.mode) {
            ObjectType::Tree => {
                let selection = if force_include {
                    ArchiveTreeSelection {
                        descend: true,
                        full_subtree: true,
                    }
                } else {
                    archive_tree_selection(&relative_path, context.pathspecs)
                };
                if !selection.descend {
                    continue;
                }
                // export-ignore on a directory drops the whole subtree (git
                // checks the attribute on the trailing-slash path, then returns
                // without recursing).
                let relative_directory = ensure_trailing_slash(&relative_path);
                if context
                    .convert
                    .is_some_and(|convert| convert.export_ignore(&relative_directory))
                {
                    continue;
                }
                if let Some(output_relative_path) =
                    strip_archive_prefix(&relative_path, context.strip_prefix)
                    && !output_relative_path.is_empty()
                {
                    let directory =
                        ensure_trailing_slash(&join_path(context.prefix, output_relative_path));
                    if emitted_directories.insert(directory.clone()) {
                        sink.emit(ArchiveEntry::Directory { path: directory })?;
                    }
                }
                mark_exact_pathspec_matches(&relative_path, context.pathspecs, matched);
                write_tree_entries(
                    sink,
                    context,
                    &entry.oid,
                    &relative_directory,
                    force_include || selection.full_subtree,
                    matched,
                    emitted_directories,
                )?;
            }
            ObjectType::Blob => {
                if !archive_blob_selected(&relative_path, context.pathspecs, force_include, matched)
                {
                    continue;
                }
                // export-ignore on a file omits it (git checks the attribute on
                // the file path before writing the entry).
                if context
                    .convert
                    .is_some_and(|convert| convert.export_ignore(&relative_path))
                {
                    continue;
                }
                let Some(output_relative_path) =
                    strip_archive_prefix(&relative_path, context.strip_prefix)
                else {
                    continue;
                };
                let path = join_path(context.prefix, output_relative_path);
                let object = context.reader.read_object(&entry.oid)?;
                if object.object_type != ObjectType::Blob {
                    return Err(GitError::InvalidObject(format!(
                        "expected blob {}, found {}",
                        entry.oid,
                        object.object_type.as_str()
                    )));
                }
                if entry.mode == 0o120000 {
                    // Symlinks are never converted (upstream only converts
                    // S_ISREG); emit the link target bytes verbatim.
                    sink.emit(ArchiveEntry::Symlink {
                        path,
                        target: &object.body,
                        oid: entry.oid,
                    })?;
                } else {
                    // Convert blob -> worktree form per the archived tree's
                    // attributes, keyed by the *tree-relative* path (git's
                    // `path_without_prefix`), not the prefixed output path.
                    // TODO(convert): upstream also applies `export-subst`
                    // (`$Format:…$`) and the `ident` filter here; neither the
                    // archive crate nor the convert engine implements those yet.
                    let body = match context.convert {
                        Some(convert) => convert.smudge(&relative_path, &object.body)?,
                        None => Cow::Borrowed(object.body.as_slice()),
                    };
                    // git classifies the *converted* content against the
                    // tree-relative path's `diff` driver.
                    let is_binary = context
                        .convert
                        .map_or_else(|| buffer_is_binary(&body), |convert| {
                            convert.is_binary(&relative_path, &body)
                        });
                    sink.emit(ArchiveEntry::File {
                        path,
                        mode: entry.mode,
                        body,
                        is_binary,
                        oid: entry.oid,
                    })?;
                }
            }
            _ => {
                return Err(GitError::InvalidObject(format!(
                    "unsupported archive tree entry mode {:o}",
                    entry.mode
                )));
            }
        }
    }
    Ok(())
}

fn strip_archive_prefix<'a>(path: &'a [u8], strip_prefix: &[u8]) -> Option<&'a [u8]> {
    if strip_prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(strip_prefix)
}

#[derive(Debug, Clone, Copy)]
struct ArchiveTreeSelection {
    descend: bool,
    full_subtree: bool,
}

fn archive_tree_selection(relative_path: &[u8], pathspecs: &[Vec<u8>]) -> ArchiveTreeSelection {
    if pathspecs.is_empty() {
        return ArchiveTreeSelection {
            descend: true,
            full_subtree: true,
        };
    }
    let directory = ensure_trailing_slash(relative_path);
    let mut descendant = false;
    for pathspec in pathspecs {
        if pathspec == relative_path || *pathspec == directory {
            return ArchiveTreeSelection {
                descend: true,
                full_subtree: true,
            };
        }
        if pathspec.starts_with(&directory) {
            descendant = true;
        }
    }
    ArchiveTreeSelection {
        descend: descendant,
        full_subtree: false,
    }
}

fn archive_blob_selected(
    relative_path: &[u8],
    pathspecs: &[Vec<u8>],
    force_include: bool,
    matched: &mut [bool],
) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    let mut selected = false;
    for (index, pathspec) in pathspecs.iter().enumerate() {
        if pathspec == relative_path {
            matched[index] = true;
            selected = true;
        }
    }
    selected || force_include
}

fn mark_exact_pathspec_matches(relative_path: &[u8], pathspecs: &[Vec<u8>], matched: &mut [bool]) {
    let directory = ensure_trailing_slash(relative_path);
    for (index, pathspec) in pathspecs.iter().enumerate() {
        if pathspec == relative_path || *pathspec == directory {
            matched[index] = true;
        }
    }
}

/// ustar `mtime` field max; a larger archive time is carried in a global pax
/// `mtime` record and the per-entry headers clamp to this value (upstream
/// `USTAR_MAX_MTIME` / `write_global_extended_header`).
const USTAR_MAX_MTIME: u64 = 0o777_7777_7777;

/// Write the global pax extended header (typeflag `g`, name `pax_global_header`)
/// when a commit id is present and/or the archive time overflows ustar. Returns
/// the (possibly clamped) per-entry mtime to use for all subsequent entries.
fn write_global_extended_header(
    writer: &mut impl Write,
    commit_id: Option<&ObjectId>,
    mtime: u64,
) -> Result<u64> {
    let mut ext_header = Vec::new();
    if let Some(commit_id) = commit_id {
        append_pax_record(&mut ext_header, b"comment", commit_id.to_string().as_bytes());
    }
    let entry_mtime = if mtime > USTAR_MAX_MTIME {
        append_pax_record(&mut ext_header, b"mtime", mtime.to_string().as_bytes());
        USTAR_MAX_MTIME
    } else {
        mtime
    };
    if ext_header.is_empty() {
        return Ok(entry_mtime);
    }
    write_ustar_header(
        writer,
        b"pax_global_header",
        0o666,
        ext_header.len() as u64,
        entry_mtime,
        b'g',
        b"",
        b"",
    )?;
    writer.write_all(&ext_header)?;
    write_padding(writer, ext_header.len())?;
    Ok(entry_mtime)
}

/// ustar `size` field max (077777777777 octal = 8 GiB - 1); larger regular
/// files carry their size in a pax `size` record instead.
const USTAR_MAX_SIZE: u64 = 0o777_7777_7777;

fn write_directory_entry(writer: &mut impl Write, path: &[u8], mtime: u64) -> Result<()> {
    // Directories never overflow ustar in our trees (no oid needed for a pax
    // fallback path); a synthetic placeholder keeps the signature uniform.
    let placeholder = ObjectId::from_raw(ObjectFormat::Sha1, &[0u8; 20]).expect("zero oid");
    write_entry_with_pax(writer, path, 0o775, 0, mtime, b'5', b"", &placeholder)
}

fn write_file_entry(
    writer: &mut impl Write,
    path: &[u8],
    mode: u32,
    body: &[u8],
    mtime: u64,
    oid: &ObjectId,
) -> Result<()> {
    write_entry_with_pax(writer, path, mode, body.len() as u64, mtime, b'0', b"", oid)?;
    writer.write_all(body)?;
    write_padding(writer, body.len())
}

fn write_symlink_entry(
    writer: &mut impl Write,
    path: &[u8],
    target: &[u8],
    mtime: u64,
    oid: &ObjectId,
) -> Result<()> {
    write_entry_with_pax(writer, path, 0o777, 0, mtime, b'2', target, oid)
}

/// Write one tar entry header, emitting a pax extended header first when the
/// path / link target / size overflow the ustar fixed fields (mirrors upstream
/// `write_tar_entry` + `write_extended_header`).
#[allow(clippy::too_many_arguments)]
fn write_entry_with_pax(
    writer: &mut impl Write,
    path: &[u8],
    mode: u32,
    size: u64,
    mtime: u64,
    typeflag: u8,
    linkname: &[u8],
    oid: &ObjectId,
) -> Result<()> {
    let mut ext_header = Vec::new();

    // Name: fits the 100-byte field, else a ustar prefix split, else a pax
    // `path` record with a `<oid>.data` placeholder name.
    let (header_name, header_prefix): (Vec<u8>, Vec<u8>) = if path.len() <= 100 {
        (path.to_vec(), Vec::new())
    } else {
        match ustar_split(path) {
            Some((name, prefix)) => (name.to_vec(), prefix.to_vec()),
            None => {
                append_pax_record(&mut ext_header, b"path", path);
                (format!("{oid}.data").into_bytes(), Vec::new())
            }
        }
    };

    // Linkname: fits the 100-byte field, else a pax `linkpath` record with a
    // `see <oid>.paxheader` placeholder.
    let header_linkname: Vec<u8> = if linkname.len() <= 100 {
        linkname.to_vec()
    } else {
        append_pax_record(&mut ext_header, b"linkpath", linkname);
        format!("see {oid}.paxheader").into_bytes()
    };

    // Large regular files carry size in a pax record; the ustar field reads 0.
    let size_in_header = if typeflag == b'0' && size > USTAR_MAX_SIZE {
        append_pax_record(&mut ext_header, b"size", size.to_string().as_bytes());
        0
    } else {
        size
    };

    if !ext_header.is_empty() {
        write_extended_header(writer, oid, &ext_header, mtime)?;
    }

    write_ustar_header(
        writer,
        &header_name,
        mode,
        size_in_header,
        mtime,
        typeflag,
        &header_linkname,
        &header_prefix,
    )
}

/// A pax extended-header tar entry (typeflag `x`, name `<oid>.paxheader`)
/// carrying `body`, followed by block padding.
fn write_extended_header(
    writer: &mut impl Write,
    oid: &ObjectId,
    body: &[u8],
    mtime: u64,
) -> Result<()> {
    let name = format!("{oid}.paxheader").into_bytes();
    write_ustar_header(writer, &name, 0o666, body.len() as u64, mtime, b'x', b"", b"")?;
    writer.write_all(body)?;
    write_padding(writer, body.len())
}

/// Append one pax record `"<len> <keyword>=<value>\n"` where `<len>` counts the
/// whole record including its own decimal digits (upstream
/// `strbuf_append_ext_header`).
fn append_pax_record(out: &mut Vec<u8>, keyword: &[u8], value: &[u8]) {
    // len = digits(len) + 1 (space) + keyword + 1 (=) + value + 1 (\n).
    let fixed = 1 + keyword.len() + 1 + value.len() + 1;
    let mut len = fixed + 1;
    loop {
        let candidate = fixed + decimal_digits(len);
        if candidate == len {
            break;
        }
        len = candidate;
    }
    out.extend_from_slice(len.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(keyword);
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn decimal_digits(mut n: usize) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

#[allow(clippy::too_many_arguments)]
fn write_ustar_header(
    writer: &mut impl Write,
    name: &[u8],
    mode: u32,
    size: u64,
    mtime: u64,
    typeflag: u8,
    linkname: &[u8],
    prefix: &[u8],
) -> Result<()> {
    let mut header = [0u8; TAR_BLOCK_SIZE];
    write_field(&mut header[0..100], name);
    write_octal(&mut header[100..108], mode as u64)?;
    write_octal(&mut header[108..116], 0)?;
    write_octal(&mut header[116..124], 0)?;
    write_octal(&mut header[124..136], size)?;
    write_octal(&mut header[136..148], mtime)?;
    header[148..156].fill(b' ');
    header[156] = typeflag;
    write_field(&mut header[157..257], linkname);
    write_field(&mut header[257..263], b"ustar");
    write_field(&mut header[263..265], b"00");
    write_field(&mut header[265..297], b"root");
    write_field(&mut header[297..329], b"root");
    write_octal(&mut header[329..337], 0)?;
    write_octal(&mut header[337..345], 0)?;
    write_field(&mut header[345..500], prefix);
    let checksum = header.iter().map(|byte| *byte as u32).sum::<u32>();
    write_checksum(&mut header[148..156], checksum);
    writer.write_all(&header)?;
    Ok(())
}

/// ustar prefix split (upstream `get_path_prefix`): find the last `/` within the
/// 155-byte prefix window such that the remaining name fits 100 bytes. Returns
/// `None` when no split works (caller falls back to a pax `path` record).
fn ustar_split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut i = path.len();
    if i > 1 && path[i - 1] == b'/' {
        i -= 1;
    }
    if i > 155 {
        i = 155;
    }
    loop {
        if i == 0 {
            break;
        }
        i -= 1;
        if path[i] == b'/' {
            break;
        }
    }
    let prefix_len = i;
    let rest = path.len() - prefix_len - 1;
    if prefix_len > 0 && rest <= 100 {
        Some((&path[prefix_len + 1..], &path[..prefix_len]))
    } else {
        None
    }
}

fn write_field(field: &mut [u8], value: &[u8]) {
    field[..value.len()].copy_from_slice(value);
}

fn write_octal(field: &mut [u8], value: u64) -> Result<()> {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    if text.len() > digits {
        return Err(GitError::Unsupported(format!(
            "tar numeric field overflow for value {value}"
        )));
    }
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
    Ok(())
}

fn write_checksum(field: &mut [u8], value: u32) {
    let text = format!("{value:07o}");
    field[..7].copy_from_slice(text.as_bytes());
    field[7] = 0;
}

fn write_padding(writer: &mut impl Write, len: usize) -> Result<()> {
    let padding = (TAR_BLOCK_SIZE - (len % TAR_BLOCK_SIZE)) % TAR_BLOCK_SIZE;
    if padding > 0 {
        writer.write_all(&vec![0; padding])?;
    }
    Ok(())
}

fn write_record_padding<W: Write + ?Sized>(writer: &mut CountingWriter<'_, W>) -> Result<()> {
    let padding = (TAR_RECORD_SIZE - (writer.written % TAR_RECORD_SIZE)) % TAR_RECORD_SIZE;
    if padding > 0 {
        writer.write_all(&vec![0; padding])?;
    }
    Ok(())
}

pub(crate) fn normalize_prefix(prefix: &[u8]) -> Result<Vec<u8>> {
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    if prefix.starts_with(b"/")
        || prefix
            .split(|byte| *byte == b'/')
            .any(|component| component == b"..")
    {
        return Err(GitError::InvalidPath(format!(
            "invalid archive prefix {}",
            String::from_utf8_lossy(prefix)
        )));
    }
    Ok(prefix.to_vec())
}

pub(crate) fn normalize_strip_prefix(prefix: &[u8]) -> Result<Vec<u8>> {
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    normalize_pathspec_component(prefix).map(|prefix| ensure_trailing_slash(&prefix))
}

fn normalize_pathspecs(pathspecs: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
    let mut normalized = Vec::with_capacity(pathspecs.len());
    for pathspec in pathspecs {
        if pathspec.is_empty() {
            return Err(GitError::InvalidPath(format!(
                "invalid archive pathspec {}",
                String::from_utf8_lossy(pathspec)
            )));
        }
        normalized.push(normalize_pathspec_component(pathspec)?);
    }
    Ok(normalized)
}

fn normalize_pathspec_component(pathspec: &[u8]) -> Result<Vec<u8>> {
    let pathspec = pathspec.strip_prefix(b"./").unwrap_or(pathspec);
    if pathspec.starts_with(b"/")
        || pathspec
            .split(|byte| *byte == b'/')
            .any(|component| component == b"..")
    {
        return Err(GitError::InvalidPath(format!(
            "invalid archive pathspec {}",
            String::from_utf8_lossy(pathspec)
        )));
    }
    Ok(pathspec.to_vec())
}

fn join_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + name.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(name);
    out
}

fn ensure_trailing_slash(path: &[u8]) -> Vec<u8> {
    let mut out = path.to_vec();
    if !out.ends_with(b"/") {
        out.push(b'/');
    }
    out
}

struct CountingWriter<'a, W: ?Sized> {
    inner: &'a mut W,
    written: usize,
}

impl<'a, W: ?Sized> CountingWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write + ?Sized> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let count = self.inner.write(buf)?;
        self.written += count;
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::BString;
    use sley_object::{EncodedObject, Tree};
    use sley_odb::{ObjectDatabase, ObjectWriter};

    #[test]
    fn tar_archive_writes_regular_executable_symlink_and_prefix_entries() {
        let format = ObjectFormat::Sha1;
        let db = ObjectDatabase::new(format);
        let regular = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n"))
            .expect("test operation should succeed");
        let executable = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"#!/bin/sh\n"))
            .expect("test operation should succeed");
        let symlink = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"regular.txt"))
            .expect("test operation should succeed");
        let tree = Tree {
            entries: vec![
                sley_object::TreeEntry {
                    mode: 0o100644,
                    name: BString::from(b"regular.txt"),
                    oid: regular,
                },
                sley_object::TreeEntry {
                    mode: 0o100755,
                    name: BString::from(b"run"),
                    oid: executable,
                },
                sley_object::TreeEntry {
                    mode: 0o120000,
                    name: BString::from(b"link"),
                    oid: symlink,
                },
            ],
        };
        let tree_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("test operation should succeed");
        let mut archive = Vec::new();
        write_tar_archive(
            &mut archive,
            &db,
            format,
            &tree_oid,
            TarArchiveOptions {
                prefix: b"pfx/".to_vec(),
                strip_prefix: Vec::new(),
                mtime: 1_700_000_000,
                commit_id: None,
                pathspecs: Vec::new(),
            },
        )
        .expect("test operation should succeed");
        assert!(archive.starts_with(b"pfx/"));
        assert_eq!(archive.len() % TAR_RECORD_SIZE, 0);
        assert!(
            archive
                .windows(b"pfx/regular.txt".len())
                .any(|window| window == b"pfx/regular.txt")
        );
        assert!(
            archive
                .windows(b"pfx/link".len())
                .any(|window| window == b"pfx/link")
        );
        assert!(archive.ends_with(&[0; TAR_BLOCK_SIZE * 2]));
    }
}
