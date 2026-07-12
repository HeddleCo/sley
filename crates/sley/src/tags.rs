//! Repository-scoped tag selection and history filters.

use std::collections::{HashMap, HashSet};
use std::fmt;

use sley_object::{ObjectType, Tag};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_refs::{Ref, RefTarget, refname_pattern_matches_case};

use crate::{GitError, ObjectFormat, ObjectId, Repository, Result};

/// Repository-level tag-list selection before presentation-specific sorting or
/// formatting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagQueryOptions {
    /// Git-style wildcard patterns matched against names below `refs/tags/`.
    pub patterns: Vec<String>,
    /// Match [`TagQueryOptions::patterns`] without ASCII case sensitivity.
    pub ignore_case: bool,
    /// Object-ish names selected by `--points-at` semantics.
    pub points_at: Vec<String>,
    /// Commit-ish names which selected tag histories must contain.
    pub contains: Vec<String>,
    /// Commit-ish names which selected tag histories must not contain.
    pub no_contains: Vec<String>,
    /// Commit-ish names whose reachable histories include selected tags.
    pub merged: Vec<String>,
    /// Commit-ish names whose reachable histories exclude selected tags.
    pub no_merged: Vec<String>,
}

impl TagQueryOptions {
    /// Construct a query with no patterns or history filters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether evaluating the query requires object/history traversal.
    pub fn needs_object_filters(&self) -> bool {
        !self.points_at.is_empty()
            || !self.contains.is_empty()
            || !self.no_contains.is_empty()
            || !self.merged.is_empty()
            || !self.no_merged.is_empty()
    }
}

/// One tag selected by [`Repository::query_tags`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagQueryEntry {
    /// Tag name with the `refs/tags/` prefix removed.
    pub name: String,
    /// Original ref-backend record, preserving its immediate target.
    pub reference: Ref,
}

/// Selected tag refs in ref-backend order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagQueryOutcome {
    /// Selected tags in ref-backend order, before presentation sorting.
    pub entries: Vec<TagQueryEntry>,
}

/// Which command-line filter supplied a malformed revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagQueryRevisionKind {
    /// A `--points-at` object-ish.
    PointsAt,
    /// A `--contains` or `--no-contains` commit-ish.
    Contains,
    /// A `--merged` or `--no-merged` commit-ish.
    Merged,
}

/// Typed failures whose byte-level presentation and exit status belong to the
/// CLI wrapper.
#[derive(Debug)]
pub enum TagQueryError {
    /// A filter revision could not be resolved.
    MalformedRevision {
        /// Filter family which supplied the revision.
        kind: TagQueryRevisionKind,
        /// Original user-provided revision string.
        spec: String,
    },
    /// A contains filter resolved to a tree/blob instead of a commit-ish.
    NotACommit {
        /// Object id before annotated-tag peeling.
        oid: ObjectId,
        /// Terminal non-commit object type.
        object_type: ObjectType,
        /// Original user-provided revision string.
        spec: String,
    },
    /// Underlying repository, object, ref, or reachability error.
    Source(GitError),
}

impl From<GitError> for TagQueryError {
    fn from(value: GitError) -> Self {
        Self::Source(value)
    }
}

impl fmt::Display for TagQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedRevision { spec, .. } => write!(f, "malformed object name {spec}"),
            Self::NotACommit {
                oid, object_type, ..
            } => write!(
                f,
                "object {oid} is a {}, not a commit",
                object_type.as_str()
            ),
            Self::Source(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for TagQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::MalformedRevision { .. } | Self::NotACommit { .. } => None,
        }
    }
}

/// Result type for repository tag queries.
pub type TagQueryResult<T> = std::result::Result<T, TagQueryError>;

impl Repository {
    /// Select tag refs using Git's tag-list pattern, points-at, contains and
    /// merged filters.
    ///
    /// Sorting, columns, annotation truncation and `for-each-ref` formatting
    /// are presentation concerns and intentionally remain outside this engine
    /// operation. Object and reachability caches are shared with the repository
    /// session, so aliases of the same tag target are peeled only once.
    pub fn query_tags(&self, options: TagQueryOptions) -> TagQueryResult<TagQueryOutcome> {
        let format = self.object_format();
        let db = self.object_database();
        let points_at =
            resolve_filter_revisions(self, &options.points_at, FilterRevision::PointsAt)?;
        let contains = resolve_filter_revisions(self, &options.contains, FilterRevision::Contains)?;
        let no_contains =
            resolve_filter_revisions(self, &options.no_contains, FilterRevision::Contains)?;
        let merged = resolve_filter_revisions(self, &options.merged, FilterRevision::Merged)?;
        let no_merged = resolve_filter_revisions(self, &options.no_merged, FilterRevision::Merged)?;

        let contains_target_set = contains.into_iter().collect::<HashSet<_>>();
        let no_contains_target_set = no_contains.into_iter().collect::<HashSet<_>>();
        let mut reachability = (!contains_target_set.is_empty()
            || !no_contains_target_set.is_empty()
            || !merged.is_empty()
            || !no_merged.is_empty())
        .then(|| sley_rev::CommitReachability::new(self.git_dir(), format, db));
        let merged_reachable =
            tag_merged_reachable_set(db, reachability.as_mut(), format, &merged)?;
        let no_merged_reachable =
            tag_merged_reachable_set(db, reachability.as_mut(), format, &no_merged)?;

        let mut filter_tip_cache = HashMap::new();
        let tag_refs = self.references().list_refs_with_prefix("refs/tags/")?;
        let contains_match_cache =
            if contains_target_set.is_empty() && no_contains_target_set.is_empty() {
                HashMap::new()
            } else {
                let mut tips = HashSet::new();
                for reference in &tag_refs {
                    if let RefTarget::Direct(oid) = &reference.target
                        && let Some(tip) = tag_filter_tip(db, format, oid, &mut filter_tip_cache)
                    {
                        tips.insert(tip);
                    }
                }
                reachability
                    .as_mut()
                    .expect("contains filter initializes reachability")
                    .target_matches(tips, &contains_target_set, &no_contains_target_set, false)?
            };

        let mut entries = Vec::new();
        for reference in tag_refs {
            let Some(name) = reference.name.strip_prefix("refs/tags/") else {
                continue;
            };
            if !options.patterns.is_empty()
                && !options
                    .patterns
                    .iter()
                    .any(|pattern| refname_pattern_matches_case(pattern, name, options.ignore_case))
            {
                continue;
            }
            if !tag_points_at(db, format, &reference.target, &points_at)?
                || !tag_contains(
                    db,
                    format,
                    &reference.target,
                    &contains_target_set,
                    &no_contains_target_set,
                    &mut filter_tip_cache,
                    &contains_match_cache,
                )
                || !tag_merged(
                    db,
                    format,
                    &reference.target,
                    &merged_reachable,
                    &no_merged_reachable,
                    &mut filter_tip_cache,
                )?
            {
                continue;
            }
            entries.push(TagQueryEntry {
                name: name.to_string(),
                reference,
            });
        }
        Ok(TagQueryOutcome { entries })
    }
}

#[derive(Debug, Clone, Copy)]
enum FilterRevision {
    PointsAt,
    Contains,
    Merged,
}

fn resolve_filter_revisions(
    repo: &Repository,
    specs: &[String],
    kind: FilterRevision,
) -> TagQueryResult<Vec<ObjectId>> {
    specs
        .iter()
        .map(|spec| resolve_filter_revision(repo, spec, kind))
        .collect()
}

fn resolve_filter_revision(
    repo: &Repository,
    spec: &str,
    kind: FilterRevision,
) -> TagQueryResult<ObjectId> {
    let oid = match repo.rev_parse(spec) {
        Ok(oid) => oid,
        Err(GitError::NotFound(_) | GitError::InvalidFormat(_) | GitError::InvalidPath(_)) => {
            return Err(TagQueryError::MalformedRevision {
                kind: match kind {
                    FilterRevision::PointsAt => TagQueryRevisionKind::PointsAt,
                    FilterRevision::Contains => TagQueryRevisionKind::Contains,
                    FilterRevision::Merged => TagQueryRevisionKind::Merged,
                },
                spec: spec.to_string(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    if !matches!(kind, FilterRevision::Contains) {
        return Ok(oid);
    }
    peel_filter_to_commit(repo, oid, spec)
}

fn peel_filter_to_commit(repo: &Repository, oid: ObjectId, spec: &str) -> TagQueryResult<ObjectId> {
    let mut current = oid;
    loop {
        let object = repo.read_object(&current)?;
        match object.object_type {
            ObjectType::Commit => return Ok(current),
            ObjectType::Tag => {
                current = Tag::parse(repo.object_format(), &object.body)?.object;
            }
            object_type => {
                return Err(TagQueryError::NotACommit {
                    oid,
                    object_type,
                    spec: spec.to_string(),
                });
            }
        }
    }
}

fn tag_contains(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    target: &RefTarget,
    contains: &HashSet<ObjectId>,
    no_contains: &HashSet<ObjectId>,
    filter_tip_cache: &mut HashMap<ObjectId, Option<ObjectId>>,
    match_cache: &HashMap<ObjectId, sley_rev::ReachabilityTargetMatch>,
) -> bool {
    if contains.is_empty() && no_contains.is_empty() {
        return true;
    }
    let RefTarget::Direct(oid) = target else {
        return false;
    };
    let Some(tip) = tag_filter_tip(db, format, oid, filter_tip_cache) else {
        return false;
    };
    match_cache
        .get(&tip)
        .is_some_and(|matched| matched.reached_required && !matched.reached_excluded)
}

fn tag_merged(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    target: &RefTarget,
    merged_reachable: &HashSet<ObjectId>,
    no_merged_reachable: &HashSet<ObjectId>,
    filter_tip_cache: &mut HashMap<ObjectId, Option<ObjectId>>,
) -> Result<bool> {
    if merged_reachable.is_empty() && no_merged_reachable.is_empty() {
        return Ok(true);
    }
    let RefTarget::Direct(oid) = target else {
        return Ok(false);
    };
    let Some(tip) = tag_filter_tip(db, format, oid, filter_tip_cache) else {
        return Ok(false);
    };
    Ok(
        (merged_reachable.is_empty() || merged_reachable.contains(&tip))
            && !no_merged_reachable.contains(&tip),
    )
}

fn tag_filter_tip(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    cache: &mut HashMap<ObjectId, Option<ObjectId>>,
) -> Option<ObjectId> {
    if let Some(tip) = cache.get(oid) {
        return *tip;
    }
    let tip = sley_rev::peel_to_commit(db, format, oid).ok();
    cache.insert(*oid, tip);
    tip
}

fn tag_merged_reachable_set(
    db: &FileObjectDatabase,
    reachability: Option<&mut sley_rev::CommitReachability<'_, FileObjectDatabase>>,
    format: ObjectFormat,
    filters: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    if filters.is_empty() {
        return Ok(HashSet::new());
    }
    let Some(reachability) = reachability else {
        return Ok(HashSet::new());
    };
    let commits = filters
        .iter()
        .map(|oid| sley_rev::peel_to_commit(db, format, oid))
        .collect::<Result<Vec<_>>>()?;
    reachability.reachable_oids(commits, false)
}

fn tag_points_at(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    target: &RefTarget,
    points_at: &[ObjectId],
) -> Result<bool> {
    if points_at.is_empty() {
        return Ok(true);
    }
    let RefTarget::Direct(oid) = target else {
        return Ok(false);
    };
    if points_at.iter().any(|point| point == oid) {
        return Ok(true);
    }
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(false);
    }
    let parsed = Tag::parse(format, &object.body)?;
    Ok(points_at.iter().any(|point| point == &parsed.object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RefChange, TagCreate};
    use sley_object::{Commit, EncodedObject};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sley-tag-query-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp repository root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_commit(repo: &Repository, parent: Option<ObjectId>, message: &[u8]) -> ObjectId {
        let commit = Commit {
            tree: ObjectId::empty_tree(repo.object_format()),
            parents: parent.into_iter().collect(),
            author: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            committer: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            encoding: None,
            message: message.to_vec(),
        };
        repo.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .expect("write commit")
    }

    fn install_tags(repo: &Repository, refs: &[(&str, ObjectId)]) {
        let changes = refs
            .iter()
            .map(|(name, oid)| {
                RefChange::new(format!("refs/tags/{name}"), RefTarget::Direct(*oid))
                    .expect("valid tag ref")
            })
            .collect::<Vec<_>>();
        repo.apply_ref_changes(&changes).expect("install tag refs");
    }

    fn names(outcome: TagQueryOutcome) -> HashSet<String> {
        outcome
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    #[test]
    fn query_filters_patterns_points_and_history_for_both_hashes() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let root = TempDir::new();
            let repo = Repository::init_with_format(root.path(), format, false).expect("init repo");
            let root_commit = write_commit(&repo, None, b"root\n");
            let child_commit = write_commit(&repo, Some(root_commit), b"child\n");
            let blob = repo.write_blob(b"blob").expect("write blob");
            let annotated = repo
                .write_annotated_tag(TagCreate {
                    object: root_commit,
                    object_type: ObjectType::Commit,
                    name: b"v-annot".to_vec(),
                    tagger: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
                    message: b"annotated\n".to_vec(),
                })
                .expect("write annotated tag");
            install_tags(
                &repo,
                &[
                    ("v-root", root_commit),
                    ("v-child", child_commit),
                    ("v-annot", annotated),
                    ("blob", blob),
                ],
            );

            let pointed = repo
                .query_tags(TagQueryOptions {
                    patterns: vec!["V-*".into()],
                    ignore_case: true,
                    points_at: vec![root_commit.to_hex()],
                    ..TagQueryOptions::default()
                })
                .expect("points-at query");
            assert_eq!(
                names(pointed),
                HashSet::from(["v-annot".to_string(), "v-root".to_string()])
            );

            let contains = repo
                .query_tags(TagQueryOptions {
                    patterns: vec!["v-*".into()],
                    contains: vec![root_commit.to_hex()],
                    no_contains: vec![child_commit.to_hex()],
                    ..TagQueryOptions::default()
                })
                .expect("contains query");
            assert_eq!(
                names(contains),
                HashSet::from(["v-annot".to_string(), "v-root".to_string()])
            );

            let merged = repo
                .query_tags(TagQueryOptions {
                    merged: vec![child_commit.to_hex()],
                    no_merged: vec![root_commit.to_hex()],
                    ..TagQueryOptions::default()
                })
                .expect("merged query");
            assert_eq!(names(merged), HashSet::from(["v-child".to_string()]));

            let err = repo
                .query_tags(TagQueryOptions {
                    contains: vec![blob.to_hex()],
                    ..TagQueryOptions::default()
                })
                .expect_err("blob is not a commit-ish");
            assert!(matches!(
                err,
                TagQueryError::NotACommit {
                    object_type: ObjectType::Blob,
                    ..
                }
            ));
        }
    }
}
