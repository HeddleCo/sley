//! The weighted-midpoint bisection primitive (upstream `bisect.c`).
//!
//! This is a faithful port of git's `find_bisection` / `do_find_bisection`,
//! the `count_distance` reachable-set weight, the `approx_halfway` early exit,
//! and the `filter_skipped` + PRN-driven `skip_away` machinery that
//! `git bisect next` uses to step over `skip`ped commits. It is a pure,
//! graph-only primitive: callers hand it the *interesting* commit set (the
//! `bad ^good` reachable set, oldest-first) as oids plus intra-set parent
//! adjacency, and it returns which commit(s) to test.
//!
//! Two callers share it:
//!
//! * `git bisect next` (sley-cli `commands::bisect`), which also drives the
//!   skip filter and checkout.
//! * `git rev-list --bisect[-vars|-all]` (sley-cli `commands::rev_list`), which
//!   formats the plumbing output ([`find_bisection`] returns the sorted picks
//!   with their distances plus `reaches`/`all`).

use crate::CommitRecord;
use sley_core::ObjectId;
use std::collections::{HashMap, HashSet};

/// The result of [`do_find_bisection`]: the chosen commit index/indices (best
/// first) and the weight (reachable interesting commits) of the best pick.
pub struct Bisection {
    /// Indices (into the oldest-first list) of the chosen commits, best first.
    /// A single entry unless `find_all` was requested.
    pub picks: Vec<usize>,
    /// Weight (reachable interesting commits) of the best pick.
    pub reaches: usize,
    /// Per-commit weights (reachable interesting commits, itself included).
    /// Fully computed only when the `find_all` path runs or no early halfway
    /// exit fired; entries for commits past an early exit may be left partial.
    pub weights: Vec<i64>,
}

/// Half-distance test from upstream `approx_halfway`: a weight is "good enough"
/// when it sits within one of half the set, or within `nr/1024` of it.
fn approx_halfway(weight: i64, nr: usize) -> bool {
    let diff = 2 * weight - nr as i64;
    match diff {
        -1..=1 => true,
        _ => diff.unsigned_abs() < (nr / 1024) as u64,
    }
}

/// Reachable-set count over the interesting graph (upstream `count_distance`
/// with COUNTED marks): how many interesting commits `start` reaches, itself
/// included.
fn count_distance(start: usize, parents: &[Vec<usize>]) -> i64 {
    let mut visited = vec![false; parents.len()];
    let mut stack = vec![start];
    let mut nr = 0i64;
    while let Some(idx) = stack.pop() {
        if visited[idx] {
            continue;
        }
        visited[idx] = true;
        nr += 1;
        for &parent in &parents[idx] {
            if !visited[parent] {
                stack.push(parent);
            }
        }
    }
    nr
}

/// The bisection "distance" of a commit with the given weight in a set of `nr`
/// commits: `min(weight, nr - weight)` (upstream's symmetric distance).
fn bisect_distance(weight: i64, nr: usize) -> i64 {
    weight.min(nr as i64 - weight)
}

/// Weighted-midpoint selection, ported from upstream `do_find_bisection` /
/// `find_bisection`. `oids` is oldest-first; `parents[i]` are the indices of
/// each commit's interesting parents. When `find_all` is false, returns the
/// first commit found approximately halfway (or the best one); otherwise
/// computes every weight and returns all candidates sorted by distance
/// (descending, ties by ascending oid).
pub fn do_find_bisection(oids: &[ObjectId], parents: &[Vec<usize>], find_all: bool) -> Bisection {
    let nr = oids.len();
    let mut weights: Vec<i64> = vec![0; nr];
    let mut counted = 0usize;

    for idx in 0..nr {
        match parents[idx].len() {
            0 => {
                weights[idx] = 1;
                counted += 1;
            }
            1 => weights[idx] = -1,
            _ => weights[idx] = -2,
        }
    }

    // The early halfway exit found this commit; constructed after any borrow on
    // `weights` ends so the vector can move into the result.
    let mut early: Option<usize> = None;

    // Count merges the expensive way first, with the halfway early exit.
    for (idx, weight) in weights.iter_mut().enumerate() {
        if *weight != -2 {
            continue;
        }
        *weight = count_distance(idx, parents);
        if !find_all && approx_halfway(*weight, nr) {
            early = Some(idx);
            break;
        }
        counted += 1;
    }
    if let Some(idx) = early {
        let reaches = weights[idx] as usize;
        return Bisection {
            picks: vec![idx],
            reaches,
            weights,
        };
    }

    // Fill in single-strand-of-pearls weights from known parents.
    'fill: while counted < nr {
        for idx in 0..nr {
            if weights[idx] >= 0 {
                continue;
            }
            let Some(&known) = parents[idx].iter().find(|&&p| weights[p] >= 0) else {
                continue;
            };
            weights[idx] = weights[known] + 1;
            counted += 1;
            if !find_all && approx_halfway(weights[idx], nr) {
                early = Some(idx);
                break 'fill;
            }
        }
    }
    if let Some(idx) = early {
        let reaches = weights[idx] as usize;
        return Bisection {
            picks: vec![idx],
            reaches,
            weights,
        };
    }

    if !find_all {
        // best_bisection: maximize min(weight, nr - weight); first wins ties.
        let mut best = 0usize;
        let mut best_distance: i64 = -1;
        for (idx, &weight) in weights.iter().enumerate() {
            let distance = bisect_distance(weight, nr);
            if distance > best_distance {
                best = idx;
                best_distance = distance;
            }
        }
        let reaches = weights[best] as usize;
        Bisection {
            picks: vec![best],
            reaches,
            weights,
        }
    } else {
        // best_bisection_sorted: distance descending, ties by ascending oid.
        let mut order: Vec<usize> = (0..nr).collect();
        order.sort_by(|&a, &b| {
            bisect_distance(weights[b], nr)
                .cmp(&bisect_distance(weights[a], nr))
                .then_with(|| oids[a].cmp(&oids[b]))
        });
        let reaches = order.first().map(|&idx| weights[idx] as usize).unwrap_or(0);
        Bisection {
            picks: order,
            reaches,
            weights,
        }
    }
}

/// The full result of a `rev-list --bisect[-vars|-all]` run: the picked commit
/// set with each commit's symmetric `dist=` (best first), plus the `reaches`
/// and `all` counts the plumbing vars are derived from.
pub struct FindBisection {
    /// `(oid, distance)` for each pick, best (largest distance) first. For
    /// `--bisect` / `--bisect-vars` this is a single entry; for `--bisect-all`
    /// it is the whole interesting set sorted by distance.
    pub picks: Vec<(ObjectId, i64)>,
    /// Weight (reachable interesting commits) of the best pick.
    pub reaches: usize,
    /// Total interesting commits in the bisected set.
    pub all: usize,
}

/// Run the bisection over an *interesting* commit set and return the picks with
/// their distances. `records` is the `bad ^good` reachable set in newest-first
/// (rev-list) order; `first_parent` restricts each commit to its first interior
/// parent (matching `--first-parent`). This is the entry point `rev-list
/// --bisect[-vars|-all]` uses. Takes borrowed records (only oid + parents are
/// read) so the caller need not deep-clone the parsed commit set.
pub fn find_bisection(
    records: &[&CommitRecord],
    find_all: bool,
    first_parent: bool,
) -> FindBisection {
    // Oldest-first list with intra-set parent adjacency (upstream gives
    // find_bisection the limited commit list in this order).
    let oids: Vec<ObjectId> = records.iter().rev().map(|record| record.oid).collect();
    let index_by_oid: HashMap<ObjectId, usize> = oids
        .iter()
        .enumerate()
        .map(|(idx, oid)| (*oid, idx))
        .collect();
    let parents: Vec<Vec<usize>> = records
        .iter()
        .rev()
        .map(|record| {
            let mut adjacent = Vec::new();
            for parent in &record.parents {
                if let Some(&pidx) = index_by_oid.get(parent) {
                    adjacent.push(pidx);
                }
                if first_parent {
                    break;
                }
            }
            adjacent
        })
        .collect();

    let all = oids.len();
    if all == 0 {
        return FindBisection {
            picks: Vec::new(),
            reaches: 0,
            all: 0,
        };
    }

    let bisection = do_find_bisection(&oids, &parents, find_all);
    // For `--bisect-all` every weight is fully computed, so each pick's distance
    // is read off the weight vector. For the single-pick case the only weight we
    // need is the best pick's, which equals `reaches` (other weights may be left
    // partial by the halfway early exit).
    let picks = if find_all {
        bisection
            .picks
            .iter()
            .map(|&idx| (oids[idx], bisect_distance(bisection.weights[idx], all)))
            .collect()
    } else {
        bisection
            .picks
            .iter()
            .map(|&idx| (oids[idx], bisect_distance(bisection.reaches as i64, all)))
            .collect()
    };
    FindBisection {
        picks,
        reaches: bisection.reaches,
        all,
    }
}

/// git's `estimate_bisect_steps`: roughly `log2(all)`, nudged for how far `all`
/// sits past the previous power of two.
pub fn estimate_bisect_steps(all: usize) -> usize {
    if all < 3 {
        return 0;
    }
    let n = (usize::BITS - 1 - all.leading_zeros()) as usize; // floor(log2(all))
    let e = 1usize << n;
    let x = all - e;
    if e < 3 * x { n } else { n - 1 }
}

/// Pseudo random number generator from upstream `bisect.c` (used by skip_away).
fn get_prn(count: usize) -> usize {
    let count = (count as u32)
        .wrapping_mul(1_103_515_245)
        .wrapping_add(12_345);
    ((count / 65_536) % 32_768) as usize
}

/// Integer square root with the same float iteration as upstream.
fn sqrti(val: i32) -> i32 {
    if val == 0 {
        return 0;
    }
    let mut x = val as f32;
    loop {
        let y = (x + (val as f32) / x) / 2.0;
        let d = if y > x { y - x } else { x - y };
        x = y;
        if d < 0.5 {
            break;
        }
    }
    x as i32
}

/// Pick a commit "away" from the (skipped) head of the list (upstream
/// `skip_away`).
fn skip_away(list: &[usize], count: usize, oids: &[ObjectId], bad: &ObjectId) -> usize {
    let prn = get_prn(count);
    let index = (count * prn / 32_768) * sqrti(prn as i32) as usize / sqrti(32_768) as usize;
    let mut previous: Option<usize> = None;
    for (i, &idx) in list.iter().enumerate() {
        if i == index {
            if oids[idx] != *bad {
                return idx;
            }
            return previous.unwrap_or(list[0]);
        }
        previous = Some(idx);
    }
    list[0]
}

/// The outcome of [`managed_skipped`].
pub enum SkipFilter {
    /// The best pick was not skipped: use it as-is.
    Clean(usize),
    /// Skips interfered: the picked index (if any usable commit remains) plus
    /// the skipped ("tried") indices.
    Skipped {
        pick: Option<usize>,
        tried: Vec<usize>,
    },
}

/// `filter_skipped` + `skip_away` (upstream `managed_skipped`). `picks` is the
/// sorted candidate order; returns which commit to test and which skipped
/// commits were "tried".
pub fn managed_skipped(
    picks: &[usize],
    oids: &[ObjectId],
    skipped: &HashSet<ObjectId>,
    bad: &ObjectId,
) -> SkipFilter {
    if skipped.is_empty() || picks.is_empty() {
        return match picks.first() {
            Some(&idx) => SkipFilter::Clean(idx),
            None => SkipFilter::Skipped {
                pick: None,
                tried: Vec::new(),
            },
        };
    }
    if !skipped.contains(&oids[picks[0]]) {
        return SkipFilter::Clean(picks[0]);
    }
    // First pick is skipped: fully filter, then skip away.
    let mut tried = Vec::new();
    let mut filtered = Vec::new();
    for &idx in picks {
        if skipped.contains(&oids[idx]) {
            tried.push(idx);
        } else {
            filtered.push(idx);
        }
    }
    if filtered.is_empty() {
        return SkipFilter::Skipped { pick: None, tried };
    }
    let pick = skip_away(&filtered, filtered.len(), oids, bad);
    SkipFilter::Skipped {
        pick: Some(pick),
        tried,
    }
}
