//! Combined / merge-commit diff (`-c` / `--cc` / `--combined-all-paths`).
//!
//! This is the single CLI-side wiring of the combined-diff renderer
//! ([`sley_diff_merge::render::render_combined_with`]) shared by `diff-tree`,
//! `show`, `log`, and `whatchanged`. It owns the repository-coupled half of
//! git's `combine-diff.c`: discovering the set of paths that EVERY parent
//! touches (`intersect_paths` / `find_paths_multitree`), reading the result and
//! per-parent blobs, and emitting the combined-raw (`::`) lines and the
//! `diff --cc`/`diff --combined` metainfo header. The renderer crate produces
//! the `@@@`-style hunk body; this module produces everything around it.
//!
//! A glob of the crate root brings every shared helper/type into scope via
//! descendant-privacy; see commands::stash for the rationale.
use crate::*;

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
    pub mode: u32,
    pub oid: Option<ObjectId>,
    /// The single-letter raw status (`M`, `A`, `D`, ...) of the result relative
    /// to this parent.
    pub status: char,
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
    let num_parent = parent_trees.len();
    let Some(first_parent_tree) = parent_trees.first() else {
        return Ok(Vec::new());
    };
    let rename_options = sley_diff_merge::RenameDetectionOptions {
        base: sley_diff_merge::DiffNameStatusOptions {
            detect_renames: false,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
        },
        detect_inexact: false,
        rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
        copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
        rename_limit: 0,
    };

    let mut first_parent_entries = sley_diff_merge::diff_name_status_trees_with_rename_options(
        db,
        format,
        first_parent_tree,
        result_tree,
        rename_options,
    )?;
    first_parent_entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let mut paths = Vec::new();
    'paths: for first_entry in first_parent_entries {
        let path = first_entry.path.as_bytes();
        let result_entry = TreePathEntry::from_new_side(&first_entry);
        let first_parent_entry = TreePathEntry::from_old_side(&first_entry);
        let Some(first_status) = combined_parent_status(first_parent_entry, result_entry) else {
            continue;
        };
        let mut parents = Vec::with_capacity(num_parent);
        parents.push(CombinedParentEntry {
            mode: first_parent_entry.map(|entry| entry.mode).unwrap_or(0),
            oid: first_parent_entry.map(|entry| entry.oid),
            status: first_status,
        });
        for parent_tree in &parent_trees[1..] {
            let parent_entry = tree_path_entry(db, format, parent_tree, path)?;
            let Some(status) = combined_parent_status(parent_entry, result_entry) else {
                continue 'paths;
            };
            parents.push(CombinedParentEntry {
                mode: parent_entry.map(|entry| entry.mode).unwrap_or(0),
                oid: parent_entry.map(|entry| entry.oid),
                status,
            });
        }
        paths.push(CombinedPath {
            path: path.to_vec(),
            result_mode: result_entry.map(|entry| entry.mode).unwrap_or(0),
            result_oid: result_entry.map(|entry| entry.oid),
            parents,
        });
    }
    Ok(paths)
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

fn combined_parent_status(
    parent: Option<TreePathEntry>,
    result: Option<TreePathEntry>,
) -> Option<char> {
    match (parent, result) {
        (None, Some(_)) => Some('A'),
        (Some(_), None) => Some('D'),
        (Some(parent), Some(result)) if parent != result => Some('M'),
        _ => None,
    }
}

fn tree_path_entry(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    path: &[u8],
) -> Result<Option<TreePathEntry>> {
    let mut current = *tree_oid;
    let mut components = path
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
        .peekable();
    if components.peek().is_none() {
        return Ok(Some(TreePathEntry {
            mode: 0o040000,
            oid: current,
        }));
    }
    while let Some(component) = components.next() {
        let object = db.read_object(&current)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::InvalidObject(format!(
                "expected tree {current}, found {}",
                object.object_type.as_str()
            )));
        }
        let mut found = None;
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry?;
            if entry.name == component {
                found = Some(TreePathEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                });
                break;
            }
        }
        let Some(entry) = found else {
            return Ok(None);
        };
        if components.peek().is_none() {
            return Ok(Some(entry));
        }
        if entry.mode != 0o040000 {
            return Ok(None);
        }
        current = entry.oid;
    }
    Ok(None)
}

/// Options shared by the combined raw / patch writers.
pub(crate) struct CombinedRenderCtx<'a> {
    pub db: &'a FileObjectDatabase,
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
    let num_parent = path.parents.len();
    for _ in 0..num_parent {
        write!(stdout, ":")?;
    }
    for parent in &path.parents {
        write!(stdout, "{:06o} ", parent.mode)?;
    }
    write!(stdout, "{:06o}", path.result_mode)?;
    for parent in &path.parents {
        write!(stdout, " {}", combined_raw_oid(parent.oid.as_ref(), ctx))?;
    }
    write!(
        stdout,
        " {} ",
        combined_raw_oid(path.result_oid.as_ref(), ctx)
    )?;
    for parent in &path.parents {
        write!(stdout, "{}", parent.status)?;
    }
    if z {
        stdout.write_all(b"\0")?;
        if ctx.all_paths {
            for _ in &path.parents {
                stdout.write_all(&path.path)?;
                stdout.write_all(b"\0")?;
            }
        }
        stdout.write_all(&path.path)?;
        stdout.write_all(b"\0")?;
    } else {
        write!(stdout, "\t")?;
        if ctx.all_paths {
            for _ in &path.parents {
                write!(stdout, "{}\t", status_quote_path(&path.path, false))?;
            }
        }
        writeln!(stdout, "{}", status_quote_path(&path.path, false))?;
    }
    Ok(())
}

/// Combined-raw name-status (`-c --name-status`): per-parent status letters +
/// path, no `::mode oid` prefix.
pub(crate) fn write_combined_name_status(
    stdout: &mut dyn Write,
    path: &CombinedPath,
    z: bool,
) -> Result<()> {
    for parent in &path.parents {
        write!(stdout, "{}", parent.status)?;
    }
    if z {
        stdout.write_all(b"\0")?;
        stdout.write_all(&path.path)?;
        stdout.write_all(b"\0")?;
    } else {
        writeln!(stdout, "\t{}", status_quote_path(&path.path, false))?;
    }
    Ok(())
}

fn combined_raw_oid(oid: Option<&ObjectId>, ctx: &CombinedRenderCtx<'_>) -> String {
    let mut hex = match oid {
        Some(oid) => {
            let hex = oid.to_hex();
            let width = ctx.raw_abbrev.unwrap_or(hex.len()).min(hex.len());
            hex[..width].to_string()
        }
        None => "0".repeat(ctx.raw_abbrev.unwrap_or(ctx.format.hex_len())),
    };
    if hex.len() < ctx.format.hex_len()
        && std::env::var("GIT_PRINT_SHA1_ELLIPSIS").is_ok_and(|value| value == "yes")
    {
        hex.push_str("...");
    }
    hex
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
        Some(oid) => read_blob(ctx.db, oid)?,
        None => Vec::new(),
    };
    let mut parent_blobs: Vec<Vec<u8>> = Vec::with_capacity(num_parent);
    for parent in &path.parents {
        parent_blobs.push(match &parent.oid {
            Some(oid) if parent.mode == 0o160000 => gitlink_diff_content(oid, false),
            Some(oid) => read_blob(ctx.db, oid)?,
            None => Vec::new(),
        });
    }
    let parent_refs: Vec<&[u8]> = parent_blobs.iter().map(|b| b.as_slice()).collect();

    let mode_differs = path.parents.iter().any(|p| p.mode != path.result_mode);

    let mut body = Vec::new();
    let render_options = sley_diff_merge::render::CombinedRenderOptions {
        dense: ctx.dense,
        context: ctx.context,
        algorithm: ctx.diff_algorithm,
        ws_ignore: ctx.ws_ignore,
    };
    let show_hunks = sley_diff_merge::render::render_combined_with(
        &mut body,
        &result_blob,
        &parent_refs,
        &render_options,
    );

    if !show_hunks && !mode_differs {
        return Ok(false);
    }

    let head = if ctx.dense {
        "diff --cc "
    } else {
        "diff --combined "
    };
    writeln!(stdout, "{head}{}", status_quote_path(&path.path, false))?;

    write!(stdout, "index ")?;
    for (i, parent) in path.parents.iter().enumerate() {
        if i > 0 {
            write!(stdout, ",")?;
        }
        write!(
            stdout,
            "{}",
            combined_patch_abbrev(parent.oid.as_ref(), ctx.patch_abbrev, ctx.format)
        )?;
    }
    writeln!(
        stdout,
        "..{}",
        combined_patch_abbrev(path.result_oid.as_ref(), ctx.patch_abbrev, ctx.format)
    )?;

    let deleted = path.result_mode == 0;
    let added = !deleted && path.parents.iter().all(|p| p.status == 'A');
    if mode_differs {
        if added {
            writeln!(stdout, "new file mode {:06o}", path.result_mode)?;
        } else {
            if deleted {
                write!(stdout, "deleted file ")?;
            }
            write!(stdout, "mode ")?;
            for (i, parent) in path.parents.iter().enumerate() {
                if i > 0 {
                    write!(stdout, ",")?;
                }
                write!(stdout, "{:06o}", parent.mode)?;
            }
            if path.result_mode != 0 {
                write!(stdout, "..{:06o}", path.result_mode)?;
            }
            writeln!(stdout)?;
        }
    }

    if ctx.all_paths {
        for parent in &path.parents {
            if parent.status == 'A' {
                writeln!(stdout, "--- /dev/null")?;
            } else {
                writeln!(
                    stdout,
                    "--- {}{}",
                    ctx.src_prefix,
                    status_quote_path(&path.path, false)
                )?;
            }
        }
    } else if added {
        writeln!(stdout, "--- /dev/null")?;
    } else {
        writeln!(
            stdout,
            "--- {}{}",
            ctx.src_prefix,
            status_quote_path(&path.path, false)
        )?;
    }
    if deleted {
        writeln!(stdout, "+++ /dev/null")?;
    } else {
        writeln!(
            stdout,
            "+++ {}{}",
            ctx.dst_prefix,
            status_quote_path(&path.path, false)
        )?;
    }

    stdout.write_all(&body)?;
    Ok(true)
}

fn combined_patch_abbrev(oid: Option<&ObjectId>, abbrev: usize, format: ObjectFormat) -> String {
    match oid {
        Some(oid) => {
            let hex = oid.to_hex();
            hex[..abbrev.min(hex.len())].to_string()
        }
        None => "0".repeat(abbrev.min(format.hex_len())),
    }
}
