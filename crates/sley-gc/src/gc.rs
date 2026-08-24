//! `git gc` planning and execution orchestration: auto decisions, lock/pid/log
//! handling, repack-flavour execution via `sley_odb::plan_gc_repack`, stem
//! management, commit-graph trigger policy, and the shared numeric/expiry
//! config parsers used across the gc family.
//!
//! Lock/pid/log semantics are byte-preserved engine behavior:
//!
//! * `gc.pid` is acquired single-step (create-new) by [`gc_acquire_pid_lock`],
//!   records the holder pid, is probed for liveness before the twelve-hour
//!   staleness window applies, and is removed only by the owner on every exit
//!   path.
//! * `gc.log` is cleared only after a successful non-auto run.
//! * The maintenance/schedule lock early-return paths deliberately leak their
//!   locks (see `maintenance::run_selected` / `update_background_schedule`);
//!   stale-lock recovery depends on that leak. No Drop guards here.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::ObjectType;
use sley_odb::{ObjectReader as _, ObjectWriter as _};
use sley_odb::{collect_reachable_object_ids, repository_objects_dir, FileObjectDatabase};
use sley_pack::{PackFile, PackInput};
use sley_refs::FileRefStore;

use crate::prune::{
    parse_prune_expire, prune_empty_loose_object_dirs, prune_object_is_expired,
    prune_packed_loose_objects, prune_recent_hook_roots, prune_recent_object_roots,
};
use crate::repack::{
    is_config_never, parse_cruft_expiration, repack_cruft_with_lazy_recent_hooks, repack_traversal_roots,
};
use crate::trace2;
use crate::count_objects::CountPackStem;
use crate::{GcServices, parse_reflog_expire_time};

#[derive(Debug, Default)]
pub struct GcOptions {
    pub quiet: bool,
    pub auto: bool,
    pub detach: Option<bool>,
    pub force: bool,
    pub skip_foreground_tasks: bool,
    pub aggressive: bool,
    pub keep_largest_pack: Option<bool>,
    pub cruft_flag: Option<bool>,
    pub prune_override: Option<Option<String>>,
    pub max_cruft_size: Option<u64>,
    pub expire_to: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn gc_run_locked(
    services: &mut GcServices,
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    options: &GcOptions,
    cruft_packs: bool,
    prune_expire: Option<String>,
    auto_mode: GcAutoMode,
) -> Result<()> {
    if !options.skip_foreground_tasks {
        gc_before_repack(services, git_dir, common_git_dir, format, config)?;
    }

    let roots = repack_traversal_roots(
        git_dir,
        common_git_dir,
        format,
        services.replace_objects,
    )?;
    let keep_pack_stems = gc_keep_pack_stems(common_git_dir, config, options)?;
    let resolved_max_cruft_size = options
        .max_cruft_size
        .or_else(|| gc_config_u64(config, "maxCruftSize"));

    // builtin/gc.c add_repack_all_option: pick the repack flavour in the ODB
    // engine, leaving this layer to execute the selected filesystem operation.
    let gc_plan = sley_odb::plan_gc_repack(sley_odb::GcRepackPlanOptions {
        incremental: auto_mode == GcAutoMode::Incremental,
        prune_expire: prune_expire.as_deref(),
        cruft_packs,
        expire_to: options.expire_to.as_deref(),
        max_cruft_size: resolved_max_cruft_size,
        repack_filter: config.get("gc", None, "repackFilter"),
        repack_filter_to: config.get("gc", None, "repackFilterTo"),
    })
    .map_err(|error| GitError::Command(error.to_string()))?;
    let trace_args = gc_plan
        .trace_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    trace_gc_repack(services, &trace_args);
    match gc_plan.mode {
        sley_odb::GcRepackMode::Incremental => {
            if let Some(result) = sley_odb::repack_loose_objects(common_git_dir, format)? {
                sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
            }
        }
        sley_odb::GcRepackMode::Immediate => {
            // prune_expire=="now" with cruft (no expire-to): immediate drop via -a.
            let repack_options = sley_odb::RepackOptions {
                local: true,
                force_rewrite: false,
                pack_kept_objects: false,
                keep_pack_stems,
            };
            if let Some(result) = sley_odb::repack_reachable_objects_with_options(
                common_git_dir,
                format,
                &roots,
                &repack_options,
            )? {
                sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
            }
            gc_remove_cruft_packs(common_git_dir)?;
            if let Some(spec) = prune_expire.as_deref() {
                let expire = parse_prune_expire(spec, "gc.pruneExpire")?;
                gc_prune_expired_loose(common_git_dir, format, &roots, expire)?;
            }
        }
        sley_odb::GcRepackMode::Cruft => {
            // Default: reachable pack + cruft pack, cruft expiry = prune_expire.
            let cruft_expiration = match prune_expire.as_deref() {
                Some(spec) => parse_cruft_expiration(spec)?,
                None => None,
            };
            let repack_options = sley_odb::RepackOptions {
                local: true,
                force_rewrite: false,
                pack_kept_objects: false,
                keep_pack_stems,
            };
            let result = repack_cruft_with_lazy_recent_hooks(
                common_git_dir,
                format,
                &roots,
                cruft_expiration,
                &repack_options,
                &sley_odb::CruftPackOptions {
                    max_pack_size: resolved_max_cruft_size,
                    ..sley_odb::CruftPackOptions::default()
                },
            )?;
            sley_odb::install_cruft_repack_result_with_expire_to(
                common_git_dir,
                format,
                &result,
                true,
                options.expire_to.as_deref().map(Path::new),
            )?;
            // Upstream's `repack --cruft` vacuums loose unreachable objects
            // into the cruft side as well, so expired ones never survive a gc.
            // Our cruft engine only classifies packed candidates; expire the
            // stale loose remainder here (recent ones fail closed toward
            // preservation inside gc_prune_expired_loose).
            if let Some(spec) = prune_expire.as_deref() {
                let expire = parse_prune_expire(spec, "gc.pruneExpire")?;
                gc_prune_expired_loose(common_git_dir, format, &roots, expire)?;
            }
        }
        sley_odb::GcRepackMode::Reachable => {
            // gc.cruftPacks=false: repack reachable objects, then prune loose
            // unreachable objects older than gc.pruneExpire/--prune.
            let filtered_repack = config.get("gc", None, "repackFilter") == Some("blob:none");
            if filtered_repack {
                gc_repack_blob_none_filter(
                    common_git_dir,
                    format,
                    &roots,
                    options.expire_to.as_deref(),
                    config.get("gc", None, "repackFilterTo"),
                )?;
            } else {
                let repack_options = sley_odb::RepackOptions {
                    local: true,
                    force_rewrite: false,
                    pack_kept_objects: false,
                    keep_pack_stems: keep_pack_stems.clone(),
                };
                if let Some(result) = sley_odb::repack_reachable_objects_with_options(
                    common_git_dir,
                    format,
                    &roots,
                    &repack_options,
                )? {
                    gc_unpack_recent_unreachable_from_repack(
                        common_git_dir,
                        format,
                        &roots,
                        prune_expire.as_deref(),
                        &keep_pack_stems,
                        &result,
                    )?;
                    sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
                }
            }
            if filtered_repack {
                return Ok(());
            }
            if let Some(spec) = prune_expire.as_deref() {
                let expire = parse_prune_expire(spec, "gc.pruneExpire")?;
                gc_prune_expired_loose(common_git_dir, format, &roots, expire)?;
            } else {
                let expire = parse_prune_expire(
                    config
                        .get("gc", None, "pruneExpire")
                        .unwrap_or("2.weeks.ago"),
                    "gc.pruneExpire",
                )?;
                gc_pack_recent_unreachable_loose(common_git_dir, format, &roots, expire)?;
            }
        }
    }

    let store = FileRefStore::new(common_git_dir, format)
        .with_reftable_lock_timeout_millis(services.reftable_lock_timeout);
    if options.auto && store.uses_reftable()? && store.reftable_table_count()? > 2 {
        store.compact_reftable_stack()?;
    }
    if let Some(result) = sley_odb::repack_promisor_objects(common_git_dir, format)? {
        sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
    }
    gc_clean_pack_garbage(&repository_objects_dir(common_git_dir).join("pack"))?;
    (services.update_server_info)()?;
    if gc_write_commit_graph(config) {
        let progress = if gc_progress_requested(options) {
            "--progress"
        } else {
            "--no-progress"
        };
        trace2::child_start(&["commit-graph", "write", "--reachable", progress]);
        (services.commit_graph_write_reachable)(progress == "--progress")?;
    }
    if options.auto && gc_too_many_loose_objects(common_git_dir, format, config)? {
        eprintln!(
            "warning: There are too many unreachable loose objects; run 'git prune' to remove them."
        );
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAutoMode {
    Full,
    Incremental,
}

pub fn gc_auto_mode(
    common_git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Option<GcAutoMode>> {
    if gc_config_i64(config, "auto").unwrap_or(6700) <= 0 {
        return Ok(None);
    }
    if gc_too_many_packs(common_git_dir, config)? {
        Ok(Some(GcAutoMode::Full))
    } else if gc_too_many_loose_objects(common_git_dir, format, config)? {
        Ok(Some(GcAutoMode::Incremental))
    } else {
        Ok(None)
    }
}

fn gc_too_many_packs(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = gc_config_i64(config, "autoPackLimit").unwrap_or(50);
    if limit <= 0 {
        return Ok(false);
    }
    let mut count = 0i64;
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    if let Ok(entries) = fs::read_dir(pack_dir) {
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("pack")
                && !path.with_extension("keep").exists()
            {
                count += 1;
            }
        }
    }
    Ok(count > limit)
}

fn gc_too_many_loose_objects(
    common_git_dir: &Path,
    _format: ObjectFormat,
    config: &GitConfig,
) -> Result<bool> {
    let limit = gc_config_i64(config, "auto").unwrap_or(6700);
    if limit <= 0 {
        return Ok(false);
    }
    let threshold = ((limit + 255) / 256) * 256;
    let sampled = gc_loose_fanout_count(common_git_dir, "17")?.saturating_mul(256);
    Ok(sampled > threshold as u64)
}

fn gc_loose_fanout_count(common_git_dir: &Path, fanout: &str) -> Result<u64> {
    let dir = repository_objects_dir(common_git_dir).join(fanout);
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

pub fn gc_before_repack(
    services: &mut GcServices,
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<()> {
    if gc_pack_refs(config, common_git_dir)? {
        crate::trace_line(services, "builtin/gc.c:0", "trace: built-in: git pack-refs --all --prune");
        (services.pack_refs_all_prune)()?;
    }
    let reflog_expire_never = is_config_never(config, "gc", "reflogExpire");
    let reflog_unreachable_never = is_config_never(config, "gc", "reflogExpireUnreachable");
    if !(reflog_expire_never && reflog_unreachable_never) {
        let mut expire_args = vec!["--all".to_string()];
        if reflog_expire_never {
            expire_args.push("--expire=never".to_string());
        }
        if reflog_unreachable_never {
            expire_args.push("--expire-unreachable=never".to_string());
        }
        crate::trace_line(
            services,
            "builtin/gc.c:0",
            &format!(
                "trace: built-in: git reflog expire {}",
                expire_args.join(" ")
            ),
        );
        // Upstream aborts the run (FAILED_RUN) when reflog expire fails;
        // swallowing here let gc report success while reflogs were stale.
        (services.reflog_expire)(&expire_args)?;
    }
    let _ = (git_dir, format);
    Ok(())
}

fn gc_pack_refs(config: &GitConfig, common_git_dir: &Path) -> Result<bool> {
    if let Some(value) = config.get("gc", None, "packRefs")
        && value.eq_ignore_ascii_case("notbare")
    {
        return Ok(sley_worktree::worktree_root_for_git_dir(common_git_dir)?.is_some());
    }
    Ok(config.get_bool("gc", None, "packRefs").unwrap_or(true))
}

fn gc_keep_pack_stems(
    common_git_dir: &Path,
    config: &GitConfig,
    options: &GcOptions,
) -> Result<HashSet<String>> {
    if options.keep_largest_pack == Some(false) {
        return Ok(HashSet::new());
    }
    if options.keep_largest_pack == Some(true) {
        return Ok(gc_largest_pack_stem(common_git_dir)?.into_iter().collect());
    }
    let Some(threshold) = gc_config_u64(config, "bigPackThreshold") else {
        return Ok(HashSet::new());
    };
    gc_pack_stems_at_least(common_git_dir, threshold)
}

fn gc_repack_blob_none_filter(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cli_expire_to: Option<&str>,
    config_filter_to: Option<&str>,
) -> Result<()> {
    let before = gc_pack_stems(common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let destination = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let excluded = HashSet::new();
    let installed = sley_odb::build_and_install_reachable_pack_filtered(
        &db,
        &destination,
        format,
        roots.iter().copied(),
        &excluded,
        sley_odb::RawPackInstallOptions::default(),
        Some(sley_odb::PackObjectFilter::BlobNone),
        None,
    )?;

    let filter_to = config_filter_to.or(cli_expire_to);
    if let Some(filter_to) = filter_to {
        gc_write_filtered_blobs(common_git_dir, format, roots, filter_to)?;
        let keep = installed.map(|pack| pack.pack_name).unwrap_or_default();
        gc_remove_pack_stems(common_git_dir, &before, &keep)?;
    }
    Ok(())
}

fn gc_write_filtered_blobs(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    filter_to: &str,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let reachable = collect_reachable_object_ids(&db, format, roots.iter().copied())?;
    let mut objects = Vec::new();
    for oid in reachable {
        let object = match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Blob => object,
            _ => continue,
        };
        objects.push((oid, object));
    }
    if objects.is_empty() {
        return Ok(());
    }
    objects.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let inputs = objects
        .iter()
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect::<Vec<_>>();
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let object_dir = gc_filter_to_object_dir(filter_to)?;
    FileObjectDatabase::new(object_dir, format).install_pack(&written)?;
    Ok(())
}

fn gc_filter_to_object_dir(filter_to: &str) -> Result<PathBuf> {
    let path = PathBuf::from(filter_to);
    let pack_dir = path
        .parent()
        .ok_or_else(|| GitError::InvalidPath(format!("invalid filter-to path '{filter_to}'")))?;
    let object_dir = pack_dir
        .parent()
        .ok_or_else(|| GitError::InvalidPath(format!("invalid filter-to path '{filter_to}'")))?;
    Ok(object_dir.to_path_buf())
}

fn gc_unpack_recent_unreachable_from_repack(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    prune_expire: Option<&str>,
    kept_pack_stems: &HashSet<String>,
    result: &sley_odb::RepackResult,
) -> Result<()> {
    // `--no-prune` (None) and `--prune=never` (i64::MIN) both mean every
    // unreachable object must survive: upstream runs `repack -A -d` without an
    // expiration, loosening ALL unreachable packed objects. The loosening here
    // is exactly what keeps them alive when `install_repack_result(prune=true)`
    // removes their source packs below — so these modes loosen everything
    // instead of early-returning.
    let expire = match prune_expire {
        None => None,
        Some(spec) => {
            let parsed = parse_prune_expire(spec, "gc.pruneExpire")?;
            if parsed == i64::MIN {
                None
            } else {
                Some(parsed)
            }
        }
    };

    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let new_index = sley_pack::PackIndex::parse(&result.idx, format)?;
    let newly_packed: HashSet<ObjectId> = new_index
        .entries
        .into_iter()
        .map(|entry| entry.oid)
        .collect();

    let mut preserve = match expire {
        None => {
            // Everything packed that the new pack and the retained packs
            // (bigPackThreshold / --keep-largest-pack) do not already carry is
            // unreachable and must be materialized loose before its source
            // pack is removed.
            let mut unreachable =
                sley_odb::packed_object_ids(repository_objects_dir(common_git_dir), format)?;
            let pack_dir = repository_objects_dir(common_git_dir).join("pack");
            for stem in kept_pack_stems {
                let Ok(bytes) = fs::read(pack_dir.join(format!("{stem}.idx"))) else {
                    continue;
                };
                if let Ok(index) = sley_pack::PackIndex::parse(&bytes, format) {
                    for entry in index.entries {
                        unreachable.remove(&entry.oid);
                    }
                }
            }
            unreachable
                .into_iter()
                .filter(|oid| !newly_packed.contains(oid))
                .collect::<Vec<_>>()
        }
        Some(expire) => {
            let mut preserve_roots = roots.to_vec();
            preserve_roots.extend(prune_recent_object_roots(
                &db,
                common_git_dir,
                format,
                expire,
            )?);
            preserve_roots.extend(prune_recent_hook_roots(common_git_dir, format)?);
            preserve_roots.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            preserve_roots.dedup();

            sley_odb::collect_reachable_object_ids_tolerating_missing(
                &db,
                format,
                preserve_roots,
            )?
            .into_iter()
            .filter(|oid| !newly_packed.contains(oid))
            .collect::<Vec<_>>()
        }
    };
    preserve.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    preserve.dedup();

    for oid in preserve {
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(_) => continue,
        };
        db.loose().write_object((*object).clone())?;
    }
    Ok(())
}

fn gc_pack_stems(common_git_dir: &Path) -> Result<HashSet<String>> {
    Ok(gc_non_cruft_pack_stems(common_git_dir)?
        .into_iter()
        .map(|(stem, _)| stem)
        .collect())
}

fn gc_remove_pack_stems(
    common_git_dir: &Path,
    stems: &HashSet<String>,
    keep_stem: &str,
) -> Result<()> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    for stem in stems {
        if stem == keep_stem {
            continue;
        }
        for ext in ["pack", "idx", "rev", "bitmap"] {
            let path = pack_dir.join(format!("{stem}.{ext}"));
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    Ok(())
}

fn gc_largest_pack_stem(common_git_dir: &Path) -> Result<Option<String>> {
    let mut best: Option<(u64, String)> = None;
    for (stem, size) in gc_non_cruft_pack_stems(common_git_dir)? {
        if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
            best = Some((size, stem));
        }
    }
    Ok(best.map(|(_, stem)| stem))
}

fn gc_pack_stems_at_least(common_git_dir: &Path, threshold: u64) -> Result<HashSet<String>> {
    let mut stems = HashSet::new();
    for (stem, size) in gc_non_cruft_pack_stems(common_git_dir)? {
        if size >= threshold {
            stems.insert(stem);
        }
    }
    Ok(stems)
}

fn gc_non_cruft_pack_stems(common_git_dir: &Path) -> Result<Vec<(String, u64)>> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return Ok(Vec::new());
    };
    let mut packs = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pack")
            || path.with_extension("mtimes").exists()
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        packs.push((stem.to_string(), entry.metadata()?.len()));
    }
    Ok(packs)
}

pub fn gc_write_commit_graph(config: &GitConfig) -> bool {
    if env::var("GIT_TEST_COMMIT_GRAPH").ok().as_deref() == Some("0") {
        return false;
    }
    config
        .get_bool("gc", None, "writeCommitGraph")
        .or_else(|| config.get_bool("core", None, "commitGraph"))
        .unwrap_or(true)
}

pub fn gc_progress_requested(options: &GcOptions) -> bool {
    !options.quiet && env::var("GIT_PROGRESS_DELAY").ok().as_deref() == Some("0")
}

pub fn gc_should_detach(config: &GitConfig, detach: Option<bool>) -> bool {
    detach.unwrap_or_else(|| config.get_bool("gc", None, "autoDetach").unwrap_or(true))
}

pub fn gc_recent_log_blocks_auto(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let path = common_git_dir.join("gc.log");
    let Ok(metadata) = fs::metadata(&path) else {
        return Ok(false);
    };
    if metadata.len() == 0 {
        return Ok(false);
    }
    let expiry = config.get("gc", None, "logExpiry").unwrap_or("1.day.ago");
    let cutoff = parse_reflog_expire_time(expiry, "gc.logExpiry")?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    if modified >= cutoff {
        eprintln!(
            "warning: The last gc run reported the following. Please correct the root cause\nand remove {}\nAutomatic cleanup will not be performed until the file is removed.\n\n{}",
            path.display(),
            fs::read_to_string(&path).unwrap_or_default()
        );
        Ok(true)
    } else {
        let _ = fs::remove_file(path);
        Ok(false)
    }
}

/// `gc.pid` staleness window for files whose holder cannot be probed (no
/// parsable pid, or no liveness facility on this platform).
const GC_PID_STALE_SECONDS: u64 = 12 * 60 * 60;

/// Outcome of attempting to take the `gc.pid` repository lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcPidLock {
    /// This process now owns `gc.pid`; it must remove it on every exit path.
    Acquired,
    /// Another (live) gc holds the lock.
    Held,
}

/// Acquire the `gc.pid` lock in one step via `O_EXCL` creation, so the old
/// check-then-write window (two gcs could pass the staleness probe and both
/// proceed to concurrent `-d` repacks) cannot occur.
///
/// A pre-existing `gc.pid` is honored only while its recorded pid is alive;
/// a dead holder (crashed gc) is recovered immediately instead of blocking
/// manual gc runs for the full twelve-hour staleness window. Files without a
/// parsable pid fall back to the mtime window.
pub fn gc_acquire_pid_lock(common_git_dir: &Path) -> Result<GcPidLock> {
    let path = common_git_dir.join("gc.pid");
    match gc_try_create_pid_file(&path) {
        Ok(()) => Ok(GcPidLock::Acquired),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            if !gc_pid_lock_abandoned(&path) {
                return Ok(GcPidLock::Held);
            }
            // The recorded holder is gone: recover. If another gc wins the
            // recovery race between our remove and re-create, its fresh
            // create-new failure reports Held.
            let _ = fs::remove_file(&path);
            if gc_try_create_pid_file(&path).is_ok() {
                Ok(GcPidLock::Acquired)
            } else {
                Ok(GcPidLock::Held)
            }
        }
        Err(err) => Err(err.into()),
    }
}

fn gc_try_create_pid_file(path: &Path) -> io::Result<()> {
    use io::Write as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let host = env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
    writeln!(file, "{} {host}", std::process::id())
}

/// True when the existing `gc.pid` can be treated as abandoned: it records a
/// dead pid, or (unparsable content) is older than [`GC_PID_STALE_SECONDS`].
fn gc_pid_lock_abandoned(path: &Path) -> bool {
    if let Ok(contents) = fs::read_to_string(path)
        && let Some(pid_text) = contents.split_whitespace().next()
        && let Ok(pid) = pid_text.parse::<u32>()
        && pid != 0
    {
        return !gc_process_alive(pid);
    }
    gc_pid_file_age_seconds(path).is_some_and(|age| age >= GC_PID_STALE_SECONDS)
}

#[cfg(unix)]
fn gc_process_alive(pid: u32) -> bool {
    sley_procinfo::process_alive(pid)
}

#[cfg(not(unix))]
fn gc_process_alive(_pid: u32) -> bool {
    // No liveness facility: fail toward "held" and honor the staleness window.
    true
}

fn gc_pid_file_age_seconds(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

fn trace_gc_repack(services: &GcServices, args: &[&str]) {
    crate::trace_line(
        services,
        "builtin/gc.c:0",
        &format!("trace: built-in: git {}", args.join(" ")),
    );
    trace2::child_start(args);
}

pub fn gc_config_i64(config: &GitConfig, key: &str) -> Option<i64> {
    // Typed classifier replaces the plain `str::parse` composition; git's
    // integer grammar (units, hex/octal) now applies, matching
    // `git_config_int`. Invalid values stay silently ignored here.
    sley_config::typed::classify_config_int(config.get("gc", None, key)?).ok()
}

pub fn gc_config_u64(config: &GitConfig, key: &str) -> Option<u64> {
    // Typed classifier replaces the hand-rolled digit-scan (`parse_gc_size`)
    // for config reads: units plus hex/octal per git_parse_unsigned, no sign.
    sley_config::typed::classify_config_size(config.get("gc", None, key)?).ok()
}

pub fn parse_gc_size(value: &str) -> Result<u64> {
    let (digits, suffix) = value.trim().split_at(
        value
            .trim()
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(value.trim().len()),
    );
    let mut size = digits
        .parse::<u64>()
        .map_err(|_| GitError::Command(format!("bad numeric config value '{value}'")))?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1024,
        "m" => 1024 * 1024,
        "g" => 1024 * 1024 * 1024,
        _ => {
            return Err(GitError::Command(format!(
                "bad numeric config value '{value}'"
            )));
        }
    };
    size = size.saturating_mul(multiplier);
    Ok(size)
}

fn gc_prune_expired_loose(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    expire: i64,
) -> Result<()> {
    if expire == i64::MIN {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut prune_roots = roots.to_vec();
    prune_roots.extend(prune_recent_object_roots(
        &db,
        common_git_dir,
        format,
        expire,
    )?);
    prune_roots.extend(prune_recent_hook_roots(common_git_dir, format)?);
    prune_roots.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    prune_roots.dedup();

    for oid in sley_odb::prune_unreachable_loose_tolerating_missing(
        common_git_dir,
        format,
        prune_roots,
        false,
    )? {
        if !prune_object_is_expired(&db, &oid, expire)? {
            continue;
        }
        let path = db.loose().object_path(&oid)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    prune_packed_loose_objects(common_git_dir, format, false)?;
    prune_empty_loose_object_dirs(&common_git_dir.join("objects"))?;
    Ok(())
}

fn gc_pack_recent_unreachable_loose(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    expire: i64,
) -> Result<()> {
    if expire == i64::MIN {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut objects = Vec::new();
    for oid in sley_odb::prune_unreachable_loose_tolerating_missing(
        common_git_dir,
        format,
        roots.to_vec(),
        false,
    )? {
        if prune_object_is_expired(&db, &oid, expire)? {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(_) => continue,
        };
        objects.push((oid, object));
    }
    if objects.is_empty() {
        return Ok(());
    }

    let inputs: Vec<_> = objects
        .iter()
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect();
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let _install = db.install_written_pack(&written)?;
    for (oid, _) in objects {
        let path = db.loose().object_path(&oid)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    prune_empty_loose_object_dirs(&common_git_dir.join("objects"))?;
    Ok(())
}

fn gc_clean_pack_garbage(pack_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return Ok(());
    };
    let mut stems: BTreeMap<String, CountPackStem> = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let stem = stem.to_string();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("pack") => stems.entry(stem).or_default().pack = Some(path),
            Some("idx") => stems.entry(stem).or_default().idx = Some(path),
            Some("keep") => stems.entry(stem).or_default().keep = Some(path),
            _ => {}
        }
    }
    for stem in stems.values() {
        if stem.pack.is_some() {
            continue;
        }
        if let Some(idx) = &stem.idx {
            remove_pack_garbage_file(idx)?;
            if let Some(keep) = &stem.keep {
                remove_pack_garbage_file(keep)?;
            }
        }
    }
    Ok(())
}

fn gc_remove_cruft_packs(common_git_dir: &Path) -> Result<()> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(());
    };
    let mut stems = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("mtimes")
            && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        {
            stems.push(stem.to_string());
        }
    }
    for stem in stems {
        for ext in ["pack", "idx", "rev", "mtimes", "bitmap"] {
            let path = pack_dir.join(format!("{stem}.{ext}"));
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    Ok(())
}

fn remove_pack_garbage_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

pub fn validate_gc_prune_expire(config: &GitConfig, git_dir: &Path) -> Result<()> {
    let Some(value) = config.get("gc", None, "pruneExpire") else {
        return Ok(());
    };
    if crate::repack::parse_cruft_expiration(value).is_ok() {
        return Ok(());
    }
    eprintln!("error: Invalid gc.pruneexpire: '{value}'");
    let config_path = git_dir.join("config");
    let line = config_line_number(&config_path, "pruneExpire").unwrap_or(0);
    eprintln!(
        "fatal: bad config variable 'gc.pruneexpire' in file '{}' at line {line}",
        display_git_config_path(git_dir, &config_path)
    );
    Err(GitError::Exit(128))
}

fn config_line_number(path: &Path, key: &str) -> Option<usize> {
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .position(|line| {
            line.trim_start()
                .split(['=', ' ', '\t'])
                .next()
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(key))
        })
        .map(|index| index + 1)
}

fn display_git_config_path(git_dir: &Path, config_path: &Path) -> String {
    if git_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".git")
        && let Some(parent) = git_dir.parent()
        && env::current_dir().is_ok_and(|cwd| cwd == parent)
    {
        return ".git/config".to_string();
    }
    config_path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod pid_lock_tests {
    use super::*;
    use std::process::Command;

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "sley-gc-pidlock-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A pid that is guaranteed dead: reap a spawned process, then reuse its id.
    fn reaped_pid() -> Option<u32> {
        let mut child = Command::new("sleep").arg("30").spawn().ok()?;
        let pid = child.id();
        child.kill().ok()?;
        child.wait().ok()?;
        Some(pid)
    }

    #[test]
    fn acquire_creates_pid_file_and_blocks_second_acquire() {
        let dir = temp_dir("acquire");
        assert_eq!(gc_acquire_pid_lock(&dir), Ok(GcPidLock::Acquired));
        let contents = fs::read_to_string(dir.join("gc.pid")).expect("read gc.pid");
        let expected = std::process::id().to_string();
        assert_eq!(contents.split_whitespace().next(), Some(expected.as_str()));
        assert_eq!(gc_acquire_pid_lock(&dir), Ok(GcPidLock::Held));
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn dead_pid_holder_is_recovered_immediately() {
        let Some(dead) = reaped_pid() else {
            return;
        };
        let dir = temp_dir("dead-pid");
        fs::write(dir.join("gc.pid"), format!("{dead} crashed-host\n"))
            .expect("write foreign gc.pid");
        // A freshly-written file with a dead pid must not wait out the 12h
        // mtime window.
        assert_eq!(gc_acquire_pid_lock(&dir), Ok(GcPidLock::Acquired));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_pid_holder_blocks_even_when_file_is_fresh() {
        let mut child = match Command::new("sleep").arg("60").spawn() {
            Ok(child) => child,
            Err(_) => return,
        };
        let live = child.id();
        let dir = temp_dir("live-pid");
        fs::write(dir.join("gc.pid"), format!("{live} live-host\n")).expect("write gc.pid");
        assert_eq!(gc_acquire_pid_lock(&dir), Ok(GcPidLock::Held));
        assert!(dir.join("gc.pid").exists(), "foreign lock must be untouched");
        fs::remove_dir_all(&dir).ok();
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn unparsable_pid_falls_back_to_staleness_window() {
        let dir = temp_dir("unparsable");
        fs::write(dir.join("gc.pid"), b"not-a-pid\n").expect("write fresh gc.pid");
        assert_eq!(gc_acquire_pid_lock(&dir), Ok(GcPidLock::Held));

        // Backdate past the window: recoverable.
        let path = dir.join("gc.pid");
        let stale = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(GC_PID_STALE_SECONDS + 60))
            .expect("stale timestamp");
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open gc.pid")
            .set_modified(stale)
            .expect("backdate gc.pid");
        assert_eq!(gc_acquire_pid_lock(&dir), Ok(GcPidLock::Acquired));
        fs::remove_dir_all(&dir).ok();
    }
}
