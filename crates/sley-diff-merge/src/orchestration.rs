//! Repository-independent merge topology planning.
//!
//! The caller resolves commits and computes merge bases. This module turns
//! those typed inputs into the topology action that a porcelain or embedding
//! should execute, without performing worktree, ref, hook, or rendering I/O.

use sley_core::ObjectId;

/// The topology action implied by a merge head and its computed merge bases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeTopologyAction {
    /// The other head is already reachable from the current head.
    AlreadyUpToDate,
    /// The current head is reachable from the other head.
    FastForward,
    /// Neither head contains the other, so a true three-way merge is needed.
    ThreeWay,
}

/// Typed inputs for topology planning after revision resolution.
#[derive(Debug, Clone, Copy)]
pub struct MergeTopologyOptions<'a> {
    /// The current `HEAD` commit.
    pub head: ObjectId,
    /// The commit being merged into `head`.
    pub other: ObjectId,
    /// Best merge bases computed for `head` and `other`.
    pub merge_bases: &'a [ObjectId],
}

/// The topology plan produced for a two-head merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeTopologyPlan {
    /// The action implied by the reachability relationship.
    pub action: MergeTopologyAction,
    /// The number of best merge bases available to a three-way engine.
    pub merge_base_count: usize,
}

impl MergeTopologyPlan {
    /// Whether the two commits have no known common ancestor.
    pub fn has_unrelated_histories(self) -> bool {
        self.merge_base_count == 0
    }
}

/// Plan the topology action for a resolved two-head merge.
pub fn plan_merge_topology(options: MergeTopologyOptions<'_>) -> MergeTopologyPlan {
    let action = if options.head == options.other
        || options
            .merge_bases
            .iter()
            .any(|base| base == &options.other)
    {
        MergeTopologyAction::AlreadyUpToDate
    } else if options.merge_bases.iter().any(|base| base == &options.head) {
        MergeTopologyAction::FastForward
    } else {
        MergeTopologyAction::ThreeWay
    };
    MergeTopologyPlan {
        action,
        merge_base_count: options.merge_bases.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_raw(ObjectFormat::Sha1, &[byte; 20]).expect("valid oid")
    }

    #[test]
    fn equal_heads_are_already_up_to_date_without_a_base() {
        let head = oid(1);
        let plan = plan_merge_topology(MergeTopologyOptions {
            head,
            other: head,
            merge_bases: &[],
        });
        assert_eq!(plan.action, MergeTopologyAction::AlreadyUpToDate);
    }

    #[test]
    fn other_as_a_base_is_already_up_to_date() {
        let head = oid(1);
        let other = oid(2);
        let plan = plan_merge_topology(MergeTopologyOptions {
            head,
            other,
            merge_bases: &[other],
        });
        assert_eq!(plan.action, MergeTopologyAction::AlreadyUpToDate);
    }

    #[test]
    fn head_as_a_base_is_fast_forward() {
        let head = oid(1);
        let plan = plan_merge_topology(MergeTopologyOptions {
            head,
            other: oid(2),
            merge_bases: &[head],
        });
        assert_eq!(plan.action, MergeTopologyAction::FastForward);
    }

    #[test]
    fn divergent_heads_require_three_way_and_report_base_shape() {
        let base_a = oid(3);
        let base_b = oid(4);
        let plan = plan_merge_topology(MergeTopologyOptions {
            head: oid(1),
            other: oid(2),
            merge_bases: &[base_a, base_b],
        });
        assert_eq!(plan.action, MergeTopologyAction::ThreeWay);
        assert_eq!(plan.merge_base_count, 2);
        assert!(!plan.has_unrelated_histories());

        let unrelated = plan_merge_topology(MergeTopologyOptions {
            head: oid(1),
            other: oid(2),
            merge_bases: &[],
        });
        assert!(unrelated.has_unrelated_histories());
    }
}
