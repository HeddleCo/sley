//! The blame assignment engine (`git blame`'s scoreboard walk).
//!
//! A behavioural port of git's `blame.c`: commit-date-ordered priority walk
//! from a start commit toward the roots, diffing every commit's blob for the
//! blamed path against each parent's blob with the shared Myers line diff;
//! lines a parent preserves migrate to that parent as (possibly split) chunks,
//! lines no parent preserves are charged to the current commit. Includes the
//! rename-following parent origin search, `-C` copy detection, the
//! `--ignore-rev` fuzzy fingerprint content matching, grafts overrides, and
//! `^<rev>` boundary detection.
//!
//! This is the algorithm core below the CLI seam: callers supply an object
//! database handle, the resolved rev/path inputs, and two hooks — an object
//! source (plain reads, or one that hydrates promisor blobs on demand) and a
//! content converter (textconv, usually the identity) — and receive structured
//! [`LineBlame`] rows plus the per-origin `previous` map the porcelain output
//! needs. Argument parsing and all rendering stay above the seam.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_diff_merge::DiffAlgorithm;
use sley_object::{Commit, EncodedObject, ObjectType, TreeEntries};
use sley_odb::FileObjectDatabase;

use crate::peel_to_tree;

/// Hook supplying blob/commit object bodies during the walk. The default
/// [`FileObjectDatabase`] implementation reads straight from the repository;
/// partial-clone hosts wrap it so missing blobs are prefetched from promisors.
pub trait BlameObjectSource {
    fn read_blame_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>>;
}

impl BlameObjectSource for FileObjectDatabase {
    fn read_blame_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        ObjectReader::read_object(self, oid)
    }
}

use sley_odb::ObjectReader;

/// Hook rendering each origin blob before it is diffed or stored in the cache.
/// With textconv disabled this is the identity ([`PassThroughConverter`]);
/// the CLI wires its userdiff-driven converter here so converted content is
/// what the walk diffs, exactly as git's `fill_textconv` does.
pub trait BlameContentConverter {
    fn convert(&mut self, path: &str, mode: u32, blob: Vec<u8>) -> Result<Vec<u8>>;
}

/// Identity converter: blame without textconv.
pub struct PassThroughConverter;

impl BlameContentConverter for PassThroughConverter {
    fn convert(&mut self, _path: &str, _mode: u32, blob: Vec<u8>) -> Result<Vec<u8>> {
        Ok(blob)
    }
}

/// The fully-resolved inputs to one blame run. Everything here is repo-relative
/// and peeled: `start_commit` names the final image's commit, `repo_path` the
/// path within it, and `final_blob` the (already converted) final image bytes.
pub struct BlameRequest<'a> {
    /// Repository object database (tree peels, name-status diffs, commit reads).
    pub db: &'a FileObjectDatabase,
    /// Blob/commit reader hook (see [`BlameObjectSource`]).
    pub reader: &'a dyn BlameObjectSource,
    pub format: ObjectFormat,
    /// Commit whose tree holds the final image.
    pub start_commit: ObjectId,
    /// Repository-root-relative path being blamed.
    pub repo_path: &'a str,
    /// Final image bytes (work-tree / `--contents` / committed copy).
    pub final_blob: &'a [u8],
    /// Follow only the first parent of merges (`--first-parent`).
    pub first_parent: bool,
    /// Tip of a `^<rev>` range: it and its ancestors are uninteresting
    /// boundaries the walk charges and stops at.
    pub boundary_tip: Option<ObjectId>,
    /// Number of `-C` options seen (0 = none).
    pub copy_level: u8,
    /// Minimum alphanumeric score for `-C` copy matching.
    pub copy_score: usize,
    /// True when `final_blob` is the virtual work-tree / `--contents` image
    /// sitting on top of `start_commit` rather than its committed blob.
    pub virtual_final: bool,
    /// Commits whose changes are skipped (`--ignore-rev` set).
    pub ignore_set: &'a HashSet<ObjectId>,
    /// Diff algorithm driving attribution.
    pub diff_algorithm: DiffAlgorithm,
    /// `.git/info/grafts` commit → parents overrides.
    pub grafts: &'a HashMap<ObjectId, Vec<ObjectId>>,
}

/// Per-origin `previous` pointers for porcelain output: maps a blamed
/// `(commit, path)` to the `(commit, path)` of the first parent the blame walk
/// descended into from it (git's `blame_origin->previous`). Root/boundary
/// commits with no such parent are simply absent.
pub type PreviousMap = HashMap<(ObjectId, String), (ObjectId, String)>;

/// The blame result for a single final-image line: one row of the structured
/// output the CLI renders as default, porcelain, or incremental output.
pub struct LineBlame {
    /// Commit the line is attributed to.
    pub commit: ObjectId,
    /// Whether that commit is a rendered boundary (root, absent `--root`).
    pub boundary: bool,
    /// Path in the blamed origin.
    pub origin_path: String,
    /// 1-based line number in the blamed origin.
    pub origin_lineno: usize,
    /// The raw line bytes, including a trailing newline when present.
    pub content: Vec<u8>,
    /// `--ignore-rev` markers carried from the blame entry (`?` / `*`).
    pub ignored: bool,
    pub unblamable: bool,
    /// Order in which this line's commit was found guilty during the walk, used
    /// to emit `--incremental` output in walk order (newest commit first).
    pub charge_seq: usize,
}

/// Parse `<git_dir>/info/grafts` into a commit → parents override map (git's
/// `read_graft_line` / `register_commit_graft`). Each non-comment line is a
/// whitespace-separated list of object names: the first names a commit, the rest
/// its grafted parents. Lines that don't parse are skipped. Empty map when the
/// file is absent.
pub fn read_graft_file(
    git_dir: &std::path::Path,
    format: ObjectFormat,
) -> HashMap<ObjectId, Vec<ObjectId>> {
    let mut map: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let Ok(content) = std::fs::read(git_dir.join("info").join("grafts")) else {
        return map;
    };
    for raw in content.split(|b| *b == b'\n') {
        let line = match raw.iter().position(|b| *b == b'#') {
            Some(idx) => &raw[..idx],
            None => raw,
        };
        let tokens: Vec<&[u8]> = line
            .split(|b| b.is_ascii_whitespace())
            .filter(|t| !t.is_empty())
            .collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(first) else {
            continue;
        };
        let Ok(commit) = ObjectId::from_hex(format, text) else {
            continue;
        };
        let parents = tokens[1..]
            .iter()
            .filter_map(|t| std::str::from_utf8(t).ok())
            .filter_map(|t| ObjectId::from_hex(format, t).ok())
            .collect();
        map.insert(commit, parents);
    }
    map
}

/// Read the blob at `repo_path` in `commit`'s tree, returning its bytes and the
/// tree entry's file mode (the mode lets textconv skip symlinks).
pub fn read_path_blob(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &ObjectId,
    repo_path: &str,
    reader: &dyn BlameObjectSource,
) -> Result<Option<(Vec<u8>, u32)>> {
    let tree_oid = peel_to_tree(db, format, commit)?;
    let Some((blob_oid, mode)) = lookup_tree_path(db, format, &tree_oid, repo_path)? else {
        return Ok(None);
    };
    let object = reader.read_blame_object(&blob_oid)?;
    if object.object_type != ObjectType::Blob {
        return Ok(None);
    }
    Ok(Some((object.body.clone(), mode)))
}

/// Walk `repo_path` component-by-component through `tree_oid`, returning the
/// blob id it names, or `None` if any component is missing or an intermediate
/// component is not a tree.
fn lookup_tree_path(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    repo_path: &str,
) -> Result<Option<(ObjectId, u32)>> {
    let components: Vec<&str> = repo_path.split('/').filter(|p| !p.is_empty()).collect();
    if components.is_empty() {
        return Ok(None);
    }
    let mut current = *tree_oid;
    let last = components.len() - 1;
    for (idx, component) in components.iter().enumerate() {
        let object = db.read_object(&current)?;
        if object.object_type != ObjectType::Tree {
            return Ok(None);
        }
        let mut matched = None;
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry?;
            if matched.is_none() && entry.name == component.as_bytes() {
                matched = Some((entry.mode, entry.oid));
            }
        }
        let Some((mode, oid)) = matched else {
            return Ok(None);
        };
        if idx == last {
            return Ok(Some((oid, mode)));
        }
        if sley_object::tree_entry_object_type(mode) != ObjectType::Tree {
            return Ok(None);
        }
        current = oid;
    }
    Ok(None)
}

/// A contiguous run of final-image lines currently suspected to originate in
/// one commit's version of the path. Mirrors git's `struct blame_entry`
/// (blame.c): `lno` is the 0-based line in the *final image*, `s_lno` the
/// 0-based line in the *suspect's* blob, and the entry covers `num_lines`
/// consecutive lines in both. The invariant `final[lno + k] == suspect[s_lno +
/// k]` for `k in 0..num_lines` is what lets us pass whole chunks to a parent.
#[derive(Clone, PartialEq, Eq, Hash)]
struct OriginKey {
    commit: ObjectId,
    path: String,
    virtual_worktree: bool,
}

#[derive(Clone, Default)]
struct BlameEntry {
    /// 0-based start line in the final image.
    lno: usize,
    /// 0-based start line in the suspect's blob.
    s_lno: usize,
    /// Number of lines this entry covers (in both the final image and the
    /// suspect's blob).
    num_lines: usize,
    /// `--ignore-rev`: this run's blame was passed *through* an ignored commit
    /// (git's `blame_entry.ignored`). Rendered with a `?` marker /
    /// `ignored` porcelain keyword when `blame.markIgnoredLines` is set.
    ignored: bool,
    /// `--ignore-rev`: this run's lines were added by an ignored commit and
    /// could not be matched to any parent line (git's `blame_entry.unblamable`).
    /// Rendered with a `*` marker / `unblamable` porcelain keyword when
    /// `blame.markUnblamableLines` is set.
    unblamable: bool,
}

/// Core blame: assign each final-image line to a commit, mirroring git's
/// diff-driven multi-pass scoreboard (blame.c `pass_blame` / `blame_chunk` /
/// `split_overlap`).
///
/// Rather than routing each final line independently to the first parent that
/// preserves it, we carry line *chunks* (`BlameEntry`) and, for each commit
/// pulled off a commit-date priority queue, diff that commit's blob against
/// each parent in turn. Lines a parent preserves migrate to the parent as
/// (possibly split) chunks with their `s_lno` rebased onto the parent's blob;
/// lines no parent preserves are charged to this commit. For a merge this
/// gives the *correct* parent (parent 0 first, the residual to parent 1, …),
/// which whole-line first-parent routing got wrong — and `-L` is simply a
/// final-range filter applied at render time over correctly-attributed chunks.
pub fn compute_blame(
    request: &BlameRequest<'_>,
    converter: &mut dyn BlameContentConverter,
) -> Result<(Vec<LineBlame>, PreviousMap)> {
    let BlameRequest {
        db,
        reader,
        format,
        start_commit,
        repo_path,
        final_blob,
        first_parent,
        boundary_tip,
        copy_level,
        copy_score,
        virtual_final,
        ignore_set,
        diff_algorithm,
        grafts,
    } = *request;

    let final_lines = sley_diff_merge::split_lines(final_blob);
    let line_count = final_lines.len();

    // Per-origin `previous` pointers for porcelain (`blame_origin->previous`).
    let mut previous_map: PreviousMap = HashMap::new();

    // Final attribution per line, filled in as commits are found guilty.
    let mut result: Vec<Option<LineBlame>> = (0..line_count).map(|_| None).collect();
    if line_count == 0 {
        return Ok((Vec::new(), previous_map));
    }

    // `git blame ^<rev>`: `<rev>` and all its ancestors are uninteresting
    // boundaries — when the walk reaches one, it charges the lines there as a
    // boundary and does not recurse into parents. Precompute that closure.
    let uninteresting = match boundary_tip {
        Some(tip) => ancestors_closure(db, format, &tip)?,
        None => HashSet::new(),
    };

    // Per-commit suspect chunks ("origin->suspects" in git). A commit is in the
    // queue iff it has at least one suspect chunk. The blob for each commit's
    // path is cached on first use.
    let mut suspects: HashMap<OriginKey, Vec<BlameEntry>> = HashMap::new();
    let mut blob_cache: HashMap<(ObjectId, String), Option<Arc<Vec<u8>>>> = HashMap::new();
    let mut date_cache: HashMap<ObjectId, i64> = HashMap::new();

    // The start commit owns the entire final image as one chunk.
    let start_key = OriginKey {
        commit: start_commit,
        path: repo_path.to_string(),
        virtual_worktree: virtual_final,
    };
    // Seed the blob cache with the final image only for a *real* start commit.
    // The virtual work-tree origin shares its commit id with the real start
    // commit (its single parent), so seeding `(commit, path)` here would poison
    // the parent's committed-blob read and make every line pass through. The
    // virtual origin gets its image from `final_blob` directly instead.
    if !virtual_final {
        blob_cache.insert(
            (start_key.commit, start_key.path.clone()),
            Some(Arc::new(final_blob.to_vec())),
        );
    }
    suspects.insert(
        start_key.clone(),
        vec![BlameEntry {
            lno: 0,
            s_lno: 0,
            num_lines: line_count,
            ..Default::default()
        }],
    );

    // Commit-date priority queue, newest first (git's
    // compare_commits_by_commit_date). We materialise the comparator lazily via
    // `pop_newest_commit`, caching each commit's date.
    let mut queue: Vec<OriginKey> = vec![start_key];

    // Monotonic charge-event counter for `--incremental` walk ordering.
    let mut next_seq = 0usize;

    while let Some(origin) = pop_newest_origin(&mut queue, db, format, &mut date_cache)? {
        let Some(mut owned) = suspects.remove(&origin) else {
            continue;
        };
        if owned.is_empty() {
            continue;
        }

        // Uninteresting (`^<rev>`) commits are boundaries: charge their lines
        // with the boundary marker and stop — do not pass blame to parents.
        if uninteresting.contains(&origin.commit) {
            charge_remaining(&mut result, &final_lines, &origin, true, owned, next_seq);
            next_seq += 1;
            continue;
        }

        // Resolve this commit and its blob for the path. The virtual work-tree
        // origin's image is the supplied `final_blob` (the worktree / contents
        // bytes), not the start commit's committed blob.
        let commit_oid = origin.commit;
        let commit_obj = db.read_object(&commit_oid)?;
        let commit = Commit::parse(format, &commit_obj.body)?;
        let child_blob: Option<Arc<Vec<u8>>> = if origin.virtual_worktree {
            Some(Arc::new(final_blob.to_vec()))
        } else {
            cached_blob(
                db,
                format,
                &commit_oid,
                &origin.path,
                &mut blob_cache,
                reader,
                converter,
            )?
        };
        let Some(child_blob) = child_blob else {
            // The path is absent at this commit (shouldn't normally happen for a
            // suspect): charge everything here so no line is lost.
            charge_remaining(&mut result, &final_lines, &origin, false, owned, next_seq);
            next_seq += 1;
            continue;
        };
        let child_lines = sley_diff_merge::split_lines(&child_blob);

        let mut parents = if origin.virtual_worktree {
            vec![origin.commit]
        } else if let Some(grafted) = grafts.get(&origin.commit) {
            // `.git/info/grafts` overrides this commit's parents.
            grafted.clone()
        } else {
            commit.parents.clone()
        };
        if first_parent {
            parents.truncate(1);
        }
        if parents.is_empty() {
            // Root commit (or `--first-parent` past a root): every remaining
            // line is its own. Render as a boundary unless `--root`.
            charge_remaining(&mut result, &final_lines, &origin, true, owned, next_seq);
            next_seq += 1;
            continue;
        }

        // Pass blame to each parent in order. `owned` shrinks as parents claim
        // chunks; whatever remains after the last parent is charged here.
        for parent in &parents {
            if owned.is_empty() {
                break;
            }
            let Some(parent_origin) =
                find_parent_origin(db, format, &origin, parent, copy_level > 1, reader)?
            else {
                continue;
            };
            // Record the first parent origin we descend into as this suspect's
            // porcelain `previous` pointer (set once, like git's
            // `origin->previous`). Skip the virtual work-tree origin: its blamed
            // lines surface as the null commit, whose `previous` comes from the
            // FakeCommit metadata instead.
            if !origin.virtual_worktree {
                previous_map
                    .entry((origin.commit, origin.path.clone()))
                    .or_insert_with(|| (parent_origin.commit, parent_origin.path.clone()));
            }
            let parent_blob = cached_blob(
                db,
                format,
                parent,
                &parent_origin.path,
                &mut blob_cache,
                reader,
                converter,
            )?;
            let Some(parent_blob) = parent_blob else {
                // Path absent in this parent: it preserves nothing, so all
                // chunks stay with the current commit for the next parent.
                continue;
            };

            // Whole-file shortcut: if the parent's blob is byte-identical, every
            // remaining chunk passes through unchanged (git's
            // pass_whole_blame / oideq(blob_oid) fast path).
            if **parent_blob == **child_blob {
                let passed = std::mem::take(&mut owned);
                queue_entries(&mut suspects, &mut queue, parent_origin, passed);
                break;
            }

            let parent_lines = sley_diff_merge::split_lines(&parent_blob);
            let passed =
                pass_blame_to_parent(&parent_lines, &child_lines, &mut owned, diff_algorithm);
            if !passed.is_empty() {
                queue_entries(&mut suspects, &mut queue, parent_origin, passed);
            }
        }

        // `--ignore-rev`: this commit is ignored, so the lines it changed (left
        // in `owned` by the normal pass) are not charged to it. Instead each is
        // fuzzily matched to a parent line and passed there marked `ignored`, or
        // kept marked `unblamable`. Mirrors git's second `pass_blame_to_parent`
        // loop (ignore_diffs = 1) over the scapegoats.
        if ignore_set.contains(&commit_oid) && !owned.is_empty() {
            let child_fps = line_fingerprints(&child_lines);
            for parent in &parents {
                if owned.is_empty() {
                    break;
                }
                let Some(parent_origin) =
                    find_parent_origin(db, format, &origin, parent, copy_level > 1, reader)?
                else {
                    continue;
                };
                let parent_blob = cached_blob(
                    db,
                    format,
                    parent,
                    &parent_origin.path,
                    &mut blob_cache,
                    reader,
                    converter,
                )?;
                let Some(parent_blob) = parent_blob else {
                    continue;
                };
                let parent_lines = sley_diff_merge::split_lines(&parent_blob);
                let parent_fps = line_fingerprints(&parent_lines);
                let passed = pass_blame_to_parent_ignore(
                    &parent_lines,
                    &child_lines,
                    &parent_fps,
                    &child_fps,
                    &mut owned,
                    diff_algorithm,
                );
                if !passed.is_empty() {
                    queue_entries(&mut suspects, &mut queue, parent_origin, passed);
                }
            }
        }

        if copy_level > 0 && !owned.is_empty() {
            let copied = find_copies_in_parents(
                db,
                format,
                &origin,
                &parents,
                copy_level,
                copy_score,
                &mut owned,
                &final_lines,
                converter,
                reader,
            )?;
            for (copy_origin, entries) in copied {
                queue_entries(&mut suspects, &mut queue, copy_origin, entries);
            }
        }

        // Anything still suspected of this commit after every parent had a turn
        // is genuinely this commit's: charge it (non-boundary — it has parents).
        if !owned.is_empty() {
            let guilty = if origin.virtual_worktree {
                OriginKey {
                    commit: ObjectId::null(format),
                    path: origin.path.clone(),
                    virtual_worktree: true,
                }
            } else {
                origin.clone()
            };
            charge_remaining(&mut result, &final_lines, &guilty, false, owned, next_seq);
            next_seq += 1;
        }
    }

    // Any line not resolved (shouldn't happen) falls back to the start commit
    // as a non-boundary so output stays well-formed.
    let mut out = Vec::with_capacity(line_count);
    for (line_index, slot) in result.into_iter().enumerate() {
        match slot {
            Some(blame) => out.push(blame),
            None => out.push(LineBlame {
                commit: start_commit,
                boundary: false,
                origin_path: repo_path.to_string(),
                origin_lineno: line_index + 1,
                content: final_lines[line_index].content.to_vec(),
                ignored: false,
                unblamable: false,
                charge_seq: usize::MAX,
            }),
        }
    }
    Ok((out, previous_map))
}

/// Diff the suspect blob (`child_lines`) against `parent_lines` and split every
/// chunk in `owned` along the diff boundaries: lines unchanged from the parent
/// migrate to `parent` (returned, with `s_lno` rebased onto the parent's blob);
/// lines that differ remain in `owned` for the next parent (or to be charged to
/// the suspect). This is the port of git's `blame_chunk` driven by
/// `blame_chunk_cb` over `diff_hunks` (blame.c).
///
/// The diff is computed *parent → child* so a hunk `(start_a, count_a) ->
/// (start_b, count_b)` says child lines `[start_b, start_b+count_b)` differ from
/// the parent, while the run before each hunk (and after the last) is common.
/// `offset = start_a - start_b` is how far the parent's line numbers lead the
/// child's across a common run.
fn pass_blame_to_parent(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
    owned: &mut Vec<BlameEntry>,
    algorithm: DiffAlgorithm,
) -> Vec<BlameEntry> {
    let hunks = diff_hunks(parent_lines, child_lines, algorithm);

    let mut passed: Vec<BlameEntry> = Vec::new();
    let mut still_ours: Vec<BlameEntry> = Vec::new();

    // Process chunks in `s_lno` (child line) order so the running `offset`
    // — how far the parent's line numbers lead the child's across the current
    // common run — stays valid. `deferred` holds split tails to re-merge in
    // order; `entries` is the original sorted suspect list.
    owned.sort_by_key(|e| e.s_lno);
    let mut entries = std::mem::take(owned).into_iter().peekable();
    let mut deferred: Vec<BlameEntry> = Vec::new();
    let mut offset: isize = 0;

    for hunk in &hunks {
        let tlno = hunk.start_b; // first child line that differs
        let same = hunk.start_b + hunk.count_b; // first child line common again

        // Pre-chunk region [.., tlno): common with the parent → pass it through.
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, tlno) {
            if e.s_lno + e.num_lines > tlno {
                // Straddles the boundary: pass the common head, defer the tail.
                let head_len = tlno - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                pass_entry(&mut e, offset, &mut passed);
                put_back(&mut deferred, tail);
            } else {
                pass_entry(&mut e, offset, &mut passed);
            }
        }

        // Differing region [tlno, same): stays with the suspect; split a chunk
        // that reaches past `same` so its common tail is handled by a later hunk.
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, same) {
            if e.s_lno + e.num_lines > same {
                let head_len = same - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                still_ours.push(e);
                put_back(&mut deferred, tail);
            } else {
                still_ours.push(e);
            }
        }

        // Advance the offset across this hunk (parent count - child count delta).
        offset = hunk.start_a as isize + hunk.count_a as isize
            - (hunk.start_b as isize + hunk.count_b as isize);
    }

    // Everything after the last hunk is common with the parent.
    while let Some(mut e) = take_next_before(&mut deferred, &mut entries, usize::MAX) {
        pass_entry(&mut e, offset, &mut passed);
    }

    *owned = still_ours;
    passed
}

/// A changed region of a parent→child line diff: parent lines
/// `[start_a, start_a+count_a)` were replaced by child lines
/// `[start_b, start_b+count_b)`. Equivalent to one `xdl` hunk / one
/// `blame_chunk_cb` call in git.
struct DiffHunk {
    start_a: usize,
    count_a: usize,
    start_b: usize,
    count_b: usize,
}

/// Convert the run-length `DiffOp` script (parent → child) into the changed
/// hunks git's `diff_hunks` yields: maximal runs of non-`Equal` ops collapse
/// into a single `(start_a, count_a, start_b, count_b)` hunk; `Equal` runs are
/// the common stretches between hunks.
fn diff_hunks(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
    algorithm: DiffAlgorithm,
) -> Vec<DiffHunk> {
    // The contiguous-append shortcut matches git's Myers behavior for "add a
    // header and append a function" edits; the alternative algorithms
    // (patience/histogram) produce their own anchoring, so only take the
    // shortcut for the Myers family.
    if matches!(algorithm, DiffAlgorithm::Myers | DiffAlgorithm::Minimal)
        && let Some(hunks) = contiguous_parent_hunks(parent_lines, child_lines)
    {
        return hunks;
    }

    let ops = sley_diff_merge::diff_lines_with_algorithm(parent_lines, child_lines, algorithm);
    let mut hunks = Vec::new();
    let mut a = 0usize; // parent line cursor
    let mut b = 0usize; // child line cursor
    let mut pending: Option<DiffHunk> = None;
    for op in ops {
        match op {
            sley_diff_merge::DiffOp::Equal(n) => {
                if let Some(h) = pending.take() {
                    hunks.push(h);
                }
                a += n;
                b += n;
            }
            sley_diff_merge::DiffOp::Delete(n) => {
                let h = pending.get_or_insert(DiffHunk {
                    start_a: a,
                    count_a: 0,
                    start_b: b,
                    count_b: 0,
                });
                h.count_a += n;
                a += n;
            }
            sley_diff_merge::DiffOp::Insert(n) => {
                let h = pending.get_or_insert(DiffHunk {
                    start_a: a,
                    count_a: 0,
                    start_b: b,
                    count_b: 0,
                });
                h.count_b += n;
                b += n;
            }
        }
    }
    if let Some(h) = pending.take() {
        hunks.push(h);
    }
    hunks
}

/// If the whole parent image appears contiguously in the child image, prefer
/// that alignment over a generic LCS. This matches blame's desired behavior for
/// "add header and append a function" edits where repeated lines such as `}`
/// otherwise tempt Myers into matching the parent's closing brace to the later
/// appended function.
fn contiguous_parent_hunks(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
) -> Option<Vec<DiffHunk>> {
    if parent_lines.is_empty() || child_lines.len() < parent_lines.len() {
        return None;
    }
    let offset = child_lines.windows(parent_lines.len()).position(|window| {
        parent_lines
            .iter()
            .zip(window)
            .all(|(parent, child)| parent.content == child.content)
    })?;

    let mut hunks = Vec::new();
    if offset > 0 {
        hunks.push(DiffHunk {
            start_a: 0,
            count_a: 0,
            start_b: 0,
            count_b: offset,
        });
    }
    let child_after = offset + parent_lines.len();
    if child_after < child_lines.len() {
        hunks.push(DiffHunk {
            start_a: parent_lines.len(),
            count_a: 0,
            start_b: child_after,
            count_b: child_lines.len() - child_after,
        });
    }
    if hunks.is_empty() { None } else { Some(hunks) }
}

/// Pass `entry` to a parent: rebase its `s_lno` by `offset` (the parent leads
/// the child by `offset` across this common run). The suspect id is left as the
/// child's; [`queue_entries`] re-stamps it with the parent before enqueuing.
fn pass_entry(entry: &mut BlameEntry, offset: isize, passed: &mut Vec<BlameEntry>) {
    let s_lno = (entry.s_lno as isize + offset) as usize;
    passed.push(BlameEntry {
        lno: entry.lno,
        s_lno,
        num_lines: entry.num_lines,
        ignored: entry.ignored,
        unblamable: entry.unblamable,
    });
}

/// Split `e` into a head of `head_len` lines (kept in `e`) and a returned tail
/// covering the remainder, mirroring git's `split_blame_at`.
fn split_entry_at(e: &mut BlameEntry, head_len: usize) -> BlameEntry {
    let tail = BlameEntry {
        lno: e.lno + head_len,
        s_lno: e.s_lno + head_len,
        num_lines: e.num_lines - head_len,
        ignored: e.ignored,
        unblamable: e.unblamable,
    };
    e.num_lines = head_len;
    tail
}

/// Pop the next chunk whose `s_lno < limit`, in global `s_lno` order, drawing
/// from `deferred` (split tails) and `entries` (the original sorted suspect
/// chunks) — both individually sorted. Returns `None` when the next chunk in
/// order starts at or past `limit` (or both streams are exhausted).
fn take_next_before(
    deferred: &mut Vec<BlameEntry>,
    entries: &mut std::iter::Peekable<std::vec::IntoIter<BlameEntry>>,
    limit: usize,
) -> Option<BlameEntry> {
    // Determine which stream has the smaller front (and that front's s_lno)
    // without consuming it.
    let (from_deferred, next_s_lno) = match (deferred.first(), entries.peek()) {
        (Some(d), Some(e)) => {
            if d.s_lno <= e.s_lno {
                (true, d.s_lno)
            } else {
                (false, e.s_lno)
            }
        }
        (Some(d), None) => (true, d.s_lno),
        (None, Some(e)) => (false, e.s_lno),
        (None, None) => return None,
    };
    if next_s_lno >= limit {
        return None;
    }
    if from_deferred {
        Some(deferred.remove(0))
    } else {
        entries.next()
    }
}

/// Return an entry to the deferred stream, keeping it sorted by `s_lno`.
fn put_back(deferred: &mut Vec<BlameEntry>, e: BlameEntry) {
    let pos = deferred
        .iter()
        .position(|d| d.s_lno > e.s_lno)
        .unwrap_or(deferred.len());
    deferred.insert(pos, e);
}

/// Queue a batch of chunks onto `parent`'s suspect list, stamping them with the
/// parent id and enqueuing the parent commit if it was not already pending.
fn queue_entries(
    suspects: &mut HashMap<OriginKey, Vec<BlameEntry>>,
    queue: &mut Vec<OriginKey>,
    origin: OriginKey,
    entries: Vec<BlameEntry>,
) {
    let slot = suspects.entry(origin.clone()).or_default();
    let was_empty = slot.is_empty();
    slot.extend(entries);
    if was_empty {
        queue.push(origin);
    }
}

/// Charge every remaining chunk to `commit_oid` as a final attribution. `seq` is
/// the walk-order sequence number of this charge event (for `--incremental`).
fn charge_remaining(
    result: &mut [Option<LineBlame>],
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    origin: &OriginKey,
    boundary: bool,
    owned: Vec<BlameEntry>,
    seq: usize,
) {
    for entry in owned {
        for k in 0..entry.num_lines {
            let final_line = entry.lno + k;
            if let Some(slot) = result.get_mut(final_line)
                && slot.is_none()
            {
                *slot = Some(LineBlame {
                    commit: origin.commit,
                    boundary,
                    origin_path: origin.path.clone(),
                    origin_lineno: entry.s_lno + k + 1,
                    content: final_lines[final_line].content.to_vec(),
                    ignored: entry.ignored,
                    unblamable: entry.unblamable,
                    charge_seq: seq,
                });
            }
        }
    }
}

fn find_parent_origin(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    origin: &OriginKey,
    parent: &ObjectId,
    allow_whole_copy: bool,
    reader: &dyn BlameObjectSource,
) -> Result<Option<OriginKey>> {
    if read_path_blob(db, format, parent, &origin.path, reader)?.is_some() {
        return Ok(Some(OriginKey {
            commit: *parent,
            path: origin.path.clone(),
            virtual_worktree: false,
        }));
    }

    let parent_tree = peel_to_tree(db, format, parent)?;
    let child_tree = peel_to_tree(db, format, &origin.commit)?;
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        &parent_tree,
        &child_tree,
        sley_diff_merge::DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: allow_whole_copy,
            find_copies_harder: allow_whole_copy,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
        },
    )?;

    for entry in entries {
        if entry.path.as_bytes() != origin.path.as_bytes() {
            continue;
        }
        let is_origin = matches!(entry.status, sley_diff_merge::NameStatus::Renamed(_))
            || (allow_whole_copy && matches!(entry.status, sley_diff_merge::NameStatus::Copied(_)));
        if is_origin && let Some(old_path) = entry.old_path {
            return Ok(Some(OriginKey {
                commit: *parent,
                path: String::from_utf8_lossy(old_path.as_bytes()).into_owned(),
                virtual_worktree: false,
            }));
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn find_copies_in_parents(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    origin: &OriginKey,
    parents: &[ObjectId],
    copy_level: u8,
    copy_score: usize,
    owned: &mut Vec<BlameEntry>,
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    converter: &mut dyn BlameContentConverter,
    reader: &dyn BlameObjectSource,
) -> Result<Vec<(OriginKey, Vec<BlameEntry>)>> {
    let mut copied_by_origin: Vec<(OriginKey, Vec<BlameEntry>)> = Vec::new();
    for parent in parents {
        if owned.is_empty() {
            break;
        }
        let candidate_paths = copy_candidate_paths(db, format, origin, parent, copy_level, reader)?;
        for path in candidate_paths {
            if owned.is_empty() {
                break;
            }
            if path == origin.path {
                continue;
            }
            let Some((raw, mode)) = read_path_blob(db, format, parent, &path, reader)? else {
                continue;
            };
            let blob = converter.convert(&path, mode, raw)?;
            let source_lines = sley_diff_merge::split_lines(&blob);
            let mut next_owned = Vec::new();
            let mut copied = Vec::new();
            for entry in std::mem::take(owned) {
                split_copy_matches(
                    entry,
                    final_lines,
                    &source_lines,
                    copy_score,
                    &mut copied,
                    &mut next_owned,
                );
            }
            *owned = next_owned;
            if !copied.is_empty() {
                copied_by_origin.push((
                    OriginKey {
                        commit: *parent,
                        path,
                        virtual_worktree: false,
                    },
                    copied,
                ));
            }
        }
    }
    Ok(copied_by_origin)
}

fn copy_candidate_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    origin: &OriginKey,
    parent: &ObjectId,
    copy_level: u8,
    reader: &dyn BlameObjectSource,
) -> Result<Vec<String>> {
    let parent_tree = peel_to_tree(db, format, parent)?;
    let use_all = copy_level >= 3
        || (copy_level >= 2 && read_path_blob(db, format, parent, &origin.path, reader)?.is_none());
    if use_all {
        let mut out = Vec::new();
        collect_tree_blob_paths(db, format, &parent_tree, Vec::new(), &mut out)?;
        return Ok(out);
    }

    let child_tree = peel_to_tree(db, format, &origin.commit)?;
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        &parent_tree,
        &child_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    let mut out = Vec::new();
    for entry in entries {
        if let Some(old_path) = entry.old_path {
            out.push(String::from_utf8_lossy(old_path.as_bytes()).into_owned());
        } else if matches!(
            entry.status,
            sley_diff_merge::NameStatus::Deleted | sley_diff_merge::NameStatus::Modified
        ) {
            out.push(String::from_utf8_lossy(entry.path.as_bytes()).into_owned());
        }
    }
    Ok(out)
}

fn collect_tree_blob_paths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    out: &mut Vec<String>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        match sley_object::tree_entry_object_type(entry.mode) {
            ObjectType::Tree => collect_tree_blob_paths(db, format, &entry.oid, path, out)?,
            ObjectType::Blob => out.push(String::from_utf8_lossy(&path).into_owned()),
            _ => {}
        }
    }
    Ok(())
}

fn split_copy_matches(
    entry: BlameEntry,
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    source_lines: &[sley_diff_merge::DiffLine<'_>],
    copy_score: usize,
    copied: &mut Vec<BlameEntry>,
    remaining: &mut Vec<BlameEntry>,
) {
    let line_in_source = |idx: usize| {
        source_lines.iter().any(|line| {
            line.content == final_lines[idx].content
                && line.has_newline == final_lines[idx].has_newline
        })
    };
    let mut cursor = 0usize;
    while cursor < entry.num_lines {
        let final_idx = entry.lno + cursor;
        let source_idx = source_lines.iter().position(|line| {
            line.content == final_lines[final_idx].content
                && line.has_newline == final_lines[final_idx].has_newline
        });
        let Some(source_start) = source_idx else {
            // Coalesce the whole run of lines absent from the source into one
            // remaining entry, so a *later* candidate can still match it as a
            // contiguous block (git splits the suspect only at match boundaries).
            // Fragmenting into single lines would drop every run below the copy
            // score threshold.
            let mut len = 1usize;
            while cursor + len < entry.num_lines && !line_in_source(entry.lno + cursor + len) {
                len += 1;
            }
            push_entry_slice(&entry, cursor, len, remaining, entry.s_lno + cursor);
            cursor += len;
            continue;
        };

        let mut len = 1usize;
        while cursor + len < entry.num_lines
            && source_start + len < source_lines.len()
            && source_lines[source_start + len].content
                == final_lines[entry.lno + cursor + len].content
            && source_lines[source_start + len].has_newline
                == final_lines[entry.lno + cursor + len].has_newline
        {
            len += 1;
        }

        if blame_entry_score(final_lines, entry.lno + cursor, len) >= copy_score {
            push_entry_slice(&entry, cursor, len, copied, source_start);
        } else {
            push_entry_slice(&entry, cursor, len, remaining, entry.s_lno + cursor);
        }
        cursor += len;
    }
}

fn push_entry_slice(
    entry: &BlameEntry,
    offset: usize,
    len: usize,
    out: &mut Vec<BlameEntry>,
    source_lno: usize,
) {
    out.push(BlameEntry {
        lno: entry.lno + offset,
        s_lno: source_lno,
        num_lines: len,
        ignored: entry.ignored,
        unblamable: entry.unblamable,
    });
}

fn blame_entry_score(
    final_lines: &[sley_diff_merge::DiffLine<'_>],
    start: usize,
    len: usize,
) -> usize {
    let mut score = 1usize;
    for line in &final_lines[start..start + len] {
        score += line
            .content
            .iter()
            .filter(|byte| byte.is_ascii_alphanumeric())
            .count();
    }
    score
}

// ===========================================================================
// `--ignore-rev` fuzzy line matching (blame.c fingerprints + guess_line_blames)
// ===========================================================================

/// A line fingerprint: the multiset of consecutive character bigrams of the
/// line (git's `struct fingerprint`). Letters are lowercased and runs of
/// whitespace collapse to a `\0` boundary, so similar lines that differ only in
/// case or spacing still share most of their bigrams. Whitespace-pair bigrams
/// (`hash == 0`) are ignored.
#[derive(Clone, Default)]
struct Fingerprint {
    counts: HashMap<u32, i32>,
}

/// Lowercase an ASCII letter (C-locale `tolower`); leave other bytes untouched.
fn ascii_tolower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
    }
}

/// C-locale `isspace`.
fn is_ascii_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Build the fingerprint of one line (its content, newline excluded — git
/// folds the trailing newline into the same `\0` boundary as end-of-line).
fn get_fingerprint(line: &[u8]) -> Fingerprint {
    let mut counts: HashMap<u32, i32> = HashMap::new();
    let mut c0: u32 = 0;
    for i in 0..=line.len() {
        let c1: u32 = if i == line.len() || is_ascii_space(line[i]) {
            0
        } else {
            ascii_tolower(line[i]) as u32
        };
        let hash = c0 | (c1 << 8);
        if hash != 0 {
            *counts.entry(hash).or_insert(0) += 1;
        }
        c0 = c1;
    }
    Fingerprint { counts }
}

/// Fingerprints for every line of a blob, in line order.
fn line_fingerprints(lines: &[sley_diff_merge::DiffLine<'_>]) -> Vec<Fingerprint> {
    lines
        .iter()
        .map(|line| get_fingerprint(line.content))
        .collect()
}

/// git's `fingerprint_similarity`: the size of the bigram-multiset intersection
/// (sum of per-bigram minimum counts).
fn fingerprint_similarity(a: &Fingerprint, b: &Fingerprint) -> i32 {
    let mut intersection = 0;
    for (hash, count_b) in &b.counts {
        if let Some(count_a) = a.counts.get(hash) {
            intersection += (*count_a).min(*count_b);
        }
    }
    intersection
}

/// git's `fingerprint_subtract`: remove `b`'s bigrams from `a` (saturating at
/// zero), so a line in A that has been claimed can't be matched as strongly
/// again.
fn fingerprint_subtract(a: &mut Fingerprint, b: &Fingerprint) {
    for (hash, count_b) in &b.counts {
        if let Some(count_a) = a.counts.get_mut(hash) {
            if *count_a <= *count_b {
                a.counts.remove(hash);
            } else {
                *count_a -= *count_b;
            }
        }
    }
}

/// git's `line_number_mapping`: maps a line number in B to its proportional
/// position in A.
struct LineNumberMapping {
    destination_start: i32,
    destination_length: i32,
    source_start: i32,
    source_length: i32,
}

fn map_line_number(line_number: i32, mapping: &LineNumberMapping) -> i32 {
    ((line_number - mapping.source_start) * 2 + 1) * mapping.destination_length
        / (mapping.source_length * 2)
        + mapping.destination_start
}

const CERTAINTY_NOT_CALCULATED: i32 = -1;
const CERTAIN_NOTHING_MATCHES: i32 = -2;

/// State threaded through the recursive fuzzy matcher. Arrays indexed by
/// `b_idx = absolute_B_line - orig_start_b` (git re-bases pointers per
/// recursion; we keep absolute-into-B indices, which is equivalent).
struct FuzzyState<'a> {
    parent_fps: &'a mut [Fingerprint],
    target_fps: &'a [Fingerprint],
    similarities: Vec<i32>,
    certainties: Vec<i32>,
    second_best_result: Vec<i32>,
    result: Vec<i32>,
    max_search_distance_a: i32,
    max_search_distance_b: i32,
    orig_start_b: i32,
    mapping: LineNumberMapping,
}

/// Index into the flat `similarities` array (git's `get_similarity`).
fn similarity_index(
    line_a: i32,
    b_idx: i32,
    closest_line_a: i32,
    max_search_distance_a: i32,
) -> usize {
    (line_a - closest_line_a + max_search_distance_a + b_idx * (max_search_distance_a * 2 + 1))
        as usize
}

/// git's `find_best_line_matches` for a single line in B.
fn find_best_line_matches(state: &mut FuzzyState, start_a: i32, length_a: i32, abs_b: i32) {
    let b_idx = (abs_b - state.orig_start_b) as usize;
    if state.certainties[b_idx] != CERTAINTY_NOT_CALCULATED {
        return;
    }

    let closest_local_line_a = map_line_number(abs_b, &state.mapping) - start_a;
    let mut search_start = closest_local_line_a - state.max_search_distance_a;
    if search_start < 0 {
        search_start = 0;
    }
    let mut search_end = closest_local_line_a + state.max_search_distance_a + 1;
    if search_end > length_a {
        search_end = length_a;
    }

    let mut best_similarity = 0;
    let mut second_best_similarity = 0;
    let mut best_similarity_index = 0;
    let mut second_best_similarity_index = 0;

    for i in search_start..search_end {
        let sim_idx = similarity_index(
            i,
            b_idx as i32,
            closest_local_line_a,
            state.max_search_distance_a,
        );
        if state.similarities[sim_idx] == -1 {
            let sim = fingerprint_similarity(
                &state.target_fps[abs_b as usize],
                &state.parent_fps[(start_a + i) as usize],
            ) * (1000 - (i - closest_local_line_a).abs());
            state.similarities[sim_idx] = sim;
        }
        let similarity = state.similarities[sim_idx];
        if similarity > best_similarity {
            second_best_similarity = best_similarity;
            second_best_similarity_index = best_similarity_index;
            best_similarity = similarity;
            best_similarity_index = i;
        } else if similarity > second_best_similarity {
            second_best_similarity = similarity;
            second_best_similarity_index = i;
        }
    }

    if best_similarity == 0 {
        state.certainties[b_idx] = CERTAIN_NOTHING_MATCHES;
        state.result[b_idx] = -1;
    } else {
        state.certainties[b_idx] = best_similarity * 2 - second_best_similarity;
        state.result[b_idx] = start_a + best_similarity_index;
        state.second_best_result[b_idx] = start_a + second_best_similarity_index;
    }
}

/// git's `fuzzy_find_matching_lines_recurse`: pick the most confidently matched
/// line as a partition, subtract its fingerprint, invalidate contradicting
/// neighbours, then recurse on either side.
fn fuzzy_recurse(state: &mut FuzzyState, start_a: i32, start_b: i32, length_a: i32, length_b: i32) {
    let mut most_certain_local_line_b: i32 = -1;
    let mut most_certain_line_certainty: i32 = -1;
    for i in 0..length_b {
        let abs_b = start_b + i;
        find_best_line_matches(state, start_a, length_a, abs_b);
        let b_idx = (abs_b - state.orig_start_b) as usize;
        if state.certainties[b_idx] > most_certain_line_certainty {
            most_certain_line_certainty = state.certainties[b_idx];
            most_certain_local_line_b = i;
        }
    }

    if most_certain_local_line_b == -1 {
        return;
    }

    let most_certain_abs_b = start_b + most_certain_local_line_b;
    let most_certain_b_idx = (most_certain_abs_b - state.orig_start_b) as usize;
    let most_certain_line_a = state.result[most_certain_b_idx];

    // Subtract the chosen B line's fingerprint from its matched A line.
    let (_left, right) = state.parent_fps.split_at_mut(most_certain_line_a as usize);
    let target_fp = &state.target_fps[most_certain_abs_b as usize];
    fingerprint_subtract(&mut right[0], target_fp);

    let mut invalidate_min = most_certain_local_line_b - state.max_search_distance_b;
    let mut invalidate_max = most_certain_local_line_b + state.max_search_distance_b + 1;
    if invalidate_min < 0 {
        invalidate_min = 0;
    }
    if invalidate_max > length_b {
        invalidate_max = length_b;
    }

    // The matched A fingerprint changed: discard cached similarities against it.
    for i in invalidate_min..invalidate_max {
        let abs_b = start_b + i;
        let closest_local_line_a = map_line_number(abs_b, &state.mapping) - start_a;
        if (most_certain_line_a - start_a - closest_local_line_a).abs()
            > state.max_search_distance_a
        {
            continue;
        }
        let sim_idx = similarity_index(
            most_certain_line_a - start_a,
            abs_b - state.orig_start_b,
            closest_local_line_a,
            state.max_search_distance_a,
        );
        state.similarities[sim_idx] = -1;
    }

    // Discard matches whose ordering now contradicts the partition.
    let mut i = most_certain_local_line_b - 1;
    while i >= invalidate_min {
        let b_idx = (start_b + i - state.orig_start_b) as usize;
        if state.certainties[b_idx] >= 0
            && (state.result[b_idx] >= most_certain_line_a
                || state.second_best_result[b_idx] >= most_certain_line_a)
        {
            state.certainties[b_idx] = CERTAINTY_NOT_CALCULATED;
        }
        i -= 1;
    }
    for i in (most_certain_local_line_b + 1)..invalidate_max {
        let b_idx = (start_b + i - state.orig_start_b) as usize;
        if state.certainties[b_idx] >= 0
            && (state.result[b_idx] <= most_certain_line_a
                || state.second_best_result[b_idx] <= most_certain_line_a)
        {
            state.certainties[b_idx] = CERTAINTY_NOT_CALCULATED;
        }
    }

    if most_certain_local_line_b > 0 {
        fuzzy_recurse(
            state,
            start_a,
            start_b,
            most_certain_line_a + 1 - start_a,
            most_certain_local_line_b,
        );
    }
    if most_certain_local_line_b + 1 < length_b {
        let second_half_start_a = most_certain_line_a;
        let offset_b = most_certain_local_line_b + 1;
        let second_half_start_b = start_b + offset_b;
        let second_half_length_a = length_a + start_a - second_half_start_a;
        let second_half_length_b = length_b + start_b - second_half_start_b;
        fuzzy_recurse(
            state,
            second_half_start_a,
            second_half_start_b,
            second_half_length_a,
            second_half_length_b,
        );
    }
}

/// git's `fuzzy_find_matching_lines`: for each line of the target chunk
/// `[tlno, same)`, return the absolute parent line it best matches, or `-1`.
/// `parent_slno` is the parent chunk start, `parent_len` its length.
fn fuzzy_find_matching_lines(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    tlno: i32,
    parent_slno: i32,
    same: i32,
    parent_len: i32,
) -> Option<Vec<i32>> {
    let start_a = parent_slno;
    let length_a = parent_len;
    let start_b = tlno;
    let length_b = same - tlno;

    if length_a <= 0 {
        return None;
    }

    let mut max_search_distance_a = 10;
    if max_search_distance_a >= length_a {
        max_search_distance_a = if length_a != 0 { length_a - 1 } else { 0 };
    }
    let max_search_distance_b = ((2 * max_search_distance_a + 1) * length_b - 1) / length_a;

    let similarity_count = (length_b * (max_search_distance_a * 2 + 1)) as usize;
    let mut parent_copy = parent_fps.to_vec();
    let mut state = FuzzyState {
        parent_fps: &mut parent_copy,
        target_fps,
        similarities: vec![-1; similarity_count],
        certainties: vec![CERTAINTY_NOT_CALCULATED; length_b as usize],
        second_best_result: vec![-1; length_b as usize],
        result: vec![-1; length_b as usize],
        max_search_distance_a,
        max_search_distance_b,
        orig_start_b: start_b,
        mapping: LineNumberMapping {
            destination_start: start_a,
            destination_length: length_a,
            source_start: start_b,
            source_length: length_b,
        },
    };

    fuzzy_recurse(&mut state, start_a, start_b, length_a, length_b);
    Some(state.result)
}

/// git's `scan_parent_range`: the second-pass fallback that scans a range of
/// parent lines for the best fingerprint match of one target line.
fn scan_parent_range(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    t_idx: usize,
    from: usize,
    nr_lines: usize,
) -> i32 {
    const FINGERPRINT_FILE_THRESHOLD: i32 = 10;
    let mut best_sim_val = FINGERPRINT_FILE_THRESHOLD;
    let mut best_sim_idx: i32 = -1;
    for (p_idx, parent_fp) in parent_fps.iter().enumerate().skip(from).take(nr_lines) {
        let sim = fingerprint_similarity(&target_fps[t_idx], parent_fp);
        if sim < best_sim_val {
            continue;
        }
        if sim == best_sim_val
            && best_sim_idx != -1
            && (best_sim_idx - t_idx as i32).abs() < (p_idx as i32 - t_idx as i32).abs()
        {
            continue;
        }
        best_sim_val = sim;
        best_sim_idx = p_idx as i32;
    }
    best_sim_idx
}

/// Where a target line in an ignored commit's change should be attributed.
#[derive(Clone, Copy)]
struct LineTracker {
    /// True when the line maps to a parent line (→ passes to parent, `ignored`);
    /// false when it has no parent match (→ stays, `unblamable`).
    is_parent: bool,
    /// The matched parent line (parent space) when `is_parent`, else the
    /// target line (target space).
    s_lno: i32,
}

fn are_lines_adjacent(first: &LineTracker, second: &LineTracker) -> bool {
    first.is_parent == second.is_parent && first.s_lno + 1 == second.s_lno
}

/// git's `guess_line_blames`: for each line of the changed region `[tlno,
/// same)`, decide whether it maps to a parent line (fuzzy match in the diff
/// chunk, else anywhere in the parent) or is unblamable.
fn guess_line_blames(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    tlno: i32,
    offset: i32,
    same: i32,
    parent_len: i32,
) -> Vec<LineTracker> {
    let parent_slno = tlno + offset;
    let fuzzy_matches =
        fuzzy_find_matching_lines(parent_fps, target_fps, tlno, parent_slno, same, parent_len);
    let count = (same - tlno) as usize;
    let validate_tiny_parent_match = parent_len <= 1 && count > parent_len.max(0) as usize;
    let mut line_blames = Vec::with_capacity(count);
    for i in 0..count {
        let target_idx = tlno + i as i32;
        let best_idx = match &fuzzy_matches {
            Some(m) if m[i] >= 0 => {
                let fuzzy_idx = m[i];
                if validate_tiny_parent_match {
                    prefer_whole_parent_match(parent_fps, target_fps, target_idx, fuzzy_idx)
                } else {
                    fuzzy_idx
                }
            }
            _ => scan_parent_range(
                parent_fps,
                target_fps,
                target_idx as usize,
                0,
                parent_fps.len(),
            ),
        };
        if best_idx >= 0 {
            line_blames.push(LineTracker {
                is_parent: true,
                s_lno: best_idx,
            });
        } else {
            line_blames.push(LineTracker {
                is_parent: false,
                s_lno: target_idx,
            });
        }
    }
    line_blames
}

fn prefer_whole_parent_match(
    parent_fps: &[Fingerprint],
    target_fps: &[Fingerprint],
    target_idx: i32,
    fuzzy_idx: i32,
) -> i32 {
    let scan_idx = scan_parent_range(
        parent_fps,
        target_fps,
        target_idx as usize,
        0,
        parent_fps.len(),
    );
    if scan_idx < 0 {
        return fuzzy_idx;
    }

    let target = &target_fps[target_idx as usize];
    let fuzzy_score = fingerprint_similarity(target, &parent_fps[fuzzy_idx as usize]);
    let scan_score = fingerprint_similarity(target, &parent_fps[scan_idx as usize]);
    if scan_score > fuzzy_score {
        scan_idx
    } else {
        fuzzy_idx
    }
}

/// git's `ignore_blame_entry`: split a blame entry over the changed region into
/// runs that go to the parent (`ignored`) or stay (`unblamable`), per the
/// `line_blames` decisions. `region_start` is `tlno` (the first target line the
/// `line_blames` array describes).
fn ignore_blame_entry(
    entry: &BlameEntry,
    region_start: usize,
    line_blames: &[LineTracker],
    passed: &mut Vec<BlameEntry>,
    still_ours: &mut Vec<BlameEntry>,
) {
    let n = entry.num_lines;
    let base = entry.s_lno - region_start;
    let mut i = 0usize;
    while i < n {
        let mut len = 1usize;
        while i + len < n
            && are_lines_adjacent(
                &line_blames[base + i + len - 1],
                &line_blames[base + i + len],
            )
        {
            len += 1;
        }
        let head = &line_blames[base + i];
        let part = BlameEntry {
            lno: entry.lno + i,
            s_lno: if head.is_parent {
                head.s_lno as usize
            } else {
                entry.s_lno + i
            },
            num_lines: len,
            ignored: if head.is_parent { true } else { entry.ignored },
            unblamable: if head.is_parent {
                entry.unblamable
            } else {
                true
            },
        };
        if head.is_parent {
            passed.push(part);
        } else {
            still_ours.push(part);
        }
        i += len;
    }
}

/// The `--ignore-rev` analogue of [`pass_blame_to_parent`] (git's `blame_chunk`
/// with `ignore_diffs = 1`): unchanged lines before each diff hunk pass to the
/// parent normally; lines inside a hunk are routed by `guess_line_blames` —
/// fuzzy-matched lines pass to the parent marked `ignored`, the rest stay marked
/// `unblamable`.
fn pass_blame_to_parent_ignore(
    parent_lines: &[sley_diff_merge::DiffLine<'_>],
    child_lines: &[sley_diff_merge::DiffLine<'_>],
    parent_fps: &[Fingerprint],
    child_fps: &[Fingerprint],
    owned: &mut Vec<BlameEntry>,
    algorithm: DiffAlgorithm,
) -> Vec<BlameEntry> {
    let hunks = diff_hunks(parent_lines, child_lines, algorithm);

    let mut passed: Vec<BlameEntry> = Vec::new();
    let mut still_ours: Vec<BlameEntry> = Vec::new();

    owned.sort_by_key(|e| e.s_lno);
    let mut entries = std::mem::take(owned).into_iter().peekable();
    let mut deferred: Vec<BlameEntry> = Vec::new();
    let mut offset: isize = 0;

    for hunk in &hunks {
        let tlno = hunk.start_b;
        let same = hunk.start_b + hunk.count_b;

        // Pre-chunk common region: pass to the parent (flags preserved).
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, tlno) {
            if e.s_lno + e.num_lines > tlno {
                let head_len = tlno - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                pass_entry(&mut e, offset, &mut passed);
                put_back(&mut deferred, tail);
            } else {
                pass_entry(&mut e, offset, &mut passed);
            }
        }

        // Changed region [tlno, same): guess each line's origin.
        let line_blames = guess_line_blames(
            parent_fps,
            child_fps,
            tlno as i32,
            offset as i32,
            same as i32,
            hunk.count_a as i32,
        );
        while let Some(mut e) = take_next_before(&mut deferred, &mut entries, same) {
            if e.s_lno + e.num_lines > same {
                let head_len = same - e.s_lno;
                let tail = split_entry_at(&mut e, head_len);
                ignore_blame_entry(&e, tlno, &line_blames, &mut passed, &mut still_ours);
                put_back(&mut deferred, tail);
            } else {
                ignore_blame_entry(&e, tlno, &line_blames, &mut passed, &mut still_ours);
            }
        }

        offset = hunk.start_a as isize + hunk.count_a as isize
            - (hunk.start_b as isize + hunk.count_b as isize);
    }

    // Trailing common region: pass to the parent.
    while let Some(mut e) = take_next_before(&mut deferred, &mut entries, usize::MAX) {
        pass_entry(&mut e, offset, &mut passed);
    }

    *owned = still_ours;
    passed
}

/// The set of `tip` and all commits reachable from it (its ancestor closure),
/// used to mark `git blame ^<rev>` boundaries as uninteresting.
fn ancestors_closure(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip: &ObjectId,
) -> Result<HashSet<ObjectId>> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut stack = vec![*tip];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse(format, &object.body)?;
        for parent in &commit.parents {
            stack.push(*parent);
        }
    }
    Ok(seen)
}

/// Read (and memoise) the blob for `repo_path` at `commit`. `None` means the
/// path is absent (or names a non-blob) at that commit.
///
/// The cache stores `Arc` copies of the *converted* form so textconv runs once
/// per (commit, path) and a hit shares the bytes instead of cloning them
/// (upstream's fill_origin_blob caches likewise).
fn cached_blob(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit: &ObjectId,
    repo_path: &str,
    cache: &mut HashMap<(ObjectId, String), Option<Arc<Vec<u8>>>>,
    reader: &dyn BlameObjectSource,
    converter: &mut dyn BlameContentConverter,
) -> Result<Option<Arc<Vec<u8>>>> {
    let key = (*commit, repo_path.to_string());
    if let Some(hit) = cache.get(&key) {
        return Ok(hit.clone());
    }
    let blob = match read_path_blob(db, format, commit, repo_path, reader)? {
        Some((raw, mode)) => Some(Arc::new(converter.convert(repo_path, mode, raw)?)),
        None => None,
    };
    cache.insert(key, blob.clone());
    Ok(blob)
}

/// The committer timestamp of an identity line, 0 when missing/unparsable (the
/// same epoch sentinel git stores in `commit->date`).
fn identity_timestamp(ident: &[u8]) -> i64 {
    sley_core::split_ident_line(ident)
        .and_then(|fields| fields.date)
        .and_then(|date| std::str::from_utf8(date).ok())
        .and_then(|date| date.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Pop the newest-by-committer-date commit from the working queue, mirroring
/// git's commit-date priority queue (`compare_commits_by_commit_date`: larger
/// date first, ties broken deterministically by ascending hex id). Commit dates
/// are memoised in `date_cache`. Returns `None` when the queue is empty.
///
/// The queue may transiently hold the same commit twice (it is enqueued when
/// its suspect list first becomes non-empty); a duplicate pops with an empty
/// suspect list and the caller simply skips it, exactly as git's `assign_blame`
/// does.
fn pop_newest_origin(
    queue: &mut Vec<OriginKey>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    date_cache: &mut HashMap<ObjectId, i64>,
) -> Result<Option<OriginKey>> {
    if queue.is_empty() {
        return Ok(None);
    }
    // Ensure every queued commit has a cached date.
    for oid in queue.iter().map(|origin| origin.commit) {
        if let std::collections::hash_map::Entry::Vacant(e) = date_cache.entry(oid) {
            let object = db.read_object(&oid)?;
            if object.object_type != ObjectType::Commit {
                return Err(GitError::InvalidObject(format!(
                    "expected commit {oid}, found {}",
                    object.object_type.as_str()
                )));
            }
            let commit = Commit::parse(format, &object.body)?;
            let ts = identity_timestamp(&commit.committer);
            e.insert(ts);
        }
    }
    let mut best = 0usize;
    for i in 1..queue.len() {
        let ti = date_cache.get(&queue[i].commit).copied().unwrap_or(0);
        let tb = date_cache.get(&queue[best].commit).copied().unwrap_or(0);
        if ti > tb
            || (ti == tb
                && (queue[i].commit.to_hex(), &queue[i].path)
                    < (queue[best].commit.to_hex(), &queue[best].path))
        {
            best = i;
        }
    }
    Ok(Some(queue.swap_remove(best)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(blob: &[u8]) -> Vec<sley_diff_merge::DiffLine<'_>> {
        sley_diff_merge::split_lines(blob)
    }

    #[test]
    fn diff_hunks_collapses_change_runs() {
        // parent: a b c   child: a X c  -> one hunk replacing line 2 (0-based 1).
        let p = lines(b"a\nb\nc\n");
        let c = lines(b"a\nX\nc\n");
        let h = diff_hunks(&p, &c, sley_diff_merge::DiffAlgorithm::Myers);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].start_a, h[0].count_a), (1, 1));
        assert_eq!((h[0].start_b, h[0].count_b), (1, 1));
    }

    #[test]
    fn diff_hunks_pure_insertion() {
        // parent: a c   child: a b c -> insert one child line at index 1.
        let p = lines(b"a\nc\n");
        let c = lines(b"a\nb\nc\n");
        let h = diff_hunks(&p, &c, sley_diff_merge::DiffAlgorithm::Myers);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].count_a, 0);
        assert_eq!((h[0].start_b, h[0].count_b), (1, 1));
    }

    #[test]
    fn diff_hunks_identical_is_empty() {
        let p = lines(b"a\nb\n");
        let c = lines(b"a\nb\n");
        assert!(diff_hunks(&p, &c, sley_diff_merge::DiffAlgorithm::Myers).is_empty());
    }

    /// A chunk that is wholly preserved by the parent migrates entirely, with
    /// its `s_lno` rebased by the offset of any inserted lines above it.
    #[test]
    fn pass_blame_routes_preserved_lines_to_parent() {
        // parent: a b   child: X a b  (one line inserted at the top). Child
        // lines 1 and 2 (a, b) are preserved; they rebase to parent lines 0, 1.
        let p = lines(b"a\nb\n");
        let c = lines(b"X\na\nb\n");
        let mut owned = vec![BlameEntry {
            lno: 0,
            s_lno: 0,
            num_lines: 3,
            ..Default::default()
        }];
        let passed =
            pass_blame_to_parent(&p, &c, &mut owned, sley_diff_merge::DiffAlgorithm::Myers);
        // The inserted line (child s_lno 0) stays with the child; lines 1..3 go
        // to the parent at parent-s_lno 0..2.
        let ours_lines: usize = owned.iter().map(|e| e.num_lines).sum();
        let passed_lines: usize = passed.iter().map(|e| e.num_lines).sum();
        assert_eq!(ours_lines, 1, "the inserted line stays with the child");
        assert_eq!(passed_lines, 2, "the two preserved lines go to the parent");
        // Preserved chunk rebased: child s_lno 1 -> parent s_lno 0.
        let first = passed
            .iter()
            .min_by_key(|e| e.lno)
            .expect("a preserved chunk was passed to the parent");
        assert_eq!(first.lno, 1);
        assert_eq!(first.s_lno, 0);
    }

    #[test]
    fn pass_blame_charges_changed_lines_to_child() {
        // parent: a b c   child: a Z c — only the middle line changed.
        let p = lines(b"a\nb\nc\n");
        let c = lines(b"a\nZ\nc\n");
        let mut owned = vec![BlameEntry {
            lno: 0,
            s_lno: 0,
            num_lines: 3,
            ..Default::default()
        }];
        let passed =
            pass_blame_to_parent(&p, &c, &mut owned, sley_diff_merge::DiffAlgorithm::Myers);
        let ours: usize = owned.iter().map(|e| e.num_lines).sum();
        let to_parent: usize = passed.iter().map(|e| e.num_lines).sum();
        assert_eq!(ours, 1, "the changed middle line is charged to the child");
        assert_eq!(to_parent, 2, "the unchanged a/c lines pass to the parent");
    }

    #[test]
    fn blame_ignore_tiny_parent_hunk_prefers_whole_parent_exact_match() {
        let parent = lines(b"#include \"c.h\"\n#include \"b.h\"\n#include \"a.h\"\n#include \"e.h\"\n#include \"d.h\"\n");
        let target = lines(b"#include \"a.h\"\n#include \"b.h\"\n#include \"c.h\"\n#include \"d.h\"\n#include \"e.h\"\n");
        let parent_fps = line_fingerprints(&parent);
        let target_fps = line_fingerprints(&target);

        // Sley's Myers op stream splits this reorder so the middle target
        // region (`b.h`, `c.h`) is compared with only the old `e.h` line. Git's
        // ignore pass still recovers the exact whole-parent matches.
        let line_blames = guess_line_blames(&parent_fps, &target_fps, 1, 2, 3, 1);

        assert_eq!(line_blames.len(), 2);
        assert!(line_blames.iter().all(|line| line.is_parent));
        assert_eq!(line_blames[0].s_lno, 1);
        assert_eq!(line_blames[1].s_lno, 0);
    }

    #[test]
    fn split_entry_at_partitions_lines() {
        let mut e = BlameEntry {
            lno: 10,
            s_lno: 4,
            num_lines: 5,
            ..Default::default()
        };
        let tail = split_entry_at(&mut e, 2);
        assert_eq!((e.lno, e.s_lno, e.num_lines), (10, 4, 2));
        assert_eq!((tail.lno, tail.s_lno, tail.num_lines), (12, 6, 3));
    }
}
