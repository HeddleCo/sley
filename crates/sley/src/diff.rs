//! Diff helpers on top of [`sley_diff_merge`].

use sley_core::ObjectId;
use sley_diff_merge::{
    DiffNameStatusOptions, NameStatusEntry, diff_name_status_trees_with_options,
};

use crate::{Repository, Result};

impl Repository {
    /// Compare two tree objects and return name-status entries (`git diff --name-status`).
    pub fn diff_name_status(
        &self,
        left: &ObjectId,
        right: &ObjectId,
    ) -> Result<Vec<NameStatusEntry>> {
        diff_name_status_trees_with_options(
            self.object_database(),
            self.object_format(),
            left,
            right,
            DiffNameStatusOptions::default(),
        )
    }
}
