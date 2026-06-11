//! `git pack-objects`: write a pack (with its `.idx` and `.rev` companions)
//! from an explicit object list on standard input, or from a revision
//! traversal (`--revs` / `--all`).
//!
//! The traversal mode mirrors upstream's `get_object_list`: wants are the
//! positive revs (every ref tip plus `HEAD` under `--all`), haves the
//! `^`-prefixed ones; the packed set is the wants' reachability closure minus
//! the haves' closure, with missing objects tolerated on the have side only
//! (upstream marks the uninteresting side best-effort). The `--local`,
//! `--honor-pack-keep` and `--incremental` exclusions reproduce
//! `want_object_in_pack`'s veto rules over every on-disk copy of an object.
//!
//! When nothing restricts the want set (no haves, no exclusion flags, bitmaps
//! allowed), a bitmapped pack whose objects are all wanted is reused verbatim
//! (upstream's pack-reuse fast path, whole-pack case): its entry bytes are
//! copied as-is and only the remaining objects are encoded fresh. The
//! `pack-reused` / `packs-reused` totals and trace2 data events report that
//! reuse exactly like upstream.

use std::io::BufRead;
use std::io::IsTerminal;
use std::sync::Arc;

use sley_pack::{pack_order_index_positions, PackInput, PackReverseIndex, PackWriteOptions};

use crate::*;

#[derive(Default)]
struct PackObjectsOptions {
    base_name: Option<String>,
    stdout_mode: bool,
    revs: bool,
    all: bool,
    local: bool,
    honor_pack_keep: bool,
    incremental: bool,
    unpacked: bool,
    use_bitmap_index: Option<bool>,
    progress: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackReuseMode {
    None,
    Single,
    Multi,
}

pub(crate) fn cmd_pack_objects(args: &[String]) -> Result<()> {
    let mut options = PackObjectsOptions::default();
    let mut saw_dashdash = false;
    for arg in args {
        match arg.as_str() {
            "--" if !saw_dashdash => saw_dashdash = true,
            "--stdout" if !saw_dashdash => options.stdout_mode = true,
            "--revs" if !saw_dashdash => options.revs = true,
            "--all" if !saw_dashdash => options.all = true,
            "--local" if !saw_dashdash => options.local = true,
            "--no-local" if !saw_dashdash => options.local = false,
            "--honor-pack-keep" if !saw_dashdash => options.honor_pack_keep = true,
            "--no-honor-pack-keep" if !saw_dashdash => options.honor_pack_keep = false,
            "--incremental" if !saw_dashdash => options.incremental = true,
            "--no-incremental" if !saw_dashdash => options.incremental = false,
            "--unpacked" if !saw_dashdash => options.unpacked = true,
            "--use-bitmap-index" if !saw_dashdash => options.use_bitmap_index = Some(true),
            "--no-use-bitmap-index" if !saw_dashdash => options.use_bitmap_index = Some(false),
            // sley's writer always emits self-contained packs and chooses
            // ofs-delta internally; the delta-encoding and path-walk toggles
            // have no separate machinery to switch.
            "--delta-base-offset" | "--no-delta-base-offset" | "--path-walk" | "--no-path-walk"
                if !saw_dashdash => {}
            "-q" | "--quiet" if !saw_dashdash => options.progress = Some(false),
            "--no-quiet" if !saw_dashdash => {}
            "--progress" | "--all-progress" | "--all-progress-implied" if !saw_dashdash => {
                options.progress = Some(true)
            }
            "--no-progress" if !saw_dashdash => options.progress = Some(false),
            "--no-all-progress" | "--no-all-progress-implied" if !saw_dashdash => {}
            value if !saw_dashdash && value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported pack-objects option {value}"
                )));
            }
            value => {
                if options.base_name.is_some() {
                    return pack_objects_usage();
                }
                options.base_name = Some(value.to_string());
            }
        }
    }
    if options.base_name.is_none() && !options.stdout_mode {
        return pack_objects_usage();
    }
    if options.base_name.is_some() && options.stdout_mode {
        return pack_objects_usage();
    }

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let database = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let progress = options
        .progress
        .unwrap_or_else(|| io::stderr().is_terminal());

    let traversal = options.revs || options.all;
    let (mut oids, mut objects, reused_packs) = if traversal {
        collect_traversal_objects(&git_dir, &common_git_dir, &database, format, &options)?
    } else {
        let oids = read_pack_objects_stdin(format)?;
        let mut objects = Vec::with_capacity(oids.len());
        for oid in &oids {
            match database.read_object(oid) {
                Ok(object) => objects.push(object),
                Err(GitError::NotFound(_)) => {
                    eprintln!("fatal: unable to read {oid}");
                    return Err(GitError::Exit(128));
                }
                Err(err) => return Err(err),
            }
        }
        (oids, objects, Vec::new())
    };
    let pack_write_options = pack_objects_write_options(&git_dir)?;
    if traversal && pack_write_options.depth == 0 {
        sort_no_delta_traversal_pack(format, &mut oids, &mut objects)?;
    }

    if progress {
        // The enumeration meter counts every packed object, the verbatim
        // reused ones included (upstream's bitmap path displays the full
        // result cardinality).
        let enumerated =
            oids.len() as u64 + reused_packs.iter().map(|reuse| reuse.count as u64).sum::<u64>();
        eprintln!("Enumerating objects: {enumerated}, done.");
    }

    let inputs: Vec<PackInput<'_>> = oids
        .iter()
        .zip(&objects)
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect();

    let pack_reused: u64 = reused_packs.iter().map(|reuse| reuse.count as u64).sum();
    let packs_reused = reused_packs.len() as u64;
    let written_total = oids.len() as u64 + pack_reused;
    let stats_line = format!(
        "Total {written_total} (delta 0), reused {pack_reused} (delta 0), pack-reused {pack_reused} (from {packs_reused})"
    );

    let result = if !reused_packs.is_empty() {
        // Verbatim whole-pack reuse: splice the bitmapped pack's entries and
        // append the rest undeltified.
        let reused_entry_bytes: Vec<&[u8]> = reused_packs
            .iter()
            .flat_map(|reuse| reuse.entry_bytes.iter().map(Vec::as_slice))
            .collect();
        let (pack, _) =
            sley_odb::assemble_pack_with_verbatim_entries(format, &reused_entry_bytes, &inputs)?;
        if options.stdout_mode {
            let mut stdout = io::stdout();
            stdout.write_all(&pack)?;
            stdout.flush()?;
            emit_pack_objects_totals(progress, &stats_line, pack_reused, packs_reused);
            return Ok(());
        }
        // File output re-indexes the assembled stream so the `.idx`/`.rev`
        // companions cover the reused entries too.
        let build = PackIndex::write_v2_for_pack(&pack, format)?;
        WrittenPackParts {
            pack,
            index: build.index,
            entries: build.entries,
            checksum: build.pack_checksum,
        }
    } else {
        let written =
            PackFile::write_packed_with_known_ids_and_options(&inputs, format, &pack_write_options)?;
        if options.stdout_mode {
            let mut stdout = io::stdout();
            stdout.write_all(&written.pack)?;
            stdout.flush()?;
            emit_pack_objects_totals(progress, &stats_line, pack_reused, packs_reused);
            return Ok(());
        }
        WrittenPackParts {
            pack: written.pack,
            index: written.index,
            entries: written.entries,
            checksum: written.checksum,
        }
    };

    let base_name = options.base_name.expect("checked above");
    let positions = pack_order_index_positions(&result.entries);
    let reverse_index = PackReverseIndex::write(format, &positions, &result.checksum)?;

    // Write the pack before its lookup companions so no reader ever sees an
    // index that points at a missing or incomplete pack.
    let checksum = result.checksum.to_hex();
    fs::write(format!("{base_name}-{checksum}.pack"), &result.pack)?;
    fs::write(format!("{base_name}-{checksum}.rev"), &reverse_index)?;
    fs::write(format!("{base_name}-{checksum}.idx"), &result.index)?;
    println!("{checksum}");
    emit_pack_objects_totals(progress, &stats_line, pack_reused, packs_reused);
    Ok(())
}

fn pack_objects_write_options(git_dir: &Path) -> Result<PackWriteOptions> {
    let config = read_repo_config(git_dir)?;
    let mut options = PackWriteOptions::new();
    if let Some(value) = config.get("pack", None, "window")
        && let Some(window) = sley_config::parse_config_int(value)
        && window == 0
    {
        options = options.with_window(0).with_depth(0).with_reorder(false);
    }
    Ok(options)
}

fn sort_no_delta_traversal_pack(
    format: ObjectFormat,
    oids: &mut Vec<ObjectId>,
    objects: &mut Vec<Arc<EncodedObject>>,
) -> Result<()> {
    let mut entries = oids
        .iter()
        .copied()
        .zip(objects.iter().cloned())
        .enumerate()
        .map(|(idx, (oid, object))| {
            let key = match object.object_type {
                ObjectType::Commit => {
                    let commit = Commit::parse_ref(format, &object.body)?;
                    PackSortKey {
                        type_rank: 0,
                        timestamp: commit_identity_timestamp_i64(commit.committer)?,
                        original_index: idx,
                    }
                }
                ObjectType::Tree => PackSortKey {
                    type_rank: 1,
                    timestamp: 0,
                    original_index: idx,
                },
                ObjectType::Blob => PackSortKey {
                    type_rank: 2,
                    timestamp: 0,
                    original_index: idx,
                },
                ObjectType::Tag => PackSortKey {
                    type_rank: 3,
                    timestamp: 0,
                    original_index: idx,
                },
            };
            Ok((key, oid, object))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|(left, left_oid, _), (right, right_oid, _)| {
        left
            .type_rank
            .cmp(&right.type_rank)
            .then_with(|| right.timestamp.cmp(&left.timestamp))
            .then_with(|| left_oid.as_bytes().cmp(right_oid.as_bytes()))
            .then_with(|| left.original_index.cmp(&right.original_index))
    });
    oids.clear();
    objects.clear();
    for (_, oid, object) in entries {
        oids.push(oid);
        objects.push(object);
    }
    Ok(())
}

struct PackSortKey {
    type_rank: u8,
    timestamp: i64,
    original_index: usize,
}

struct WrittenPackParts {
    pack: Vec<u8>,
    index: Vec<u8>,
    entries: Vec<sley_pack::PackIndexEntry>,
    checksum: ObjectId,
}

/// The final progress totals line and the trace2 data events upstream emits
/// after writing the pack (builtin/pack-objects.c `cmd_pack_objects` tail).
fn emit_pack_objects_totals(progress: bool, stats_line: &str, pack_reused: u64, packs_reused: u64) {
    if progress {
        eprintln!("{stats_line}");
    }
    sley_core::trace2::data("pack-objects", "pack-reused", pack_reused);
    sley_core::trace2::data("pack-objects", "packs-reused", packs_reused);
}

/// A bitmapped pack every object of which is wanted, so its bytes are spliced
/// into the output verbatim.
struct VerbatimPackReuse {
    entry_bytes: Vec<Vec<u8>>,
    count: u32,
}

type TraversalPackObjects = (Vec<ObjectId>, Vec<Arc<EncodedObject>>, Vec<VerbatimPackReuse>);

/// Enumerate the want set for the traversal mode and decide on pack reuse.
/// Returns the objects to encode fresh (oids + bodies) and the optional
/// verbatim reuse (whose objects are excluded from the fresh list).
fn collect_traversal_objects(
    git_dir: &Path,
    common_git_dir: &Path,
    database: &FileObjectDatabase,
    format: ObjectFormat,
    options: &PackObjectsOptions,
) -> Result<TraversalPackObjects> {
    let mut wants: Vec<ObjectId> = Vec::new();
    let mut haves: Vec<ObjectId> = Vec::new();
    if options.all {
        let store = FileRefStore::new(git_dir, format);
        for reference in store.list_refs()? {
            if let RefTarget::Direct(oid) = reference.target {
                wants.push(oid);
            }
        }
        if let Ok(head) = resolve_revision(git_dir, format, "HEAD") {
            wants.push(head);
        }
    }
    if options.revs {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            let rev = line.trim_end_matches(['\n', '\r']);
            if rev.is_empty() {
                continue;
            }
            let (negative, rev) = match rev.strip_prefix('^') {
                Some(rest) => (true, rest),
                None => (false, rev),
            };
            let oid = match resolve_revision(git_dir, format, rev) {
                Ok(oid) => oid,
                Err(_) => {
                    eprintln!("fatal: bad revision '{rev}'");
                    return Err(GitError::Exit(128));
                }
            };
            if negative {
                haves.push(oid);
            } else {
                wants.push(oid);
            }
        }
    }

    // Uninteresting closure first: tolerant of missing objects (upstream
    // never needs to open the have side's missing history).
    let excluded = tolerant_reachable_closure(database, format, &haves)?;
    let mut want_oids: Vec<ObjectId> = Vec::new();
    let mut want_objects: Vec<Arc<EncodedObject>> = Vec::new();
    let mut want_set: HashSet<ObjectId> = HashSet::new();
    {
        let mut pending: Vec<ObjectId> = wants.iter().rev().copied().collect();
        while let Some(oid) = pending.pop() {
            if excluded.contains(&oid) || !want_set.insert(oid) {
                continue;
            }
            let object = database.read_object(&oid)?;
            match object.object_type {
                ObjectType::Commit => {
                    let commit = Commit::parse_ref(format, &object.body)?;
                    pending.extend(commit.parents.iter().rev());
                    pending.push(commit.tree);
                }
                ObjectType::Tree => {
                    for entry in TreeEntries::new(format, &object.body) {
                        let entry = entry?;
                        if !entry.is_gitlink() {
                            pending.push(entry.oid);
                        }
                    }
                }
                ObjectType::Tag => {
                    let tag = Tag::parse_ref(format, &object.body)?;
                    pending.push(tag.object);
                }
                ObjectType::Blob => {}
            }
            want_oids.push(oid);
            want_objects.push(object);
        }
    }

    // want_object_in_pack's veto rules over every on-disk copy.
    if options.local || options.honor_pack_keep || options.incremental || options.unpacked {
        let locations = ObjectLocationScan::scan(common_git_dir, format)?;
        let mut kept_oids = Vec::with_capacity(want_oids.len());
        let mut kept_objects = Vec::with_capacity(want_objects.len());
        for (oid, object) in want_oids.into_iter().zip(want_objects) {
            if locations.wanted(&oid, options) {
                kept_oids.push(oid);
                kept_objects.push(object);
            }
        }
        want_oids = kept_oids;
        want_objects = kept_objects;
    }

    // Pack reuse (whole-pack case): a bitmapped pack can be reused when every
    // object it holds survived the positive-minus-negative traversal.
    let mut reused_packs = Vec::new();
    if !options.local
        && !options.honor_pack_keep
        && !options.incremental
        && !options.unpacked
        && options.use_bitmap_index != Some(false)
        && let Some(candidates) = find_verbatim_reusable_packs(
            common_git_dir,
            format,
            &want_set,
            pack_reuse_mode(git_dir)?,
        )?
    {
        let reused: HashSet<ObjectId> = candidates
            .iter()
            .flat_map(|candidate| candidate.oids.iter().copied())
            .collect();
        let mut kept_oids = Vec::with_capacity(want_oids.len().saturating_sub(reused.len()));
        let mut kept_objects = Vec::with_capacity(kept_oids.capacity());
        for (oid, object) in want_oids.into_iter().zip(want_objects) {
            if !reused.contains(&oid) {
                kept_oids.push(oid);
                kept_objects.push(object);
            }
        }
        want_oids = kept_oids;
        want_objects = kept_objects;
        reused_packs = candidates
            .into_iter()
            .map(|candidate| VerbatimPackReuse {
                count: candidate.oids.len() as u32,
                entry_bytes: candidate.entry_bytes,
            })
            .collect();
    }

    Ok((want_oids, want_objects, reused_packs))
}

struct ReusablePackCandidate {
    entry_bytes: Vec<Vec<u8>>,
    oids: HashSet<ObjectId>,
}

fn pack_reuse_mode(git_dir: &Path) -> Result<PackReuseMode> {
    let config = read_repo_config(git_dir)?;
    let mut mode = if config
        .get_bool("feature", None, "experimental")
        .unwrap_or(false)
    {
        PackReuseMode::Multi
    } else {
        PackReuseMode::Single
    };
    if let Some(entry) = config.get_entry("pack", None, "allowPackReuse") {
        mode = match entry {
            None => PackReuseMode::Single,
            Some(value) if value.eq_ignore_ascii_case("single") => PackReuseMode::Single,
            Some(value) if value.eq_ignore_ascii_case("multi") => PackReuseMode::Multi,
            Some(value) => match sley_config::parse_config_bool(value) {
                Some(true) => PackReuseMode::Single,
                Some(false) => PackReuseMode::None,
                None => {
                    eprintln!("fatal: invalid pack.allowPackReuse value: '{value}'");
                    return Err(GitError::Exit(128));
                }
            },
        };
    }
    Ok(mode)
}

/// Find local bitmapped packs whose every object is in `want_set`: each such
/// pack can be spliced into the output verbatim.
fn find_verbatim_reusable_packs(
    common_git_dir: &Path,
    format: ObjectFormat,
    want_set: &HashSet<ObjectId>,
    reuse_mode: PackReuseMode,
) -> Result<Option<Vec<ReusablePackCandidate>>> {
    if matches!(reuse_mode, PackReuseMode::None) {
        return Ok(None);
    }
    let pack_dir = sley_odb::repository_objects_dir(common_git_dir).join("pack");
    if let Some(candidates) =
        find_midx_verbatim_reusable_packs(&pack_dir, format, want_set, reuse_mode)?
    {
        return Ok(Some(candidates));
    }
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(None);
    };
    let mut bitmap_stems: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("bitmap")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("pack-"))
        {
            bitmap_stems.push(path.with_extension(""));
        }
    }
    bitmap_stems.sort();
    for stem in bitmap_stems {
        let idx_path = stem.with_extension("idx");
        let pack_path = stem.with_extension("pack");
        let Ok(idx_bytes) = fs::read(&idx_path) else {
            continue;
        };
        let Ok(index) = PackIndex::parse(&idx_bytes, format) else {
            continue;
        };
        if index.entries.is_empty() {
            continue;
        }
        let Ok(pack_bytes) = fs::read(&pack_path) else {
            continue;
        };
        let wanted: HashSet<ObjectId> = index.entries.iter().map(|entry| entry.oid).collect();
        let Some(entry_bytes) = raw_pack_entries_for_oids(
            format,
            &pack_bytes,
            &index.entries,
            &wanted,
            false,
        )?
        else {
            continue;
        };
        return Ok(Some(vec![ReusablePackCandidate {
            entry_bytes,
            oids: index.entries.into_iter().map(|entry| entry.oid).collect(),
        }]));
    }
    Ok(None)
}

fn find_midx_verbatim_reusable_packs(
    pack_dir: &Path,
    format: ObjectFormat,
    want_set: &HashSet<ObjectId>,
    reuse_mode: PackReuseMode,
) -> Result<Option<Vec<ReusablePackCandidate>>> {
    let midx_path = pack_dir.join("multi-pack-index");
    if !midx_path.exists() {
        return Ok(None);
    }
    let Ok(midx_bytes) = fs::read(&midx_path) else {
        return Ok(None);
    };
    let Ok(midx) = MultiPackIndex::parse(&midx_bytes, format) else {
        return Ok(None);
    };
    if !pack_dir
        .join(format!("multi-pack-index-{}.bitmap", midx.checksum.to_hex()))
        .exists()
    {
        return Ok(None);
    }

    let mut pack_ids: Vec<u32> = (0..midx.pack_names.len() as u32).collect();
    if let Some(bitmapped) = &midx.bitmapped_packs {
        pack_ids.sort_by_key(|pack_id| {
            bitmapped
                .get(*pack_id as usize)
                .map(|entry| entry.bitmap_pos)
                .unwrap_or(u32::MAX)
        });
    }
    if reuse_mode == PackReuseMode::Single {
        pack_ids.truncate(1);
    }

    let mut candidates = Vec::new();
    for pack_id in pack_ids {
        let Some(pack_name) = midx.pack_names.get(pack_id as usize) else {
            continue;
        };
        let idx_path = pack_dir.join(pack_name);
        let pack_path = idx_path.with_extension("pack");
        let Ok(idx_bytes) = fs::read(&idx_path) else {
            continue;
        };
        let Ok(index) = PackIndex::parse(&idx_bytes, format) else {
            continue;
        };
        if index.entries.is_empty() {
            continue;
        }
        let Ok(pack_bytes) = fs::read(&pack_path) else {
            continue;
        };
        let all_pack_oids: HashSet<ObjectId> = index.entries.iter().map(|entry| entry.oid).collect();
        let wanted_count = index
            .entries
            .iter()
            .filter(|entry| want_set.contains(&entry.oid))
            .count();
        let whole_pack = wanted_count == index.entries.len();
        if whole_pack {
            let Some(entry_bytes) = raw_pack_entries_for_oids(
                format,
                &pack_bytes,
                &index.entries,
                &all_pack_oids,
                false,
            )?
            else {
                continue;
            };
            candidates.push(ReusablePackCandidate {
                entry_bytes,
                oids: all_pack_oids,
            });
        } else if let Some((oids, entry_bytes)) =
            raw_partial_pack_entries_for_wanted_oids(format, &pack_bytes, &index.entries, want_set)?
        {
            candidates.push(ReusablePackCandidate { entry_bytes, oids });
        }
    }
    if candidates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(candidates))
    }
}

fn raw_partial_pack_entries_for_wanted_oids(
    format: ObjectFormat,
    pack_bytes: &[u8],
    index_entries: &[sley_pack::PackIndexEntry],
    want_set: &HashSet<ObjectId>,
) -> Result<Option<(HashSet<ObjectId>, Vec<Vec<u8>>)>> {
    let oids: HashSet<ObjectId> = index_entries
        .iter()
        .filter(|entry| want_set.contains(&entry.oid))
        .map(|entry| entry.oid)
        .collect();
    if oids.is_empty() {
        return Ok(None);
    }
    let Some(entry_bytes) = raw_pack_entries_for_oids(format, pack_bytes, index_entries, &oids, true)?
    else {
        return Ok(None);
    };
    Ok(Some((oids, entry_bytes)))
}

fn raw_pack_entries_for_oids(
    format: ObjectFormat,
    pack_bytes: &[u8],
    index_entries: &[sley_pack::PackIndexEntry],
    wanted: &HashSet<ObjectId>,
    reject_wanted_deltas: bool,
) -> Result<Option<Vec<Vec<u8>>>> {
    let hash_len = format.raw_len();
    if pack_bytes.len() < 12 + hash_len || &pack_bytes[..4] != b"PACK" {
        return Ok(None);
    }
    let trailer_offset = pack_bytes.len() - hash_len;
    let mut by_offset: Vec<&sley_pack::PackIndexEntry> = index_entries.iter().collect();
    by_offset.sort_by_key(|entry| entry.offset);

    let mut out = Vec::new();
    for (idx, entry) in by_offset.iter().enumerate() {
        if !wanted.contains(&entry.oid) {
            continue;
        }
        let start = usize::try_from(entry.offset)
            .map_err(|_| GitError::InvalidFormat("pack offset out of range".into()))?;
        let end = by_offset
            .get(idx + 1)
            .map(|next| usize::try_from(next.offset))
            .transpose()
            .map_err(|_| GitError::InvalidFormat("pack offset out of range".into()))?
            .unwrap_or(trailer_offset);
        if start < 12 || end > trailer_offset || start >= end {
            return Ok(None);
        }
        let kind = (pack_bytes[start] >> 4) & 0x07;
        if reject_wanted_deltas && (kind == 6 || kind == 7) {
            return Ok(None);
        }
        out.push(pack_bytes[start..end].to_vec());
    }
    Ok(Some(out))
}

/// The reachability closure of `starts`, skipping objects that cannot be read
/// (the have side of a pack-objects traversal is best-effort upstream).
fn tolerant_reachable_closure(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut pending: Vec<ObjectId> = starts.to_vec();
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = match database.read_object(&oid) {
            Ok(object) => object,
            Err(GitError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        match object.object_type {
            ObjectType::Commit => {
                let commit = Commit::parse_ref(format, &object.body)?;
                pending.extend(commit.parents);
                pending.push(commit.tree);
            }
            ObjectType::Tree => {
                for entry in TreeEntries::new(format, &object.body) {
                    let entry = entry?;
                    if !entry.is_gitlink() {
                        pending.push(entry.oid);
                    }
                }
            }
            ObjectType::Tag => {
                let tag = Tag::parse_ref(format, &object.body)?;
                pending.push(tag.object);
            }
            ObjectType::Blob => {}
        }
    }
    Ok(seen)
}

/// Every on-disk location of the repository's objects, for the
/// `want_object_in_pack` veto rules: loose copies in alternates, and each
/// pack's membership with its locality and `.keep` state.
struct ObjectLocationScan {
    nonlocal_loose: HashSet<ObjectId>,
    packs: Vec<PackMembership>,
}

struct PackMembership {
    oids: HashSet<ObjectId>,
    local: bool,
    keep: bool,
}

impl ObjectLocationScan {
    fn scan(common_git_dir: &Path, format: ObjectFormat) -> Result<Self> {
        let objects_dir = sley_odb::repository_objects_dir(common_git_dir);
        let mut object_dirs: Vec<(PathBuf, bool)> = vec![(objects_dir.clone(), true)];
        // info/alternates: one path per line, relative entries resolved
        // against the objects directory (upstream link_alt_odb_entry).
        if let Ok(alternates) = fs::read_to_string(objects_dir.join("info/alternates")) {
            for line in alternates.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let path = PathBuf::from(line);
                let path = if path.is_absolute() {
                    path
                } else {
                    objects_dir.join(path)
                };
                object_dirs.push((path, false));
            }
        }

        let mut nonlocal_loose = HashSet::new();
        let mut packs = Vec::new();
        for (dir, local) in &object_dirs {
            if !local {
                collect_loose_oids(dir, format, &mut nonlocal_loose)?;
            }
            let pack_dir = dir.join("pack");
            let Ok(entries) = fs::read_dir(&pack_dir) else {
                continue;
            };
            for entry in entries {
                let path = entry?.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                    continue;
                }
                let Ok(bytes) = fs::read(&path) else {
                    continue;
                };
                let Ok(index) = PackIndex::parse(&bytes, format) else {
                    continue;
                };
                packs.push(PackMembership {
                    oids: index.entries.into_iter().map(|entry| entry.oid).collect(),
                    local: *local,
                    keep: path.with_extension("keep").exists(),
                });
            }
        }
        Ok(Self {
            nonlocal_loose,
            packs,
        })
    }

    /// Upstream `want_object_in_pack` / `want_found_object`: any matching
    /// copy vetoes the object; otherwise it is packed.
    fn wanted(&self, oid: &ObjectId, options: &PackObjectsOptions) -> bool {
        if options.local && self.nonlocal_loose.contains(oid) {
            return false;
        }
        for pack in &self.packs {
            if !pack.oids.contains(oid) {
                continue;
            }
            if options.incremental || options.unpacked {
                return false;
            }
            if options.local && !pack.local {
                return false;
            }
            if options.honor_pack_keep && pack.local && pack.keep {
                return false;
            }
        }
        true
    }
}

/// Collect every loose object id under `objects_dir`'s fanout directories.
fn collect_loose_oids(
    objects_dir: &Path,
    format: ObjectFormat,
    into: &mut HashSet<ObjectId>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let fanout = entry.file_name();
        let Some(fanout) = fanout.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(files) = fs::read_dir(entry.path()) else {
            continue;
        };
        for file in files {
            let name = file?.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Ok(oid) = ObjectId::from_hex(format, &format!("{fanout}{name}")) {
                into.insert(oid);
            }
        }
    }
    Ok(())
}

/// Read the object list from standard input, mirroring upstream's
/// `read_object_list_from_stdin`: one object id per line with an optional
/// name hint after it, `-<oid>` edge lines validated then skipped (preferred
/// bases are delta heuristics, never pack members), and garbage rejected with
/// git's exact message and exit code. Duplicate ids collapse to their first
/// occurrence, as `add_object_entry` does.
fn read_pack_objects_stdin(format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let hex_len = format.raw_len() * 2;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = Vec::new();
    let mut seen = HashSet::new();
    let mut oids = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.first() == Some(&b'-') {
            if parse_pack_objects_oid(&line[1..], hex_len, format).is_none() {
                return pack_objects_garbage("expected edge object ID", &line);
            }
            // Edge (preferred-base) objects are only delta-base hints; they
            // are never added to the pack, and sley's pack writer picks its
            // own delta bases, so a validated edge line is simply skipped.
            continue;
        }
        let Some(oid) = parse_pack_objects_oid(&line, hex_len, format) else {
            return pack_objects_garbage("expected object ID", &line);
        };
        if seen.insert(oid) {
            oids.push(oid);
        }
    }
    Ok(oids)
}

/// Parse the leading `hex_len` bytes of `line` as an object id, returning
/// `None` when the line is too short or not hex — the caller reports git's
/// "got garbage" error. Anything after the id (a name hint) is ignored.
fn parse_pack_objects_oid(line: &[u8], hex_len: usize, format: ObjectFormat) -> Option<ObjectId> {
    let hex = line.get(..hex_len)?;
    let hex = std::str::from_utf8(hex).ok()?;
    ObjectId::from_hex(format, hex).ok()
}

/// Report a garbage input line exactly like upstream's
/// `die(_("expected [edge ]object ID, got garbage:\n %s"), line)`: the raw
/// line keeps its trailing newline (when present) and `die` appends one more.
fn pack_objects_garbage<T>(what: &str, line: &[u8]) -> Result<T> {
    eprint!(
        "fatal: {what}, got garbage:\n {}\n",
        String::from_utf8_lossy(line)
    );
    Err(GitError::Exit(128))
}

fn pack_objects_usage<T>() -> Result<T> {
    eprintln!("usage: git pack-objects --stdout [<options>] [< <ref-list> | < <object-list>]");
    eprintln!("   or: git pack-objects [<options>] <base-name> [< <ref-list> | < <object-list>]");
    Err(GitError::Exit(129))
}
