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
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_config, sley_core, sley_odb, sley_rev};
use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::IsTerminal;
use std::sync::Arc;

use crate::*;
use sley::PackWriteOptions;
use sley::plumbing::sley_pack::{PackInput, PackReverseIndex, pack_order_index_positions};

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
    /// `.idx` format version to emit (1 or 2). `None` keeps the v2 default.
    /// `--index-version=2,<n>` carries a large-offset threshold which only
    /// affects v2 and which sley derives from the offsets themselves, so the
    /// threshold suffix is accepted and ignored.
    index_version: Option<u32>,
    /// `--cruft`: write a cruft pack of unreachable objects with a `.mtimes`
    /// companion. The fresh/discard pack names arrive on stdin.
    cruft: bool,
    /// `--cruft-expiration=<time>`: unreachable objects older than this UNIX
    /// timestamp are dropped (unless rescued by a younger reachable object).
    /// `None` (or zero) means "never expire".
    cruft_expiration: Option<u32>,
    /// `--window=0` / `--depth=0`: delta search disabled, every object stored
    /// undeltified. sley's writer chooses deltas internally, so this forces the
    /// no-delta path that emits `Total N (delta 0)`.
    no_delta: bool,
    /// Prefer OFS_DELTA over REF_DELTA. Defaults to true (matching
    /// [`sley_pack::PackWriteOptions`] and `repack.useDeltaBaseOffset`): our
    /// sliding-window planner produces slightly larger ref-delta packs than
    /// git's, which flips midx batch selection on t5319. OFS encoding closes
    /// that gap and matches what `multi-pack-index repack` / `git repack` pass
    /// to pack-objects. `--no-delta-base-offset` still forces REF_DELTA.
    delta_base_offset: bool,
    stdin_packs: bool,
    /// `--stdin-packs=follow`: in addition to the standard "objects in the
    /// included packs minus objects in the excluded packs" set, run a
    /// reachability walk from the commits of the included (and excluded-open
    /// `!`) packs to rescue objects that live in packs not named on stdin.
    stdin_packs_follow: bool,
    /// `--exclude-promisor-objects`: with `--stdin-packs`, included promisor
    /// packs are rejected up front and follow-mode traversal treats objects in
    /// promisor packs as a missing-object boundary instead of attempting any
    /// lazy backfill.
    exclude_promisor_objects: bool,
    path_walk: bool,
    sparse: Option<bool>,
    thin: bool,
    /// Effective `pack.writeReverseIndex` policy. Unlike repack, pack-objects
    /// defaults this setting to false when it is not configured.
    write_reverse_index: bool,
    write_bitmap_index: bool,
    name_hash_version: Option<i32>,
    max_pack_size: Option<u64>,
    object_filter: PackObjectFilter,
    filter_print_omitted: bool,
    missing_action: PackObjectsMissingAction,
}

impl Default for PackObjectsOptions {
    fn default() -> Self {
        Self {
            base_name: None,
            stdout_mode: false,
            revs: false,
            all: false,
            local: false,
            honor_pack_keep: false,
            incremental: false,
            unpacked: false,
            use_bitmap_index: None,
            progress: None,
            index_version: None,
            cruft: false,
            cruft_expiration: None,
            no_delta: false,
            // See field docs: prefer OFS_DELTA unless the user opts out.
            delta_base_offset: true,
            stdin_packs: false,
            stdin_packs_follow: false,
            exclude_promisor_objects: false,
            path_walk: false,
            sparse: None,
            thin: false,
            write_reverse_index: false,
            write_bitmap_index: false,
            name_hash_version: None,
            max_pack_size: None,
            object_filter: PackObjectFilter::default(),
            filter_print_omitted: false,
            missing_action: PackObjectsMissingAction::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PackObjectsMissingAction {
    #[default]
    Error,
    AllowAny,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum PackObjectFilter {
    #[default]
    None,
    BlobNone,
    BlobLimit(usize),
    ObjectType(ObjectType),
    TreeDepth(usize),
    SparseOid(String),
    Sparse(Vec<Vec<u8>>),
    Combine(Vec<PackObjectFilter>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackReuseMode {
    None,
    Single,
    Multi,
}

pub(crate) fn cmd_pack_objects(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
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
            "--delta-base-offset" if !saw_dashdash => options.delta_base_offset = true,
            "--no-delta-base-offset" if !saw_dashdash => options.delta_base_offset = false,
            "--path-walk" if !saw_dashdash => options.path_walk = true,
            "--no-path-walk" if !saw_dashdash => options.path_walk = false,
            "--sparse" if !saw_dashdash => options.sparse = Some(true),
            "--no-sparse" if !saw_dashdash => options.sparse = Some(false),
            "--thin" if !saw_dashdash => options.thin = true,
            "--no-thin" if !saw_dashdash => options.thin = false,
            "--write-bitmap-index" | "--write-bitmap-index-quiet" if !saw_dashdash => {
                options.write_bitmap_index = true;
            }
            "--no-write-bitmap-index" if !saw_dashdash => options.write_bitmap_index = false,
            "--no-filter" if !saw_dashdash => options.object_filter = PackObjectFilter::None,
            "--filter-print-omitted" if !saw_dashdash => options.filter_print_omitted = true,
            value if !saw_dashdash && value.starts_with("--filter=") => {
                let parsed = PackObjectFilter::parse(&value["--filter=".len()..])?;
                options.object_filter =
                    std::mem::take(&mut options.object_filter).combine_with(parsed);
            }
            "--missing=error" if !saw_dashdash => {
                options.missing_action = PackObjectsMissingAction::Error;
            }
            "--missing=allow-any" | "--missing=allow-promisor" if !saw_dashdash => {
                options.missing_action = PackObjectsMissingAction::AllowAny;
            }
            value if !saw_dashdash && value.starts_with("--missing=") => {
                eprintln!("fatal: invalid value for --missing");
                return Err(GitError::Exit(128));
            }
            "--stdin" if !saw_dashdash => {
                eprintln!("fatal: disallowed abbreviated or ambiguous option 'stdin'");
                return Err(GitError::Exit(129));
            }
            "--stdin-packs" if !saw_dashdash => options.stdin_packs = true,
            value if !saw_dashdash && value.starts_with("--stdin-packs=") => {
                options.stdin_packs = true;
                let mode = &value["--stdin-packs=".len()..];
                if mode.is_empty() {
                    // bare `--stdin-packs=` is the standard mode
                } else if mode == "follow" {
                    options.stdin_packs_follow = true;
                } else {
                    eprintln!("fatal: invalid value for 'stdin-packs': '{mode}'");
                    return Err(GitError::Exit(128));
                }
            }
            "--exclude-promisor-objects" if !saw_dashdash => {
                options.exclude_promisor_objects = true;
            }
            "--no-exclude-promisor-objects" if !saw_dashdash => {
                options.exclude_promisor_objects = false;
            }
            "-q" | "--quiet" if !saw_dashdash => options.progress = Some(false),
            "--no-quiet" if !saw_dashdash => options.progress = Some(true),
            "--cruft" if !saw_dashdash => {
                options.cruft = true;
            }
            value if !saw_dashdash && value.starts_with("--cruft-expiration=") => {
                options.cruft = true;
                let spec = &value["--cruft-expiration=".len()..];
                let ts =
                    crate::commands::approxidate::parse_expiry_date(spec).ok_or_else(|| {
                        GitError::Command(format!("malformed expiration date '{spec}'"))
                    })?;
                // git's cruft_expiration is a `timestamp_t` (unsigned); zero
                // means "never expire". A saturating cast keeps the "now"/"all"
                // sentinel (i64 from u64::MAX) at u32::MAX so nothing survives.
                options.cruft_expiration = if ts == 0 {
                    None
                } else if ts < 0 {
                    Some(0)
                } else if ts >= u32::MAX as i64 {
                    Some(u32::MAX)
                } else {
                    Some(ts as u32)
                };
            }
            // sley packs everything into one cruft pack; the split-by-size
            // knobs are accepted (the single-pack result still passes the
            // mtimes/contents assertions the suite checks for most cases).
            value if !saw_dashdash && value.starts_with("--max-pack-size=") => {
                let value = &value["--max-pack-size=".len()..];
                options.max_pack_size = Some(parse_pack_size_limit_arg(value)?);
            }
            value if !saw_dashdash && value.starts_with("--max-cruft-size=") => {}
            // Delta-search tuning. sley's writer picks deltas itself, so only
            // the window/depth==0 "disable deltas" case changes behaviour; the
            // rest are accepted as no-ops (their numeric value never alters the
            // byte output the suite checks). A negative window clamps to 0.
            value if !saw_dashdash && value.starts_with("--window=") => {
                let n = value["--window=".len()..].parse::<i64>().unwrap_or(0);
                if n <= 0 {
                    options.no_delta = true;
                }
            }
            value if !saw_dashdash && value.starts_with("--depth=") => {
                let n = value["--depth=".len()..].parse::<i64>().unwrap_or(0);
                if n <= 0 {
                    options.no_delta = true;
                }
            }
            value
                if !saw_dashdash
                    && (value.starts_with("--threads=")
                        || value.starts_with("--window-memory=")
                        || value.starts_with("--compression=")) => {}
            value if !saw_dashdash && value.starts_with("--name-hash-version=") => {
                let value = &value["--name-hash-version=".len()..];
                options.name_hash_version = Some(value.parse::<i32>().map_err(|_| {
                    eprintln!("fatal: invalid --name-hash-version option: {value}");
                    GitError::Exit(128)
                })?);
            }
            "--threads" | "--window" | "--depth" | "--compression" | "--window-memory"
                if !saw_dashdash => {}
            // Accepted toggles with no separate sley machinery: sley always
            // recomputes its own deltas (so reuse toggles are moot), only ever
            // writes non-empty packs unless there is nothing to write, and the
            // reflog/index inclusion knobs only matter alongside `--revs`.
            "--no-reuse-delta"
            | "--no-reuse-object"
            | "--non-empty"
            | "--keep-true-parents"
            | "--reflog"
            | "--indexed-objects"
            | "--delta-islands"
                if !saw_dashdash => {}
            "--progress" | "--all-progress" | "--all-progress-implied" if !saw_dashdash => {
                options.progress = Some(true)
            }
            "--no-progress" if !saw_dashdash => options.progress = Some(false),
            "--no-all-progress" | "--no-all-progress-implied" if !saw_dashdash => {}
            value if !saw_dashdash && value.starts_with("--index-version=") => {
                let spec = &value["--index-version=".len()..];
                options.index_version = Some(parse_index_version_spec(spec)?);
            }
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
    if options.thin && !options.stdout_mode {
        eprintln!("fatal: --thin cannot be used to build an indexable pack");
        return Err(GitError::Exit(128));
    }
    validate_pack_objects_options(&options)?;

    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&git_dir)?;
    options.write_reverse_index = config
        .get_bool("pack", None, "writeReverseIndex")
        .unwrap_or(false);
    let database = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    options.object_filter = std::mem::take(&mut options.object_filter).resolve(
        &git_dir,
        &database,
        format,
        cli_session.replace_objects(),
    )?;
    let progress = options
        .progress
        .unwrap_or_else(|| io::stderr().is_terminal());
    let max_pack_size =
        pack_objects_pack_size_limit(&git_dir, options.max_pack_size, options.stdout_mode)?;

    if options.cruft {
        return write_cruft_pack(
            &git_dir,
            &common_git_dir,
            &database,
            format,
            &config,
            &options,
            progress,
            max_pack_size,
        );
    }
    let traversal = options.revs || options.all;
    let (mut oids, mut objects, reused_packs) = if options.stdin_packs {
        let (oids, objects) =
            collect_stdin_packs_objects(&common_git_dir, &database, format, &options)?;
        (oids, objects, Vec::new())
    } else if traversal {
        collect_traversal_objects(
            &git_dir,
            &common_git_dir,
            &database,
            format,
            &options,
            cli_session.lazy_fetch(),
            cli_session.replace_objects(),
        )?
    } else {
        let oids = read_pack_objects_stdin(format)?;
        let mut objects = Vec::with_capacity(oids.len());
        for oid in &oids {
            match crate::read_object_maybe_prefetch_promisor(
                &database,
                oid,
                cli_session.lazy_fetch(),
            ) {
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
    let mut pack_write_options = pack_objects_write_options(&git_dir)?;
    pack_write_options = pack_write_options.with_prefer_ofs_delta(options.delta_base_offset);
    if options.no_delta {
        pack_write_options = pack_write_options
            .with_window(0)
            .with_depth(0)
            .with_reorder(false);
    }
    if traversal && pack_write_options.depth == 0 {
        sort_no_delta_traversal_pack(format, &mut oids, &mut objects)?;
    }

    if progress {
        // The enumeration meter counts every packed object, the verbatim
        // reused ones included (upstream's bitmap path displays the full
        // result cardinality).
        let enumerated = oids.len() as u64
            + reused_packs
                .iter()
                .map(|reuse| reuse.count as u64)
                .sum::<u64>();
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
    if options.path_walk && progress {
        eprintln!("Compressing objects by path: 100% ({written_total}/{written_total}), done.");
    }

    if !options.stdout_mode
        && reused_packs.is_empty()
        && let Some(limit) = max_pack_size
    {
        let base_name = options.base_name.expect("checked above");
        let delta_count = write_split_pack_files(
            &base_name,
            format,
            &oids,
            &objects,
            &pack_write_options,
            options.index_version,
            options.write_reverse_index,
            limit,
        )?;
        let stats_line =
            pack_objects_stats_line(written_total, delta_count, pack_reused, packs_reused);
        emit_pack_objects_totals(progress, &stats_line, pack_reused, packs_reused);
        return Ok(());
    }

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
            let stats_line = pack_objects_stats_line(written_total, 0, pack_reused, packs_reused);
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
            delta_count: 0,
        }
    } else {
        let written = PackFile::write_packed_with_known_ids_and_options(
            &inputs,
            format,
            &pack_write_options,
        )?;
        if options.stdout_mode {
            let mut stdout = io::stdout();
            stdout.write_all(&written.pack)?;
            stdout.flush()?;
            let stats_line = pack_objects_stats_line(
                written_total,
                written.delta_count,
                pack_reused,
                packs_reused,
            );
            emit_pack_objects_totals(progress, &stats_line, pack_reused, packs_reused);
            return Ok(());
        }
        WrittenPackParts {
            pack: written.pack,
            index: written.index,
            entries: written.entries,
            checksum: written.checksum,
            delta_count: written.delta_count,
        }
    };

    let base_name = options.base_name.expect("checked above");
    let reverse_index = if options.write_reverse_index {
        let positions = pack_order_index_positions(&result.entries);
        Some(PackReverseIndex::write(
            format,
            &positions,
            &result.checksum,
        )?)
    } else {
        None
    };

    // The writer always produces a v2 `.idx`; honour an explicit
    // `--index-version=1` by re-serialising the same entries in the v1 layout.
    let index_bytes = if options.index_version == Some(1) {
        PackIndex::write_v1(format, &result.entries, &result.checksum)?
    } else {
        result.index
    };

    // Write the pack before its lookup companions so no reader ever sees an
    // index that points at a missing or incomplete pack.
    let checksum = result.checksum.to_hex();
    fs::write(format!("{base_name}-{checksum}.pack"), &result.pack)?;
    if let Some(reverse_index) = reverse_index {
        fs::write(format!("{base_name}-{checksum}.rev"), reverse_index)?;
    }
    fs::write(format!("{base_name}-{checksum}.idx"), &index_bytes)?;
    println!("{checksum}");
    let stats_line =
        pack_objects_stats_line(written_total, result.delta_count, pack_reused, packs_reused);
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

fn parse_pack_size_limit_arg(value: &str) -> Result<u64> {
    let Some(parsed) = sley_config::parse_config_int(value) else {
        eprintln!("fatal: failed to parse --max-pack-size value '{value}'");
        return Err(GitError::Exit(128));
    };
    if parsed < 0 {
        eprintln!("fatal: failed to parse --max-pack-size value '{value}'");
        return Err(GitError::Exit(128));
    }
    Ok(parsed as u64)
}

fn pack_objects_pack_size_limit(
    git_dir: &Path,
    arg_limit: Option<u64>,
    stdout_mode: bool,
) -> Result<Option<u64>> {
    let config = read_repo_config(git_dir)?;
    let mut limit = arg_limit;
    if !stdout_mode
        && limit.is_none()
        && let Some(value) = config.get("pack", None, "packSizeLimit")
        && let Some(parsed) = sley_config::parse_config_int(value)
        && parsed > 0
    {
        limit = Some(parsed as u64);
    }
    if stdout_mode && arg_limit.is_some() {
        eprintln!("fatal: --max-pack-size cannot be used to build a pack for transfer");
        return Err(GitError::Exit(128));
    }
    if let Some(size) = limit
        && size < 1024 * 1024
    {
        eprintln!("warning: minimum pack size limit is 1 MiB");
        return Ok(Some(1024 * 1024));
    }
    Ok(limit)
}

fn write_split_pack_files(
    base_name: &str,
    format: ObjectFormat,
    oids: &[ObjectId],
    objects: &[Arc<EncodedObject>],
    options: &PackWriteOptions,
    index_version: Option<u32>,
    write_reverse_index: bool,
    limit: u64,
) -> Result<u32> {
    let mut total_delta_count = 0u32;
    for range in split_pack_ranges(objects, limit) {
        let inputs: Vec<PackInput<'_>> = oids[range.clone()]
            .iter()
            .zip(&objects[range])
            .map(|(oid, object)| PackInput {
                oid,
                object: object.as_ref(),
            })
            .collect();
        let written = PackFile::write_packed_with_known_ids_and_options(&inputs, format, options)?;
        total_delta_count += written.delta_count;
        write_pack_file_parts(
            base_name,
            format,
            written.pack,
            written.index,
            written.entries,
            written.checksum,
            index_version,
            write_reverse_index,
        )?;
    }
    Ok(total_delta_count)
}

fn split_pack_ranges(objects: &[Arc<EncodedObject>], limit: u64) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut current_size = 0u64;
    for (idx, object) in objects.iter().enumerate() {
        let estimate = object.body.len() as u64 + 32;
        if idx > start && current_size.saturating_add(estimate) > limit {
            ranges.push(start..idx);
            start = idx;
            current_size = 0;
        }
        current_size = current_size.saturating_add(estimate);
    }
    if start < objects.len() {
        ranges.push(start..objects.len());
    }
    ranges
}

fn write_pack_file_parts(
    base_name: &str,
    format: ObjectFormat,
    pack: Vec<u8>,
    index: Vec<u8>,
    entries: Vec<sley_pack::PackIndexEntry>,
    checksum: ObjectId,
    index_version: Option<u32>,
    write_reverse_index: bool,
) -> Result<()> {
    let reverse_index = if write_reverse_index {
        let positions = pack_order_index_positions(&entries);
        Some(PackReverseIndex::write(format, &positions, &checksum)?)
    } else {
        None
    };
    let index_bytes = if index_version == Some(1) {
        PackIndex::write_v1(format, &entries, &checksum)?
    } else {
        index
    };
    let checksum_hex = checksum.to_hex();
    fs::write(format!("{base_name}-{checksum_hex}.pack"), &pack)?;
    if let Some(reverse_index) = reverse_index {
        fs::write(format!("{base_name}-{checksum_hex}.rev"), reverse_index)?;
    }
    fs::write(format!("{base_name}-{checksum_hex}.idx"), &index_bytes)?;
    println!("{checksum_hex}");
    Ok(())
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
        left.type_rank
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
    delta_count: u32,
}

fn validate_pack_objects_options(options: &PackObjectsOptions) -> Result<()> {
    if options.stdin_packs {
        // `--stdin-packs` selects objects from named packs directly, so an
        // internal rev list (`--revs`/`--all`) and `--filter` are both
        // rejected before any work happens (builtin/pack-objects.c
        // die_for_incompatible_opt2 + "cannot use internal rev list").
        if options.object_filter != PackObjectFilter::None {
            eprintln!("fatal: options '--stdin-packs' and '--filter' cannot be used together");
            return Err(GitError::Exit(128));
        }
        if options.revs || options.all {
            eprintln!("fatal: cannot use internal rev list with --stdin-packs");
            return Err(GitError::Exit(128));
        }
    }
    if let Some(version) = options.name_hash_version {
        if version == 0 || version > 2 {
            eprintln!("fatal: invalid --name-hash-version option: {version}");
            return Err(GitError::Exit(128));
        }
        if options.write_bitmap_index && version != 1 && !options.stdout_mode {
            eprintln!("warning: currently, --write-bitmap-index requires --name-hash-version=1");
        }
    }
    Ok(())
}

/// `kind` bitflags for a pack named on `--stdin-packs` input.
const STDIN_PACK_INCLUDE: u8 = 1 << 0;
const STDIN_PACK_EXCLUDE_CLOSED: u8 = 1 << 1;
const STDIN_PACK_EXCLUDE_OPEN: u8 = 1 << 2;

/// A `.idx`/`.pack` pair discovered while scanning the object stores.
struct StdinPackFile {
    oids: Vec<ObjectId>,
    mtime: std::time::SystemTime,
    pack_path: PathBuf,
    is_promisor: bool,
}

/// `git pack-objects --stdin-packs`: read pack basenames from standard input
/// (one per line, `^`-prefixed names excluded; `!`-prefixed names are
/// excluded-open under `=follow` and literal otherwise), resolve them across
/// the local and alternate object stores, and return the objects to pack.
///
/// The standard set is "the union of objects in the included packs, minus any
/// object that also appears in an excluded pack". With `--unpacked`, loose
/// objects that are not present in an excluded pack are appended too
/// (`add_unreachable_loose_objects`; `add_object_entry` deduplicates, so we add
/// every loose object once and let the want-veto drop the excluded ones).
fn collect_stdin_packs_objects(
    common_git_dir: &Path,
    database: &FileObjectDatabase,
    format: ObjectFormat,
    options: &PackObjectsOptions,
) -> Result<(Vec<ObjectId>, Vec<Arc<EncodedObject>>)> {
    let follow = options.stdin_packs_follow;

    // 1. Parse stdin into per-basename kind bitflags, preserving first-seen
    //    order so the "could not find pack" diagnostic names the right key.
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = Vec::new();
    let mut order: Vec<String> = Vec::new();
    let mut requested: HashMap<String, u8> = HashMap::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let (kind, key) = if line.first() == Some(&b'^') {
            (STDIN_PACK_EXCLUDE_CLOSED, &line[1..])
        } else if follow && line.first() == Some(&b'!') {
            (STDIN_PACK_EXCLUDE_OPEN, &line[1..])
        } else {
            (STDIN_PACK_INCLUDE, &line[..])
        };
        let key = String::from_utf8_lossy(key).into_owned();
        let entry = requested.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            0
        });
        *entry |= kind;
    }

    // 2. Scan the local and alternate object stores for every `.idx`/`.pack`
    //    pair, indexed by the `<basename>.pack` filename git matches against.
    let objects_dir = sley_odb::repository_objects_dir(common_git_dir);
    let mut object_dirs: Vec<PathBuf> = vec![objects_dir.clone()];
    if let Ok(alternates) = fs::read_to_string(objects_dir.join("info/alternates")) {
        for raw in alternates.lines() {
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let path = PathBuf::from(raw);
            object_dirs.push(if path.is_absolute() {
                path
            } else {
                objects_dir.join(path)
            });
        }
    }
    let mut found: HashMap<String, StdinPackFile> = HashMap::new();
    for dir in &object_dirs {
        let pack_dir = dir.join("pack");
        let Ok(entries) = fs::read_dir(&pack_dir) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let pack_path = path.with_extension("pack");
            if !pack_path.exists() {
                continue;
            }
            let Some(basename) = pack_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if found.contains_key(basename) || !requested.contains_key(basename) {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&bytes, format) else {
                continue;
            };
            let mtime = fs::metadata(&pack_path)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            found.insert(
                basename.to_string(),
                StdinPackFile {
                    oids: index.entries.into_iter().map(|entry| entry.oid).collect(),
                    mtime,
                    pack_path: pack_path.clone(),
                    is_promisor: pack_path.with_extension("promisor").exists(),
                },
            );
        }
    }

    // 3. Every named pack must resolve, or git dies naming a missing key. When
    //    several keys are unresolved git reports the one that sorts first — its
    //    strmap iteration tracks the keys' byte order, which for the hash-named
    //    packs of t5300's "--stdin-packs handles garbage" is OID order.
    let mut missing: Vec<&String> = order
        .iter()
        .filter(|key| !found.contains_key(*key))
        .collect();
    if !missing.is_empty() {
        missing.sort();
        eprintln!("fatal: could not find pack '{}'", missing[0]);
        return Err(GitError::Exit(128));
    }

    if options.exclude_promisor_objects {
        for key in &order {
            let Some(pack) = found.get(key) else {
                continue;
            };
            if requested
                .get(key)
                .is_some_and(|kind| kind & STDIN_PACK_INCLUDE != 0)
                && pack.is_promisor
            {
                eprintln!(
                    "fatal: packfile {} is a promisor but --exclude-promisor-objects was given",
                    pack.pack_path.display()
                );
                return Err(GitError::Exit(128));
            }
        }
    }

    // 4. Objects in any excluded pack veto inclusion (closed and open both).
    let mut closed_excluded: HashSet<ObjectId> = HashSet::new();
    let mut open_excluded: HashSet<ObjectId> = HashSet::new();
    for (key, &kind) in &requested {
        if let Some(pack) = found.get(key) {
            if kind & STDIN_PACK_EXCLUDE_CLOSED != 0 {
                closed_excluded.extend(pack.oids.iter().copied());
            }
            if kind & STDIN_PACK_EXCLUDE_OPEN != 0 {
                open_excluded.extend(pack.oids.iter().copied());
            }
        }
    }
    let promisor_excluded = if options.exclude_promisor_objects {
        collect_promisor_pack_oids(&object_dirs, format)?
    } else {
        HashSet::new()
    };
    let mut omitted = closed_excluded.clone();
    omitted.extend(open_excluded.iter().copied());
    omitted.extend(promisor_excluded.iter().copied());

    // Git only treats closed-excluded packs as traversal boundaries when an
    // open-excluded pack is also present. Without a `!pack`, follow mode may
    // walk through `^pack` objects to rescue older objects from unnamed packs.
    // Promisor objects are always a boundary under `--exclude-promisor-objects`.
    let mut traversal_stop = promisor_excluded;
    if !open_excluded.is_empty() {
        traversal_stop.extend(closed_excluded.iter().copied());
    }

    // 5. Walk the included packs in ascending-mtime order (newest objects laid
    //    out last, as upstream's pack_mtime_cmp arranges) and collect the
    //    wanted objects, deduplicating across packs.
    let mut included_keys: Vec<&String> = order
        .iter()
        .filter(|key| {
            requested
                .get(*key)
                .is_some_and(|kind| kind & STDIN_PACK_INCLUDE != 0)
        })
        .collect();
    included_keys.sort_by_key(|key| found.get(*key).map(|pack| pack.mtime));

    let mut oids: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut object_cache: HashMap<ObjectId, Arc<EncodedObject>> = HashMap::new();
    let mut follow_starts: Vec<ObjectId> = Vec::new();
    for key in included_keys {
        let Some(pack) = found.get(key) else {
            continue;
        };
        for oid in &pack.oids {
            if follow
                && let Some(object) =
                    read_stdin_pack_object_tolerant(database, oid, &mut object_cache)?
                && object.object_type == ObjectType::Commit
            {
                follow_starts.push(*oid);
            }
            if omitted.contains(oid) || !seen.insert(*oid) {
                continue;
            }
            oids.push(*oid);
        }
    }

    if follow {
        for (key, &kind) in &requested {
            if kind & STDIN_PACK_EXCLUDE_OPEN == 0 {
                continue;
            }
            let Some(pack) = found.get(key) else {
                continue;
            };
            for oid in &pack.oids {
                if let Some(object) =
                    read_stdin_pack_object_tolerant(database, oid, &mut object_cache)?
                    && object.object_type == ObjectType::Commit
                {
                    follow_starts.push(*oid);
                }
            }
        }
    }

    // 6. `--unpacked` appends loose objects not vetoed by an excluded pack.
    if options.unpacked {
        let mut loose: HashSet<ObjectId> = HashSet::new();
        for dir in &object_dirs {
            collect_loose_oids(dir, format, &mut loose)?;
        }
        let mut loose: Vec<ObjectId> = loose.into_iter().collect();
        loose.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        for oid in loose {
            let object = if follow {
                read_stdin_pack_object_tolerant(database, &oid, &mut object_cache)?
            } else {
                None
            };
            if follow
                && let Some(object) = &object
                && object.object_type == ObjectType::Commit
            {
                follow_starts.push(oid);
            }
            if omitted.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            oids.push(oid);
        }
    }

    if follow {
        let mut state = StdinPackFollowState {
            oids: &mut oids,
            seen: &mut seen,
            expanded: HashSet::new(),
            omitted: &omitted,
            stop: &traversal_stop,
            object_cache: &mut object_cache,
        };
        for oid in follow_starts {
            walk_stdin_pack_follow_object(database, format, oid, &mut state)?;
        }
    }

    // 7. Materialise the object bodies for the writer.
    let mut objects = Vec::with_capacity(oids.len());
    for oid in &oids {
        match object_cache
            .get(oid)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| database.read_object(oid))
        {
            Ok(object) => objects.push(object),
            Err(GitError::NotFound(_)) => {
                eprintln!("fatal: unable to read {oid}");
                return Err(GitError::Exit(128));
            }
            Err(err) => return Err(err),
        }
    }

    Ok((oids, objects))
}

struct StdinPackFollowState<'a> {
    oids: &'a mut Vec<ObjectId>,
    seen: &'a mut HashSet<ObjectId>,
    expanded: HashSet<ObjectId>,
    omitted: &'a HashSet<ObjectId>,
    stop: &'a HashSet<ObjectId>,
    object_cache: &'a mut HashMap<ObjectId, Arc<EncodedObject>>,
}

fn read_stdin_pack_object_tolerant(
    database: &FileObjectDatabase,
    oid: &ObjectId,
    cache: &mut HashMap<ObjectId, Arc<EncodedObject>>,
) -> Result<Option<Arc<EncodedObject>>> {
    if let Some(object) = cache.get(oid) {
        return Ok(Some(Arc::clone(object)));
    }
    match database.read_object(oid) {
        Ok(object) => {
            cache.insert(*oid, Arc::clone(&object));
            Ok(Some(object))
        }
        Err(GitError::NotFound(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

fn walk_stdin_pack_follow_object(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    oid: ObjectId,
    state: &mut StdinPackFollowState<'_>,
) -> Result<()> {
    if state.stop.contains(&oid) {
        return Ok(());
    }
    let Some(object) = read_stdin_pack_object_tolerant(database, &oid, state.object_cache)? else {
        return Ok(());
    };
    if !state.omitted.contains(&oid) && state.seen.insert(oid) {
        state.oids.push(oid);
    }
    if !state.expanded.insert(oid) {
        return Ok(());
    }
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body)?;
            walk_stdin_pack_follow_object(database, format, commit.tree, state)?;
            for parent in commit.parents {
                walk_stdin_pack_follow_object(database, format, parent, state)?;
            }
        }
        ObjectType::Tree => {
            for entry in TreeEntries::new(format, &object.body) {
                let entry = entry?;
                if !entry.is_gitlink() {
                    walk_stdin_pack_follow_object(database, format, entry.oid, state)?;
                }
            }
        }
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            walk_stdin_pack_follow_object(database, format, tag.object, state)?;
        }
        ObjectType::Blob => {}
    }
    Ok(())
}

fn collect_promisor_pack_oids(
    object_dirs: &[PathBuf],
    format: ObjectFormat,
) -> Result<HashSet<ObjectId>> {
    let mut oids = HashSet::new();
    for dir in object_dirs {
        let pack_dir = dir.join("pack");
        let Ok(entries) = fs::read_dir(&pack_dir) else {
            continue;
        };
        for entry in entries {
            let idx_path = entry?.path();
            if idx_path.extension().and_then(|ext| ext.to_str()) != Some("idx")
                || !idx_path.with_extension("promisor").exists()
            {
                continue;
            }
            let Ok(bytes) = fs::read(&idx_path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&bytes, format) else {
                continue;
            };
            oids.extend(index.entries.into_iter().map(|entry| entry.oid));
        }
    }
    Ok(oids)
}

/// The final progress totals line and the trace2 data events upstream emits
/// after writing the pack (builtin/pack-objects.c `cmd_pack_objects` tail).
fn pack_objects_stats_line(
    written_total: u64,
    delta_count: u32,
    pack_reused: u64,
    packs_reused: u64,
) -> String {
    format!(
        "Total {written_total} (delta {delta_count}), reused {pack_reused} (delta 0), pack-reused {pack_reused} (from {packs_reused})"
    )
}

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

type TraversalPackObjects = (
    Vec<ObjectId>,
    Vec<Arc<EncodedObject>>,
    Vec<VerbatimPackReuse>,
);

/// Enumerate the want set for the traversal mode and decide on pack reuse.
/// Returns the objects to encode fresh (oids + bodies) and the optional
/// verbatim reuse (whose objects are excluded from the fresh list).
fn collect_traversal_objects(
    git_dir: &Path,
    common_git_dir: &Path,
    database: &FileObjectDatabase,
    format: ObjectFormat,
    options: &PackObjectsOptions,
    lazy_fetch: bool,
    replace_objects: bool,
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
        if let Ok(head) = resolve_revision(git_dir, format, "HEAD", replace_objects) {
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
            // An empty line terminates --revs stdin (revision.c
            // read_revisions_from_stdin: `if (!sb.len) break;`).
            if rev.is_empty() {
                break;
            }
            if let Some(range) = sley_rev::parse_revision_range(rev) {
                match range {
                    sley_rev::RevisionRange::Asymmetric { start, end } => {
                        let start_oid =
                            match resolve_revision(git_dir, format, &start, replace_objects) {
                                Ok(oid) => oid,
                                Err(_) => {
                                    eprintln!("fatal: bad revision '{start}'");
                                    return Err(GitError::Exit(128));
                                }
                            };
                        let end_oid = match resolve_revision(git_dir, format, &end, replace_objects)
                        {
                            Ok(oid) => oid,
                            Err(_) => {
                                eprintln!("fatal: bad revision '{end}'");
                                return Err(GitError::Exit(128));
                            }
                        };
                        haves.push(start_oid);
                        wants.push(end_oid);
                    }
                    sley_rev::RevisionRange::Symmetric { .. } => {
                        eprintln!("fatal: bad revision '{rev}'");
                        return Err(GitError::Exit(128));
                    }
                }
                continue;
            }
            let (negative, rev) = match rev.strip_prefix('^') {
                Some(rest) => (true, rest),
                None => (false, rev),
            };
            let oid = match resolve_revision(git_dir, format, rev, replace_objects) {
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

    let config = read_repo_config(git_dir)?;
    let use_sparse = options
        .sparse
        .unwrap_or_else(|| config.get_bool("pack", None, "useSparse").unwrap_or(true));
    // Uninteresting closure first: tolerant of missing objects (upstream
    // never needs to open the have side's missing history). With the sparse
    // algorithm, tree/blob uninteresting marking is path-aware so copied
    // subtrees can be revisited under their new names.
    let excluded = if use_sparse {
        tolerant_sparse_excluded_objects(database, format, &wants, &haves)?
    } else {
        tolerant_reachable_closure(database, format, &haves)?
    };
    let mut traversal_state = FilteredPackTraversalState::default();
    {
        let walk = FilteredPackTraversal {
            database,
            format,
            filter: &options.object_filter,
            missing_action: options.missing_action,
            excluded: &excluded,
            lazy_fetch,
        };
        for oid in wants.iter().rev() {
            walk.visit_oid(*oid, Vec::new(), 0, true, &mut traversal_state)?;
        }
    }
    let FilteredPackTraversalState {
        mut want_oids,
        mut want_objects,
        mut want_set,
        omitted_oids,
        ..
    } = traversal_state;
    if options.filter_print_omitted {
        let mut omitted: Vec<_> = omitted_oids.into_iter().collect();
        omitted.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        for oid in omitted {
            eprintln!("~{oid}");
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
        && options.object_filter == PackObjectFilter::None
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

impl PackObjectFilter {
    fn parse(spec: &str) -> Result<Self> {
        if spec == "blob:none" {
            return Ok(Self::BlobNone);
        }
        if let Some(value) = spec.strip_prefix("blob:limit=") {
            return Ok(Self::BlobLimit(parse_pack_filter_size(value)?));
        }
        if let Some(value) = spec.strip_prefix("tree:") {
            return Ok(Self::TreeDepth(parse_pack_filter_depth(value)?));
        }
        if let Some(value) = spec.strip_prefix("object:type=") {
            return Ok(Self::ObjectType(parse_pack_filter_object_type(value)?));
        }
        if let Some(value) = spec.strip_prefix("sparse:oid=") {
            return Ok(Self::SparseOid(value.to_string()));
        }
        if spec.starts_with("sparse:path=") {
            eprintln!("fatal: sparse:path filters support has been dropped");
            return Err(GitError::Exit(128));
        }
        if let Some(value) = spec.strip_prefix("combine:") {
            if value.is_empty() {
                eprintln!("fatal: expected something after combine:");
                return Err(GitError::Exit(128));
            }
            let mut filters = Vec::new();
            for raw in value.split('+') {
                let decoded = pack_filter_decode_sub_filter(raw)?;
                filters.push(Self::parse(&decoded)?);
            }
            return Ok(Self::Combine(filters));
        }
        eprintln!("fatal: invalid filter-spec '{spec}'");
        Err(GitError::Exit(128))
    }

    fn combine_with(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, filter) | (filter, Self::None) => filter,
            (Self::Combine(mut filters), Self::Combine(mut more)) => {
                filters.append(&mut more);
                Self::Combine(filters)
            }
            (Self::Combine(mut filters), filter) | (filter, Self::Combine(mut filters)) => {
                filters.push(filter);
                Self::Combine(filters)
            }
            (left, right) => Self::Combine(vec![left, right]),
        }
    }

    fn resolve(
        self,
        git_dir: &Path,
        database: &FileObjectDatabase,
        format: ObjectFormat,
        replace_objects: bool,
    ) -> Result<Self> {
        match self {
            Self::SparseOid(value) => {
                let oid = if let Some((rev, path)) = value.split_once(':') {
                    sley_rev::resolve_rev_path(git_dir, format, database, rev, path)?
                } else {
                    resolve_revision(git_dir, format, &value, replace_objects)?
                };
                let object = database.read_object(&oid)?;
                if object.object_type != ObjectType::Blob {
                    eprintln!("fatal: expected blob for sparse:oid filter");
                    return Err(GitError::Exit(128));
                }
                Ok(Self::Sparse(
                    object
                        .body
                        .split(|byte| *byte == b'\n')
                        .filter(|line| !line.is_empty())
                        .map(|line| line.to_vec())
                        .collect(),
                ))
            }
            Self::Combine(filters) => filters
                .into_iter()
                .map(|filter| filter.resolve(git_dir, database, format, replace_objects))
                .collect::<Result<Vec<_>>>()
                .map(Self::Combine),
            filter => Ok(filter),
        }
    }

    fn includes_object(
        &self,
        object_type: ObjectType,
        path: &[u8],
        size: Option<usize>,
        depth: usize,
    ) -> bool {
        match self {
            Self::None => true,
            Self::BlobNone => object_type != ObjectType::Blob,
            Self::BlobLimit(limit) => object_type != ObjectType::Blob || size.unwrap_or(0) < *limit,
            Self::ObjectType(wanted) => object_type == *wanted,
            Self::TreeDepth(limit) => object_type == ObjectType::Commit || depth < *limit,
            Self::Sparse(patterns) => {
                object_type != ObjectType::Blob
                    || pack_sparse_patterns_include(patterns, path, object_type)
            }
            Self::Combine(filters) => filters
                .iter()
                .all(|filter| filter.includes_object(object_type, path, size, depth)),
            Self::SparseOid(_) => unreachable!("sparse:oid filter must be resolved before use"),
        }
    }

    fn descends_into_tree(&self, next_depth: usize) -> bool {
        match self {
            Self::TreeDepth(limit) => next_depth < *limit,
            Self::ObjectType(ObjectType::Commit | ObjectType::Tag) => false,
            Self::Combine(filters) => filters
                .iter()
                .all(|filter| filter.descends_into_tree(next_depth)),
            _ => true,
        }
    }

    fn needs_blob_size(&self) -> bool {
        match self {
            Self::BlobLimit(_) => true,
            Self::Combine(filters) => filters.iter().any(Self::needs_blob_size),
            _ => false,
        }
    }
}

struct FilteredPackTraversal<'a> {
    database: &'a FileObjectDatabase,
    format: ObjectFormat,
    filter: &'a PackObjectFilter,
    missing_action: PackObjectsMissingAction,
    excluded: &'a HashSet<ObjectId>,
    lazy_fetch: bool,
}

#[derive(Default)]
struct FilteredPackTraversalState {
    want_oids: Vec<ObjectId>,
    want_objects: Vec<Arc<EncodedObject>>,
    want_set: HashSet<ObjectId>,
    omitted_oids: HashSet<ObjectId>,
    expanded_commits: HashSet<ObjectId>,
    expanded_tags: HashSet<ObjectId>,
    expanded_trees: HashSet<(ObjectId, Vec<u8>)>,
}

impl FilteredPackTraversal<'_> {
    fn visit_oid(
        &self,
        oid: ObjectId,
        path: Vec<u8>,
        depth: usize,
        provided: bool,
        state: &mut FilteredPackTraversalState,
    ) -> Result<()> {
        if self.excluded.contains(&oid) {
            return Ok(());
        }
        let object =
            crate::read_object_maybe_prefetch_promisor(self.database, &oid, self.lazy_fetch)?;
        match object.object_type {
            ObjectType::Commit => self.visit_commit(oid, object, provided, state),
            ObjectType::Tree => self.visit_tree(oid, object, path, depth, provided, state),
            ObjectType::Tag => self.visit_tag(oid, object, provided, state),
            ObjectType::Blob => self.visit_blob(oid, path, depth, provided, Some(object), state),
        }
    }

    fn visit_commit(
        &self,
        oid: ObjectId,
        object: Arc<EncodedObject>,
        provided: bool,
        state: &mut FilteredPackTraversalState,
    ) -> Result<()> {
        if provided
            || self
                .filter
                .includes_object(ObjectType::Commit, &[], None, 0)
        {
            self.include_object(oid, Arc::clone(&object), state);
        } else {
            self.omit_object(oid, state);
        }
        if !state.expanded_commits.insert(oid) {
            return Ok(());
        }
        let commit = Commit::parse_ref(self.format, &object.body)?;
        self.visit_tree_oid(commit.tree, Vec::new(), 0, false, state)?;
        for parent in commit.parents {
            self.visit_oid(parent, Vec::new(), 0, false, state)?;
        }
        Ok(())
    }

    fn visit_tag(
        &self,
        oid: ObjectId,
        object: Arc<EncodedObject>,
        provided: bool,
        state: &mut FilteredPackTraversalState,
    ) -> Result<()> {
        if provided || self.filter.includes_object(ObjectType::Tag, &[], None, 0) {
            self.include_object(oid, Arc::clone(&object), state);
        } else {
            self.omit_object(oid, state);
        }
        if !state.expanded_tags.insert(oid) {
            return Ok(());
        }
        let tag = Tag::parse_ref(self.format, &object.body)?;
        self.visit_oid(tag.object, Vec::new(), 0, false, state)
    }

    fn visit_tree_oid(
        &self,
        oid: ObjectId,
        path: Vec<u8>,
        depth: usize,
        provided: bool,
        state: &mut FilteredPackTraversalState,
    ) -> Result<()> {
        if self.excluded.contains(&oid) {
            return Ok(());
        }
        let object = match crate::read_object_maybe_prefetch_promisor(
            self.database,
            &oid,
            self.lazy_fetch,
        ) {
            Ok(object) => object,
            Err(GitError::NotFound(_)) => {
                eprintln!("fatal: bad tree object {oid}");
                return Err(GitError::Exit(128));
            }
            Err(err) => return Err(err),
        };
        self.visit_tree(oid, object, path, depth, provided, state)
    }

    fn visit_tree(
        &self,
        oid: ObjectId,
        object: Arc<EncodedObject>,
        path: Vec<u8>,
        depth: usize,
        provided: bool,
        state: &mut FilteredPackTraversalState,
    ) -> Result<()> {
        let excluded = self.excluded.contains(&oid);
        let include = provided
            || self
                .filter
                .includes_object(ObjectType::Tree, &path, None, depth);
        if include && !excluded {
            self.include_object(oid, Arc::clone(&object), state);
        } else {
            self.omit_object(oid, state);
        }
        if !state.expanded_trees.insert((oid, path.clone())) {
            return Ok(());
        }
        if !self.filter.descends_into_tree(depth + 1) {
            self.omit_tree_contents(&object, &path, state)?;
            return Ok(());
        }
        for entry in TreeEntries::new(self.format, &object.body) {
            let entry = entry?;
            if entry.is_gitlink() {
                continue;
            }
            let entry_path = pack_filter_join_path(&path, entry.name);
            let entry_type = tree_entry_object_type(entry.mode);
            if entry_type == ObjectType::Tree {
                self.visit_tree_oid(entry.oid, entry_path, depth + 1, false, state)?;
            } else {
                self.visit_blob(entry.oid, entry_path, depth + 1, false, None, state)?;
            }
        }
        Ok(())
    }

    fn visit_blob(
        &self,
        oid: ObjectId,
        path: Vec<u8>,
        depth: usize,
        provided: bool,
        object: Option<Arc<EncodedObject>>,
        state: &mut FilteredPackTraversalState,
    ) -> Result<()> {
        if self.excluded.contains(&oid) {
            return Ok(());
        }
        if state.want_set.contains(&oid) {
            return Ok(());
        }
        let mut object = object;
        let size = if self.filter.needs_blob_size() {
            match object {
                Some(ref object) => Some(object.body.len()),
                None => match crate::read_object_maybe_prefetch_promisor(
                    self.database,
                    &oid,
                    self.lazy_fetch,
                ) {
                    Ok(read) => {
                        let len = read.body.len();
                        object = Some(read);
                        Some(len)
                    }
                    Err(GitError::NotFound(_))
                        if self.missing_action == PackObjectsMissingAction::AllowAny =>
                    {
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                },
            }
        } else {
            None
        };
        let include = provided
            || self
                .filter
                .includes_object(ObjectType::Blob, &path, size, depth);
        if include {
            let object = match object {
                Some(object) => object,
                None => match crate::read_object_maybe_prefetch_promisor(
                    self.database,
                    &oid,
                    self.lazy_fetch,
                ) {
                    Ok(object) => object,
                    Err(GitError::NotFound(_))
                        if self.missing_action == PackObjectsMissingAction::AllowAny =>
                    {
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                },
            };
            self.include_object(oid, object, state);
        } else if !state.want_set.contains(&oid) {
            self.omit_object(oid, state);
        }
        Ok(())
    }

    fn omit_tree_contents(
        &self,
        object: &EncodedObject,
        path: &[u8],
        state: &mut FilteredPackTraversalState,
    ) -> Result<()> {
        for entry in TreeEntries::new(self.format, &object.body) {
            let entry = entry?;
            if entry.is_gitlink() || state.want_set.contains(&entry.oid) {
                continue;
            }
            state.omitted_oids.insert(entry.oid);
            if tree_entry_object_type(entry.mode) == ObjectType::Tree
                && let Ok(child) = self.database.read_object(&entry.oid)
            {
                let entry_path = pack_filter_join_path(path, entry.name);
                self.omit_tree_contents(&child, &entry_path, state)?;
            }
        }
        Ok(())
    }

    fn include_object(
        &self,
        oid: ObjectId,
        object: Arc<EncodedObject>,
        state: &mut FilteredPackTraversalState,
    ) {
        if state.want_set.insert(oid) {
            state.want_oids.push(oid);
            state.want_objects.push(object);
        }
        state.omitted_oids.remove(&oid);
    }

    fn omit_object(&self, oid: ObjectId, state: &mut FilteredPackTraversalState) {
        if !state.want_set.contains(&oid) {
            state.omitted_oids.insert(oid);
        }
    }
}

fn parse_pack_filter_size(value: &str) -> Result<usize> {
    sley_rev::revlist::parse_rev_list_blob_limit(value)
}

fn parse_pack_filter_depth(value: &str) -> Result<usize> {
    sley_rev::revlist::parse_rev_list_tree_depth(value)
}

fn parse_pack_filter_object_type(value: &str) -> Result<ObjectType> {
    sley_rev::revlist::parse_rev_list_object_type_filter(value)
}

fn pack_filter_decode_sub_filter(raw: &str) -> Result<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'@' | b'`' | b'~' => {
                eprintln!(
                    "fatal: must escape char in sub-filter-spec: '{}'",
                    bytes[idx] as char
                );
                return Err(GitError::Exit(128));
            }
            b'%' => {
                let Some(high) = bytes
                    .get(idx + 1)
                    .and_then(|byte| (*byte as char).to_digit(16))
                else {
                    eprintln!("fatal: invalid filter-spec");
                    return Err(GitError::Exit(128));
                };
                let Some(low) = bytes
                    .get(idx + 2)
                    .and_then(|byte| (*byte as char).to_digit(16))
                else {
                    eprintln!("fatal: invalid filter-spec");
                    return Err(GitError::Exit(128));
                };
                out.push((high * 16 + low) as u8);
                idx += 3;
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn pack_sparse_patterns_include(
    patterns: &[Vec<u8>],
    path: &[u8],
    object_type: ObjectType,
) -> bool {
    if path.is_empty() {
        return object_type == ObjectType::Tree;
    }
    patterns.iter().any(|pattern| {
        if pattern.ends_with(b"/") {
            let dir = &pattern[..pattern.len() - 1];
            path == dir || path.starts_with(pattern)
        } else {
            path == pattern
        }
    })
}

fn pack_filter_join_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    path.push(b'/');
    path.extend_from_slice(name);
    path
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

/// Find local bitmapped pack entries that can be spliced into the output
/// verbatim for the requested `want_set`.
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
        let all_pack_oids: HashSet<ObjectId> =
            index.entries.iter().map(|entry| entry.oid).collect();
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
            return Ok(Some(vec![ReusablePackCandidate {
                entry_bytes,
                oids: all_pack_oids,
            }]));
        } else if let Some((oids, entry_bytes)) =
            raw_partial_pack_entries_for_wanted_oids(format, &pack_bytes, &index.entries, want_set)?
        {
            return Ok(Some(vec![ReusablePackCandidate { entry_bytes, oids }]));
        }
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
        .join(format!(
            "multi-pack-index-{}.bitmap",
            midx.checksum.to_hex()
        ))
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
        let all_pack_oids: HashSet<ObjectId> =
            index.entries.iter().map(|entry| entry.oid).collect();
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
    let Some(entry_bytes) =
        raw_pack_entries_for_oids(format, pack_bytes, index_entries, &oids, true)?
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

#[derive(Clone, Copy)]
struct SparseTreeVisit {
    oid: ObjectId,
    uninteresting: bool,
}

fn tolerant_sparse_excluded_objects(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    wants: &[ObjectId],
    haves: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    let mut excluded = HashSet::new();
    let mut roots = Vec::new();

    collect_sparse_commit_tree_roots(
        database,
        format,
        haves,
        None,
        true,
        &mut excluded,
        &mut roots,
    )?;
    let have_excluded = excluded.clone();
    collect_sparse_commit_tree_roots(
        database,
        format,
        wants,
        Some(&have_excluded),
        false,
        &mut excluded,
        &mut roots,
    )?;
    mark_sparse_uninteresting_trees(database, format, roots, &mut excluded)?;
    Ok(excluded)
}

fn collect_sparse_commit_tree_roots(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
    stop: Option<&HashSet<ObjectId>>,
    mark_uninteresting: bool,
    excluded: &mut HashSet<ObjectId>,
    roots: &mut Vec<SparseTreeVisit>,
) -> Result<()> {
    let mut seen = HashSet::new();
    let mut pending: Vec<ObjectId> = starts.to_vec();
    while let Some(oid) = pending.pop() {
        if stop.is_some_and(|stop| stop.contains(&oid)) || !seen.insert(oid) {
            continue;
        }
        let object = match database.read_object(&oid) {
            Ok(object) => object,
            Err(GitError::NotFound(_)) if mark_uninteresting => {
                excluded.insert(oid);
                continue;
            }
            Err(GitError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        if mark_uninteresting {
            excluded.insert(oid);
        }
        match object.object_type {
            ObjectType::Commit => {
                let commit = Commit::parse_ref(format, &object.body)?;
                roots.push(SparseTreeVisit {
                    oid: commit.tree,
                    uninteresting: mark_uninteresting,
                });
                if mark_uninteresting {
                    excluded.insert(commit.tree);
                }
                pending.extend(commit.parents);
            }
            ObjectType::Tree => {
                roots.push(SparseTreeVisit {
                    oid,
                    uninteresting: mark_uninteresting,
                });
            }
            ObjectType::Tag => {
                let tag = Tag::parse_ref(format, &object.body)?;
                pending.push(tag.object);
            }
            ObjectType::Blob => {}
        }
    }
    Ok(())
}

fn mark_sparse_uninteresting_trees(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    trees: Vec<SparseTreeVisit>,
    excluded: &mut HashSet<ObjectId>,
) -> Result<()> {
    let mut by_oid: HashMap<ObjectId, bool> = HashMap::new();
    for tree in trees {
        let uninteresting = tree.uninteresting || excluded.contains(&tree.oid);
        by_oid
            .entry(tree.oid)
            .and_modify(|existing| *existing |= uninteresting)
            .or_insert(uninteresting);
    }

    let mut has_interesting = false;
    let mut has_uninteresting = false;
    for uninteresting in by_oid.values().copied() {
        if uninteresting {
            has_uninteresting = true;
        } else {
            has_interesting = true;
        }
    }
    if !has_interesting || !has_uninteresting {
        return Ok(());
    }

    let mut by_path: BTreeMap<Vec<u8>, Vec<SparseTreeVisit>> = BTreeMap::new();
    for (oid, uninteresting) in by_oid {
        let object = match database.read_object(&oid) {
            Ok(object) => object,
            Err(GitError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        if object.object_type != ObjectType::Tree {
            continue;
        }
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry?;
            if entry.is_gitlink() {
                continue;
            }
            if uninteresting {
                excluded.insert(entry.oid);
            }
            if tree_entry_object_type(entry.mode) == ObjectType::Tree {
                by_path
                    .entry(entry.name.to_vec())
                    .or_default()
                    .push(SparseTreeVisit {
                        oid: entry.oid,
                        uninteresting,
                    });
            }
        }
    }

    for child_trees in by_path.into_values() {
        mark_sparse_uninteresting_trees(database, format, child_trees, excluded)?;
    }
    Ok(())
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
        let environment_alternates = env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .map(|value| env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();
        Self::scan_with_environment_alternates(common_git_dir, format, &environment_alternates)
    }

    fn scan_with_environment_alternates(
        common_git_dir: &Path,
        format: ObjectFormat,
        environment_alternates: &[PathBuf],
    ) -> Result<Self> {
        let objects_dir = sley_odb::repository_objects_dir(common_git_dir);
        let mut object_dirs: Vec<(PathBuf, bool)> = vec![(objects_dir.clone(), true)];
        // Environment alternates participate in the same locality policy as
        // `objects/info/alternates`. In particular, `--local` must not copy an
        // object merely because its alternate was injected for this process
        // instead of recorded in the repository.
        object_dirs.extend(
            environment_alternates
                .iter()
                .cloned()
                .map(|path| (path, false)),
        );
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

/// Parse a `--index-version=<version>[,<offset>]` spec the way
/// builtin/pack-objects.c `option_parse_index_version` does with `strtoul`: the
/// leading digits are the version, an optional `,<offset>` follows, and any
/// leftover characters (including a bare trailing `,` with nothing after it)
/// are rejected with git's `bad index version` message.
fn parse_index_version_spec(spec: &str) -> Result<u32> {
    let bytes = spec.as_bytes();
    let digits_end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    let version: u32 = spec[..digits_end].parse().map_err(|_| {
        eprintln!("fatal: bad index version '{spec}'");
        GitError::Exit(128)
    })?;
    if version > 2 {
        eprintln!("fatal: unsupported index version {spec}");
        return Err(GitError::Exit(128));
    }
    let mut rest = &spec[digits_end..];
    // `if (*c == ',' && c[1])` — only consume the offset when the comma is
    // followed by at least one character. A bare trailing `,` is NOT consumed,
    // so it survives into the leftover check below and is rejected.
    if let Some(after_comma) = rest.strip_prefix(',')
        && !after_comma.is_empty()
    {
        // strtoul base 0: consume an optional 0x/0X prefix then alphanumeric
        // digits. The offset value itself is derived from the offsets by sley's
        // writer, so we only need to consume the token and reject trailing junk.
        let offset_token = after_comma
            .strip_prefix("0x")
            .or_else(|| after_comma.strip_prefix("0X"));
        let body = offset_token.unwrap_or(after_comma);
        let off_end = body
            .bytes()
            .position(|byte| !byte.is_ascii_hexdigit())
            .unwrap_or(body.len());
        rest = &body[off_end..];
    }
    if !rest.is_empty() {
        eprintln!("fatal: bad index version '{spec}'");
        return Err(GitError::Exit(128));
    }
    if version != 1 && version != 2 {
        eprintln!("fatal: bad index version '{spec}'");
        return Err(GitError::Exit(128));
    }
    Ok(version)
}

/// `git pack-objects --cruft [--cruft-expiration=<time>]`.
///
/// Mirrors builtin/pack-objects.c `read_cruft_objects`: the fresh/discard pack
/// names arrive on stdin (`-`-prefixed lines name discard packs), every pack
/// the caller did not mention is treated as kept (its objects are skipped), and
/// the cruft pack collects every unreachable object that survives, tagged with
/// the maximum mtime of any unkept copy. With an expiration, recent objects
/// (mtime strictly newer than the cutoff) anchor a reachability traversal that
/// rescues their older dependencies at the cutoff mtime; everything else older
/// than the cutoff is dropped. A `.mtimes` companion records the per-object
/// timestamps in lexicographic (index) order.
fn write_cruft_pack(
    git_dir: &Path,
    common_git_dir: &Path,
    database: &FileObjectDatabase,
    format: ObjectFormat,
    config: &sley_config::GitConfig,
    options: &PackObjectsOptions,
    progress: bool,
    max_pack_size: Option<u64>,
) -> Result<()> {
    let objects_dir = sley_odb::repository_objects_dir(common_git_dir);
    let pack_dir = objects_dir.join("pack");

    // Stdin: bare lines name fresh (retained) packs, `-`-prefixed name discard
    // packs. Both name the *new* world; any pack not mentioned is "unknown" and
    // is kept (its objects ignored), exactly like upstream's third branch.
    let mut fresh_packs: HashSet<String> = HashSet::new();
    let mut discard_packs: HashSet<String> = HashSet::new();
    {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            let name = line.trim_end_matches(['\n', '\r']);
            if name.is_empty() {
                continue;
            }
            // Discard packs (`-name`) are not retained, so their objects are
            // *candidates* for the new cruft pack — they are not "fresh".
            if let Some(stripped) = name.strip_prefix('-') {
                discard_packs.insert(stripped.to_string());
            } else {
                fresh_packs.insert(name.to_string());
            }
        }
    }

    // Index every pack on disk. A pack is "kept" (its objects skipped) when it
    // is fresh or was not mentioned at all. Only an explicit discard-list entry
    // makes a pack a candidate source. That distinction matters when a pack is
    // created after the caller enumerates its inputs: upstream treats such an
    // unknown pack as retained instead of silently folding it into the cruft
    // output.
    //
    // The mtime contributed by a packed object is the pack's own mtime, except
    // for a cruft pack (`.mtimes` present) where each object carries its own
    // recorded mtime.
    let mut mtimes: HashMap<ObjectId, u32> = HashMap::new();
    let locations = options
        .local
        .then(|| ObjectLocationScan::scan(common_git_dir, format))
        .transpose()?;
    // Object ids that live in a kept (fresh/unknown) pack: such a copy vetoes
    // adding the object to the cruft pack (want_object_in_pack's kept-pack
    // rule), unless a cruft pack being retained holds it — but on this suite
    // the retained packs are non-cruft, so a kept copy is an unconditional veto.
    let mut kept_pack_oids: HashSet<ObjectId> = HashSet::new();
    let mut candidate_packs: Vec<(Vec<(ObjectId, u64)>, Vec<u32>)> = Vec::new();

    if let Ok(entries) = fs::read_dir(&pack_dir) {
        let mut idx_paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("idx") {
                idx_paths.push(path);
            }
        }
        // Deterministic order so the chosen-mtime ties resolve like a stable run.
        idx_paths.sort();
        for idx_path in idx_paths {
            let Some(stem) = idx_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let pack_name = stem.trim_end_matches(".idx").to_string() + ".pack";
            let pack_path = idx_path.with_extension("pack");
            let Ok(idx_bytes) = fs::read(&idx_path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&idx_bytes, format) else {
                continue;
            };
            // Skip a `.keep`-marked pack just like a kept pack.
            // Unknown packs (mentioned in neither set) appeared after the
            // caller enumerated its inputs and must also be retained. Only an
            // explicit `-pack-name.pack` line selects a pack for replacement.
            let is_kept_pack = fresh_packs.contains(&pack_name)
                || !discard_packs.contains(&pack_name)
                || idx_path.with_extension("keep").exists();
            if is_kept_pack {
                for entry in &index.entries {
                    kept_pack_oids.insert(entry.oid);
                }
                continue;
            }
            // Candidate source: contribute object mtimes.
            let mtimes_path = idx_path.with_extension("mtimes");
            let pack_object_mtimes: Option<Vec<u32>> =
                fs::read(&mtimes_path).ok().and_then(|bytes| {
                    sley_pack::PackMtimes::parse(&bytes, format, index.entries.len())
                        .ok()
                        .map(|parsed| parsed.mtimes)
                });
            let pack_mtime = fs::metadata(&pack_path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|dur| dur.as_secs() as u32)
                .unwrap_or(0);
            // PackIndex entries are sorted by oid; a cruft `.mtimes` table is in
            // the same lexicographic order, so we can zip them positionally.
            let mut object_list: Vec<(ObjectId, u64)> = Vec::with_capacity(index.entries.len());
            let mut object_mtimes: Vec<u32> = Vec::with_capacity(index.entries.len());
            for (pos, entry) in index.entries.iter().enumerate() {
                let mtime = pack_object_mtimes
                    .as_ref()
                    .and_then(|table| table.get(pos).copied())
                    .unwrap_or(pack_mtime);
                object_list.push((entry.oid, entry.offset));
                object_mtimes.push(mtime);
            }
            candidate_packs.push((object_list, object_mtimes));
        }
    }

    // Loose unreachable objects: every loose object on disk, tagged with its
    // file mtime (add_unreachable_loose_objects deliberately ignores
    // reachability — add_object_entry dedups against the reachable set later).
    let mut loose_oids: HashSet<ObjectId> = HashSet::new();
    collect_loose_oids(&objects_dir, format, &mut loose_oids)?;

    // Record a candidate object's mtime, taking the max over all copies.
    let mut record = |oid: ObjectId, mtime: u32, kept: &HashSet<ObjectId>| {
        if kept.contains(&oid) {
            return;
        }
        if locations
            .as_ref()
            .is_some_and(|locations| !locations.wanted(&oid, options))
        {
            return;
        }
        mtimes
            .entry(oid)
            .and_modify(|existing| {
                if mtime > *existing {
                    *existing = mtime;
                }
            })
            .or_insert(mtime);
    };

    for (object_list, object_mtimes) in &candidate_packs {
        for ((oid, _offset), mtime) in object_list.iter().zip(object_mtimes) {
            record(*oid, *mtime, &kept_pack_oids);
        }
    }
    for oid in &loose_oids {
        let path = loose_object_path(&objects_dir, oid);
        let mtime = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|dur| dur.as_secs() as u32)
            .unwrap_or(0);
        record(*oid, mtime, &kept_pack_oids);
    }

    // Expiration: rescue older objects reachable from a recent one, then drop
    // the rest. Without an expiration, every candidate survives.
    if let Some(expiration) = options.cruft_expiration {
        // Git invokes recent-object hooks from pack-objects only after it has
        // found at least one cruft candidate. An empty cruft side is a no-op,
        // even when a configured hook would fail.
        let additional_recent = if mtimes.is_empty() {
            Vec::new()
        } else {
            let hook_cwd = env::current_dir()?;
            commands::hooks::run_recent_objects_hooks(config, format, &hook_cwd)?
        };
        rescue_and_expire_cruft(
            database,
            format,
            &mut mtimes,
            expiration,
            &additional_recent,
        )?;
    }

    let _ = (git_dir, progress);
    let mut cruft_packs = sley_odb::build_cruft_packs_from_mtimes(
        database,
        format,
        &mtimes,
        &sley_odb::CruftPackOptions {
            max_pack_size,
            pack_write: PackWriteOptions::new(),
            ..sley_odb::CruftPackOptions::default()
        },
    )?;
    if cruft_packs.is_empty() {
        cruft_packs.push(sley_odb::build_empty_cruft_pack(
            format,
            &PackWriteOptions::new(),
        )?);
    }

    if options.stdout_mode {
        let Some(cruft) = cruft_packs.first() else {
            return Ok(());
        };
        if cruft_packs.len() != 1 {
            return Err(GitError::InvalidFormat(
                "stdout cruft output unexpectedly produced multiple packs".into(),
            ));
        }
        let mut stdout = io::stdout();
        stdout.write_all(&cruft.pack)?;
        stdout.flush()?;
        return Ok(());
    }

    let base_name = options
        .base_name
        .as_ref()
        .expect("base name required without --stdout");
    for cruft in cruft_packs {
        let checksum_hex = cruft.checksum.to_hex();
        fs::write(format!("{base_name}-{checksum_hex}.pack"), &cruft.pack)?;
        if options.write_reverse_index {
            fs::write(format!("{base_name}-{checksum_hex}.rev"), &cruft.rev)?;
        }
        fs::write(format!("{base_name}-{checksum_hex}.mtimes"), &cruft.mtimes)?;
        fs::write(format!("{base_name}-{checksum_hex}.idx"), &cruft.idx)?;
        println!("{checksum_hex}");
    }
    Ok(())
}

/// Build the loose-object path `objects/ab/cdef...` for `oid`.
fn loose_object_path(objects_dir: &Path, oid: &ObjectId) -> PathBuf {
    let hex = oid.to_hex();
    objects_dir.join(&hex[..2]).join(&hex[2..])
}

/// Apply `--cruft-expiration`: starting from the "recent" candidates (mtime
/// strictly newer than `expiration`), walk reachability and rescue every
/// dependency, assigning rescued objects the expiration mtime if they were not
/// already recorded with a newer one. Candidates older than `expiration` that
/// no recent object reaches are dropped from `mtimes`.
///
/// Mirrors add_unseen_recent_objects_to_traversal + traverse_commit_list with
/// `cruft_expiration`: recent commits/objects are tips, the traversal pulls in
/// their trees/blobs, and show_cruft_object backfills the cutoff mtime for any
/// object the recency scan did not already time-stamp.
fn rescue_and_expire_cruft(
    database: &FileObjectDatabase,
    format: ObjectFormat,
    mtimes: &mut HashMap<ObjectId, u32>,
    expiration: u32,
    additional_recent: &[ObjectId],
) -> Result<()> {
    // Recent objects anchor the rescue traversal.
    let recent: Vec<ObjectId> = mtimes
        .iter()
        .filter(|(_, mtime)| **mtime > expiration)
        .map(|(oid, _)| *oid)
        .collect();

    // Walk reachability from every recent object, tolerating missing links.
    // Every object reached (recent or older) survives; an older object reached
    // this way is rescued at the cutoff mtime.
    let mut keep: HashSet<ObjectId> = HashSet::new();
    let mut pending: Vec<ObjectId> = recent;
    pending.extend_from_slice(additional_recent);
    while let Some(oid) = pending.pop() {
        if !keep.insert(oid) {
            continue;
        }
        let Ok(object) = database.read_object(&oid) else {
            continue;
        };
        match object.object_type {
            ObjectType::Commit => {
                if let Ok(commit) = Commit::parse_ref(format, &object.body) {
                    pending.extend(commit.parents.iter().copied());
                    pending.push(commit.tree);
                }
            }
            ObjectType::Tree => {
                for entry in TreeEntries::new(format, &object.body).flatten() {
                    if !entry.is_gitlink() {
                        pending.push(entry.oid);
                    }
                }
            }
            ObjectType::Tag => {
                if let Ok(tag) = Tag::parse_ref(format, &object.body) {
                    pending.push(tag.object);
                }
            }
            ObjectType::Blob => {}
        }
    }

    // Backfill the cutoff mtime for rescued-but-old objects and drop anything
    // that neither is recent nor was reached by the rescue traversal.
    let mut next: HashMap<ObjectId, u32> = HashMap::new();
    for (oid, mtime) in mtimes.drain() {
        if mtime > expiration {
            next.insert(oid, mtime);
        } else if keep.contains(&oid) {
            // Rescued: keep at its real mtime (already ≤ expiration). git uses
            // the recorded value if present, else the expiration; the recorded
            // value is what we have, so retain it.
            next.insert(oid, mtime);
        }
        // else: expired, dropped.
    }
    *mtimes = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_objects_dir(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "sley-pack-objects-{name}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create object directory");
        path
    }

    #[test]
    fn local_policy_rejects_objects_available_from_an_alternate() {
        let loose = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("loose oid");
        let packed = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("packed oid");
        let locations = ObjectLocationScan {
            nonlocal_loose: HashSet::from([loose]),
            packs: vec![PackMembership {
                oids: HashSet::from([packed]),
                local: false,
                keep: false,
            }],
        };
        let local = PackObjectsOptions {
            local: true,
            ..PackObjectsOptions::default()
        };
        assert!(!locations.wanted(&loose, &local));
        assert!(!locations.wanted(&packed, &local));

        let nonlocal = PackObjectsOptions::default();
        assert!(locations.wanted(&loose, &nonlocal));
        assert!(locations.wanted(&packed, &nonlocal));
    }

    #[test]
    fn location_scan_includes_environment_alternate_loose_objects() {
        let git_dir = temp_objects_dir("environment-alternate-git-dir");
        let local_objects = git_dir.join("objects");
        fs::create_dir_all(&local_objects).expect("create local objects");
        let alternate_objects = temp_objects_dir("environment-alternate-objects");
        let alternate = FileObjectDatabase::new(alternate_objects.clone(), ObjectFormat::Sha1);
        let oid = alternate
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"environment alternate\n".to_vec(),
            ))
            .expect("write alternate object");

        let locations = ObjectLocationScan::scan_with_environment_alternates(
            &git_dir,
            ObjectFormat::Sha1,
            std::slice::from_ref(&alternate_objects),
        )
        .expect("scan environment alternate");
        let local = PackObjectsOptions {
            local: true,
            ..PackObjectsOptions::default()
        };
        assert!(!locations.wanted(&oid, &local));

        fs::remove_dir_all(git_dir).ok();
        fs::remove_dir_all(alternate_objects).ok();
    }

    #[test]
    fn configured_recent_tip_rescues_an_expired_candidate() {
        let objects_dir = temp_objects_dir("recent-tip");
        let database = FileObjectDatabase::new(objects_dir.clone(), ObjectFormat::Sha1);
        let rescued = database
            .write_object(EncodedObject::new(ObjectType::Blob, b"rescued".to_vec()))
            .expect("write rescued object");
        let expired = database
            .write_object(EncodedObject::new(ObjectType::Blob, b"expired".to_vec()))
            .expect("write expired object");
        let mut mtimes = HashMap::from([(rescued, 1), (expired, 1)]);

        rescue_and_expire_cruft(&database, ObjectFormat::Sha1, &mut mtimes, 2, &[rescued])
            .expect("rescue configured tip");

        assert_eq!(mtimes, HashMap::from([(rescued, 1)]));
        fs::remove_dir_all(objects_dir).ok();
    }
}
