//! Repack planning and strategy selection: preferred bitmap tips,
//! bitmapPseudoMerge groups, traversal roots, object filters, geometric and
//! cruft repack execution, and dumb-transport server-info fast paths.
//!
//! Extracted verbatim from the CLI command tier; porcelain (argv parsing,
//! usage rendering) stays behind in `sley-cli`.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::Path;

use regex::Regex;
use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::ObjectType;
use sley_odb::ObjectReader as _;
use sley_odb::{FileObjectDatabase, repository_objects_dir};
use sley_pack::{MultiPackIndex, PackWriteOptions};
use sley_refs::{FileRefStore, RefTarget};

use crate::gc::parse_gc_size;
use crate::midx;
use crate::prune::{
    prune_head_root, prune_packed_loose_objects, prune_recent_hook_roots,
    prune_repack_shallow_file, prune_worktree_git_dirs, reflog_roots_from_dir,
};
use crate::trace2::{self, perf_data};
use crate::{
    GcServices, common_git_dir_for_git_dir, read_repo_config, repo_object_format,
    resolve_ref_to_oid, resolve_revision,
};

/// The commit oids that get bitmap selection preference, mirroring upstream's
/// `NEEDS_BITMAP` marking: tips of refs under the `pack.preferBitmapTips`
/// hierarchies (each config value names a ref prefix, normalised to end with
/// `/`), peeled to commits. Empty when the config is unset.
pub(crate) fn repack_preferred_bitmap_tips(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<HashSet<ObjectId>> {
    let config = read_repo_config(git_dir)?;
    let mut prefixes: Vec<String> = Vec::new();
    for value in config.get_all("pack", None, "preferBitmapTips") {
        let Some(prefix) = value else {
            // A bare `[pack] preferBitmapTips` key: git reports the missing
            // value but continues the repack (string_list config callback).
            eprintln!("error: missing value for 'pack.preferbitmaptips'");
            continue;
        };
        if prefix.ends_with('/') {
            prefixes.push(prefix.to_string());
        } else {
            prefixes.push(format!("{prefix}/"));
        }
    }
    let mut tips = HashSet::new();
    if prefixes.is_empty() {
        return Ok(tips);
    }
    let store = FileRefStore::new(git_dir, format);
    for reference in store.list_refs()? {
        if !prefixes
            .iter()
            .any(|prefix| reference.name.starts_with(prefix))
        {
            continue;
        }
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        if let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) {
            tips.insert(commit);
        }
    }
    Ok(tips)
}

#[derive(Clone)]
struct PseudoMergeCandidate {
    oid: ObjectId,
    date: i64,
}

#[derive(Default)]
struct PseudoMergeMatches {
    stable: Vec<PseudoMergeCandidate>,
    unstable: Vec<PseudoMergeCandidate>,
}

struct PseudoMergeConfigBuilder {
    pattern: Option<String>,
    decay: f64,
    max_merges: usize,
    sample_rate: f64,
    threshold: i64,
    stable_threshold: i64,
    stable_size: usize,
}

struct PseudoMergeConfig {
    name: String,
    pattern: Regex,
    capture_count: usize,
    decay: f64,
    max_merges: usize,
    sample_rate: f64,
    threshold: i64,
    stable_threshold: i64,
    stable_size: usize,
}

impl PseudoMergeConfigBuilder {
    fn new() -> Result<Self> {
        Ok(Self {
            pattern: None,
            decay: 1.0,
            max_merges: 64,
            sample_rate: 1.0,
            threshold: parse_pseudo_merge_expiry("1.week.ago")?,
            stable_threshold: parse_pseudo_merge_expiry("1.month.ago")?,
            stable_size: 512,
        })
    }
}

fn parse_pseudo_merge_expiry(value: &str) -> Result<i64> {
    let timestamp = sley_core::date::approxidate::parse_expiry_date(value)
        .ok_or_else(|| GitError::Command(format!("invalid timestamp '{value}'")))?;
    let unsigned = timestamp as u64;
    Ok(if unsigned >= i64::MAX as u64 {
        i64::MAX
    } else {
        unsigned as i64
    })
}

fn load_pseudo_merge_configs(git_dir: &Path) -> Result<Vec<PseudoMergeConfig>> {
    let config = read_repo_config(git_dir)?;
    let mut builders: BTreeMap<String, PseudoMergeConfigBuilder> = BTreeMap::new();
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("bitmapPseudoMerge") {
            continue;
        }
        let Some(name) = section.subsection.as_ref() else {
            continue;
        };
        for entry in &section.entries {
            let builder = match builders.entry(name.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(PseudoMergeConfigBuilder::new()?)
                }
            };
            let value = entry.value.as_deref().unwrap_or("");
            if entry.key.eq_ignore_ascii_case("pattern") {
                builder.pattern = Some(value.to_string());
            } else if entry.key.eq_ignore_ascii_case("decay") {
                if let Ok(decay) = value.trim().parse::<f64>()
                    && decay >= 0.0
                {
                    builder.decay = decay;
                }
            } else if entry.key.eq_ignore_ascii_case("sampleRate") {
                if let Ok(sample_rate) = value.trim().parse::<f64>()
                    && (0.0..=1.0).contains(&sample_rate)
                {
                    builder.sample_rate = sample_rate;
                }
            } else if entry.key.eq_ignore_ascii_case("threshold") {
                builder.threshold = parse_pseudo_merge_expiry(value)?;
            } else if entry.key.eq_ignore_ascii_case("maxMerges") {
                if let Some(max_merges) = sley_config::parse_config_int(value)
                    && max_merges >= 0
                {
                    builder.max_merges = max_merges as usize;
                }
            } else if entry.key.eq_ignore_ascii_case("stableThreshold") {
                builder.stable_threshold = parse_pseudo_merge_expiry(value)?;
            } else if entry.key.eq_ignore_ascii_case("stableSize")
                && let Some(stable_size) = sley_config::parse_config_int(value)
                && stable_size > 0
            {
                builder.stable_size = stable_size as usize;
            }
        }
    }

    let mut groups = Vec::new();
    for (name, builder) in builders {
        if builder.threshold < builder.stable_threshold {
            eprintln!(
                "fatal: pseudo-merge group '{name}' has unstable threshold before stable one"
            );
            return Err(GitError::Exit(128));
        }
        let Some(pattern) = builder.pattern else {
            eprintln!("fatal: pseudo-merge group '{name}' missing required pattern");
            return Err(GitError::Exit(128));
        };
        let anchored = if pattern.starts_with('^') {
            pattern
        } else {
            format!("^{pattern}")
        };
        let regex = Regex::new(&anchored).map_err(|_| {
            GitError::Command(format!(
                "failed to load pseudo-merge regex for {name}: '{anchored}'"
            ))
        })?;
        groups.push(PseudoMergeConfig {
            name,
            capture_count: regex.captures_len().saturating_sub(1),
            pattern: regex,
            decay: builder.decay,
            max_merges: builder.max_merges,
            sample_rate: builder.sample_rate,
            threshold: builder.threshold,
            stable_threshold: builder.stable_threshold,
            stable_size: builder.stable_size,
        });
    }
    Ok(groups)
}

fn pseudo_merge_match_key(config: &PseudoMergeConfig, refname: &str) -> Option<String> {
    let captures = config.pattern.captures(refname)?;
    let mut parts = Vec::new();
    if config.capture_count == 0 {
        if let Some(full) = captures.get(0) {
            parts.push(full.as_str());
        }
    } else {
        for index in 1..=config.capture_count {
            if let Some(capture) = captures.get(index) {
                parts.push(capture.as_str());
            }
        }
    }
    Some(parts.join("-"))
}

fn push_pseudo_merge_candidate_groups(
    out: &mut Vec<sley_odb::BitmapPseudoMergeGroup>,
    commits: &[PseudoMergeCandidate],
    exclude_selected: bool,
    partition: Option<sley_odb::BitmapPseudoMergePartition>,
) {
    if commits.is_empty() {
        return;
    }
    out.push(sley_odb::BitmapPseudoMergeGroup {
        commits: commits.iter().map(|candidate| candidate.oid).collect(),
        exclude_selected,
        partition,
    });
}

pub(crate) fn repack_pseudo_merge_groups(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<Vec<sley_odb::BitmapPseudoMergeGroup>> {
    let configs = load_pseudo_merge_configs(git_dir)?;
    if configs.is_empty() {
        return Ok(Vec::new());
    }
    let mut matches: Vec<BTreeMap<String, PseudoMergeMatches>> =
        configs.iter().map(|_| BTreeMap::new()).collect();
    let store = FileRefStore::new(git_dir, format);
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let Ok(commit_oid) = sley_rev::peel_to_commit(db, format, &oid) else {
            continue;
        };
        let Ok(object) = db.read_object(&commit_oid) else {
            continue;
        };
        let Ok(commit) = sley_object::Commit::parse_ref(format, &object.body) else {
            continue;
        };
        let date = sley_rev::revlist::commit_identity_timestamp_i64(commit.committer).unwrap_or(0);
        for (index, config) in configs.iter().enumerate() {
            let Some(key) = pseudo_merge_match_key(config, &reference.name) else {
                continue;
            };
            let entry = matches[index].entry(key).or_default();
            let candidate = PseudoMergeCandidate {
                oid: commit_oid,
                date,
            };
            if date <= config.stable_threshold {
                entry.stable.push(candidate);
            } else if date <= config.threshold {
                entry.unstable.push(candidate);
            }
        }
    }

    let mut groups = Vec::new();
    for (config, group_matches) in configs.iter().zip(matches.iter_mut()) {
        let _ = &config.name;
        for entry in group_matches.values_mut() {
            entry.stable.sort_by_key(|candidate| candidate.date);
            entry.unstable.sort_by_key(|candidate| candidate.date);

            for chunk in entry.stable.chunks(config.stable_size) {
                push_pseudo_merge_candidate_groups(&mut groups, chunk, false, None);
            }

            if !entry.unstable.is_empty() && config.max_merges > 0 {
                push_pseudo_merge_candidate_groups(
                    &mut groups,
                    &entry.unstable,
                    true,
                    Some(sley_odb::BitmapPseudoMergePartition {
                        max_merges: config.max_merges,
                        decay: config.decay,
                        sample_rate: config.sample_rate,
                    }),
                );
            }
        }
    }
    Ok(groups)
}

/// The traversal roots `repack -a` packs from, mirroring upstream's
/// `pack-objects --all --reflog --indexed-objects` invocation: every direct
/// ref target, `HEAD`, both sides of every reflog entry, and the blobs in the
/// index. Unresolvable roots are skipped (the closure walk also tolerates
/// missing objects — stale reflogs are expected).
///
/// Like upstream, this examines *every* linked worktree: each worktree's
/// `HEAD`, its index (cached/staged objects), and its own reflogs all anchor
/// reachability. Without them a `repack -a -d` would drop commits that only a
/// linked worktree's detached HEAD or staged files reference.
///
/// KNOWN GAP (documented, not fixed here): [`FileRefStore::list_refs`] walks
/// only the current worktree's ref storage, so per-worktree refs (e.g.
/// `refs/bisect/*` while a bisect is active in another worktree) are not
/// collected either; upstream protects those. Closing this needs a sley-refs
/// API for enumerating per-worktree ref stores — see review #232 (FIX-C M2).
pub(crate) fn repack_traversal_roots(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    let store = FileRefStore::new(git_dir, format);
    for reference in store.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target {
            roots.push(oid);
        }
    }
    if let Ok(head) = resolve_revision(git_dir, format, "HEAD", replace_objects) {
        roots.push(head);
    }
    roots.extend(reflog_traversal_roots(git_dir, common_git_dir, format)?);
    // Indexed objects (upstream `--indexed-objects`): cache entries, the
    // cache-tree extension, and resolve-undo blobs all keep pending objects
    // alive across a repack (t7700 "pending objects are repacked appropriately").
    roots.extend(index_traversal_roots(&git_dir.join("index"), format)?);
    // Linked worktrees: upstream pack-objects examines every worktree's HEAD,
    // index, and per-worktree reflogs by default. The current worktree is
    // covered above (with replacement-aware HEAD resolution); the common dir
    // and every worktrees/<id> admin dir get the raw treatment here.
    for worktree_git_dir in prune_worktree_git_dirs(git_dir, common_git_dir)? {
        if worktree_git_dir == git_dir {
            continue;
        }
        if let Some(oid) = prune_head_root(&store, &worktree_git_dir, format)? {
            roots.push(oid);
        }
        roots.extend(index_traversal_roots(
            &worktree_git_dir.join("index"),
            format,
        )?);
        roots.extend(reflog_roots_from_dir(
            &worktree_git_dir.join("logs"),
            format,
        )?);
    }
    Ok(roots)
}

fn index_traversal_roots(index_path: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    let bytes = match fs::read(index_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(roots),
        Err(error) => return Err(error.into()),
    };
    let index = sley_index::Index::parse(&bytes, format)?;
    for entry in &index.entries {
        roots.push(entry.oid);
    }
    if let Some(cache_tree) = index.cache_tree(format)? {
        collect_cache_tree_oids(&cache_tree, &mut roots);
    }
    for record in index.resolve_undo_records(format)? {
        for stage in record.stages.into_iter().flatten() {
            roots.push(stage.oid);
        }
    }
    Ok(roots)
}

fn collect_cache_tree_oids(tree: &sley_index::CacheTree, roots: &mut Vec<ObjectId>) {
    if let Some(oid) = tree.oid {
        roots.push(oid);
    }
    for child in &tree.subtrees {
        collect_cache_tree_oids(&child.tree, roots);
    }
}

fn parse_repack_object_filter(specs: &[String]) -> Result<Option<sley_odb::PackObjectFilter>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut filter = sley_odb::PackObjectFilter::BlobNone; // placeholder replaced below
    let mut started = false;
    for spec in specs {
        let parsed = parse_one_repack_filter(spec)?;
        filter = if started {
            combine_repack_filters(filter, parsed)
        } else {
            started = true;
            parsed
        };
    }
    Ok(Some(filter))
}

fn parse_one_repack_filter(spec: &str) -> Result<sley_odb::PackObjectFilter> {
    if spec == "blob:none" {
        return Ok(sley_odb::PackObjectFilter::BlobNone);
    }
    if let Some(value) = spec.strip_prefix("blob:limit=") {
        let limit = parse_gc_size(value)?;
        return Ok(sley_odb::PackObjectFilter::BlobLimit(limit));
    }
    if let Some(value) = spec.strip_prefix("tree:") {
        let depth: u32 = value
            .parse()
            .map_err(|_| GitError::Command(format!("invalid tree filter depth '{value}'")))?;
        return Ok(sley_odb::PackObjectFilter::TreeDepth(depth));
    }
    Err(GitError::Command(format!(
        "unsupported repack filter '{spec}'"
    )))
}

fn combine_repack_filters(
    left: sley_odb::PackObjectFilter,
    right: sley_odb::PackObjectFilter,
) -> sley_odb::PackObjectFilter {
    // Prefer TreeDepth when combining with BlobNone (tree:N already omits blobs).
    // For other pairs keep the more restrictive blob filter and tree depth.
    match (left, right) {
        (sley_odb::PackObjectFilter::BlobNone, sley_odb::PackObjectFilter::TreeDepth(d))
        | (sley_odb::PackObjectFilter::TreeDepth(d), sley_odb::PackObjectFilter::BlobNone) => {
            sley_odb::PackObjectFilter::TreeDepth(d)
        }
        (sley_odb::PackObjectFilter::BlobLimit(a), sley_odb::PackObjectFilter::BlobLimit(b)) => {
            sley_odb::PackObjectFilter::BlobLimit(a.min(b))
        }
        (sley_odb::PackObjectFilter::TreeDepth(a), sley_odb::PackObjectFilter::TreeDepth(b)) => {
            sley_odb::PackObjectFilter::TreeDepth(a.min(b))
        }
        (other, sley_odb::PackObjectFilter::BlobNone)
        | (sley_odb::PackObjectFilter::BlobNone, other) => other,
        (left, _) => left,
    }
}

fn reflog_traversal_roots(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut roots = reflog_roots_from_dir(&common_git_dir.join("logs"), format)?;
    if git_dir != common_git_dir {
        roots.extend(reflog_roots_from_dir(&git_dir.join("logs"), format)?);
    }
    Ok(roots)
}

/// Update dumb-transport metadata from the object types already established by
/// an all-into-one repack. Lightweight tags and ordinary refs need no object
/// body read. If any ref points outside the result or at an annotated tag, the
/// caller falls back to the generic update-server-info path, which performs the
/// full peel through the ODB.
fn repack_try_update_server_info_from_result(
    common_git_dir: &Path,
    format: ObjectFormat,
    result: &sley_odb::RepackResult,
) -> Result<bool> {
    let store = FileRefStore::new(common_git_dir, format);
    let refs = store.list_refs()?;
    let mut info_refs = Vec::with_capacity(refs.len() * (format.hex_len() + 32));
    for reference in refs {
        let oid = match &reference.target {
            RefTarget::Direct(oid) => *oid,
            RefTarget::Symbolic(_) => {
                let Some(oid) = resolve_ref_to_oid(&store, &reference.name)? else {
                    continue;
                };
                oid
            }
        };
        match result.cached_object_type(&oid) {
            Some(ObjectType::Tag) | None => return Ok(false),
            Some(ObjectType::Commit | ObjectType::Tree | ObjectType::Blob) => {}
        }
        info_refs.extend_from_slice(oid.to_hex().as_bytes());
        info_refs.push(b'\t');
        info_refs.extend_from_slice(reference.name.as_bytes());
        info_refs.push(b'\n');
    }

    let shared_repository = sley_formats::SharedRepositoryPermissions::from_git_dir(common_git_dir);
    let info_dir = common_git_dir.join("info");
    shared_repository.create_dir_all(&info_dir)?;
    repack_write_server_info_file(&info_dir.join("refs"), &info_refs, &shared_repository)?;

    let objects_dir = repository_objects_dir(common_git_dir);
    let objects_info_dir = objects_dir.join("info");
    shared_repository.create_dir_all(&objects_info_dir)?;
    let pack_dir = objects_dir.join("pack");
    let mut packs = Vec::new();
    if pack_dir.exists() {
        for entry in fs::read_dir(&pack_dir)? {
            let path = entry?.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("pack") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(hash) = name
                .strip_prefix("pack-")
                .and_then(|name| name.strip_suffix(".pack"))
            else {
                continue;
            };
            if ObjectId::from_hex(format, hash).is_ok() && path.with_extension("idx").is_file() {
                packs.push(name.to_string());
            }
        }
    }
    packs.sort();
    let mut info_packs = Vec::with_capacity(packs.len() * (format.hex_len() + 9));
    for name in packs {
        info_packs.extend_from_slice(b"P ");
        info_packs.extend_from_slice(name.as_bytes());
        info_packs.push(b'\n');
    }
    info_packs.push(b'\n');
    repack_write_server_info_file(
        &objects_info_dir.join("packs"),
        &info_packs,
        &shared_repository,
    )?;
    Ok(true)
}

fn repack_write_server_info_file(
    path: &Path,
    content: &[u8],
    shared_repository: &sley_formats::SharedRepositoryPermissions,
) -> Result<()> {
    if !fs::read(path).is_ok_and(|existing| existing == content) {
        fs::write(path, content)?;
    }
    shared_repository.adjust_file(path)
}
// --- section break: cruft/geometric helpers ---
pub(crate) fn validate_repack_cruft_numeric_config(config: &GitConfig) -> Result<()> {
    // Upstream forwards these values verbatim to `pack-objects
    // --window/--depth/--threads`, where parse-options enforces a signed
    // int32 with optional k/m/g units (hex/octal accepted, negatives legal):
    //   malformed  -> error: option `<opt>' expects an integer value with an
    //                 optional k/m/g suffix            (exit 129)
    //   overflow   -> error: value <v> for option `<opt>' not in range
    //                 [-2147483648,2147483647]        (exit 129)
    const OPTIONS: [(&str, &str); 3] = [
        ("cruftwindow", "window"),
        ("cruftdepth", "depth"),
        ("cruftthreads", "threads"),
    ];
    for (key, option) in OPTIONS {
        let Some(value) = config.get("repack", None, key) else {
            continue;
        };
        match sley_config::typed::classify_config_i32(value) {
            Ok(_) => {}
            Err(sley_config::typed::BadNumericKind::InvalidUnit) => {
                eprintln!(
                    "error: option `{option}' expects an integer value with an optional k/m/g suffix"
                );
                return Err(GitError::Exit(129));
            }
            Err(_) => {
                eprintln!(
                    "error: value {value} for option `{option}' not in range [-2147483648,2147483647]"
                );
                return Err(GitError::Exit(129));
            }
        }
    }
    Ok(())
}

pub fn parse_geometric_factor(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| GitError::Command(format!("cannot parse geometric factor: {value}")))
}

pub fn parse_repack_window(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid window size: {value}")))
}

pub fn resolve_cruft_pack_size(
    max_pack_size: Option<u64>,
    max_cruft_size: Option<u64>,
    configured_pack_size: Option<u64>,
) -> Option<u64> {
    max_cruft_size
        .filter(|size| *size > 0)
        .or_else(|| max_pack_size.filter(|size| *size > 0))
        .or_else(|| configured_pack_size.filter(|size| *size > 0))
}

pub fn strip_pack_suffix(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.strip_suffix(".pack")
        .or_else(|| base.strip_suffix(".idx"))
        .unwrap_or(base)
        .to_string()
}

#[allow(clippy::too_many_arguments)]
pub fn run_geometric(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    split_factor: u64,
    prune: bool,
    quiet: bool,
    write_midx: bool,
    write_bitmaps: bool,
    midx_must_contain_cruft: bool,
    keep_packs: &[String],
    _pack_kept_objects: bool,
) -> Result<()> {
    let kept_stems: HashSet<String> = keep_packs.iter().cloned().collect();
    let existing_midx_pack_names = read_ordinary_midx_pack_names(common_git_dir, format)?;
    let geometric = sley_odb::repack_geometric_with_options(
        common_git_dir,
        format,
        split_factor,
        &kept_stems,
        sley_odb::GeometricRepackOptions {
            follow_reachable: write_midx && !midx_must_contain_cruft,
        },
    )?;

    if geometric.result.is_none() {
        if !quiet {
            println!("Nothing new to pack.");
        }
        // With no new pack and no previous MIDX, Git conservatively includes
        // cruft because a reachable pack may refer into it. With an existing
        // MIDX, preserve its proven exclusion unless it names an unknown pack.
        if write_midx && pack_dir_has_packs(common_git_dir, format)? {
            let selection = sley_odb::geometric_repack_midx_selection(
                common_git_dir,
                &geometric,
                midx_must_contain_cruft,
                existing_midx_pack_names.as_ref(),
            )?;
            let mut midx_args = Vec::new();
            if write_bitmaps {
                midx_args.push("--bitmap".to_string());
            }
            midx::write_with_pack_names(
                Path::new("."),
                common_git_dir,
                &midx_args,
                Some(selection.pack_names),
            )?;
        }
        return Ok(());
    }

    // A geometric repack writes its bitmap through the MIDX (not a pack bitmap),
    // so only pass pack-bitmap tips when not writing a MIDX.
    let bitmap_tips = if write_bitmaps && !write_midx {
        let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
        Some(repack_preferred_bitmap_tips(common_git_dir, &db, format)?)
    } else {
        None
    };
    sley_odb::install_geometric_repack_result(
        common_git_dir,
        format,
        &geometric,
        prune,
        bitmap_tips.as_ref(),
    )?;

    if write_midx && pack_dir_has_packs(common_git_dir, format)? {
        let selection = sley_odb::geometric_repack_midx_selection(
            common_git_dir,
            &geometric,
            midx_must_contain_cruft,
            existing_midx_pack_names.as_ref(),
        )?;
        let mut midx_args: Vec<String> = Vec::new();
        if write_bitmaps {
            midx_args.push("--bitmap".to_string());
        }
        if let Some(preferred) = selection.preferred_pack_name {
            midx_args.push(format!("--preferred-pack={preferred}"));
        }
        midx::write_with_pack_names(
            Path::new("."),
            common_git_dir,
            &midx_args,
            Some(selection.pack_names),
        )?;
    }
    let _ = git_dir;
    Ok(())
}

/// Parse pack-objects' cruft cutoff. Unlike config expiry dates, the
/// `--cruft-expiration` and `--unpack-unreachable` callbacks use `approxidate`,
/// so `now` is the actual current timestamp and future-dated objects remain
/// recent. `all` retains the explicit expire-everything sentinel.
pub fn parse_cruft_expiration(spec: &str) -> Result<Option<u32>> {
    if matches!(spec, "never" | "false") {
        return Ok(None);
    }
    let ts = if spec == "all" {
        u64::MAX
    } else {
        sley_core::date::approxidate::parse_approxidate(spec)
            .ok_or_else(|| GitError::Command(format!("malformed expiration date '{spec}'")))?
            .max(0) as u64
    };
    Ok(if ts == 0 {
        None
    } else if ts >= u32::MAX as u64 {
        Some(u32::MAX)
    } else {
        Some(ts as u32)
    })
}

/// True when `<section>.<key>` is set to a "never"/"false" timestamp sentinel.
pub(crate) fn is_config_never(config: &GitConfig, section: &str, key: &str) -> bool {
    matches!(
        config.get(section, None, key),
        Some("never") | Some("false")
    )
}
/// `git repack --cruft [--cruft-expiration=<t>] [--expire-to=<dir>] [-d]`.
#[allow(clippy::too_many_arguments)]
pub fn run_cruft(
    replace_objects: bool,
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    prune: bool,
    local: bool,
    cruft_expiration: Option<u32>,
    expire_to: Option<&str>,
    write_midx: bool,
    keep_packs: &[String],
    pack_kept_objects: bool,
    max_pack_size: Option<u64>,
    cruft_window: usize,
    combine_cruft_below_size: Option<u64>,
) -> Result<()> {
    let roots = repack_traversal_roots(git_dir, common_git_dir, format, replace_objects)?;
    let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
    let options = sley_odb::RepackOptions {
        local,
        force_rewrite: false,
        pack_kept_objects,
        keep_pack_stems,
    };

    let cruft_options = sley_odb::CruftPackOptions {
        max_pack_size,
        combine_cruft_below_size,
        pack_write: PackWriteOptions::new().with_window(cruft_window),
    };
    let window_arg = format!("--window={}", cruft_options.pack_write.window);
    trace2::child_start(&["pack-objects", &window_arg, "--cruft"]);
    let result = repack_cruft_or_bad_object(repack_cruft_with_lazy_recent_hooks(
        common_git_dir,
        format,
        &roots,
        cruft_expiration,
        &options,
        &cruft_options,
    ))?;
    sley_odb::install_cruft_repack_result_with_expire_to(
        common_git_dir,
        format,
        &result,
        prune,
        expire_to.map(Path::new),
    )?;

    if write_midx && pack_dir_has_packs(common_git_dir, format)? {
        midx::write(Path::new("."), common_git_dir, &[])?;
    }
    Ok(())
}

/// Build a cruft result first to determine whether pack-objects has any cruft
/// candidates. Git does not invoke `gc.recentObjectsHook` for an empty cruft
/// side, so only enumerate configured roots when that preliminary plan contains
/// surviving or expired unreachable objects. Both passes are read-only; callers
/// install files only after the hook-backed result succeeds.
pub(crate) fn repack_cruft_with_lazy_recent_hooks(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cruft_expiration: Option<u32>,
    options: &sley_odb::RepackOptions,
    cruft_options: &sley_odb::CruftPackOptions,
) -> Result<sley_odb::CruftRepackResult> {
    let preliminary = sley_odb::repack_cruft_with_pack_options(
        common_git_dir,
        format,
        roots,
        cruft_expiration,
        options,
        cruft_options,
    )?;
    let has_cruft_candidates = preliminary.cruft.is_some()
        || !preliminary.additional_cruft.is_empty()
        || preliminary.expired.is_some();
    if cruft_expiration.is_none() || !has_cruft_candidates {
        return Ok(preliminary);
    }
    let recent_roots = prune_recent_hook_roots(common_git_dir, format)?;
    if recent_roots.is_empty() {
        return Ok(preliminary);
    }
    sley_odb::repack_cruft_with_pack_options_and_recent_roots(
        common_git_dir,
        format,
        roots,
        &recent_roots,
        cruft_expiration,
        options,
        cruft_options,
    )
}

pub(crate) fn repack_cruft_or_bad_object(
    result: Result<sley_odb::CruftRepackResult>,
) -> Result<sley_odb::CruftRepackResult> {
    match result {
        Ok(result) => Ok(result),
        Err(GitError::NotFound(kind)) => {
            if let Some(oid) = kind.object_id() {
                eprintln!("fatal: bad object {oid}");
                Err(GitError::Exit(128))
            } else {
                Err(GitError::NotFound(kind))
            }
        }
        Err(err) => Err(err),
    }
}

/// True when `objects/pack` holds at least one `.pack` file.
pub(crate) fn object_dir_has_alternates(common_git_dir: &Path) -> bool {
    if env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES").is_some() {
        return true;
    }
    repository_objects_dir(common_git_dir)
        .join("info")
        .join("alternates")
        .exists()
}

pub(crate) fn pack_dir_has_kept_packs(common_git_dir: &Path) -> Result<bool> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("keep") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn pack_dir_has_promisor_packs(common_git_dir: &Path) -> Result<bool> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("promisor") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn remove_pack_bitmap_sidecars(common_git_dir: &Path) -> Result<()> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("pack-") && name.ends_with(".bitmap") {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    Ok(())
}

fn pack_dir_has_packs(common_git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    let _ = format;
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("pack") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Snapshot the ordinary MIDX pack table before a repack mutates pack files.
/// The engine uses this to distinguish cruft which was already required for a
/// bitmap closure from cruft which a new follow-reachable pack can supersede.
pub(crate) fn read_ordinary_midx_pack_names(
    common_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<HashSet<String>>> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    let Ok(midx_bytes) = fs::read(&midx_path) else {
        return Ok(None);
    };
    let Ok(midx) = MultiPackIndex::parse(&midx_bytes, format) else {
        return Ok(None);
    };
    Ok(Some(midx.pack_names.into_iter().collect()))
}

/// Parsed `git repack` options. The CLI owns argv parsing and usage rendering;
/// this struct is the handoff into the engine.
#[derive(Debug, Default)]
pub struct RepackCommandOptions {
    pub prune: bool,
    pub quiet: bool,
    pub all: bool,
    pub unpack_unreachable: bool,
    pub unpack_unreachable_before: Option<Option<u32>>,
    pub keep_unreachable: bool,
    pub local: bool,
    pub write_bitmaps: Option<bool>,
    pub geometric: Option<u64>,
    pub write_midx: bool,
    pub keep_packs: Vec<String>,
    pub pack_kept_objects: bool,
    pub force_rewrite: bool,
    pub update_server_info: Option<bool>,
    pub cruft: bool,
    pub cruft_expiration: Option<Option<u32>>,
    pub expire_to: Option<String>,
    pub max_pack_size: Option<u64>,
    pub max_cruft_size: Option<u64>,
    pub combine_cruft_below_size: Option<u64>,
    pub window: Option<usize>,
    pub filter_specs: Vec<String>,
    pub filter_to: Option<String>,
    pub name_hash_version: Option<i32>,
}

/// Execute a parsed `git repack` invocation against one repository.
pub fn run_repack(
    services: &mut GcServices,
    git_dir: &Path,
    replace_objects: bool,
    options: &RepackCommandOptions,
) -> Result<()> {
    let RepackCommandOptions {
        prune,
        quiet,
        all,
        unpack_unreachable,
        unpack_unreachable_before,
        keep_unreachable,
        local,
        write_bitmaps,
        geometric,
        write_midx,
        keep_packs,
        pack_kept_objects,
        force_rewrite,
        update_server_info,
        cruft,
        cruft_expiration,
        expire_to,
        max_pack_size,
        max_cruft_size,
        combine_cruft_below_size,
        window,
        filter_specs,
        filter_to,
        name_hash_version,
    } = options;
    let prune = *prune;
    let quiet = *quiet;
    let all = *all;
    let unpack_unreachable = *unpack_unreachable;
    let unpack_unreachable_before = *unpack_unreachable_before;
    let keep_unreachable = *keep_unreachable;
    let local = *local;
    let write_bitmaps = *write_bitmaps;
    let geometric = *geometric;
    let write_midx = *write_midx;
    let pack_kept_objects = *pack_kept_objects;
    let force_rewrite = *force_rewrite;
    let update_server_info = *update_server_info;
    let cruft = *cruft;
    let cruft_expiration = *cruft_expiration;
    let expire_to = expire_to.clone();
    let max_pack_size = *max_pack_size;
    let max_cruft_size = *max_cruft_size;
    let combine_cruft_below_size = *combine_cruft_below_size;
    let window = *window;
    let filter_specs = filter_specs.clone();
    let filter_to = filter_to.clone();
    let name_hash_version = *name_hash_version;
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let format = repo_object_format(&common_git_dir)?;
    // `--path-walk` is accepted; selection still uses the same reachability walk
    let pack_filter = parse_repack_object_filter(&filter_specs)?;
    if filter_to.is_some() && pack_filter.is_none() {
        return Err(GitError::Command(
            "option '--filter-to' can only be used along with '--filter'".into(),
        ));
    }
    // `--name-hash-version` is accepted for CLI compatibility with git repack.
    // Sley repacks in-process (no pack-objects child), and the pack writer
    // does not implement version-specific delta name grouping. Bitmap
    // name-hash caches always use version 1 (`pack_name_hash`). Do not emit a
    // synthetic pack-objects TRACE2 child_start: that would claim a child argv
    // that never ran. Version 2 with bitmaps only warns, matching pack-objects.
    if let Some(version) = name_hash_version
        && !(1..=2).contains(&version)
    {
        eprintln!("fatal: invalid --name-hash-version option: {version}");
        return Err(GitError::Exit(128));
    }
    let config = read_repo_config(&common_git_dir)?;
    let repack_roots = if all {
        Some(repack_traversal_roots(
            git_dir,
            &common_git_dir,
            format,
            replace_objects,
        )?)
    } else {
        None
    };
    let update_server_info = update_server_info.unwrap_or_else(|| {
        config
            .get_bool("repack", None, "updateServerInfo")
            .unwrap_or(true)
    });
    let mut has_promisor_packs = pack_dir_has_promisor_packs(&common_git_dir)?;
    if let Some(roots) = repack_roots.as_deref()
        && !has_promisor_packs
        && services
            .has_promisor_remote
            .is_some_and(|probe| probe(&config))
    {
        if let Some(hydrate) = services.hydrate_promisor_remotes.as_mut() {
            hydrate(&common_git_dir, format, roots)?;
        }
        has_promisor_packs = pack_dir_has_promisor_packs(&common_git_dir)?;
    }
    let config_write_bitmaps = config.get_bool("repack", None, "writeBitmaps");
    let write_reverse_index = config
        .get_bool("pack", None, "writeReverseIndex")
        .unwrap_or(true);
    let write_bitmap_lookup_table = config
        .get_bool("pack", None, "writeBitmapLookupTable")
        .unwrap_or(false);
    let write_bitmap_hash_cache = config
        .get_bool("pack", None, "writeBitmapHashCache")
        .unwrap_or(true);
    let midx_must_contain_cruft = config
        .get_bool("repack", None, "midxMustContainCruft")
        .unwrap_or(true);
    let auto_bare_bitmaps = write_bitmaps.is_none()
        && config_write_bitmaps.is_none()
        && all
        && !write_midx
        && config.get("pack", None, "packSizeLimit").is_none()
        && sley_worktree::worktree_root_for_git_dir(&common_git_dir)?.is_none()
        && !pack_dir_has_kept_packs(&common_git_dir)?
        && !has_promisor_packs;
    let mut write_bitmaps = match write_bitmaps {
        Some(explicit) => explicit,
        None => config_write_bitmaps.unwrap_or(auto_bare_bitmaps),
    };
    let include_kept_objects =
        pack_kept_objects || (write_bitmaps && !write_midx && !auto_bare_bitmaps);

    if write_bitmaps && name_hash_version.is_some_and(|version| version != 1) {
        // Match pack-objects: bitmaps require name-hash version 1; sley always
        // writes the v1 cache and continues after warning (git auto-switches).
        eprintln!("warning: currently, --write-bitmap-index requires --name-hash-version=1");
    }

    if write_bitmaps && local && object_dir_has_alternates(&common_git_dir) {
        eprintln!("warning: disabling bitmap writing, as some objects are not being packed");
        write_bitmaps = false;
    }
    if write_bitmaps && pack_filter.is_some() {
        eprintln!("fatal: cannot write bitmap index with pack filters");
        return Err(GitError::Exit(128));
    }
    if write_bitmaps && all && has_promisor_packs {
        eprintln!("fatal: cannot write bitmap index for a repack with promisor packs");
        return Err(GitError::Exit(128));
    }

    if let Some(split_factor) = geometric {
        // `--geometric` and `-a`/`-A` are mutually exclusive (builtin/repack.c).
        if all {
            return Err(GitError::Command(
                "options '--geometric' and '-A/-a' cannot be used together".into(),
            ));
        }
        return run_geometric(
            git_dir,
            &common_git_dir,
            format,
            split_factor,
            prune,
            quiet,
            write_midx,
            write_bitmaps,
            midx_must_contain_cruft,
            keep_packs,
            include_kept_objects,
        );
    }

    if cruft {
        validate_repack_cruft_numeric_config(&config)?;
        let configured_pack_size = config
            .get("pack", None, "packSizeLimit")
            .map(parse_gc_size)
            .transpose()?;
        let cruft_pack_size =
            resolve_cruft_pack_size(max_pack_size, max_cruft_size, configured_pack_size);
        // Cruft-specific config intentionally overrides the general command
        // option. Otherwise the command option overrides pack.window. The
        // value was shape-validated above against pack-objects' option
        // grammar; resolve it with the same classifier so units (`2k`) work.
        let default_window = PackWriteOptions::new().window;
        let cruft_window = if let Some(value) = config.get("repack", None, "cruftWindow") {
            sley_config::typed::classify_config_size(value)
                .ok()
                .and_then(|parsed| usize::try_from(parsed).ok())
                .unwrap_or(default_window)
        } else if let Some(value) = window {
            value
        } else if let Some(value) = config.get("pack", None, "window") {
            parse_repack_window(value)?
        } else {
            default_window
        };
        return run_cruft(
            replace_objects,
            git_dir,
            &common_git_dir,
            format,
            prune,
            local,
            cruft_expiration.flatten(),
            expire_to.as_deref(),
            write_midx,
            keep_packs,
            include_kept_objects,
            cruft_pack_size,
            cruft_window,
            combine_cruft_below_size.filter(|size| *size > 0),
        );
    }

    if write_bitmaps && !all && !write_midx {
        // Upstream cmd_repack: bitmaps require an all-into-one repack.
        eprintln!(
            "fatal: Incremental repacks are incompatible with bitmap indexes.  Use
--no-write-bitmap-index or disable the pack.writeBitmaps configuration."
        );
        return Err(GitError::Exit(128));
    }

    // `-A -d` differs from `-a -d`: objects that are no longer reachable must
    // be materialized loose before their source packs are removed. Build that
    // transition as one engine outcome so neither the CLI nor concurrent
    // readers observe a gap between pruning the pack and writing the loose
    // copies.
    if all && unpack_unreachable && prune && !keep_unreachable {
        if pack_filter.is_some() {
            return Err(GitError::Command(
                "--unpack-unreachable cannot be combined with --filter".into(),
            ));
        }
        let roots = repack_roots.as_deref().ok_or_else(|| {
            GitError::Command("internal: all-object repack missing traversal roots".into())
        })?;
        let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
        let options = sley_odb::RepackOptions {
            local,
            force_rewrite,
            pack_kept_objects: include_kept_objects,
            keep_pack_stems,
        };
        let recent_roots = prune_recent_hook_roots(&common_git_dir, format)?;
        let unpacked = sley_odb::repack_reachable_objects_unpack_unreachable(
            &common_git_dir,
            format,
            roots,
            &options,
            unpack_unreachable_before.flatten(),
            &recent_roots,
        )?;
        sley_odb::install_repack_with_unpacked_unreachable(
            &common_git_dir,
            format,
            &unpacked,
            true,
        )?;
        if unpacked.repack.as_ref().is_none_or(|result| {
            result.loose_object_prune_outcome() != sley_odb::LooseObjectPruneOutcome::Complete
        }) {
            prune_packed_loose_objects(&common_git_dir, format, false)?;
        }
        if !write_bitmaps || write_midx {
            remove_pack_bitmap_sidecars(&common_git_dir)?;
        }
        if write_midx {
            let mut midx_args = Vec::new();
            if write_bitmaps {
                midx_args.push("--bitmap".to_string());
            }
            midx::write(Path::new("."), &common_git_dir, &midx_args)?;
        }
        if update_server_info {
            let updated = match unpacked.repack.as_ref() {
                Some(result) => {
                    repack_try_update_server_info_from_result(&common_git_dir, format, result)?
                }
                None => false,
            };
            if !updated {
                (services.update_server_info)()?;
            }
        }
        prune_repack_shallow_file(&common_git_dir, format, roots)?;
        // `repack -A -d` never loosens promisor objects (they stay in retained
        // `.promisor` packs). Emit the TRACE2_PERF counter git's pack-objects
        // path records so t5616 can observe `loosened:0`.
        if has_promisor_packs {
            perf_data("loosen_unused_packed_objects/loosened", "0");
        }
        let _ = quiet;
        return Ok(());
    }

    // `-a`: pack the reachability closure of refs/HEAD/reflogs/index (borrowed
    // objects included, unreachable ones dropped). Without `-a`, pack only
    // loose objects and leave existing packs in place.
    let result = if all && keep_unreachable {
        sley_odb::repack_all_objects(&common_git_dir, format)?
    } else if all {
        let roots = repack_roots.as_deref().ok_or_else(|| {
            GitError::Command("internal: all-object repack missing traversal roots".into())
        })?;
        let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
        let options = sley_odb::RepackOptions {
            local,
            force_rewrite,
            pack_kept_objects: include_kept_objects,
            keep_pack_stems,
        };
        match pack_filter.as_ref() {
            Some(filter) => sley_odb::repack_reachable_objects_with_object_filter(
                &common_git_dir,
                format,
                roots,
                &options,
                filter,
                filter_to.as_deref().map(Path::new),
                max_pack_size,
            )?,
            None => sley_odb::repack_reachable_objects_with_options(
                &common_git_dir,
                format,
                roots,
                &options,
            )?,
        }
    } else {
        let roots = repack_traversal_roots(git_dir, &common_git_dir, format, replace_objects)?;
        sley_odb::repack_reachable_loose_objects(&common_git_dir, format, &roots)?
    };
    let mut loose_prune_complete = false;
    if let Some(result) = result.as_ref() {
        let (bitmap_tips, bitmap_pseudo_merge_groups) = if write_bitmaps {
            let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
            (
                Some(repack_preferred_bitmap_tips(&common_git_dir, &db, format)?),
                Some(repack_pseudo_merge_groups(&common_git_dir, &db, format)?),
            )
        } else {
            (None, None)
        };
        if write_bitmaps && write_bitmap_lookup_table {
            sley_core::trace2::region("pack-bitmap-write", "writing_lookup_table");
        }
        sley_odb::install_repack_result_with_bitmap_options(
            &common_git_dir,
            format,
            result,
            sley_odb::RepackInstallOptions::new(prune)
                .with_reverse_index(write_reverse_index)
                .with_bitmap_extensions(write_bitmap_lookup_table, write_bitmap_hash_cache),
            bitmap_tips.as_ref(),
            bitmap_pseudo_merge_groups.as_deref(),
        )?;
        loose_prune_complete =
            result.loose_object_prune_outcome() == sley_odb::LooseObjectPruneOutcome::Complete;
    }
    if prune && !loose_prune_complete {
        prune_packed_loose_objects(&common_git_dir, format, false)?;
        if all && has_promisor_packs {
            perf_data("loosen_unused_packed_objects/loosened", "0");
        }
    }
    if all && (!write_bitmaps || write_midx) {
        remove_pack_bitmap_sidecars(&common_git_dir)?;
    }
    // Writing a multi-pack bitmap supersedes per-pack bitmaps for the same
    // packs (git's `remove_redundant_bitmaps`).
    if write_midx && write_bitmaps {
        remove_pack_bitmap_sidecars(&common_git_dir)?;
    }
    if write_midx {
        let mut midx_args = Vec::new();
        if write_bitmaps {
            midx_args.push("--bitmap".to_string());
        }
        midx::write(Path::new("."), &common_git_dir, &midx_args)?;
    }
    if update_server_info {
        let updated_from_result = match result.as_ref() {
            Some(result) => {
                repack_try_update_server_info_from_result(&common_git_dir, format, result)?
            }
            None => false,
        };
        if !updated_from_result {
            (services.update_server_info)()?;
        }
    }
    if all
        && prune
        && let Some(roots) = repack_roots.as_deref()
    {
        prune_repack_shallow_file(&common_git_dir, format, roots)?;
    }
    let _ = quiet;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_traversal_propagates_malformed_root_extensions() {
        let format = ObjectFormat::Sha1;
        for (signature, body) in [
            (*b"TREE", b"\0".as_slice()),
            (*b"REUC", b"unterminated-path".as_slice()),
        ] {
            let mut extensions = Vec::new();
            extensions.extend_from_slice(&signature);
            extensions.extend_from_slice(&(body.len() as u32).to_be_bytes());
            extensions.extend_from_slice(body);
            let index = sley_index::Index {
                version: 2,
                entries: Vec::new(),
                extensions,
                checksum: None,
            };
            let bytes = index
                .write(format)
                .expect("write malformed extension fixture");
            let path = std::env::temp_dir().join(format!(
                "sley-gc-index-extension-{}-{}",
                std::process::id(),
                String::from_utf8_lossy(&signature)
            ));
            fs::write(&path, bytes).expect("write index fixture");

            assert!(
                index_traversal_roots(&path, format).is_err(),
                "malformed {} extension was silently ignored",
                String::from_utf8_lossy(&signature)
            );
            fs::remove_file(path).expect("remove index fixture");
        }
    }
}
