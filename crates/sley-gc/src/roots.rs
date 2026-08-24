//! Typed garbage-collection roots and provenance-preserving deduplication.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use sley_core::ObjectId;

/// The repository structure that made an object a garbage-collection root.
///
/// Keeping this information until the walk starts makes root diagnostics
/// actionable without requiring duplicate object walks. Paths are retained
/// exactly as they were inspected, including linked-worktree administration
/// directories.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum GcRootSource {
    Ref { name: String },
    Head { git_dir: Arc<Path> },
    IndexEntry { index: Arc<Path> },
    IndexCacheTree { index: Arc<Path> },
    IndexResolveUndo { index: Arc<Path> },
    Reflog { path: Arc<Path> },
    StateFile { path: Arc<Path> },
    CommandLine { revision: String },
}

/// One object and the source that requires garbage collection to retain it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcRoot {
    pub oid: ObjectId,
    pub source: GcRootSource,
}

/// Deduplicated roots with all distinct provenance retained per object.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcRootSet {
    roots: BTreeMap<ObjectId, BTreeSet<GcRootSource>>,
    order: Vec<ObjectId>,
}

impl GcRootSet {
    pub fn insert(&mut self, root: GcRoot) {
        match self.roots.entry(root.oid) {
            Entry::Vacant(entry) => {
                self.order.push(root.oid);
                entry.insert(BTreeSet::from([root.source]));
            }
            Entry::Occupied(mut entry) => {
                entry.get_mut().insert(root.source);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn object_ids(&self) -> impl ExactSizeIterator<Item = ObjectId> + '_ {
        self.order.iter().copied()
    }

    pub fn sorted_object_ids(&self) -> impl ExactSizeIterator<Item = ObjectId> + '_ {
        self.roots.keys().copied()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&ObjectId, &BTreeSet<GcRootSource>)> + '_ {
        self.roots.iter()
    }

    pub fn sources(&self, oid: &ObjectId) -> Option<&BTreeSet<GcRootSource>> {
        self.roots.get(oid)
    }

    pub fn into_object_ids(self) -> Vec<ObjectId> {
        self.order
    }

    pub fn into_sorted_object_ids(self) -> Vec<ObjectId> {
        self.roots.into_keys().collect()
    }
}

impl Extend<GcRoot> for GcRootSet {
    fn extend<T: IntoIterator<Item = GcRoot>>(&mut self, roots: T) {
        for root in roots {
            self.insert(root);
        }
    }
}

impl FromIterator<GcRoot> for GcRootSet {
    fn from_iter<T: IntoIterator<Item = GcRoot>>(roots: T) -> Self {
        let mut set = Self::default();
        set.extend(roots);
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::ObjectFormat;

    #[test]
    fn deduplicates_objects_without_discarding_provenance() {
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, &"1".repeat(40)).expect("valid oid");
        let mut roots = GcRootSet::default();
        roots.insert(GcRoot {
            oid,
            source: GcRootSource::Ref {
                name: "refs/heads/main".into(),
            },
        });
        roots.insert(GcRoot {
            oid,
            source: GcRootSource::Head {
                git_dir: Arc::from(Path::new(".git")),
            },
        });
        roots.insert(GcRoot {
            oid,
            source: GcRootSource::Head {
                git_dir: Arc::from(Path::new(".git")),
            },
        });

        assert_eq!(roots.len(), 1);
        assert_eq!(roots.sources(&oid).expect("root sources").len(), 2);
        assert_eq!(roots.object_ids().collect::<Vec<_>>(), vec![oid]);
    }

    #[test]
    fn preserves_first_seen_order_while_offering_sorted_projection() {
        let high = ObjectId::from_hex(ObjectFormat::Sha1, &"f".repeat(40)).expect("valid oid");
        let low = ObjectId::from_hex(ObjectFormat::Sha1, &"1".repeat(40)).expect("valid oid");
        let source = GcRootSource::Ref {
            name: "refs/heads/main".into(),
        };
        let mut roots = GcRootSet::default();
        roots.insert(GcRoot {
            oid: high,
            source: source.clone(),
        });
        roots.insert(GcRoot { oid: low, source });

        assert_eq!(roots.object_ids().collect::<Vec<_>>(), vec![high, low]);
        assert_eq!(
            roots.sorted_object_ids().collect::<Vec<_>>(),
            vec![low, high]
        );
    }
}
