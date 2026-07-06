//! `ls-tree` / `cat-file -t tree` tree listing and format placeholders.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{self, BufWriter, Write};

use sley::{GitError, ObjectFormat, ObjectId, Result};
use sley::plumbing::sley_object::{
    ObjectType, TreeEntries, TreeEntry, TreeEntryRef, tree_entry_object_type,
};
use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectReader};

use crate::sley_object;
use crate::status_format::write_status_quoted_path;


#[derive(Debug, Clone, Copy)]
pub(crate) struct TreePrintOptions<'a> {
    pub(crate) name_only: bool,
    pub(crate) object_only: bool,
    pub(crate) long: bool,
    pub(crate) show_trees: bool,
    pub(crate) tree_only: bool,
    pub(crate) oid_abbrev: Option<usize>,
    pub(crate) format_spec: Option<&'a str>,
    pub(crate) nul: bool,
}

pub(crate) trait TreeEntryView {
    fn mode(&self) -> u32;
    fn oid(&self) -> &ObjectId;
}

impl TreeEntryView for TreeEntry {
    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> &ObjectId {
        &self.oid
    }
}

impl TreeEntryView for TreeEntryRef<'_> {
    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> &ObjectId {
        &self.oid
    }
}

pub(crate) fn print_tree(
    db: Option<&FileObjectDatabase>,
    format: ObjectFormat,
    body: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    print_tree_with_prefix(db, format, body, b"", options)
}

pub(crate) fn write_object_id_hex<W: Write + ?Sized>(
    writer: &mut W,
    oid: &ObjectId,
    width: Option<usize>,
) -> Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hex_len = oid.format().hex_len();
    let width = width
        .map(|width| width.clamp(4, hex_len))
        .unwrap_or(hex_len);
    let mut out = [0u8; 64];
    for (index, byte) in oid.as_bytes().iter().copied().enumerate() {
        out[index * 2] = HEX[(byte >> 4) as usize];
        out[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    writer.write_all(&out[..width])?;
    Ok(())
}

pub(crate) fn print_tree_with_prefix(
    db: Option<&FileObjectDatabase>,
    format: ObjectFormat,
    body: &[u8],
    prefix: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = BufWriter::with_capacity(128 * 1024, stdout.lock());
    let mut path = prefix.to_vec();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        if options.tree_only && tree_entry_object_type(entry.mode()) == ObjectType::Blob {
            continue;
        }
        let path_len = path.len();
        path.extend_from_slice(entry.name);
        print_tree_entry_to_writer(&mut stdout, db, &entry, &path, options)?;
        path.truncate(path_len);
    }
    stdout.flush()?;
    Ok(())
}

pub(crate) fn print_tree_entry_to_writer(
    writer: &mut impl Write,
    db: Option<&FileObjectDatabase>,
    entry: &impl TreeEntryView,
    path: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    if let Some(format) = options.format_spec {
        write_tree_entry_format(writer, db, entry, path, options, format)?;
    } else if options.object_only {
        write_tree_oid(writer, entry.oid(), options)?;
    } else if options.name_only {
        write_tree_path(writer, path, options)?;
    } else {
        let object_type = tree_entry_object_type(entry.mode());
        write!(writer, "{:06o} {} ", entry.mode(), object_type.as_str())?;
        write_tree_oid(writer, entry.oid(), options)?;
        if options.long {
            let size = tree_entry_size_field(db, object_type, entry.oid())?;
            write!(writer, " {size:>7}")?;
        }
        writer.write_all(b"\t")?;
        write_tree_path(writer, path, options)?;
    }
    if options.nul {
        writer.write_all(&[0])?;
    } else {
        writer.write_all(b"\n")?;
    }
    Ok(())
}

pub(crate) fn write_tree_path(
    writer: &mut impl Write,
    path: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    if options.nul {
        writer.write_all(path)?;
    } else {
        write_status_quoted_path(writer, path, false)?;
    }
    Ok(())
}

pub(crate) fn write_tree_oid(
    writer: &mut impl Write,
    oid: &ObjectId,
    options: TreePrintOptions<'_>,
) -> Result<()> {
    write_object_id_hex(writer, oid, options.oid_abbrev)
}

pub(crate) fn write_tree_entry_format(
    writer: &mut impl Write,
    db: Option<&FileObjectDatabase>,
    entry: &impl TreeEntryView,
    path: &[u8],
    options: TreePrintOptions<'_>,
    format: &str,
) -> Result<()> {
    let object_type = tree_entry_object_type(entry.mode());
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            write!(writer, "{ch}")?;
            continue;
        }
        match chars.peek().copied() {
            Some('%') => {
                chars.next();
                writer.write_all(b"%")?;
            }
            Some('x') => {
                chars.next();
                let high = chars.next().ok_or_else(|| {
                    GitError::Command("ls-tree --format %x requires two hex digits".into())
                })?;
                let low = chars.next().ok_or_else(|| {
                    GitError::Command("ls-tree --format %x requires two hex digits".into())
                })?;
                let byte = (format_hex_nibble(high)? << 4) | format_hex_nibble(low)?;
                writer.write_all(&[byte])?;
            }
            Some('(') => {
                chars.next();
                let mut placeholder = String::new();
                for ch in chars.by_ref() {
                    if ch == ')' {
                        break;
                    }
                    placeholder.push(ch);
                }
                write_tree_format_placeholder(
                    writer,
                    db,
                    entry,
                    object_type,
                    path,
                    options,
                    &placeholder,
                )?;
            }
            _ => {
                return Err(GitError::Command(format!(
                    "unsupported ls-tree --format escape %{ch}",
                    ch = chars.next().unwrap_or('%')
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn write_tree_format_placeholder(
    writer: &mut impl Write,
    db: Option<&FileObjectDatabase>,
    entry: &impl TreeEntryView,
    object_type: ObjectType,
    path: &[u8],
    options: TreePrintOptions<'_>,
    placeholder: &str,
) -> Result<()> {
    match placeholder {
        "objectmode" => write!(writer, "{:06o}", entry.mode())?,
        "objecttype" => writer.write_all(object_type.as_str().as_bytes())?,
        "objectname" => write_tree_oid(writer, entry.oid(), options)?,
        "objectsize" => {
            writer.write_all(tree_entry_size_field(db, object_type, entry.oid())?.as_bytes())?
        }
        "objectsize:padded" => write!(
            writer,
            "{:>7}",
            tree_entry_size_field(db, object_type, entry.oid())?
        )?,
        "path" => write_tree_path(writer, path, options)?,
        _ => {
            return Err(GitError::Command(format!(
                "unsupported ls-tree --format placeholder %({placeholder})"
            )));
        }
    }
    Ok(())
}

pub(crate) fn format_hex_nibble(ch: char) -> Result<u8> {
    match ch {
        '0'..='9' => Ok(ch as u8 - b'0'),
        'a'..='f' => Ok(ch as u8 - b'a' + 10),
        'A'..='F' => Ok(ch as u8 - b'A' + 10),
        _ => Err(GitError::Command(format!(
            "invalid ls-tree --format hex digit {ch}"
        ))),
    }
}

pub(crate) fn tree_entry_size_field(
    db: Option<&FileObjectDatabase>,
    object_type: ObjectType,
    oid: &ObjectId,
) -> Result<String> {
    if object_type != ObjectType::Blob {
        return Ok("-".into());
    }
    let db =
        db.ok_or_else(|| GitError::Command("ls-tree --long requires an object database".into()))?;
    if let Some((_, size)) = db.read_object_header(oid)? {
        return Ok(size.to_string());
    }
    Ok(db.read_object(oid)?.body.len().to_string())
}

pub(crate) fn find_tree_entry(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    components: &[&str],
) -> Result<Option<sley_object::TreeEntry>> {
    let Some((component, rest)) = components.split_first() else {
        return Ok(None);
    };
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        if entry.name != component.as_bytes() {
            continue;
        }
        if rest.is_empty() {
            return Ok(Some(TreeEntry::from(entry)));
        }
        if entry.mode != 0o040000 {
            return Ok(None);
        }
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::InvalidObject(format!(
                "expected tree {}, found {}",
                entry.oid,
                object.object_type.as_str()
            )));
        }
        return find_tree_entry(db, format, &object.body, rest);
    }
    Ok(None)
}
