//! Multi-pack-index chain management: layer writes, incremental chains,
//! bitmap tips, compaction, verify, expire-sidecars, migration from single
//! MIDX, and the default-MIDX rewrite used after repack/expire.
//!
//! Temp-file names (`.bitmap.tmp`, `multi-pack-index-{checksum}.{midx,
//! bitmap,rev}`) are byte-preserved upstream artifacts. Every midx/chain
//! artifact write lands via a sibling `*.tmp` file plus rename so readers
//! never observe a truncated index (matching the `.bitmap` precedent).

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::{
    repository_objects_dir, FileObjectDatabase,
};
use sley_odb::ObjectReader as _;
use sley_pack::{
    pack_order_index_positions, MultiPackIndex, MultiPackIndexEntry, PackFile, PackIndex,
    PackReverseIndex, PackWriteLimits, PackWriteOptions,
};

const MULTI_PACK_INDEX_USAGE: &str = "\n";

/// Write `bytes` to `path` through a sibling `*.tmp` file and rename, so a
/// crash mid-write can never leave a truncated midx, chain, or sidecar file
/// behind (a half-written chain would wedge incremental midx operations).
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(".tmp");
    let temp = PathBuf::from(temp_name);
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
}

use crate::repack::{repack_preferred_bitmap_tips, repack_pseudo_merge_groups};
use crate::{read_repo_config, repo_object_format, resolve_path_under};

pub fn write(cwd: &Path, git_dir: &Path, args: &[String]) -> Result<()> {
    write_with_pack_names(cwd, git_dir, args, None)
}

pub fn write_with_pack_names(
    cwd: &Path,
    git_dir: &Path,
    args: &[String],
    selected_pack_names: Option<Vec<String>>,
) -> Result<()> {
    let format = repo_object_format(git_dir)?;
    let config = read_repo_config(git_dir)?;
    let write_bitmap_lookup_table = config
        .get_bool("pack", None, "writeBitmapLookupTable")
        .unwrap_or(false);
    let write_bitmap_hash_cache = config
        .get_bool("pack", None, "writeBitmapHashCache")
        .unwrap_or(true);
    let mut object_dir: Option<PathBuf> = None;
    let mut stdin_packs = false;
    let mut write_bitmap = false;
    let mut incremental = false;
    let mut preferred_pack_name: Option<String> = None;
    let mut refs_snapshot: Option<PathBuf> = None;
    let mut write_chain_file = true;
    let mut base_checksum: Option<String> = None;
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_path_under(cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_path_under(cwd, value));
            }
            "--stdin-packs" => stdin_packs = true,
            "--no-stdin-packs" => stdin_packs = false,
            "--bitmap" => write_bitmap = true,
            "--no-bitmap" => write_bitmap = false,
            "--incremental" => incremental = true,
            "--no-incremental" => incremental = false,
            "--write-chain-file" => write_chain_file = true,
            "--no-write-chain-file" => write_chain_file = false,
            "--base" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--base requires a value".into()))?;
                base_checksum = Some(value.clone());
            }
            "--preferred-pack" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--preferred-pack requires a value".into()))?;
                preferred_pack_name = Some(value.clone());
            }
            value if value.starts_with("--preferred-pack=") => {
                preferred_pack_name = Some(value["--preferred-pack=".len()..].to_string());
            }
            value if value.starts_with("--base=") => {
                base_checksum = Some(value["--base=".len()..].to_string());
            }
            "--refs-snapshot" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--refs-snapshot requires a value".into()))?;
                refs_snapshot = Some(resolve_path_under(cwd, value));
            }
            value if value.starts_with("--refs-snapshot=") => {
                refs_snapshot = Some(resolve_path_under(cwd, &value["--refs-snapshot=".len()..]));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index write option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(git_dir));
    let pack_dir = object_dir.join("pack");
    fs::create_dir_all(&pack_dir)?;
    if !write_chain_file && !incremental {
        eprintln!("error: cannot use --no-write-chain-file without --incremental");
        return Err(GitError::Exit(128));
    }
    if base_checksum.is_some() && write_chain_file {
        eprintln!("error: cannot use --base without --no-write-chain-file");
        return Err(GitError::Exit(128));
    }
    if incremental {
        return write_incremental(MidxWriteIncremental {
            git_dir,
            object_dir: &object_dir,
            pack_dir: &pack_dir,
            format,
            stdin_packs,
            write_bitmap,
            preferred_pack_name: preferred_pack_name.as_deref(),
            refs_snapshot: refs_snapshot.as_deref(),
            write_chain_file,
            base_checksum: base_checksum.as_deref(),
            progress,
        });
    }
    if progress {
        // Upstream shows a delayed progress meter labelled this way; with
        // GIT_PROGRESS_DELAY=0 it appears immediately. We emit a single line so
        // `--progress` produces non-empty stderr and the default stays silent.
        eprintln!("Adding packfiles to multi-pack-index");
    }

    // If a midx already exists on disk but its trailing checksum does not match
    // its contents, upstream refuses to reuse it and warns before rebuilding
    // from scratch (midx-write.c: "ignoring existing multi-pack-index; checksum
    // mismatch").
    let existing_midx = pack_dir.join("multi-pack-index");
    if let Ok(bytes) = fs::read(&existing_midx)
        && bytes.len() > format.raw_len()
    {
        let checksum_offset = bytes.len() - format.raw_len();
        if let Ok(actual) = sley_core::digest_bytes(format, &bytes[..checksum_offset])
            && actual.as_bytes() != &bytes[checksum_offset..]
        {
            eprintln!("warning: ignoring existing multi-pack-index; checksum mismatch");
        }
    }

    let mut pack_names = if let Some(pack_names) = selected_pack_names {
        pack_names
    } else if stdin_packs {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        input
            .lines()
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect()
    } else {
        let mut pack_names = Vec::new();
        for entry in fs::read_dir(&pack_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            pack_names.push(name.to_string());
        }
        pack_names
    };
    pack_names.sort();

    if pack_names.is_empty() {
        // Upstream resolves the preferred-pack name against the (here empty)
        // pack set first, warning when it is unknown, and only then refuses to
        // write a midx that would index zero packs (midx-write.c: "no pack
        // files to index."), exiting non-zero.
        if let Some(name) = &preferred_pack_name {
            eprintln!("warning: unknown preferred pack: '{name}'");
        }
        eprintln!("error: no pack files to index.");
        return Err(GitError::Exit(1));
    }
    let write_reverse_index = write_bitmap
        && env::var("GIT_TEST_MIDX_WRITE_REV").is_ok_and(|value| value == "1" || value == "true");
    let layer = build_midx_layer(
        sley_odb::MultiPackIndexLayerOptions {
            object_dir,
            format,
            version: 1,
            pack_names,
            excluded_oids: HashSet::new(),
            write_bitmap,
            preferred_pack_name,
            skip_if_unchanged: true,
        },
        |db| {
            Ok(sley_odb::MultiPackIndexBitmapInputs {
                preferred_tips: midx_bitmap_tips(git_dir, db, format, refs_snapshot.as_deref())?,
                pseudo_merge_groups: repack_pseudo_merge_groups(git_dir, db, format)?,
                write_lookup_table: write_bitmap_lookup_table,
                write_hash_cache: write_bitmap_hash_cache,
                restrict_to_tips: refs_snapshot.is_some(),
                write_reverse_index,
                missing_closure: sley_odb::MissingMidxBitmapPolicy::Error,
            })
        },
    )?;
    if layer.unchanged {
        return Ok(());
    }
    let midx_checksum = layer.checksum;
    let bitmap_name = format!("multi-pack-index-{midx_checksum}.bitmap");

    // The engine constructs every dependent artifact before the MIDX lands;
    // a bitmap closure failure therefore leaves no partially updated index.
    atomic_write(&pack_dir.join("multi-pack-index"), &layer.midx)?;
    remove_incremental_midx_dir(&pack_dir)?;

    let rev_name = format!("multi-pack-index-{midx_checksum}.rev");
    if let Some(reverse_index) = &layer.reverse_index {
        atomic_write(&pack_dir.join(&rev_name), reverse_index)?;
    }

    // Clear midx bitmap/rev sidecars that don't belong to this write: stale
    // checksums always; the current checksum's too when no bitmap was asked
    // for (upstream clear_midx_files_ext keeps only what it just wrote).
    for entry in fs::read_dir(&pack_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("multi-pack-index-")
            && (name.ends_with(".bitmap") || name.ends_with(".rev"))
            && (!layer.wrote_bitmap || name != bitmap_name)
            && (layer.reverse_index.is_none() || name != rev_name)
        {
            let _ = fs::remove_file(&path);
        }
    }

    if let Some(bitmap) = layer.bitmap {
        if write_bitmap_lookup_table {
            sley_core::trace2::region("pack-bitmap-write", "writing_lookup_table");
        }
        let bitmap_path = pack_dir.join(&bitmap_name);
        let temp_path = bitmap_path.with_extension("bitmap.tmp");
        fs::write(&temp_path, &bitmap)?;
        fs::rename(&temp_path, &bitmap_path)?;
    }
    Ok(())
}

fn build_midx_layer<F>(
    options: sley_odb::MultiPackIndexLayerOptions,
    bitmap_inputs: F,
) -> Result<sley_odb::MultiPackIndexLayerOutcome>
where
    F: FnOnce(&FileObjectDatabase) -> Result<sley_odb::MultiPackIndexBitmapInputs>,
{
    sley_odb::build_multi_pack_index_layer(options, bitmap_inputs, render_midx_event)
        .map_err(render_midx_error)
}

fn render_midx_event(event: sley_odb::MultiPackIndexEvent) {
    match event {
        sley_odb::MultiPackIndexEvent::UnknownPreferredPack(name) => {
            eprintln!("warning: unknown preferred pack: '{name}'");
        }
        sley_odb::MultiPackIndexEvent::RefusingEmptyBitmap => {
            eprintln!("warning: refusing to write multi-pack .bitmap without any objects");
        }
    }
}

fn render_midx_error(err: sley_odb::MultiPackIndexLayerError) -> GitError {
    match err {
        sley_odb::MultiPackIndexLayerError::Source(err) => err,
        sley_odb::MultiPackIndexLayerError::CouldNotLoadPack => {
            eprintln!("error: could not load pack");
            GitError::Exit(1)
        }
        sley_odb::MultiPackIndexLayerError::EmptyPreferredPack(path) => {
            eprintln!(
                "error: cannot select preferred pack {} with no objects",
                path.display()
            );
            GitError::Exit(255)
        }
        sley_odb::MultiPackIndexLayerError::BitmapUnavailable => {
            eprintln!("fatal: could not write multi-pack bitmap");
            GitError::Exit(1)
        }
    }
}

struct MidxWriteIncremental<'a> {
    git_dir: &'a Path,
    object_dir: &'a Path,
    pack_dir: &'a Path,
    format: ObjectFormat,
    stdin_packs: bool,
    write_bitmap: bool,
    preferred_pack_name: Option<&'a str>,
    refs_snapshot: Option<&'a Path>,
    write_chain_file: bool,
    base_checksum: Option<&'a str>,
    progress: bool,
}

#[derive(Clone)]
struct IncrementalMidxLayer {
    midx: MultiPackIndex,
}

fn write_incremental(options: MidxWriteIncremental<'_>) -> Result<()> {
    if options.progress {
        eprintln!("Adding packfiles to multi-pack-index");
    }

    let mut chain = read_midx_chain(options.pack_dir)?;
    if let Some(checksum) = migrate_single_midx_to_incremental(options.pack_dir, options.format)?
        && !chain.iter().any(|existing| existing == &checksum)
    {
        chain.push(checksum);
    }

    let base_chain = incremental_midx_base_chain(&chain, options.base_checksum)?;
    let layers = read_incremental_midx_layers(options.pack_dir, options.format, &base_chain)?;
    let mut chained_pack_names = HashSet::new();
    let mut chained_oids = HashSet::new();
    for layer in &layers {
        chained_pack_names.extend(layer.midx.pack_names.iter().cloned());
        chained_oids.extend(layer.midx.objects.iter().map(|entry| entry.oid));
    }

    let mut pack_names = collect_midx_pack_names(options.pack_dir, options.stdin_packs)?;
    pack_names.retain(|name| !chained_pack_names.contains(name));

    if pack_names.is_empty() {
        if chain.is_empty() {
            if let Some(name) = options.preferred_pack_name {
                eprintln!("warning: unknown preferred pack: '{name}'");
            }
            eprintln!("error: no pack files to index.");
            return Err(GitError::Exit(1));
        }
        if options.write_chain_file {
            write_midx_chain(options.pack_dir, &chain)?;
        }
        return Ok(());
    }

    let layer = build_midx_layer_from_packs(
        options.git_dir,
        options.object_dir,
        options.format,
        pack_names,
        &chained_oids,
        options.write_bitmap,
        options.preferred_pack_name,
        options.refs_snapshot,
        1,
    )?;
    install_incremental_midx_layer(options.pack_dir, options.format, &layer)?;
    if options.write_chain_file {
        chain.push(layer.checksum);
        write_midx_chain(options.pack_dir, &chain)?;
        clear_incremental_midx_sidecars(options.pack_dir, options.format)?;
    } else {
        println!("{}", layer.checksum);
    }
    Ok(())
}

fn incremental_midx_base_chain(
    chain: &[String],
    base_checksum: Option<&str>,
) -> Result<Vec<String>> {
    match base_checksum {
        None => Ok(chain.to_vec()),
        // Empty `--base=` (e.g. broken `$(nth_line …)` in t5334) is treated as
        // "no base override" — matching Git 2.55.0, which never threads
        // `incremental_base` into `write_midx_file` on the non-stdin path and
        // therefore silently ignores the option.
        Some("") => Ok(chain.to_vec()),
        Some("none") => Ok(Vec::new()),
        Some(base) => {
            let Some(index) = chain.iter().position(|checksum| checksum == base) else {
                eprintln!("error: unknown incremental MIDX base: {base}");
                return Err(GitError::Exit(1));
            };
            Ok(chain[..=index].to_vec())
        }
    }
}

struct BuiltMidxLayer {
    checksum: String,
    midx: Vec<u8>,
    bitmap: Option<Vec<u8>>,
    rev: Option<Vec<u8>>,
}

#[allow(clippy::too_many_arguments)]
fn build_midx_layer_from_packs(
    git_dir: &Path,
    object_dir: &Path,
    format: ObjectFormat,
    pack_names: Vec<String>,
    excluded_oids: &HashSet<ObjectId>,
    write_bitmap: bool,
    preferred_pack_name: Option<&str>,
    refs_snapshot: Option<&Path>,
    version: u8,
) -> Result<BuiltMidxLayer> {
    let write_reverse_index = write_bitmap
        && env::var("GIT_TEST_MIDX_WRITE_REV").is_ok_and(|value| value == "1" || value == "true");
    let outcome = build_midx_layer(
        sley_odb::MultiPackIndexLayerOptions {
            object_dir: object_dir.to_path_buf(),
            format,
            version,
            pack_names,
            excluded_oids: excluded_oids.clone(),
            write_bitmap,
            preferred_pack_name: preferred_pack_name.map(ToString::to_string),
            skip_if_unchanged: false,
        },
        |db| {
            Ok(sley_odb::MultiPackIndexBitmapInputs {
                preferred_tips: midx_bitmap_tips(git_dir, db, format, refs_snapshot)?,
                pseudo_merge_groups: repack_pseudo_merge_groups(git_dir, db, format)?,
                write_lookup_table: false,
                write_hash_cache: false,
                restrict_to_tips: false,
                write_reverse_index,
                missing_closure: sley_odb::MissingMidxBitmapPolicy::WriteEmpty,
            })
        },
    )?;
    Ok(BuiltMidxLayer {
        checksum: outcome.checksum.to_hex(),
        midx: outcome.midx,
        bitmap: outcome.bitmap,
        rev: outcome.reverse_index,
    })
}

/// Resolve the exact tip universe for a MIDX bitmap write. A refs snapshot is
/// a replacement for the live ref store, not an additive preference list:
/// plain and `+`-prefixed rows both name visible tips (`+` additionally marks
/// a preferred tip upstream, which sley's selection set already models).
fn midx_bitmap_tips(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    refs_snapshot: Option<&Path>,
) -> Result<HashSet<ObjectId>> {
    let Some(snapshot) = refs_snapshot else {
        return repack_preferred_bitmap_tips(git_dir, db, format);
    };
    let mut tips = HashSet::new();
    for line in fs::read_to_string(snapshot)?.lines() {
        let hex = line.strip_prefix('+').unwrap_or(line).trim();
        if hex.is_empty() {
            continue;
        }
        let oid = ObjectId::from_hex(format, hex)?;
        let commit = sley_rev::peel_to_commit(db, format, &oid)?;
        tips.insert(commit);
    }
    Ok(tips)
}

fn install_incremental_midx_layer(
    pack_dir: &Path,
    _format: ObjectFormat,
    layer: &BuiltMidxLayer,
) -> Result<()> {
    let midx_dir = incremental_midx_dir(pack_dir);
    fs::create_dir_all(&midx_dir)?;
    let midx_path = midx_dir.join(format!("multi-pack-index-{}.midx", layer.checksum));
    atomic_write(&midx_path, &layer.midx)?;
    if let Some(bitmap) = &layer.bitmap {
        atomic_write(
            &midx_dir.join(format!("multi-pack-index-{}.bitmap", layer.checksum)),
            bitmap,
        )?;
    }
    if let Some(rev) = &layer.rev {
        atomic_write(
            &midx_dir.join(format!("multi-pack-index-{}.rev", layer.checksum)),
            rev,
        )?;
    }
    Ok(())
}

fn collect_midx_pack_names(pack_dir: &Path, stdin_packs: bool) -> Result<Vec<String>> {
    let mut pack_names = if stdin_packs {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        input
            .lines()
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect()
    } else {
        let mut names = Vec::new();
        for entry in fs::read_dir(pack_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            names.push(name.to_string());
        }
        names
    };
    pack_names.sort();
    Ok(pack_names)
}

fn incremental_midx_dir(pack_dir: &Path) -> PathBuf {
    pack_dir.join("multi-pack-index.d")
}

fn midx_chain_path(pack_dir: &Path) -> PathBuf {
    incremental_midx_dir(pack_dir).join("multi-pack-index-chain")
}

fn read_midx_chain(pack_dir: &Path) -> Result<Vec<String>> {
    let path = midx_chain_path(pack_dir);
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn write_midx_chain(pack_dir: &Path, chain: &[String]) -> Result<()> {
    let midx_dir = incremental_midx_dir(pack_dir);
    fs::create_dir_all(&midx_dir)?;
    let mut contents = String::new();
    for checksum in chain {
        contents.push_str(checksum);
        contents.push('\n');
    }
    atomic_write(&midx_chain_path(pack_dir), contents.as_bytes())?;
    Ok(())
}

fn read_incremental_midx_layers(
    pack_dir: &Path,
    format: ObjectFormat,
    chain: &[String],
) -> Result<Vec<IncrementalMidxLayer>> {
    let midx_dir = incremental_midx_dir(pack_dir);
    let mut layers = Vec::with_capacity(chain.len());
    for checksum in chain {
        let path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
        let midx = MultiPackIndex::parse(&fs::read(path)?, format)?;
        layers.push(IncrementalMidxLayer { midx });
    }
    Ok(layers)
}

fn migrate_single_midx_to_incremental(
    pack_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<String>> {
    let midx_path = pack_dir.join("multi-pack-index");
    let Ok(bytes) = fs::read(&midx_path) else {
        return Ok(None);
    };
    if bytes.len() < format.raw_len() {
        return Ok(None);
    }
    let checksum = ObjectId::from_raw(format, &bytes[bytes.len() - format.raw_len()..])?.to_hex();
    let midx_dir = incremental_midx_dir(pack_dir);
    fs::create_dir_all(&midx_dir)?;
    let layer_path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
    atomic_write(&layer_path, &bytes)?;
    let _ = fs::remove_file(&midx_path);
    for ext in ["bitmap", "rev"] {
        let from = pack_dir.join(format!("multi-pack-index-{checksum}.{ext}"));
        if from.exists() {
            let to = midx_dir.join(format!("multi-pack-index-{checksum}.{ext}"));
            match fs::rename(&from, &to) {
                Ok(()) => {}
                Err(_) => {
                    fs::copy(&from, &to)?;
                    let _ = fs::remove_file(&from);
                }
            }
        }
    }
    Ok(Some(checksum))
}

fn remove_incremental_midx_dir(pack_dir: &Path) -> Result<()> {
    let midx_dir = incremental_midx_dir(pack_dir);
    let preserve_empty_dir = midx_dir.exists();
    match fs::remove_dir_all(&midx_dir) {
        Ok(()) if preserve_empty_dir => {
            fs::create_dir_all(&midx_dir).map_err(|err| GitError::Io(err.to_string()))
        }
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

fn clear_incremental_midx_sidecars(pack_dir: &Path, format: ObjectFormat) -> Result<()> {
    let chain = read_midx_chain(pack_dir)?;
    let keep: HashSet<String> = chain.into_iter().collect();
    let midx_dir = incremental_midx_dir(pack_dir);
    let Ok(entries) = fs::read_dir(&midx_dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("multi-pack-index-") {
            continue;
        }
        let Some((checksum, ext)) = name
            .strip_prefix("multi-pack-index-")
            .and_then(|rest| rest.rsplit_once('.'))
        else {
            continue;
        };
        if checksum.len() != format.hex_len() || !matches!(ext, "midx" | "bitmap" | "rev") {
            continue;
        }
        if !keep.contains(checksum) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

pub fn compact(
    cwd: &Path,
    git_dir: &Path,
    args: &[String],
) -> Result<()> {
    let format = repo_object_format(git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut write_bitmap = false;
    let mut endpoints = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_path_under(cwd, value));
            }
            value if value.starts_with("--object-dir=") => {
                object_dir = Some(resolve_path_under(cwd, &value["--object-dir=".len()..]));
            }
            "--bitmap" => write_bitmap = true,
            "--no-bitmap" => write_bitmap = false,
            // Compaction is inherently an incremental-chain operation; both
            // spellings are accepted and behave identically.
            "--incremental" | "--no-incremental" => {}
            "--progress" | "--no-progress" => {}
            other if other.starts_with('-') => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index compact option {other}"
                )));
            }
            other => endpoints.push(other.to_string()),
        }
    }
    if endpoints.len() != 2 {
        eprint!("{MULTI_PACK_INDEX_USAGE}");
        return Err(GitError::Exit(129));
    }
    let config = read_repo_config(git_dir)?;
    if config
        .get_entry("midx", None, "version")
        .flatten()
        .is_some_and(|value| value.trim() == "1")
    {
        eprintln!("fatal: cannot perform MIDX compaction with v1 format");
        return Err(GitError::Exit(128));
    }

    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(git_dir));
    let pack_dir = object_dir.join("pack");
    let mut chain = read_midx_chain(&pack_dir)?;
    let layers = read_incremental_midx_layers(&pack_dir, format, &chain)?;
    let Some(from_idx) = chain.iter().position(|checksum| checksum == &endpoints[0]) else {
        eprintln!("fatal: could not find MIDX: {}", endpoints[0]);
        return Err(GitError::Exit(128));
    };
    let Some(to_idx) = chain.iter().position(|checksum| checksum == &endpoints[1]) else {
        eprintln!("fatal: could not find MIDX: {}", endpoints[1]);
        return Err(GitError::Exit(128));
    };
    if from_idx == to_idx {
        eprintln!("fatal: MIDX compaction endpoints must be unique");
        return Err(GitError::Exit(128));
    }
    if from_idx > to_idx {
        eprintln!(
            "fatal: MIDX {} must be an ancestor of {}",
            endpoints[0], endpoints[1]
        );
        return Err(GitError::Exit(128));
    }

    let mut pack_names = Vec::new();
    let mut objects = Vec::new();
    let mut pack_name_to_id = HashMap::new();
    for layer in &layers[from_idx..=to_idx] {
        for name in &layer.midx.pack_names {
            if !pack_name_to_id.contains_key(name) {
                let id = pack_names.len() as u32;
                pack_name_to_id.insert(name.clone(), id);
                pack_names.push(name.clone());
            }
        }
        for entry in &layer.midx.objects {
            let Some(old_name) = layer.midx.pack_names.get(entry.pack_int_id as usize) else {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index object points past pack table".into(),
                ));
            };
            let new_pack = *pack_name_to_id.get(old_name).ok_or_else(|| {
                GitError::InvalidFormat("compacted MIDX pack missing from table".into())
            })?;
            objects.push(MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: new_pack,
                offset: entry.offset,
                force_large_offset: entry.force_large_offset,
            });
        }
    }
    objects.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
    objects.dedup_by(|next, kept| next.oid == kept.oid);

    let write_reverse_index = write_bitmap
        && env::var("GIT_TEST_MIDX_WRITE_REV").is_ok_and(|value| value == "1" || value == "true");
    let compacted = sley_odb::build_multi_pack_index_layer_from_entries(
        sley_odb::MultiPackIndexEntryLayerOptions {
            object_dir,
            format,
            version: 2,
            pack_names,
            objects,
            write_bitmap,
            preferred_pack: write_bitmap.then_some(0),
        },
        |db| {
            Ok(sley_odb::MultiPackIndexBitmapInputs {
                preferred_tips: midx_bitmap_tips(git_dir, db, format, None)?,
                pseudo_merge_groups: repack_pseudo_merge_groups(git_dir, db, format)?,
                write_lookup_table: false,
                write_hash_cache: false,
                restrict_to_tips: false,
                write_reverse_index,
                missing_closure: sley_odb::MissingMidxBitmapPolicy::WriteEmpty,
            })
        },
    )
    .map_err(render_midx_error)?;
    let compacted = BuiltMidxLayer {
        checksum: compacted.checksum.to_hex(),
        midx: compacted.midx,
        bitmap: compacted.bitmap,
        rev: compacted.reverse_index,
    };
    install_incremental_midx_layer(&pack_dir, format, &compacted)?;

    let old: HashSet<String> = chain[from_idx..=to_idx].iter().cloned().collect();
    chain.splice(from_idx..=to_idx, std::iter::once(compacted.checksum));
    write_midx_chain(&pack_dir, &chain)?;

    let retained: HashSet<String> = chain.iter().cloned().collect();
    for checksum in old {
        if retained.contains(&checksum) {
            continue;
        }
        for ext in ["midx", "bitmap", "rev"] {
            let _ = fs::remove_file(
                incremental_midx_dir(&pack_dir).join(format!("multi-pack-index-{checksum}.{ext}")),
            );
        }
    }
    clear_incremental_midx_sidecars(&pack_dir, format)?;
    Ok(())
}

/// Scan `<object_dir>/pack` for `.idx` files and write a fresh, non-bitmap
/// multi-pack-index over them, applying upstream's cross-pack duplicate
/// resolution (keep the copy from the newest pack, ties broken by lowest pack
/// id). This is the default `multi-pack-index write` behaviour, factored out so
/// `repack` and `expire` can rewrite the midx after changing the pack set.
/// Packs named in `skip` (by `.idx` basename) are left out of the new midx —
/// `expire` passes the packs it is about to delete so the rewritten index
/// lands before any unlink.
fn write_default_midx(object_dir: &Path, format: ObjectFormat, skip: &[String]) -> Result<()> {
    let pack_dir = object_dir.join("pack");
    let mut pack_names = Vec::new();
    for entry in fs::read_dir(&pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && !skip.iter().any(|skipped| skipped == name)
        {
            pack_names.push(name.to_string());
        }
    }
    pack_names.sort();
    if pack_names.is_empty() {
        // Nothing to index; remove any stale midx so callers observe the empty
        // state the way upstream leaves it after dropping the last pack.
        let _ = fs::remove_file(pack_dir.join("multi-pack-index"));
        return Ok(());
    }

    let pack_mtimes: Vec<std::time::SystemTime> = pack_names
        .iter()
        .map(|name| {
            fs::metadata(pack_dir.join(name).with_extension("pack"))
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
        .collect();
    let pack_mtime = |pack_int_id: u32| -> std::time::SystemTime {
        pack_mtimes
            .get(pack_int_id as usize)
            .copied()
            .unwrap_or(std::time::UNIX_EPOCH)
    };

    let mut objects = Vec::new();
    for (pack_int_id, pack_name) in pack_names.iter().enumerate() {
        let index =
            PackIndex::parse_without_checksum(&fs::read(pack_dir.join(pack_name))?, format)?;
        for entry in index.entries {
            objects.push(MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: pack_int_id as u32,
                offset: entry.offset,
                force_large_offset: false,
            });
        }
    }

    objects.sort_by(|left, right| {
        left.oid
            .as_bytes()
            .cmp(right.oid.as_bytes())
            .then_with(|| pack_mtime(right.pack_int_id).cmp(&pack_mtime(left.pack_int_id)))
            .then_with(|| left.pack_int_id.cmp(&right.pack_int_id))
    });
    objects.dedup_by(|next, kept| next.oid == kept.oid);

    let midx = MultiPackIndex::write(format, 1, &pack_names, &objects)?;

    // Clear stale bitmap/rev sidecars not produced by this (non-bitmap) write.
    for entry in fs::read_dir(&pack_dir)? {
        let path = entry?.path();
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with("multi-pack-index-")
            && (name.ends_with(".bitmap") || name.ends_with(".rev"))
        {
            let _ = fs::remove_file(&path);
        }
    }

    atomic_write(&pack_dir.join("multi-pack-index"), &midx)?;
    Ok(())
}

/// Parse the `--object-dir`/`--progress` options shared by `repack` and
/// `expire`, returning the resolved object dir and whether progress is forced.
fn parse_midx_object_dir_and_progress(
    args: &[String],
    cwd: &Path,
    git_dir: &Path,
    subcommand: &str,
) -> Result<(PathBuf, bool, Option<u64>)> {
    let mut object_dir: Option<PathBuf> = None;
    let mut progress = false;
    let mut batch_size: Option<u64> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_path_under(cwd, value));
            }
            value if value.starts_with("--object-dir=") => {
                let value = &value["--object-dir=".len()..];
                object_dir = Some(resolve_path_under(cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--batch-size" if subcommand == "repack" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--batch-size requires a value".into()))?;
                batch_size = Some(value.parse().map_err(|_| {
                    GitError::Command("option `batch-size' expects a numerical value".into())
                })?);
            }
            value if subcommand == "repack" && value.starts_with("--batch-size=") => {
                let value = &value["--batch-size=".len()..];
                batch_size = Some(value.parse().map_err(|_| {
                    GitError::Command("option `batch-size' expects a numerical value".into())
                })?);
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index {subcommand} option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(git_dir));
    Ok((object_dir, progress, batch_size))
}

/// Whether a pack (named by its `.idx` basename) carries a `.keep` companion.
fn pack_has_keep(pack_dir: &Path, idx_name: &str) -> bool {
    let keep = pack_dir.join(idx_name).with_extension("keep");
    keep.exists()
}

/// Whether a pack (named by its `.idx` basename) is a cruft pack, i.e. has a
/// `.mtimes` companion (upstream marks cruft packs this way).
fn pack_is_cruft(pack_dir: &Path, idx_name: &str) -> bool {
    pack_dir.join(idx_name).with_extension("mtimes").exists()
}

pub fn repack(
    cwd: &Path,
    git_dir: &Path,
    args: &[String],
) -> Result<()> {
    let format = repo_object_format(git_dir)?;
    let (object_dir, progress, batch_size) =
        parse_midx_object_dir_and_progress(args, cwd, git_dir, "repack")?;
    let pack_dir = object_dir.join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    if !midx_path.exists() {
        return Ok(());
    }
    let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, format)?;
    let num_packs = midx.pack_names.len();

    // referenced[i] = number of midx objects whose copy lives in pack i, and
    // per-pack file metadata used for batch selection.
    let mut referenced = vec![0u64; num_packs];
    for entry in &midx.objects {
        if (entry.pack_int_id as usize) < num_packs {
            referenced[entry.pack_int_id as usize] += 1;
        }
    }
    let mut pack_objects_total = vec![0u64; num_packs];
    let mut pack_size = vec![0u64; num_packs];
    let mut pack_mtime = vec![std::time::UNIX_EPOCH; num_packs];
    for (i, name) in midx.pack_names.iter().enumerate() {
        if let Ok(index) =
            PackIndex::parse_without_checksum(&fs::read(pack_dir.join(name))?, format)
        {
            pack_objects_total[i] = index.entries.len() as u64;
        }
        let pack_path = pack_dir.join(name).with_extension("pack");
        if let Ok(meta) = fs::metadata(&pack_path) {
            pack_size[i] = meta.len();
            if let Ok(mtime) = meta.modified() {
                pack_mtime[i] = mtime;
            }
        }
    }

    let config = read_repo_config(git_dir)?;
    let pack_kept_objects = config
        .get_bool("repack", None, "packKeptObjects")
        .unwrap_or(false);

    let want = |i: usize| -> bool {
        if !pack_kept_objects && pack_has_keep(&pack_dir, &midx.pack_names[i]) {
            return false;
        }
        if pack_is_cruft(&pack_dir, &midx.pack_names[i]) {
            return false;
        }
        pack_objects_total[i] > 0
    };

    let mut include = vec![false; num_packs];
    match batch_size {
        None | Some(0) => {
            for (i, slot) in include.iter_mut().enumerate() {
                if want(i) {
                    *slot = true;
                }
            }
        }
        Some(batch_size) => {
            if pack_size
                .iter()
                .enumerate()
                .filter_map(|(i, size)| want(i).then_some(*size))
                .min()
                .is_some_and(|min_size| batch_size <= min_size)
            {
                return Ok(());
            }

            // Visit packs smallest-mtime first; include the smaller packs whose
            // expected (reference-proportional) size keeps the running total
            // under the batch, skipping any single pack already >= batch.
            let mut order: Vec<usize> = (0..num_packs).collect();
            order.sort_by(|&a, &b| pack_mtime[a].cmp(&pack_mtime[b]));
            let mut total: u64 = 0;
            for i in order {
                if total >= batch_size {
                    break;
                }
                if !want(i) {
                    continue;
                }
                // expected_size ~= referenced/num_objects * pack_size, in the
                // same shifted-integer form upstream uses.
                let objects = pack_objects_total[i].max(1);
                let mut expected = (referenced[i] << 14) / objects;
                expected = expected.saturating_mul(pack_size[i]);
                expected = (expected + (1 << 13)) >> 14;
                if expected >= batch_size {
                    continue;
                }
                total = total.saturating_add(expected);
                include[i] = true;
            }
        }
    }

    let packs_to_repack = include.iter().filter(|&&v| v).count();
    if packs_to_repack <= 1 {
        return Ok(());
    }
    if progress {
        // Upstream forwards `--progress` to the spawned pack-objects, which
        // prints its own meters once it actually has packs to combine. A single
        // line keeps `--progress` non-empty while the default stays silent.
        eprintln!("Repacking multi-pack-index");
    }

    // Collect the oids whose copy lives in an included pack, then build one new
    // pack from them and rewrite the midx. Objects stream through the writer's
    // bounded compression windows instead of all being resident at once.
    let db = FileObjectDatabase::new(object_dir.clone(), format);
    let inputs_oids: Vec<ObjectId> = midx
        .objects
        .iter()
        .filter(|entry| {
            include
                .get(entry.pack_int_id as usize)
                .copied()
                .unwrap_or(false)
        })
        .map(|entry| entry.oid)
        .collect();
    let object_count = u32::try_from(inputs_oids.len())
        .map_err(|_| GitError::InvalidFormat("too many objects to repack".into()))?;

    let mut pack_bytes = Vec::new();
    let summary = PackFile::write_packed_from_source_to_writer(
        inputs_oids.iter().copied(),
        object_count,
        format,
        &PackWriteOptions::new(),
        PackWriteLimits::default(),
        |oid| db.read_object(oid),
        &mut pack_bytes,
    )?;
    let checksum = summary.checksum.to_hex();
    let base = pack_dir.join(format!("pack-{checksum}"));
    let positions = pack_order_index_positions(&summary.entries);
    let reverse_index = PackReverseIndex::write(format, &positions, &summary.checksum)?;
    fs::write(base.with_extension("pack"), &pack_bytes)?;
    fs::write(base.with_extension("rev"), &reverse_index)?;
    fs::write(base.with_extension("idx"), &summary.index)?;

    write_default_midx(&object_dir, format, &[])
}

pub fn verify(
    cwd: &Path,
    git_dir: &Path,
    args: &[String],
) -> Result<()> {
    let format = repo_object_format(git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    // Upstream defaults progress to a tty heuristic; when stderr is not a tty
    // it stays off. `--progress` forces it on, `--no-progress` forces it off.
    // Our test oracle never has a tty, so default off and honour the flags.
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_path_under(cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_path_under(cwd, value));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index verify option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(git_dir));
    verify_midx_at(&object_dir, format, progress)
}

/// Run the full upstream `verify_midx_file` pass over the multi-pack-index in
/// `<object_dir>/pack/multi-pack-index`, emitting git-exact error substrings to
/// stderr and returning `GitError::Exit(1)` on any detected corruption.
///
/// Parse-time corruptions (signature, version, hash version, chunk table,
/// fanout order, pack names order, pack-int-id) abort with git's load-time
/// `die()`/`error()` strings; verify-time corruptions (incorrect checksum,
/// failed pack load, no oid, oid lookup order, incorrect object offset) are
/// reported the way upstream's verify pass reports them.
pub fn verify_midx_at(
    object_dir: &Path,
    format: ObjectFormat,
    progress: bool,
) -> Result<()> {
    let pack_dir = object_dir.join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    let bytes = match fs::read(&midx_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // No midx ⇒ upstream verify is a no-op success.
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    let parsed = match parse_midx_for_verify(&bytes, format) {
        Ok(parsed) => parsed,
        Err(message) => {
            if message == "multi-pack-index large offset out of bounds" {
                eprintln!("error: incorrect object offset");
            } else {
                eprintln!("error: {message}");
            }
            return Err(GitError::Exit(1));
        }
    };

    let mut reported = false;
    let mut report = |message: String| {
        eprintln!("error: {message}");
        reported = true;
    };

    // Verify-time checksum check: recompute over everything but the trailing
    // hash and compare.
    let checksum_offset = bytes.len() - format.raw_len();
    let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
    if actual_checksum.as_bytes() != &bytes[checksum_offset..] {
        report("incorrect checksum".to_string());
    }

    if progress {
        eprintln!("Looking for referenced packfiles");
    }

    // Load each referenced pack-index; a missing/corrupt one is reported but
    // not fatal to the rest of the pass.
    let mut pack_indexes: Vec<Option<PackIndex>> = Vec::with_capacity(parsed.pack_names.len());
    for (position, name) in parsed.pack_names.iter().enumerate() {
        match fs::read(pack_dir.join(name))
            .ok()
            .and_then(|raw| PackIndex::parse_without_checksum(&raw, format).ok())
        {
            Some(index) => pack_indexes.push(Some(index)),
            None => {
                report(format!("failed to load pack in position {position}"));
                pack_indexes.push(None);
            }
        }
    }

    if parsed.object_count == 0 {
        report("the midx contains no oid".to_string());
        return if reported {
            Err(GitError::Exit(1))
        } else {
            Ok(())
        };
    }

    if progress {
        eprintln!("Verifying OID order in multi-pack-index");
    }
    for window in parsed.entries.windows(2) {
        if window[0].oid.as_bytes() >= window[1].oid.as_bytes() {
            report(format!(
                "oid lookup out of order: oid[?] = {} >= {} = oid[?]",
                window[0].oid.to_hex(),
                window[1].oid.to_hex()
            ));
        }
    }

    if progress {
        eprintln!("Verifying object offsets");
    }
    // Build per-pack offset lookups once, then check each midx entry's offset
    // against the pack's own .idx.
    for (idx, entry) in parsed.entries.iter().enumerate() {
        let Some(Some(index)) = pack_indexes.get(entry.pack_int_id as usize) else {
            // A pack that failed to load was already reported above.
            continue;
        };
        let pack_offset = index.find(&entry.oid).map(|e| e.offset);
        match pack_offset {
            Some(offset) if offset == entry.offset => {}
            Some(offset) => report(format!(
                "incorrect object offset for oid[{idx}] = {}: {:x} != {:x}",
                entry.oid.to_hex(),
                entry.offset,
                offset
            )),
            None => report(format!(
                "failed to load pack entry for oid[{idx}] = {}",
                entry.oid.to_hex()
            )),
        }
    }

    if reported {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

/// A minimal parse of the multi-pack-index used purely by verify, reproducing
/// upstream `load_multi_pack_index`'s die/error wording so corruption tests
/// match byte-for-byte. Returns the human-facing message string on failure.
struct VerifyMidx {
    pack_names: Vec<String>,
    object_count: usize,
    entries: Vec<MultiPackIndexEntry>,
}

fn parse_midx_for_verify(
    bytes: &[u8],
    format: ObjectFormat,
) -> std::result::Result<VerifyMidx, String> {
    let hash_len = format.raw_len();
    const MIDX_HEADER_SIZE: usize = 12;
    if bytes.len() < MIDX_HEADER_SIZE + 12 + hash_len {
        return Err("multi-pack-index file is too small".to_string());
    }
    if &bytes[..4] != b"MIDX" {
        return Err(format!(
            "multi-pack-index signature 0x{:08x} does not match signature 0x{:08x}",
            sley_core::primitives::u32_be(&bytes[..4]),
            u32::from_be_bytes(*b"MIDX")
        ));
    }
    let version = bytes[4];
    if version != 1 && version != 2 {
        return Err(format!("multi-pack-index version {version} not recognized"));
    }
    let hash_version = bytes[5];
    let expected_hash_version = match format {
        ObjectFormat::Sha1 => 1u8,
        ObjectFormat::Sha256 => 2u8,
    };
    if hash_version != expected_hash_version {
        return Err(format!(
            "multi-pack-index hash version {hash_version} does not match version {expected_hash_version}"
        ));
    }
    let num_chunks = bytes[6] as usize;
    let num_packs = sley_core::primitives::u32_be(&bytes[8..12]) as usize;

    // Table of contents: num_chunks entries of (id:4, offset:8) plus a
    // terminating entry. Reproduce read_table_of_contents's check order so the
    // truncated/extended/improper-offset corruptions report git's exact text.
    let checksum_offset = bytes.len() - hash_len;
    let mut chunks: Vec<([u8; 4], u64)> = Vec::with_capacity(num_chunks);
    let mut toc = MIDX_HEADER_SIZE;
    for _ in 0..num_chunks {
        if toc + 12 + 12 > bytes.len() {
            return Err("multi-pack-index file is too small".to_string());
        }
        let chunk_id = [bytes[toc], bytes[toc + 1], bytes[toc + 2], bytes[toc + 3]];
        let chunk_offset = sley_core::primitives::u64_be(&bytes[toc + 4..toc + 12]);
        if chunk_id == [0, 0, 0, 0] {
            return Err("terminating chunk id appears earlier than expected".to_string());
        }
        // CHUNK alignment for midx is 1 byte, so alignment never trips.
        let next_offset = sley_core::primitives::u64_be(&bytes[toc + 12 + 4..toc + 12 + 12]);
        if next_offset < chunk_offset || next_offset > checksum_offset as u64 {
            return Err(format!(
                "improper chunk offset(s) {chunk_offset:x} and {next_offset:x}"
            ));
        }
        chunks.push((chunk_id, chunk_offset));
        toc += 12;
    }
    // The final (terminating) entry must have a zero id.
    let final_id = [bytes[toc], bytes[toc + 1], bytes[toc + 2], bytes[toc + 3]];
    if final_id != [0, 0, 0, 0] {
        return Err(format!(
            "final chunk has non-zero id {:x}",
            sley_core::primitives::u32_be(&final_id)
        ));
    }
    let final_offset = sley_core::primitives::u64_be(&bytes[toc + 4..toc + 12]);

    // Resolve a chunk's data slice using the next chunk's start (or the
    // terminator) as the end.
    let chunk_slice = |want: &[u8; 4]| -> Option<(usize, usize)> {
        for i in 0..chunks.len() {
            if &chunks[i].0 == want {
                let start = chunks[i].1 as usize;
                let end = if i + 1 < chunks.len() {
                    chunks[i + 1].1 as usize
                } else {
                    final_offset as usize
                };
                return Some((start, end));
            }
        }
        None
    };

    // PNAM (required).
    let Some((pnam_start, pnam_end)) = chunk_slice(b"PNAM") else {
        return Err("multi-pack-index required pack-name chunk missing or corrupted".to_string());
    };
    let pnam = &bytes[pnam_start..pnam_end.min(bytes.len())];
    let mut pack_names = Vec::with_capacity(num_packs);
    let mut cursor = 0usize;
    for _ in 0..num_packs {
        let Some(nul) = pnam[cursor..].iter().position(|b| *b == 0) else {
            return Err("multi-pack-index pack-name chunk is too short".to_string());
        };
        let name = String::from_utf8_lossy(&pnam[cursor..cursor + nul]).into_owned();
        if version == 1
            && let Some(prev) = pack_names.last()
            && &name <= prev
        {
            return Err(format!(
                "multi-pack-index pack names out of order: '{prev}' before '{name}'"
            ));
        }
        pack_names.push(name);
        cursor += nul + 1;
    }

    // OIDF (required) — fanout monotonicity is checked at load time.
    let Some((oidf_start, oidf_end)) = chunk_slice(b"OIDF") else {
        return Err("multi-pack-index required OID fanout chunk missing or corrupted".to_string());
    };
    let oidf = &bytes[oidf_start..oidf_end.min(bytes.len())];
    if oidf.len() != 256 * 4 {
        return Err("multi-pack-index OID fanout is of the wrong size".to_string());
    }
    let fanout: Vec<u32> = (0..256).map(|i| sley_core::primitives::u32_be(&oidf[i * 4..i * 4 + 4])).collect();
    for i in 0..255 {
        if fanout[i] > fanout[i + 1] {
            return Err(format!(
                "oid fanout out of order: fanout[{i}] = {:x} > {:x} = fanout[{}]",
                fanout[i],
                fanout[i + 1],
                i + 1
            ));
        }
    }
    let object_count = fanout[255] as usize;

    // OIDL (required).
    let Some((oidl_start, oidl_end)) = chunk_slice(b"OIDL") else {
        return Err("multi-pack-index required OID lookup chunk missing or corrupted".to_string());
    };
    let oidl = &bytes[oidl_start..oidl_end.min(bytes.len())];
    if oidl.len() != object_count * hash_len {
        return Err("multi-pack-index OID lookup chunk is the wrong size".to_string());
    }

    // OOFF (required).
    let Some((ooff_start, ooff_end)) = chunk_slice(b"OOFF") else {
        return Err(
            "multi-pack-index required object offsets chunk missing or corrupted".to_string(),
        );
    };
    let ooff = &bytes[ooff_start..ooff_end.min(bytes.len())];
    if ooff.len() != object_count * 8 {
        return Err("multi-pack-index object offset chunk is the wrong size".to_string());
    }

    // LOFF (optional).
    let loff = chunk_slice(b"LOFF").map(|(s, e)| &bytes[s..e.min(bytes.len())]);

    let mut entries = Vec::with_capacity(object_count);
    for i in 0..object_count {
        let oid = ObjectId::from_raw(format, &oidl[i * hash_len..i * hash_len + hash_len])
            .map_err(|err| err.to_string())?;
        let pack_int_id = sley_core::primitives::u32_be(&ooff[i * 8..i * 8 + 4]);
        if pack_int_id as usize >= num_packs {
            return Err(format!(
                "bad pack-int-id: {pack_int_id} ({num_packs} total packs)"
            ));
        }
        let raw_offset = sley_core::primitives::u32_be(&ooff[i * 8 + 4..i * 8 + 8]);
        let offset = if raw_offset & 0x8000_0000 == 0 {
            u64::from(raw_offset)
        } else {
            let large_idx = (raw_offset & 0x7fff_ffff) as usize;
            let loff = loff.ok_or_else(|| "multi-pack-index missing LOFF chunk".to_string())?;
            if large_idx * 8 + 8 > loff.len() {
                return Err("multi-pack-index large offset out of bounds".to_string());
            }
            sley_core::primitives::u64_be(&loff[large_idx * 8..large_idx * 8 + 8])
        };
        entries.push(MultiPackIndexEntry {
            oid,
            pack_int_id,
            offset,
            force_large_offset: raw_offset & 0x8000_0000 != 0,
        });
    }

    Ok(VerifyMidx {
        pack_names,
        object_count,
        entries,
    })
}

pub fn expire(
    cwd: &Path,
    git_dir: &Path,
    args: &[String],
) -> Result<()> {
    let format = repo_object_format(git_dir)?;
    let (object_dir, progress, _) =
        parse_midx_object_dir_and_progress(args, cwd, git_dir, "expire")?;
    let pack_dir = object_dir.join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    if !midx_path.exists() {
        return Ok(());
    }
    let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, format)?;
    if progress {
        // Upstream shows two delayed progress meters during expiration; with
        // GIT_PROGRESS_DELAY=0 they appear immediately, even when nothing is
        // dropped. Emit a line so `--progress` is non-empty and default silent.
        eprintln!("Counting referenced objects");
    }
    let num_packs = midx.pack_names.len();

    // Count how many of the midx's surviving objects each pack actually
    // provides. A pack whose objects are all dedup-covered by newer packs ends
    // up with zero references and is a deletion candidate.
    let mut count = vec![0u64; num_packs];
    for entry in &midx.objects {
        if (entry.pack_int_id as usize) < num_packs {
            count[entry.pack_int_id as usize] += 1;
        }
    }

    let mut dropped = Vec::new();
    for (i, name) in midx.pack_names.iter().enumerate() {
        if count[i] != 0 {
            continue;
        }
        // Never expire a kept or cruft pack.
        if pack_has_keep(&pack_dir, name) || pack_is_cruft(&pack_dir, name) {
            continue;
        }
        dropped.push(name.clone());
    }

    if !dropped.is_empty() {
        // Upstream rewrites the multi-pack-index BEFORE unlinking the expired
        // packs: a crash (or parse failure) in between must never leave a live
        // midx referencing deleted packs — our own verify would call that
        // state corrupt. This mirrors the compaction order above, where the
        // replacement layer is installed before the old ones are removed.
        write_default_midx(&object_dir, format, &dropped)?;
        for name in &dropped {
            // Drop the pack and all its companions.
            let stem = pack_dir.join(name);
            for ext in ["pack", "idx", "rev", "bitmap", "mtimes", "keep"] {
                let _ = fs::remove_file(stem.with_extension(ext));
            }
        }
    }
    Ok(())
}
