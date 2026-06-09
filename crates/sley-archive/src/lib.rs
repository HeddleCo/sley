use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{ObjectType, TreeEntries, tree_entry_object_type};
use sley_odb::ObjectReader;
use std::collections::HashSet;
use std::io::Write;

const TAR_BLOCK_SIZE: usize = 512;
const TAR_RECORD_SIZE: usize = 10 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TarArchiveOptions {
    pub prefix: Vec<u8>,
    pub strip_prefix: Vec<u8>,
    pub mtime: u64,
    pub commit_id: Option<ObjectId>,
    pub pathspecs: Vec<Vec<u8>>,
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
    let mut writer = CountingWriter::new(writer);
    let pathspecs = normalize_pathspecs(&options.pathspecs)?;
    let strip_prefix = normalize_strip_prefix(&options.strip_prefix)?;
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
    if let Some(commit_id) = &options.commit_id {
        write_global_pax_comment(&mut writer, commit_id, options.mtime)?;
    }
    let prefix = normalize_prefix(&options.prefix)?;
    if !prefix.is_empty() && prefix.ends_with(b"/") {
        write_directory_entry(&mut writer, &prefix, options.mtime)?;
        emitted_directories.insert(prefix.clone());
    }
    let context = ArchiveWriteContext {
        reader,
        format,
        prefix: &prefix,
        strip_prefix: &strip_prefix,
        mtime: options.mtime,
        pathspecs: &pathspecs,
    };
    write_tree_entries(
        &mut writer,
        &context,
        tree_oid,
        b"",
        false,
        &mut matched,
        &mut emitted_directories,
    )?;
    writer.write_all(&[0; TAR_BLOCK_SIZE])?;
    writer.write_all(&[0; TAR_BLOCK_SIZE])?;
    write_record_padding(&mut writer)?;
    Ok(())
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
    mtime: u64,
    pathspecs: &'a [Vec<u8>],
}

fn write_tree_entries<R, W>(
    writer: &mut W,
    context: &ArchiveWriteContext<'_, R>,
    tree_oid: &ObjectId,
    relative_prefix: &[u8],
    force_include: bool,
    matched: &mut [bool],
    emitted_directories: &mut HashSet<Vec<u8>>,
) -> Result<()>
where
    R: ObjectReader,
    W: Write,
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
                if let Some(output_relative_path) =
                    strip_archive_prefix(&relative_path, context.strip_prefix)
                    && !output_relative_path.is_empty()
                {
                    let directory =
                        ensure_trailing_slash(&join_path(context.prefix, output_relative_path));
                    if emitted_directories.insert(directory.clone()) {
                        write_directory_entry(writer, &directory, context.mtime)?;
                    }
                }
                mark_exact_pathspec_matches(&relative_path, context.pathspecs, matched);
                let relative_directory = ensure_trailing_slash(&relative_path);
                write_tree_entries(
                    writer,
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
                    write_symlink_entry(writer, &path, &object.body, context.mtime)?;
                } else {
                    let mode = if entry.mode & 0o111 != 0 {
                        0o775
                    } else {
                        0o664
                    };
                    write_file_entry(writer, &path, mode, &object.body, context.mtime)?;
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

fn write_global_pax_comment(
    writer: &mut impl Write,
    commit_id: &ObjectId,
    mtime: u64,
) -> Result<()> {
    let comment = format!("comment={commit_id}\n");
    let mut length = comment.len() + 2;
    loop {
        let candidate = length.to_string().len() + 1 + comment.len();
        if candidate == length {
            break;
        }
        length = candidate;
    }
    let record = format!("{length} {comment}");
    write_header(
        writer,
        b"pax_global_header",
        0o666,
        record.len() as u64,
        mtime,
        b'g',
        b"",
    )?;
    writer.write_all(record.as_bytes())?;
    write_padding(writer, record.len())?;
    Ok(())
}

fn write_directory_entry(writer: &mut impl Write, path: &[u8], mtime: u64) -> Result<()> {
    write_header(writer, path, 0o775, 0, mtime, b'5', b"")
}

fn write_file_entry(
    writer: &mut impl Write,
    path: &[u8],
    mode: u32,
    body: &[u8],
    mtime: u64,
) -> Result<()> {
    write_header(writer, path, mode, body.len() as u64, mtime, b'0', b"")?;
    writer.write_all(body)?;
    write_padding(writer, body.len())
}

fn write_symlink_entry(
    writer: &mut impl Write,
    path: &[u8],
    target: &[u8],
    mtime: u64,
) -> Result<()> {
    write_header(writer, path, 0o777, 0, mtime, b'2', target)
}

fn write_header(
    writer: &mut impl Write,
    path: &[u8],
    mode: u32,
    size: u64,
    mtime: u64,
    typeflag: u8,
    linkname: &[u8],
) -> Result<()> {
    let (name, prefix) = split_tar_path(path)?;
    if linkname.len() > 100 {
        return Err(GitError::Unsupported(format!(
            "archive symlink target is too long: {} bytes",
            linkname.len()
        )));
    }
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

fn split_tar_path(path: &[u8]) -> Result<(&[u8], &[u8])> {
    if path.len() <= 100 {
        return Ok((path, b""));
    }
    for index in (0..path.len()).rev() {
        if path[index] != b'/' {
            continue;
        }
        let prefix = &path[..index];
        let name = &path[index + 1..];
        if !prefix.is_empty() && prefix.len() <= 155 && !name.is_empty() && name.len() <= 100 {
            return Ok((name, prefix));
        }
    }
    Err(GitError::Unsupported(format!(
        "archive path is too long for ustar: {}",
        String::from_utf8_lossy(path)
    )))
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

fn write_record_padding<W: Write>(writer: &mut CountingWriter<'_, W>) -> Result<()> {
    let padding = (TAR_RECORD_SIZE - (writer.written % TAR_RECORD_SIZE)) % TAR_RECORD_SIZE;
    if padding > 0 {
        writer.write_all(&vec![0; padding])?;
    }
    Ok(())
}

fn normalize_prefix(prefix: &[u8]) -> Result<Vec<u8>> {
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

fn normalize_strip_prefix(prefix: &[u8]) -> Result<Vec<u8>> {
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

struct CountingWriter<'a, W> {
    inner: &'a mut W,
    written: usize,
}

impl<'a, W> CountingWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write> Write for CountingWriter<'_, W> {
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
        let mut db = ObjectDatabase::new(format);
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
