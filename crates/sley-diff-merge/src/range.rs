//! Patch-series assignment for `range-diff`.
//!
//! The engine is deliberately repository-agnostic: callers render each commit
//! into normalized patch text and pass borrowed subjects/diff bodies here.

use std::collections::{HashMap, HashSet};

const COST_MAX: i32 = i32::MAX / 4;
type AssignmentMemo = HashMap<(usize, u64), i32>;

/// Borrowed patch data needed to assign two range-diff series.
#[derive(Clone, Copy, Debug)]
pub struct PatchRef<'a> {
    pub subject: &'a str,
    pub diff: &'a [u8],
    pub diff_size: i32,
}

struct AssignmentCtx<'a> {
    left_ids: &'a [usize],
    right_ids: &'a [usize],
    left: &'a [PatchRef<'a>],
    right: &'a [PatchRef<'a>],
    creation_factor: i32,
    match_costs: Vec<Vec<i32>>,
}

/// Assign corresponding patches between two patch series.
///
/// Returns `(left_index, right_index)` pairs for exact matches and the best
/// inexact assignments selected by the range-diff cost model.
pub fn assign_patch_series(
    left: &[PatchRef<'_>],
    right: &[PatchRef<'_>],
    creation_factor: i32,
) -> Vec<(usize, usize)> {
    let mut pairs = find_exact_matches(left, right);
    let matched_left = pairs.iter().map(|(li, _)| *li).collect::<HashSet<_>>();
    let matched_right = pairs.iter().map(|(_, rj)| *rj).collect::<HashSet<_>>();
    let left_unmatched = (0..left.len())
        .filter(|idx| !matched_left.contains(idx))
        .collect::<Vec<_>>();
    let right_unmatched = (0..right.len())
        .filter(|idx| !matched_right.contains(idx))
        .collect::<Vec<_>>();
    if left_unmatched.is_empty() || right_unmatched.len() > 62 {
        return pairs;
    }

    let match_costs = precompute_match_costs(left, right, &left_unmatched, &right_unmatched);
    let ctx = AssignmentCtx {
        left_ids: &left_unmatched,
        right_ids: &right_unmatched,
        left,
        right,
        creation_factor,
        match_costs,
    };
    let mut memo = HashMap::new();
    assignment_dp(0, 0, &ctx, &mut memo);
    backtrack_assignment(0, 0, &ctx, &mut memo, &mut pairs);
    pairs
}

fn find_exact_matches(left: &[PatchRef<'_>], right: &[PatchRef<'_>]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut used = HashSet::new();
    for (i, left_patch) in left.iter().enumerate() {
        for (j, right_patch) in right.iter().enumerate() {
            if used.contains(&j) {
                continue;
            }
            if left_patch.diff == right_patch.diff {
                pairs.push((i, j));
                used.insert(j);
                break;
            }
        }
    }
    pairs
}

fn precompute_match_costs(
    left: &[PatchRef<'_>],
    right: &[PatchRef<'_>],
    left_ids: &[usize],
    right_ids: &[usize],
) -> Vec<Vec<i32>> {
    let mut rendered = Vec::new();
    left_ids
        .iter()
        .map(|&li| {
            right_ids
                .iter()
                .map(|&rj| {
                    let cost = diff_size_into(&mut rendered, left[li].diff, right[rj].diff);
                    if left[li].subject == right[rj].subject {
                        cost / 2
                    } else {
                        cost
                    }
                })
                .collect()
        })
        .collect()
}

fn assignment_dp(pos: usize, used: u64, ctx: &AssignmentCtx<'_>, memo: &mut AssignmentMemo) -> i32 {
    if let Some(value) = memo.get(&(pos, used)) {
        return *value;
    }
    let cost = if pos == ctx.left_ids.len() {
        let mut cost = 0;
        for (bit, &rid) in ctx.right_ids.iter().enumerate() {
            if used & (1u64 << bit) == 0 {
                cost = saturating_cost(cost, ctx.right[rid].diff_size * ctx.creation_factor / 100);
            }
        }
        cost
    } else {
        let li = ctx.left_ids[pos];
        let delete_cost = ctx.left[li].diff_size * ctx.creation_factor / 100;
        let tail_cost = assignment_dp(pos + 1, used, ctx, memo);
        let mut best = saturating_cost(delete_cost, tail_cost);

        for bit in 0..ctx.right_ids.len() {
            if used & (1u64 << bit) != 0 {
                continue;
            }
            let tail_cost = assignment_dp(pos + 1, used | (1u64 << bit), ctx, memo);
            let cost = saturating_cost(ctx.match_costs[pos][bit], tail_cost);
            if cost < best {
                best = cost;
            }
        }
        best
    };
    memo.insert((pos, used), cost);
    cost
}

fn backtrack_assignment(
    pos: usize,
    used: u64,
    ctx: &AssignmentCtx<'_>,
    memo: &mut AssignmentMemo,
    pairs: &mut Vec<(usize, usize)>,
) {
    if pos == ctx.left_ids.len() {
        return;
    }
    let li = ctx.left_ids[pos];
    let delete_cost = ctx.left[li].diff_size * ctx.creation_factor / 100;
    let tail_cost = assignment_dp(pos + 1, used, ctx, memo);
    let mut best = saturating_cost(delete_cost, tail_cost);
    let mut best_match = None;

    for (bit, &rj) in ctx.right_ids.iter().enumerate() {
        if used & (1u64 << bit) != 0 {
            continue;
        }
        let tail_cost = assignment_dp(pos + 1, used | (1u64 << bit), ctx, memo);
        let cost = saturating_cost(ctx.match_costs[pos][bit], tail_cost);
        if cost < best {
            best = cost;
            best_match = Some((bit, rj));
        }
    }

    if let Some((bit, rj)) = best_match {
        backtrack_assignment(pos + 1, used | (1u64 << bit), ctx, memo, pairs);
        pairs.push((li, rj));
    } else {
        backtrack_assignment(pos + 1, used, ctx, memo, pairs);
    }
}

fn saturating_cost(a: i32, b: i32) -> i32 {
    a.saturating_add(b).min(COST_MAX)
}

fn diff_size_into(rendered: &mut Vec<u8>, left: &[u8], right: &[u8]) -> i32 {
    rendered.clear();
    let mut heading = section_heading_classifier();
    let mut opts = crate::render::HunkRenderOptions {
        context: 3,
        heading: Some(&mut heading),
        ..Default::default()
    };
    crate::render::render_hunks(rendered, Some(left), Some(right), &mut opts);
    rendered.iter().filter(|b| **b == b'\n').count() as i32
}

fn section_heading_classifier() -> impl FnMut(&[u8]) -> Option<Vec<u8>> {
    move |line: &[u8]| {
        let line = trim_ascii_end(line);
        if line.starts_with(b" ## ") && line.ends_with(b" ##") {
            return Some(line[4..line.len() - 3].to_vec());
        }
        let line = line.strip_prefix(b" ").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(b"@@ ") {
            return Some(rest.to_vec());
        }
        None
    }
}

fn trim_ascii_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && line[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch<'a>(subject: &'a str, diff: &'a [u8]) -> PatchRef<'a> {
        PatchRef {
            subject,
            diff,
            diff_size: diff.iter().filter(|b| **b == b'\n').count() as i32,
        }
    }

    #[test]
    fn exact_matches_are_greedy_and_do_not_reuse_rhs() {
        let left = [patch("one", b"same\n"), patch("two", b"same\n")];
        let right = [patch("right one", b"same\n"), patch("right two", b"same\n")];

        assert_eq!(assign_patch_series(&left, &right, 60), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn inexact_assignment_prefers_lowest_patch_diff_cost() {
        let left = [patch("topic", b" ## file ##\n-old\n+new\n")];
        let right = [
            patch("other", b" ## file ##\n-completely\n+different\n"),
            patch("topic", b" ## file ##\n-old\n+newer\n"),
        ];

        assert_eq!(assign_patch_series(&left, &right, 1_000), vec![(0, 1)]);
    }

    #[test]
    fn too_many_rhs_candidates_keeps_only_exact_matches() {
        let left = [patch("exact", b"same\n"), patch("unmatched", b"left\n")];
        let mut right = vec![patch("exact", b"same\n")];
        right.extend((0..63).map(|_| patch("candidate", b"right\n")));

        assert_eq!(assign_patch_series(&left, &right, 60), vec![(0, 0)]);
    }
}
