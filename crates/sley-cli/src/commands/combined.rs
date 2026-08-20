//! Combined / merge-commit diff (`-c` / `--cc` / `--combined-all-paths`).
//!
//! This is the single CLI-side repository adapter shared by `diff-tree`,
//! `show`, `log`, and `whatchanged`. It discovers the paths every parent
//! touches (`intersect_paths` / `find_paths_multitree`) and resolves their blob
//! contents. Typed metadata, raw records, patch headers, path quoting, and the
//! `@@@` hunk body are all rendered by `sley-diff-merge::porcelain`.
//!
//! A glob of the crate root brings every shared helper/type into scope via
//! descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley::plumbing::sley_diff_merge;

/// One path of a combined merge diff: the merge result plus each parent's state
/// for that path (mirrors git's `struct combine_diff_path`).
pub(crate) struct CombinedPath {
    pub path: Vec<u8>,
    /// Result mode/oid (`0`/`None` when the result deleted the path).
    pub result_mode: u32,
    pub result_oid: Option<ObjectId>,
    /// Per-parent state in parent order.
    pub parents: Vec<CombinedParentEntry>,
}

pub(crate) struct CombinedParentEntry {
    /// Parent-side pathname. It differs from the result path for a rename and
    /// is emitted by `--combined-all-paths`.
    pub path: Vec<u8>,
    pub mode: u32,
    pub oid: Option<ObjectId>,
    /// The single-letter raw status (`M`, `A`, `D`, ...) of the result relative
    /// to this parent.
    pub status: char,
}

#[derive(Clone, Copy)]
pub(crate) struct CombinedPathOptions {
    pub detect_renames: bool,
    pub rename_empty: bool,
    pub rename_threshold: u8,
    /// Include changed subtree entries (`diff-tree -t`) in addition to leaves.
    pub include_trees: bool,
}

impl Default for CombinedPathOptions {
    fn default() -> Self {
        Self {
            detect_renames: false,
            rename_empty: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            include_trees: false,
        }
    }
}

/// Compute the combined path set for a merge: the intersection of the per-parent
/// recursive name-status diffs (parent tree -> result tree), in tree order.
/// git always scans combined paths recursively and without rename detection.
pub(crate) fn combined_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    result_tree: &ObjectId,
    parent_trees: &[ObjectId],
) -> Result<Vec<CombinedPath>> {
    combined_paths_with_options(
        db,
        format,
        result_tree,
        parent_trees,
        CombinedPathOptions::default(),
    )
}

pub(crate) fn combined_paths_with_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    result_tree: &ObjectId,
    parent_trees: &[ObjectId],
    path_options: CombinedPathOptions,
) -> Result<Vec<CombinedPath>> {
    if parent_trees.is_empty() {
        return Ok(Vec::new());
    }
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: path_options.detect_renames,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: path_options.rename_empty,
        detect_inexact: path_options.detect_renames,
        rename_threshold: path_options.rename_threshold,
        copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
        rename_limit: 0,
        ..Default::default()
    };

    let mut per_parent = parent_trees
        .iter()
        .map(|parent_tree| {
            sley_diff_merge::diff_name_status_trees_with_options(
                db,
                format,
                parent_tree,
                result_tree,
                options,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let mut first_parent_entries = per_parent.remove(0);
    first_parent_entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let other_parents = per_parent
        .into_iter()
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| (entry.path.as_bytes().to_vec(), entry))
                .collect::<BTreeMap<_, _>>()
        })
        .collect::<Vec<_>>();
    let mut paths = Vec::new();
    'paths: for first_entry in first_parent_entries {
        let path = first_entry.path.as_bytes();
        let result_entry = TreePathEntry::from_new_side(&first_entry);
        let first_parent_entry = TreePathEntry::from_old_side(&first_entry);
        let mut parents = Vec::with_capacity(parent_trees.len());
        parents.push(CombinedParentEntry {
            path: first_entry
                .old_path
                .as_ref()
                .map(|path| path.as_bytes())
                .unwrap_or(path)
                .to_vec(),
            mode: first_parent_entry.map(|entry| entry.mode).unwrap_or(0),
            oid: first_parent_entry.map(|entry| entry.oid),
            status: first_entry.status.code(),
        });
        for entries in &other_parents {
            let Some(entry) = entries.get(path) else {
                continue 'paths;
            };
            let parent_entry = TreePathEntry::from_old_side(entry);
            parents.push(CombinedParentEntry {
                path: entry
                    .old_path
                    .as_ref()
                    .map(|path| path.as_bytes())
                    .unwrap_or(path)
                    .to_vec(),
                mode: parent_entry.map(|entry| entry.mode).unwrap_or(0),
                oid: parent_entry.map(|entry| entry.oid),
                status: entry.status.code(),
            });
        }
        paths.push(CombinedPath {
            path: path.to_vec(),
            result_mode: result_entry.map(|entry| entry.mode).unwrap_or(0),
            result_oid: result_entry.map(|entry| entry.oid),
            parents,
        });
    }
    if path_options.include_trees {
        let result_trees = collect_combined_subtrees(db, format, result_tree)?;
        let parent_trees = parent_trees
            .iter()
            .map(|tree| collect_combined_subtrees(db, format, tree))
            .collect::<Result<Vec<_>>>()?;
        for (path, result_entry) in result_trees {
            let mut parents = Vec::with_capacity(parent_trees.len());
            let mut changed_from_every_parent = true;
            for trees in &parent_trees {
                let parent_entry = trees.get(&path).copied();
                let status = match parent_entry {
                    Some(parent) if parent != result_entry => 'M',
                    None => 'A',
                    _ => {
                        changed_from_every_parent = false;
                        break;
                    }
                };
                parents.push(CombinedParentEntry {
                    path: path.clone(),
                    mode: parent_entry.map(|entry| entry.mode).unwrap_or(0),
                    oid: parent_entry.map(|entry| entry.oid),
                    status,
                });
            }
            if !changed_from_every_parent {
                continue;
            }
            paths.push(CombinedPath {
                path,
                result_mode: result_entry.mode,
                result_oid: Some(result_entry.oid),
                parents,
            });
        }
        paths.sort_by(|left, right| left.path.cmp(&right.path));
    }
    Ok(paths)
}

fn collect_combined_subtrees(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    root: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, TreePathEntry>> {
    fn collect(
        db: &FileObjectDatabase,
        format: ObjectFormat,
        tree: &ObjectId,
        prefix: &[u8],
        out: &mut BTreeMap<Vec<u8>, TreePathEntry>,
    ) -> Result<()> {
        let object = db.read_object(tree)?;
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry?;
            if entry.mode != 0o040000 {
                continue;
            }
            let mut path = Vec::with_capacity(prefix.len() + entry.name.len() + 1);
            if !prefix.is_empty() {
                path.extend_from_slice(prefix);
                path.push(b'/');
            }
            path.extend_from_slice(entry.name);
            out.insert(
                path.clone(),
                TreePathEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            );
            collect(db, format, &entry.oid, &path, out)?;
        }
        Ok(())
    }

    let mut out = BTreeMap::new();
    collect(db, format, root, &[], &mut out)?;
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TreePathEntry {
    mode: u32,
    oid: ObjectId,
}

impl TreePathEntry {
    fn from_old_side(entry: &sley_diff_merge::NameStatusEntry) -> Option<Self> {
        Some(Self {
            mode: entry.old_mode?,
            oid: entry.old_oid?,
        })
    }

    fn from_new_side(entry: &sley_diff_merge::NameStatusEntry) -> Option<Self> {
        Some(Self {
            mode: entry.new_mode?,
            oid: entry.new_oid?,
        })
    }
}

/// Options shared by the combined raw / patch writers.
pub(crate) struct CombinedRenderCtx<'a> {
    pub db: &'a FileObjectDatabase,
    /// Whether missing promisor objects may be fetched while rendering.
    pub lazy_fetch: bool,
    pub format: ObjectFormat,
    /// `true` for `--cc` (dense simplification), `false` for `-c`.
    pub dense: bool,
    /// `--combined-all-paths`: list each parent's path on its own `---` line.
    pub all_paths: bool,
    /// Unified-context (always 3 for combined diffs today).
    pub context: usize,
    pub ws_ignore: sley_diff_merge::WsIgnore,
    pub diff_algorithm: sley_diff_merge::DiffAlgorithm,
    pub src_prefix: &'a str,
    pub dst_prefix: &'a str,
    /// Patch index-line abbreviation width.
    pub patch_abbrev: usize,
    /// Raw-mode oid abbreviation (`None` = full id).
    pub raw_abbrev: Option<usize>,
}

/// Emit one combined-raw (`::`) entry — git's `show_raw_diff` RAW format.
pub(crate) fn write_combined_raw(
    stdout: &mut dyn Write,
    ctx: &CombinedRenderCtx<'_>,
    path: &CombinedPath,
    z: bool,
) -> Result<()> {
    let parents = combined_engine_parents(path);
    sley_diff_merge::porcelain::render_combined_raw_entry(
        stdout,
        combined_engine_entry(path, &parents),
        combined_engine_options(ctx),
        z,
    )
    .map_err(|error| GitError::Io(error.to_string()))?;
    Ok(())
}

/// Combined-raw name-status (`-c --name-status`): per-parent status letters +
/// path, no `::mode oid` prefix.
pub(crate) fn write_combined_name_status(
    stdout: &mut dyn Write,
    path: &CombinedPath,
    all_paths: bool,
    z: bool,
) -> Result<()> {
    let parents = combined_engine_parents(path);
    sley_diff_merge::porcelain::render_combined_name_status_entry(
        stdout,
        combined_engine_entry(path, &parents),
        all_paths,
        z,
    )
    .map_err(|error| GitError::Io(error.to_string()))?;
    Ok(())
}

pub(crate) fn combined_path_matches_find_objects(
    path: &CombinedPath,
    targets: &[ObjectId],
) -> bool {
    if targets.is_empty() {
        return true;
    }
    let first_parent = path.parents.first();
    targets.iter().any(|target| {
        let old_has = first_parent
            .and_then(|parent| parent.oid.as_ref())
            .is_some_and(|oid| oid == target);
        let new_has = path.result_oid.as_ref().is_some_and(|oid| oid == target);
        old_has != new_has
    })
}

/// Emit one combined-patch file — git's `show_patch_diff`. Returns `true` when a
/// header+body was emitted (some hunks survived, or modes differ).
pub(crate) fn write_combined_patch(
    stdout: &mut dyn Write,
    ctx: &CombinedRenderCtx<'_>,
    path: &CombinedPath,
) -> Result<bool> {
    let num_parent = path.parents.len();
    // A gitlink (submodule) oid is a commit, not a blob: git's combine-diff
    // `grab_blob` synthesizes `Subproject commit <hex>\n` for `S_ISGITLINK(mode)`
    // before any object read, exactly as the non-combined diff path does. Reading
    // it as a blob would error or yield garbage.
    let result_blob = match &path.result_oid {
        Some(oid) if path.result_mode == 0o160000 => gitlink_diff_content(oid, false),
        Some(oid) => read_blob(ctx.db, oid, ctx.lazy_fetch)?,
        None => Vec::new(),
    };
    let mut parent_blobs: Vec<Vec<u8>> = Vec::with_capacity(num_parent);
    for parent in &path.parents {
        parent_blobs.push(match &parent.oid {
            Some(oid) if parent.mode == 0o160000 => gitlink_diff_content(oid, false),
            Some(oid) => read_blob(ctx.db, oid, ctx.lazy_fetch)?,
            None => Vec::new(),
        });
    }
    let parent_refs: Vec<&[u8]> = parent_blobs.iter().map(|b| b.as_slice()).collect();

    let parents = combined_engine_parents(path);
    let outcome = sley_diff_merge::porcelain::render_combined_patch(
        stdout,
        combined_engine_entry(path, &parents),
        &result_blob,
        &parent_refs,
        combined_engine_options(ctx),
    )
    .map_err(|error| GitError::Io(error.to_string()))?;
    Ok(outcome.records_written != 0)
}

fn combined_engine_parents(
    path: &CombinedPath,
) -> Vec<sley_diff_merge::porcelain::CombinedDiffParent<'_>> {
    path.parents
        .iter()
        .map(|parent| sley_diff_merge::porcelain::CombinedDiffParent {
            path: &parent.path,
            mode: parent.mode,
            oid: parent.oid,
            status: parent.status,
        })
        .collect()
}

fn combined_engine_entry<'a>(
    path: &'a CombinedPath,
    parents: &'a [sley_diff_merge::porcelain::CombinedDiffParent<'a>],
) -> sley_diff_merge::porcelain::CombinedDiffEntry<'a> {
    sley_diff_merge::porcelain::CombinedDiffEntry {
        result_path: &path.path,
        result_mode: path.result_mode,
        result_oid: path.result_oid,
        parents,
    }
}

fn combined_engine_options<'a>(
    ctx: &'a CombinedRenderCtx<'a>,
) -> sley_diff_merge::porcelain::CombinedFormatOptions<'a> {
    sley_diff_merge::porcelain::CombinedFormatOptions {
        object_format: ctx.format,
        dense: ctx.dense,
        all_paths: ctx.all_paths,
        context: ctx.context,
        ws_ignore: ctx.ws_ignore,
        algorithm: ctx.diff_algorithm,
        src_prefix: ctx.src_prefix.as_bytes(),
        dst_prefix: ctx.dst_prefix.as_bytes(),
        patch_abbrev: ctx.patch_abbrev,
        raw_abbrev: ctx.raw_abbrev,
        print_hash_ellipsis: std::env::var("GIT_PRINT_SHA1_ELLIPSIS")
            .is_ok_and(|value| value == "yes"),
    }
}
