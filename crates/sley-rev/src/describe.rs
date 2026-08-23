//! The `git describe` candidate-search core (`builtin/describe.c`'s
//! `describe_commit` walk + `finish_depth_computation`).
//!
//! Tags are gathered above the seam (ref walking + `--match`/`--exclude`
//! filtering stay with the caller); this module runs the commit-date-ordered
//! priority walk from the target commit that registers candidates as it
//! reaches their tagged commits, propagates each candidate's reachability flag
//! to ancestors so its depth equals the count of commits reachable from the
//! target but not from the candidate (`git rev-list <target> ^<tag>`), picks
//! the winner (smallest depth, ties broken by registration order), and finishes
//! the winner's depth over any commits left unprocessed.

use std::collections::{BinaryHeap, HashMap, HashSet};

use sley_core::{ObjectFormat, ObjectId, Result};
use sley_odb::FileObjectDatabase;

use crate::CommitMetadataReader;

/// One gathered tag as the search walk sees it: the commit it names, its
/// display name (for the `--debug` trace), whether it may serve as a
/// candidate under the caller's mode (`--tags`/`--all`; annotated tags
/// always qualify), and whether it is an annotated tag (drives the
/// "try --tags" hint when none are reachable).
pub struct DescribeCandidate {
    /// The commit the tag points at (peeled).
    pub commit: ObjectId,
    /// Display name, e.g. `v1.0`.
    pub name: String,
    /// Whether this tag may be registered as a candidate under the active mode.
    pub eligible: bool,
    /// Whether the tag is an annotated tag object.
    pub annotated: bool,
}

/// Walk controls lifted from the CLI options.
#[derive(Clone, Copy)]
pub struct DescribeWalkOptions {
    /// `--candidates=<n>` budget (0 = exact match only).
    pub max_candidates: usize,
    /// Follow only the first parent of merges.
    pub first_parent: bool,
    /// `--debug`: mirror upstream's walk trace on stderr.
    pub debug: bool,
}

impl Default for DescribeWalkOptions {
    fn default() -> Self {
        Self {
            max_candidates: 10,
            first_parent: false,
            debug: false,
        }
    }
}

/// The winning candidate: the tagged commit plus its computed depth.
pub struct DescribeWinner {
    /// Identifies the winning [`DescribeCandidate`] (its peeled commit).
    pub tagged_commit: ObjectId,
    /// Commits reachable from the target but not from the candidate's tag.
    pub depth: u32,
}

/// The outcome of the describe walk: the winning candidate (with the commits
/// traversed) if one was found, plus the count of reachable unannotated tags
/// skipped in the default mode.
pub struct DescribeSearchOutcome {
    pub winner: Option<DescribeWinner>,
    pub traversed: usize,
    pub unannotated_cnt: usize,
}

/// Per-candidate state during the priority walk.
struct PossibleTag<'a> {
    tag: &'a DescribeCandidate,
    /// Bit identifying commits reachable from this candidate's tagged commit.
    flag: u32,
    /// Count of traversed commits not reachable from this candidate.
    depth: u32,
    /// Order in which this candidate was registered during the commit-date walk;
    /// breaks ties between equal-depth candidates, matching git's `compare_pt`.
    found_order: usize,
}

/// Item for the commit-date priority queue. `Ord` yields a max-heap on date so
/// the newest commit is processed first; ties fall back to oid for determinism.
struct QueueItem {
    date: i64,
    oid: ObjectId,
    parents: Vec<ObjectId>,
}

fn queue_item(
    metadata: &mut CommitMetadataReader<'_, FileObjectDatabase>,
    oid: &ObjectId,
    first_parent: bool,
) -> Result<QueueItem> {
    let mut commit = metadata.get(oid)?;
    if first_parent {
        commit.parents.truncate(1);
    }
    Ok(QueueItem {
        date: commit.commit_time,
        oid: commit.oid,
        parents: commit.parents.clone(),
    })
}

impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.date == other.date && self.oid == other.oid
    }
}

impl Eq for QueueItem {}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.date
            .cmp(&other.date)
            .then_with(|| self.oid.to_hex().cmp(&other.oid.to_hex()))
    }
}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Per-candidate flags live in a u32; bit 0 is reserved (git's flags are 1-based
/// via `1u << match_cnt` after the post-increment), so we can track up to 31
/// candidates. git is likewise bounded by its commit-flag bits.
const MAX_FLAG_CANDIDATES: usize = 31;

/// Run the commit-date-ordered priority walk from `target`, returning the winning
/// candidate together with the number of commits traversed. Returns
/// `winner: None` when no candidate tag is reachable. This mirrors git's
/// `describe_commit` walk, including the `depth = seen_commits - 1` seeding, the
/// per-commit depth increments, the early-exit, and the
/// `finish_depth_computation` tail.
pub fn describe_search(
    git_dir: &std::path::Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    candidates: &[DescribeCandidate],
    target: &ObjectId,
    options: &DescribeWalkOptions,
) -> Result<DescribeSearchOutcome> {
    let mut metadata = CommitMetadataReader::new(git_dir, format, db);
    let by_commit: HashMap<ObjectId, &DescribeCandidate> =
        candidates.iter().map(|tag| (tag.commit, tag)).collect();

    let mut possible: Vec<PossibleTag<'_>> = Vec::new();
    // Flags carried by each commit: the union of candidate bits whose tagged
    // commit this commit is an ancestor-or-self of, propagated to parents.
    let mut flags: HashMap<ObjectId, u32> = HashMap::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut queue: BinaryHeap<QueueItem> = BinaryHeap::new();

    queue.push(queue_item(&mut metadata, target, options.first_parent)?);
    seen.insert(*target);

    let effective_max = options.max_candidates.min(MAX_FLAG_CANDIDATES);
    let names_size = candidates.len();
    let mut seen_commits = 0usize;
    let mut annotated_cnt = 0usize;
    // Reachable unannotated tags skipped because the default mode wants annotated
    // tags; drives the "try --tags" hint when no annotated tag is reachable.
    let mut unannotated_cnt = 0usize;
    // The commit at which we stopped because the candidate budget was exhausted;
    // it must be re-fed to the depth-finishing pass.
    let mut gave_up: Option<QueueItem> = None;

    while let Some(item) = queue.pop() {
        seen_commits += 1;

        // Stop collecting once the candidate budget (or the entire tag universe)
        // is exhausted; the winner's depth is finished separately below.
        if possible.len() == effective_max || possible.len() == names_size {
            gave_up = Some(item);
            seen_commits -= 1;
            break;
        }
        let oid = item.oid;

        if let Some(best) = by_commit.get(&oid).copied() {
            if !best.eligible {
                // A reachable unannotated tag we would have used with `--tags`.
                unannotated_cnt += 1;
            } else if possible.len() < effective_max {
                // git assigns the flag/found_order from the post-incremented
                // match count, making both 1-based.
                let found_order = possible.len() + 1;
                let flag = 1u32 << found_order;
                let depth = (seen_commits - 1) as u32;
                *flags.entry(oid).or_insert(0) |= flag;
                if options.debug {
                    eprintln!(" annotated {depth:>10} {}", best.name);
                }
                possible.push(PossibleTag {
                    tag: best,
                    flag,
                    depth,
                    found_order,
                });
                if best.annotated {
                    annotated_cnt += 1;
                }
            }
        }

        // Every candidate not reached by this commit grows its depth by one.
        let commit_flags = flags.get(&oid).copied().unwrap_or(0);
        for candidate in &mut possible {
            if commit_flags & candidate.flag == 0 {
                candidate.depth += 1;
            }
        }

        // Early exit: if the queue is drained to commits all covered by the best
        // candidate(s), remaining depth is already final.
        if annotated_cnt > 0 && queue.is_empty() {
            let mut best_depth = u32::MAX;
            let mut best_within = 0u32;
            for candidate in &possible {
                if candidate.depth < best_depth {
                    best_depth = candidate.depth;
                    best_within = candidate.flag;
                } else if candidate.depth == best_depth {
                    best_within |= candidate.flag;
                }
            }
            if commit_flags & best_within == best_within {
                break;
            }
        }

        for parent in item.parents {
            *flags.entry(parent).or_insert(0) |= commit_flags;
            if seen.insert(parent) {
                queue.push(queue_item(&mut metadata, &parent, options.first_parent)?);
            }
        }
    }

    if possible.is_empty() {
        return Ok(DescribeSearchOutcome {
            winner: None,
            traversed: 0,
            unannotated_cnt,
        });
    }

    // Pick the winner: smallest depth, ties broken by registration order.
    let mut best_index = 0;
    for index in 1..possible.len() {
        let challenger = &possible[index];
        let leader = &possible[best_index];
        if challenger.depth < leader.depth
            || (challenger.depth == leader.depth && challenger.found_order < leader.found_order)
        {
            best_index = index;
        }
    }

    // Finish the winner's depth over any commits left unprocessed (because the
    // walk stopped early or gave up on the candidate budget).
    let best_flag = possible[best_index].flag;
    if let Some(gave_up) = gave_up {
        seen.remove(&gave_up.oid);
        queue.push(gave_up);
    }
    let extra = finish_depth_computation(
        &mut metadata,
        options.first_parent,
        &mut queue,
        &mut flags,
        &mut seen,
        best_flag,
    )?;
    possible[best_index].depth += extra;

    seen_commits += extra as usize;
    let best = possible.swap_remove(best_index);
    Ok(DescribeSearchOutcome {
        winner: Some(DescribeWinner {
            tagged_commit: best.tag.commit,
            depth: best.depth,
        }),
        traversed: seen_commits,
        unannotated_cnt,
    })
}

/// Continue walking the leftover queue to finish counting the winning
/// candidate's depth, mirroring git's `finish_depth_computation`: every commit
/// not reachable from the winning tag still lies between the target and that
/// tag and adds one to its depth. Returns the additional depth accumulated.
fn finish_depth_computation(
    metadata: &mut CommitMetadataReader<'_, FileObjectDatabase>,
    first_parent: bool,
    queue: &mut BinaryHeap<QueueItem>,
    flags: &mut HashMap<ObjectId, u32>,
    seen: &mut HashSet<ObjectId>,
    best_flag: u32,
) -> Result<u32> {
    // Commits currently queued that the winner does not yet reach.
    let mut unflagged: HashSet<ObjectId> = queue
        .iter()
        .filter(|item| flags.get(&item.oid).copied().unwrap_or(0) & best_flag == 0)
        .map(|item| item.oid)
        .collect();
    let mut extra = 0u32;
    while let Some(item) = queue.pop() {
        let oid = item.oid;
        let commit_flags = flags.get(&oid).copied().unwrap_or(0);
        if commit_flags & best_flag != 0 {
            // The winner reaches this commit; once nothing unflagged remains the
            // depth can no longer grow.
            if unflagged.is_empty() {
                break;
            }
        } else {
            unflagged.remove(&oid);
            extra += 1;
        }
        for parent in item.parents {
            let flag_before = flags.get(&parent).copied().unwrap_or(0) & best_flag;
            let was_seen = seen.contains(&parent);
            if !was_seen {
                seen.insert(parent);
            }
            *flags.entry(parent).or_insert(0) |= commit_flags;
            let flag_after = flags.get(&parent).copied().unwrap_or(0) & best_flag;
            if !was_seen {
                queue.push(queue_item(metadata, &parent, first_parent)?);
                if flag_after == 0 {
                    unflagged.insert(parent);
                }
            } else if flag_before == 0 && flag_after != 0 {
                unflagged.remove(&parent);
            }
        }
    }
    Ok(extra)
}
