//! `git name-rev`'s reverse-mapping core (`builtin/name-rev.c`).
//!
//! Names commits by the closest ref that reaches them, expressed in `git
//! rev-parse` syntax (`tag~3`, `branch~2^2`, ...): every tip seeds a
//! first-parent-preferring walk, each commit keeps the "best" name under a
//! tag-then-distance-then-date ordering. Ref gathering/filtering and all
//! output rendering stay above the seam; this module owns the naming walk and
//! its bookkeeping.

use std::collections::HashMap;

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};

/// `MERGE_TRAVERSAL_WEIGHT` from upstream: crossing into a non-first parent is
/// treated as a very long hop so first-parent ancestry is strongly preferred.
const MERGE_TRAVERSAL_WEIGHT: i64 = 65535;

/// A ref selected as a starting point ("tip") for naming.
pub struct Tip {
    pub oid: ObjectId,
    /// Display name for the ref after prefix shortening (e.g. `tags/v1`, `main`).
    pub refname: String,
    /// The commit this tip resolves to (after peeling tag objects), if any.
    pub commit: Option<ObjectId>,
    /// Tag date for annotated tags, else the commit date; `i64::MAX` when unknown.
    pub taggerdate: i64,
    /// Whether the ref lives under `refs/tags/`.
    pub from_tag: bool,
    /// Whether the ref pointed at a tag object that had to be dereferenced.
    pub deref: bool,
}

/// Commit headers used by `name-rev`, cached per command invocation.
#[derive(Clone)]
pub struct CommitMetadata {
    pub parents: Vec<ObjectId>,
    pub committerdate: i64,
}

#[derive(Default)]
pub struct CommitMetadataCache {
    commits: HashMap<ObjectId, CommitMetadata>,
}

impl CommitMetadataCache {
    pub fn get_cached(&self, oid: &ObjectId) -> Option<&CommitMetadata> {
        self.commits.get(oid)
    }

    pub fn get_or_read(
        &mut self,
        db: &FileObjectDatabase,
        format: ObjectFormat,
        oid: &ObjectId,
    ) -> Result<Option<&CommitMetadata>> {
        if !self.commits.contains_key(oid) {
            let object = db.read_object(oid)?;
            if object.object_type != ObjectType::Commit {
                return Ok(None);
            }
            let commit = Commit::parse(format, &object.body)?;
            self.commits.insert(
                *oid,
                CommitMetadata {
                    parents: commit.parents,
                    committerdate: committer_timestamp(&commit.committer).unwrap_or(i64::MAX),
                },
            );
        }
        Ok(self.commits.get(oid))
    }

    pub fn get_or_parse_commit(
        &mut self,
        format: ObjectFormat,
        oid: &ObjectId,
        body: &[u8],
    ) -> Result<&CommitMetadata> {
        if !self.commits.contains_key(oid) {
            let commit = Commit::parse(format, body)?;
            self.commits.insert(
                *oid,
                CommitMetadata {
                    parents: commit.parents,
                    committerdate: committer_timestamp(&commit.committer).unwrap_or(i64::MAX),
                },
            );
        }
        self.commits.get(oid).ok_or_else(|| {
            GitError::InvalidObject(format!("commit metadata missing for {oid}"))
        })
    }
}

/// The best name discovered for a commit during the walk.
#[derive(Clone)]
pub struct RevName {
    pub tip_name: String,
    pub generation: i64,
    pub distance: i64,
    pub taggerdate: i64,
    pub from_tag: bool,
}

/// Seed a naming walk from every tip in upstream's `cmp_by_tag_and_age` order.
pub fn name_all_tips(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tips: &[Tip],
    rev_names: &mut HashMap<ObjectId, RevName>,
    commit_cache: &mut CommitMetadataCache,
) -> Result<()> {
    let mut order: Vec<usize> = (0..tips.len()).collect();
    // Stable sort over the alphabetically-ordered tips: tags first, then older
    // dates first; equal keys keep the alphabetical input order.
    order.sort_by(|&left, &right| {
        let a = &tips[left];
        let b = &tips[right];
        b.from_tag
            .cmp(&a.from_tag)
            .then_with(|| a.taggerdate.cmp(&b.taggerdate))
    });
    for index in order {
        let tip = &tips[index];
        let Some(commit) = &tip.commit else {
            continue;
        };
        name_rev(db, format, commit, tip, rev_names, commit_cache)?;
    }
    Ok(())
}

/// Walk first-parent-first from a tip, recording the best name for each commit.
fn name_rev(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start: &ObjectId,
    tip: &Tip,
    rev_names: &mut HashMap<ObjectId, RevName>,
    commit_cache: &mut CommitMetadataCache,
) -> Result<()> {
    let tip_name = if tip.deref {
        format!("{}^0", tip.refname)
    } else {
        tip.refname.clone()
    };
    if !create_or_update_name(rev_names, start, tip.taggerdate, 0, 0, tip.from_tag) {
        return Ok(());
    }
    if let Some(name) = rev_names.get_mut(start) {
        name.tip_name = tip_name;
    }

    let mut stack = vec![*start];
    while let Some(oid) = stack.pop() {
        let Some(current) = rev_names.get(&oid).cloned() else {
            continue;
        };
        let Some(commit) = commit_cache.get_or_read(db, format, &oid)? else {
            continue;
        };
        // Push parents so the first parent is processed before the others, just
        // like upstream's two-stack arrangement.
        let mut to_queue = Vec::new();
        for (index, parent) in commit.parents.iter().enumerate() {
            let parent_number = index + 1;
            let (generation, distance) = if parent_number > 1 {
                (0, current.distance + MERGE_TRAVERSAL_WEIGHT)
            } else {
                (current.generation + 1, current.distance + 1)
            };
            if create_or_update_name(
                rev_names,
                parent,
                tip.taggerdate,
                generation,
                distance,
                tip.from_tag,
            ) {
                let parent_tip_name = if parent_number > 1 {
                    get_parent_name(&current, parent_number)
                } else {
                    current.tip_name.clone()
                };
                if let Some(name) = rev_names.get_mut(parent) {
                    name.tip_name = parent_tip_name;
                }
                to_queue.push(*parent);
            }
        }
        while let Some(parent) = to_queue.pop() {
            stack.push(parent);
        }
    }
    Ok(())
}

/// Insert or replace the name for `commit` when the candidate is strictly better.
/// Returns whether the slot was (re)claimed, signalling that the walk should
/// descend through this commit's parents.
fn create_or_update_name(
    rev_names: &mut HashMap<ObjectId, RevName>,
    commit: &ObjectId,
    taggerdate: i64,
    generation: i64,
    distance: i64,
    from_tag: bool,
) -> bool {
    if let Some(existing) = rev_names.get(commit)
        && !is_better_name(existing, taggerdate, generation, distance, from_tag)
    {
        return false;
    }
    rev_names.insert(
        *commit,
        RevName {
            tip_name: String::new(),
            generation,
            distance,
            taggerdate,
            from_tag,
        },
    );
    true
}

/// Upstream `is_better_name`: tags beat non-tags; otherwise prefer the smaller
/// effective distance, then the older date.
fn is_better_name(
    name: &RevName,
    taggerdate: i64,
    generation: i64,
    distance: i64,
    from_tag: bool,
) -> bool {
    let name_distance = effective_distance(name.distance, name.generation);
    let new_distance = effective_distance(distance, generation);
    if from_tag && name.from_tag {
        return name_distance > new_distance;
    }
    if name.from_tag != from_tag {
        return from_tag;
    }
    if name_distance != new_distance {
        return name_distance > new_distance;
    }
    if name.taggerdate != taggerdate {
        return name.taggerdate > taggerdate;
    }
    false
}

fn effective_distance(distance: i64, generation: i64) -> i64 {
    distance
        + if generation > 0 {
            MERGE_TRAVERSAL_WEIGHT
        } else {
            0
        }
}

/// Build a non-first-parent's name: strip a trailing `^0`, fold in the run of
/// first-parent steps as `~<generation>`, then append `^<parent_number>`.
fn get_parent_name(name: &RevName, parent_number: usize) -> String {
    let base = name.tip_name.strip_suffix("^0").unwrap_or(&name.tip_name);
    if name.generation > 0 {
        format!("{base}~{}^{parent_number}", name.generation)
    } else {
        format!("{base}^{parent_number}")
    }
}

/// Render a commit's stored name, collapsing the `^0`/`~<generation>` suffixes
/// exactly as upstream's `get_rev_name`.
pub fn rev_name_string(name: &RevName) -> String {
    if name.generation == 0 {
        name.tip_name.clone()
    } else {
        let base = name.tip_name.strip_suffix("^0").unwrap_or(&name.tip_name);
        format!("{base}~{}", name.generation)
    }
}

/// Parse the trailing `<unix-seconds> <tz>` of a committer/tagger identity line.
pub fn committer_timestamp(ident: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(ident).ok()?;
    let close = text.rfind('>')?;
    let rest = text[close + 1..].trim();
    let seconds = rest.split_whitespace().next()?;
    seconds.parse::<i64>().ok()
}
