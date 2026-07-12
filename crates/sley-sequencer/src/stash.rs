//! Typed stash-ref state transitions.
//!
//! A stash is a sequenced stack encoded as `refs/stash` plus a reflog. These
//! pure plans centralize newest-first selector semantics and ref/reflog update
//! construction while leaving frontend diagnostics and persistence policy to
//! callers.

use sley_core::ObjectId;
use sley_refs::{RefTarget, RefUpdate, ReflogEntry};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashDropPlan {
    pub dropped: ReflogEntry,
    /// Remaining reflog entries in storage order (oldest to newest).
    pub remaining: Vec<ReflogEntry>,
    /// New `refs/stash` tip, or `None` when dropping the final entry deletes it.
    pub new_tip: Option<ObjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashDropError {
    Empty,
    OutOfRange { available: usize },
}

impl fmt::Display for StashDropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("stash is empty"),
            Self::OutOfRange { available } => {
                write!(f, "stash selector exceeds {available} entries")
            }
        }
    }
}

impl std::error::Error for StashDropError {}

/// Plan dropping `stash@{selector}` from an oldest-to-newest reflog.
pub fn plan_stash_drop(
    mut entries: Vec<ReflogEntry>,
    selector: usize,
) -> Result<StashDropPlan, StashDropError> {
    if entries.is_empty() {
        return Err(StashDropError::Empty);
    }
    if selector >= entries.len() {
        return Err(StashDropError::OutOfRange {
            available: entries.len(),
        });
    }
    let entry_index = entries.len() - 1 - selector;
    let dropped = entries.remove(entry_index);
    let new_tip = entries.last().map(|entry| entry.new_oid);
    Ok(StashDropPlan {
        dropped,
        remaining: entries,
        new_tip,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashStoreOptions {
    pub current: Option<RefTarget>,
    pub stash_oid: ObjectId,
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
}

#[derive(Debug)]
pub struct StashStorePlan {
    pub old_oid: ObjectId,
    /// `None` when `refs/stash` already names `stash_oid`.
    pub update: Option<RefUpdate>,
}

/// Plan updating `refs/stash` and appending its matching reflog entry.
pub fn plan_stash_store(options: StashStoreOptions) -> StashStorePlan {
    let old_oid = match options.current {
        Some(RefTarget::Direct(oid)) => oid,
        Some(RefTarget::Symbolic(_)) | None => ObjectId::null(options.stash_oid.format()),
    };
    let update = (old_oid != options.stash_oid).then(|| RefUpdate {
        name: "refs/stash".to_string(),
        expected: None,
        new: RefTarget::Direct(options.stash_oid),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: options.stash_oid,
            committer: options.committer,
            message: options.message,
        }),
    });
    StashStorePlan { old_oid, update }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;

    #[test]
    fn drop_selector_is_newest_first_and_repoints_to_remaining_top() {
        let entries = vec![entry(1), entry(2), entry(3)];
        let plan = plan_stash_drop(entries, 1).expect("drop middle stash");
        assert_eq!(plan.dropped.new_oid, oid(ObjectFormat::Sha1, 2));
        assert_eq!(
            plan.remaining
                .iter()
                .map(|entry| entry.new_oid)
                .collect::<Vec<_>>(),
            vec![oid(ObjectFormat::Sha1, 1), oid(ObjectFormat::Sha1, 3)]
        );
        assert_eq!(plan.new_tip, Some(oid(ObjectFormat::Sha1, 3)));
    }

    #[test]
    fn drop_final_entry_deletes_tip_and_reports_bounds() {
        let plan = plan_stash_drop(vec![entry(1)], 0).expect("drop final stash");
        assert!(plan.remaining.is_empty());
        assert_eq!(plan.new_tip, None);
        assert_eq!(plan_stash_drop(Vec::new(), 0), Err(StashDropError::Empty));
        assert_eq!(
            plan_stash_drop(vec![entry(1)], 1),
            Err(StashDropError::OutOfRange { available: 1 })
        );
    }

    #[test]
    fn store_plan_is_hash_aware_and_suppresses_noop_updates() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let stash_oid = oid(format, 2);
            let plan = plan_stash_store(StashStoreOptions {
                current: None,
                stash_oid,
                committer: b"Stash <stash@example.invalid> 1 +0000".to_vec(),
                message: b"save".to_vec(),
            });
            assert_eq!(plan.old_oid, ObjectId::null(format));
            let update = plan.update.expect("new stash update");
            assert_eq!(update.name, "refs/stash");
            assert_eq!(update.new, RefTarget::Direct(stash_oid));

            let noop = plan_stash_store(StashStoreOptions {
                current: Some(RefTarget::Direct(stash_oid)),
                stash_oid,
                committer: Vec::new(),
                message: Vec::new(),
            });
            assert!(noop.update.is_none());
        }
    }

    fn entry(value: u8) -> ReflogEntry {
        ReflogEntry {
            old_oid: oid(ObjectFormat::Sha1, value.saturating_sub(1)),
            new_oid: oid(ObjectFormat::Sha1, value),
            committer: b"Stash <stash@example.invalid> 1 +0000".to_vec(),
            message: format!("stash {value}").into_bytes(),
        }
    }

    fn oid(format: ObjectFormat, value: u8) -> ObjectId {
        let byte = format!("{value:x}");
        ObjectId::from_hex(format, &byte.repeat(format.hex_len())).expect("valid test oid")
    }
}
