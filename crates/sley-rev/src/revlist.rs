use crate::{
    CommitMetadata, CommitRecord, RevisionRange, RevisionSelection, RevisionSelectionItem,
    parse_revision_range,
};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};

pub fn parse_rev_list_blob_limit(value: &str) -> Result<usize> {
    // `blob:limit=<n>` accepts a `git_parse_ulong` value: base-0 with an optional
    // case-insensitive k/m/g (1024-scaled) suffix, matching upstream's filter-spec parser.
    git_parse_blob_limit(value)
        .and_then(|limit| usize::try_from(limit).ok())
        .ok_or_else(|| {
            eprintln!("fatal: invalid filter-spec 'blob:limit={value}'");
            GitError::Exit(128)
        })
}

/// `git_parse_ulong` for `blob:limit`: a base-0 integer (decimal, `0x` hex, leading-`0` octal)
/// with an optional case-insensitive `k`/`m`/`g` suffix scaling by 1024/1024²/1024³.
pub fn git_parse_blob_limit(value: &str) -> Option<u64> {
    if value.is_empty() || value.contains('-') {
        return None;
    }
    let (digits, factor) = match value.as_bytes()[value.len() - 1] {
        b'k' | b'K' => (&value[..value.len() - 1], 1024u64),
        b'm' | b'M' => (&value[..value.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    let base = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        u64::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse::<u64>().ok()?
    };
    base.checked_mul(factor)
}

pub fn parse_rev_list_tree_depth(value: &str) -> Result<usize> {
    value.parse::<usize>().map_err(|_| {
        eprintln!("fatal: expected 'tree:<depth>'");
        GitError::Exit(128)
    })
}

pub fn parse_rev_list_object_type_filter(value: &str) -> Result<ObjectType> {
    match value {
        "blob" => Ok(ObjectType::Blob),
        "tree" => Ok(ObjectType::Tree),
        "commit" => Ok(ObjectType::Commit),
        "tag" => Ok(ObjectType::Tag),
        _ => {
            eprintln!("fatal: '{value}' for 'object:type=<type>' is not a valid object type");
            Err(GitError::Exit(128))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevListOrdering {
    Default,
    /// `--topo-order` — git's `REV_SORT_IN_GRAPH_ORDER`: a strict topological
    /// linearization whose tie-break preserves the traversal (commit-date) order
    /// via a LIFO emission queue with reversed initial tips.
    Topo,
    /// `--date-order` — git's `REV_SORT_BY_COMMIT_DATE`: topological with a
    /// committer-time priority queue tie-break.
    Date,
    /// `--author-date-order` — git's `REV_SORT_BY_AUTHOR_DATE`: topological with
    /// an author-time priority queue tie-break.
    AuthorDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevListMissingAction {
    Error,
    Print,
    PrintInfo,
    AllowAny,
    AllowPromisor,
    ExcludePromisor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevListWalkWithMissing {
    pub records: Vec<CommitRecord>,
    pub missing: Vec<ObjectId>,
}

/// `--topo-order` (git's `REV_SORT_IN_GRAPH_ORDER`).
///
/// Reproduces `sort_in_topological_order` byte-for-byte for the graph-order
/// sort: indegrees are computed from a committer-date-ordered pass, the initial
/// tips (indegree 1) are collected in that order and then *reversed*, and
/// emission is LIFO — parents are pushed onto the tail of the work queue when
/// their last child is emitted, and the next commit is popped from the tail.
/// This preserves the traversal order at the tips while guaranteeing no parent
/// precedes any of its children.
pub fn rev_list_topo_order(records: Vec<&CommitRecord>) -> Result<Vec<&CommitRecord>> {
    // git's `revs->commits` reaches `sort_in_topological_order` already in
    // committer-date order; reproduce that input ordering first so the tip /
    // LIFO sequence matches.
    let records = rev_list_commit_date_input_order(records)?;
    Ok(rev_list_topo_emit(records, None))
}

pub fn rev_list_date_order(records: Vec<&CommitRecord>) -> Result<Vec<&CommitRecord>> {
    let timestamps = records
        .iter()
        .map(|record| commit_identity_timestamp_i64(&record.commit.committer))
        .collect::<Result<Vec<_>>>()?;
    Ok(rev_list_ready_order(records, |idx| {
        (timestamps[idx], Reverse(idx))
    }))
}

/// `--author-date-order` (git's `REV_SORT_BY_AUTHOR_DATE`).
///
/// git feeds `sort_in_topological_order` the commit-date-ordered list and then
/// pops by author date, breaking equal-author-date ties by the prio-queue's
/// *insertion counter* (the order commits entered the queue: tips in input
/// order, then each parent when its last child is emitted). The static
/// input-index tiebreak used by [`rev_list_date_order`] diverges from this when
/// two simultaneously-ready commits share an author timestamp (t6012 #5: D and
/// B both at the same author date — git emits D first because its merge child E
/// lists D before B). Reproduce git's input order + insertion-counter tiebreak.
pub fn rev_list_author_date_order(records: Vec<&CommitRecord>) -> Result<Vec<&CommitRecord>> {
    let records = rev_list_commit_date_input_order(records)?;
    let timestamps = records
        .iter()
        .map(|record| commit_identity_timestamp_i64(&record.commit.author))
        .collect::<Result<Vec<_>>>()?;
    Ok(rev_list_prio_emit(records, &timestamps))
}

/// Faithful port of git's `sort_in_topological_order` prio-queue emission: pop
/// the highest-priority (newest-dated) ready commit, breaking ties by insertion
/// order. Tips are inserted in input order; a parent is inserted when its last
/// child is emitted (iterating that child's parents in order), exactly mirroring
/// git's `prio_queue` `ctr` tiebreak.
fn rev_list_prio_emit<'a>(
    records: Vec<&'a CommitRecord>,
    priority: &[i64],
) -> Vec<&'a CommitRecord> {
    let index_by_oid = records
        .iter()
        .enumerate()
        .map(|(idx, record)| (record.oid, idx))
        .collect::<HashMap<_, _>>();
    let n = records.len();
    // git's indegree: 1 plus one per in-list child. A commit is a tip / becomes
    // emittable when its indegree falls to 1 (all children emitted).
    let mut indegree = vec![1usize; n];
    for record in &records {
        for parent in &record.parents {
            if let Some(&parent_idx) = index_by_oid.get(parent) {
                indegree[parent_idx] += 1;
            }
        }
    }
    // `ctr[idx]` is the prio-queue insertion counter; `u64::MAX` = not yet queued.
    let mut ctr = vec![u64::MAX; n];
    let mut next_ctr = 0u64;
    let mut ready: Vec<usize> = Vec::new();
    for idx in 0..n {
        if indegree[idx] == 1 {
            ctr[idx] = next_ctr;
            next_ctr += 1;
            ready.push(idx);
        }
    }
    let mut out = Vec::with_capacity(n);
    while !ready.is_empty() {
        // Newest priority first; ties broken by the *smallest* insertion counter
        // (earlier into the queue), matching git's prio_queue compare-on-ctr.
        let pos = ready
            .iter()
            .enumerate()
            .max_by_key(|(_, idx)| (priority[**idx], Reverse(ctr[**idx])))
            .map(|(pos, _)| pos)
            .expect("ready is not empty");
        let idx = ready.swap_remove(pos);
        let record = records[idx];
        for parent in &record.parents {
            if let Some(&parent_idx) = index_by_oid.get(parent) {
                if indegree[parent_idx] == 0 {
                    continue;
                }
                indegree[parent_idx] -= 1;
                if indegree[parent_idx] == 1 {
                    ctr[parent_idx] = next_ctr;
                    next_ctr += 1;
                    ready.push(parent_idx);
                }
            }
        }
        indegree[idx] = 0;
        out.push(record);
    }
    // Defensive: any commit never queued (only reachable through a cycle, which a
    // DAG never produces) is appended in input order.
    for (idx, record) in records.into_iter().enumerate() {
        if ctr[idx] == u64::MAX {
            out.push(record);
        }
    }
    out
}

/// Order a reachable commit set into the committer-date order git's traversal
/// produces before it hands the list to `sort_in_topological_order`. Newest
/// committer time first, ties broken by the SMALLER oid (matching git's
/// `(commit_time, Reverse(oid))` priority during the limiting walk).
pub fn rev_list_commit_date_input_order(records: Vec<&CommitRecord>) -> Result<Vec<&CommitRecord>> {
    let mut keyed = records
        .into_iter()
        .map(|record| {
            commit_identity_timestamp_i64(&record.commit.committer).map(|ts| (ts, record))
        })
        .collect::<Result<Vec<_>>>()?;
    // Newest first; for equal times the smaller oid first.
    keyed.sort_by(|(ta, a), (tb, b)| tb.cmp(ta).then_with(|| a.oid.cmp(&b.oid)));
    Ok(keyed.into_iter().map(|(_, record)| record).collect())
}

/// Linearize `records` (already in git's input order) topologically using a
/// LIFO emission queue with reversed initial tips — git's graph-order sort.
///
/// `priority` is unused for graph order (`None`); the parameter is reserved so a
/// future date-keyed prio-queue variant can share this readiness machinery, but
/// the date orders currently route through [`rev_list_ready_order`] which is
/// already byte-identical to git for them.
pub fn rev_list_topo_emit<'a>(
    records: Vec<&'a CommitRecord>,
    priority: Option<&[i64]>,
) -> Vec<&'a CommitRecord> {
    let _ = priority;
    let index_by_oid = records
        .iter()
        .enumerate()
        .map(|(idx, record)| (record.oid, idx))
        .collect::<HashMap<_, _>>();
    // Indegree: mark every listed commit 1, then for each listed parent that is
    // itself in the set, increment. A commit whose indegree stays 1 is a tip.
    let mut indegree = vec![1usize; records.len()];
    for record in &records {
        for parent in &record.parents {
            if let Some(&pi) = index_by_oid.get(parent) {
                indegree[pi] += 1;
            }
        }
    }
    // Tips in input order, then reversed for LIFO emission.
    let mut queue: Vec<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(idx, deg)| (*deg == 1).then_some(idx))
        .collect();
    queue.reverse();
    let mut out = Vec::with_capacity(records.len());
    while let Some(idx) = queue.pop() {
        let record = records[idx];
        for parent in &record.parents {
            if let Some(&pi) = index_by_oid.get(parent) {
                if indegree[pi] == 0 {
                    continue;
                }
                indegree[pi] -= 1;
                if indegree[pi] == 1 {
                    queue.push(pi);
                }
            }
        }
        indegree[idx] = 0;
        out.push(record);
    }
    out
}

pub fn rev_list_ready_order<K: Ord>(
    records: Vec<&CommitRecord>,
    ready_key: impl Fn(usize) -> K,
) -> Vec<&CommitRecord> {
    let index_by_oid = records
        .iter()
        .enumerate()
        .map(|(idx, record)| (record.oid, idx))
        .collect::<HashMap<_, _>>();
    let mut remaining_children = vec![0usize; records.len()];
    for record in &records {
        for parent in &record.parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] += 1;
            }
        }
    }
    let mut ready = remaining_children
        .iter()
        .enumerate()
        .filter_map(|(idx, child_count)| (*child_count == 0).then_some(idx))
        .collect::<Vec<_>>();
    let mut emitted = vec![false; records.len()];
    let mut out = Vec::with_capacity(records.len());
    while !ready.is_empty() {
        let ready_pos = ready
            .iter()
            .enumerate()
            .max_by_key(|(_, idx)| ready_key(**idx))
            .map(|(pos, _)| pos)
            .expect("ready is not empty");
        let idx = ready.swap_remove(ready_pos);
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        let record = records[idx];
        out.push(record);
        for parent in &record.parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] = remaining_children[parent_idx].saturating_sub(1);
                if remaining_children[parent_idx] == 0 && !emitted[parent_idx] {
                    ready.push(parent_idx);
                }
            }
        }
    }
    for (idx, record) in records.into_iter().enumerate() {
        if !emitted[idx] {
            out.push(record);
        }
    }
    out
}

/// Date-order a metadata-only commit list. Mirrors [`rev_list_date_order`] /
/// [`rev_list_ready_order`] exactly (topological readiness + a
/// `(commit_time, Reverse(idx))` key), but on [`CommitMetadata`] whose
/// committer time came from the commit-graph — so the order is byte-identical to
/// the full-record path without reading any commit object.
pub fn rev_list_metadata_date_order(records: Vec<CommitMetadata>) -> Vec<CommitMetadata> {
    let index_by_oid = records
        .iter()
        .enumerate()
        .map(|(idx, record)| (record.oid, idx))
        .collect::<HashMap<_, _>>();
    let mut remaining_children = vec![0usize; records.len()];
    for record in &records {
        for parent in &record.parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] += 1;
            }
        }
    }
    let mut ready = remaining_children
        .iter()
        .enumerate()
        .filter_map(|(idx, child_count)| (*child_count == 0).then_some(idx))
        .collect::<Vec<_>>();
    let mut emitted = vec![false; records.len()];
    let mut order = Vec::with_capacity(records.len());
    while !ready.is_empty() {
        let ready_pos = ready
            .iter()
            .enumerate()
            .max_by_key(|(_, idx)| (records[**idx].commit_time, Reverse(**idx)))
            .map(|(pos, _)| pos)
            .expect("ready is not empty");
        let idx = ready.swap_remove(ready_pos);
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        order.push(idx);
        for parent in &records[idx].parents {
            if let Some(parent_idx) = index_by_oid.get(parent).copied() {
                remaining_children[parent_idx] = remaining_children[parent_idx].saturating_sub(1);
                if remaining_children[parent_idx] == 0 && !emitted[parent_idx] {
                    ready.push(parent_idx);
                }
            }
        }
    }
    for (idx, was_emitted) in emitted.iter().enumerate() {
        if !was_emitted {
            order.push(idx);
        }
    }
    let mut slots = records.into_iter().map(Some).collect::<Vec<_>>();
    order
        .into_iter()
        .filter_map(|idx| slots[idx].take())
        .collect()
}

pub fn rev_list_walk_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
    first_parent: bool,
) -> Result<Vec<CommitRecord>> {
    rev_list_walk_commits_with_missing(
        db,
        format,
        starts,
        first_parent,
        RevListMissingAction::Error,
    )
}

pub fn rev_list_walk_commits_with_missing(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
    first_parent: bool,
    missing_action: RevListMissingAction,
) -> Result<Vec<CommitRecord>> {
    Ok(rev_list_walk_commits_with_missing_details(
        db,
        format,
        starts,
        first_parent,
        missing_action,
    )?
    .records)
}

pub fn rev_list_walk_commits_with_missing_details(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
    first_parent: bool,
    missing_action: RevListMissingAction,
) -> Result<RevListWalkWithMissing> {
    if !first_parent {
        return rev_list_walk_commits_all_parents_with_missing(db, format, starts, missing_action);
    }
    validate_commit_graph_for_rev_walk(db, format)?;
    let grafts = load_commit_grafts(db, format);
    let mut seen = HashSet::new();
    let mut missing_seen = HashSet::new();
    let mut pending = starts.into_iter().collect::<VecDeque<_>>();
    let mut out = Vec::new();
    let mut missing = Vec::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if missing_action == RevListMissingAction::ExcludePromisor && db.is_promised_object(&oid) {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(err) if rev_list_missing_allowed(db, &oid, missing_action) => {
                let _ = err;
                if matches!(
                    missing_action,
                    RevListMissingAction::Print | RevListMissingAction::PrintInfo
                ) && missing_seen.insert(oid)
                {
                    missing.push(oid);
                }
                continue;
            }
            Err(err) => return Err(err),
        };
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        let parents = graft_overridden_parents(db, &grafts, &oid, commit.parents.clone());
        if let Some(parent) = parents.first() {
            pending.push_back(*parent);
        }
        out.push(CommitRecord {
            oid,
            parents,
            commit,
        });
    }
    Ok(RevListWalkWithMissing {
        records: out,
        missing,
    })
}

/// Commit grafts from `$GIT_DIR/info/grafts`: each non-comment line
/// `<commit> <parent>...` overrides that commit's parent list during traversal
/// (git's `read_graft_line` / `lookup_commit_graft`). `$GIT_DIR` is the parent
/// of the database's objects directory; a missing/empty file yields no grafts.
pub fn load_commit_grafts(
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> HashMap<ObjectId, Vec<ObjectId>> {
    let Some(git_dir) = db.objects_dir().parent() else {
        return HashMap::new();
    };
    load_commit_grafts_from_git_dir(git_dir, format)
}

pub fn load_commit_grafts_from_git_dir(
    git_dir: &std::path::Path,
    format: ObjectFormat,
) -> HashMap<ObjectId, Vec<ObjectId>> {
    let Ok(contents) = std::fs::read_to_string(git_dir.join("info").join("grafts")) else {
        return HashMap::new();
    };
    let mut grafts = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut oids = line
            .split_whitespace()
            .filter_map(|hex| ObjectId::from_hex(format, hex).ok());
        let Some(commit) = oids.next() else {
            continue;
        };
        grafts.insert(commit, oids.collect::<Vec<_>>());
    }
    grafts
}

/// Apply a commit-graft override (if any) to a commit's parsed parents before
/// the shallow-boundary seam runs.
fn graft_overridden_parents(
    db: &FileObjectDatabase,
    grafts: &HashMap<ObjectId, Vec<ObjectId>>,
    oid: &ObjectId,
    parents: Vec<ObjectId>,
) -> Vec<ObjectId> {
    let overridden = match grafts.get(oid) {
        Some(graft) => graft.clone(),
        None => parents,
    };
    sley_odb::grafted_parents(db, oid, overridden)
}

fn rev_list_walk_commits_all_parents_with_missing(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
    missing_action: RevListMissingAction,
) -> Result<RevListWalkWithMissing> {
    validate_commit_graph_for_rev_walk(db, format)?;
    let grafts = load_commit_grafts(db, format);
    let mut seen = HashSet::new();
    let mut missing_seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.into_iter().collect();
    let mut out = Vec::new();
    let mut missing = Vec::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if missing_action == RevListMissingAction::ExcludePromisor && db.is_promised_object(&oid) {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(err) if rev_list_missing_allowed(db, &oid, missing_action) => {
                let _ = err;
                if matches!(
                    missing_action,
                    RevListMissingAction::Print | RevListMissingAction::PrintInfo
                ) && missing_seen.insert(oid)
                {
                    missing.push(oid);
                }
                continue;
            }
            Err(err) => return Err(err),
        };
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        let parents = graft_overridden_parents(db, &grafts, &oid, commit.parents.clone());
        pending.extend(parents.iter().cloned());
        out.push(CommitRecord {
            oid,
            parents,
            commit,
        });
    }
    Ok(RevListWalkWithMissing {
        records: out,
        missing,
    })
}

fn rev_list_missing_allowed(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    missing_action: RevListMissingAction,
) -> bool {
    match missing_action {
        RevListMissingAction::Error => false,
        RevListMissingAction::Print
        | RevListMissingAction::PrintInfo
        | RevListMissingAction::AllowAny => true,
        RevListMissingAction::AllowPromisor | RevListMissingAction::ExcludePromisor => {
            db.is_promised_object(oid)
        }
    }
}

fn validate_commit_graph_for_rev_walk(db: &FileObjectDatabase, format: ObjectFormat) -> Result<()> {
    let Some(git_dir) = db.objects_dir().parent() else {
        return Ok(());
    };
    match crate::load_direct_commit_graph(git_dir, format) {
        crate::DirectCommitGraph::Invalid(message) => Err(GitError::InvalidFormat(message)),
        crate::DirectCommitGraph::Missing | crate::DirectCommitGraph::Raw(_) => Ok(()),
    }
}

pub fn rev_list_walk_commits_all_parents(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
    missing_action: RevListMissingAction,
) -> Result<Vec<CommitRecord>> {
    Ok(rev_list_walk_commits_all_parents_with_missing(db, format, starts, missing_action)?.records)
}

pub fn rev_list_no_walk_commits(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<CommitRecord>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for oid in starts {
        if !seen.insert(oid) {
            continue;
        }
        out.push(read_rev_list_commit_record(db, format, oid)?);
    }
    Ok(out)
}

pub fn read_rev_list_commit_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: ObjectId,
) -> Result<CommitRecord> {
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(format, &object.body)?;
    let parents = commit.parents.clone();
    Ok(CommitRecord {
        oid,
        parents,
        commit,
    })
}

pub fn add_rev_list_revision_arg(
    value: &str,
    not: bool,
    includes: &mut Vec<String>,
    excludes: &mut Vec<String>,
    linear_ranges: &mut Vec<(String, String, bool)>,
    symmetric_ranges: &mut Vec<(String, String, bool)>,
) -> Result<()> {
    if let Some(exclude) = value.strip_prefix('^')
        && !exclude.is_empty()
    {
        if not {
            includes.push(exclude.to_string());
        } else {
            excludes.push(exclude.to_string());
        }
        return Ok(());
    }
    let selection = if value.contains("..") {
        let Some(range) = parse_revision_range(value) else {
            return Err(GitError::Command(format!(
                "unsupported rev-list range {value}"
            )));
        };
        let mut selection = RevisionSelection::new();
        selection.range(range);
        selection
    } else {
        RevisionSelection::from_specs([value])?
    };
    for item in selection.items() {
        match item {
            RevisionSelectionItem::Include(rev) => {
                if not {
                    excludes.push(rev.clone());
                } else {
                    includes.push(rev.clone());
                }
            }
            RevisionSelectionItem::Exclude(rev) => {
                if not {
                    includes.push(rev.clone());
                } else {
                    excludes.push(rev.clone());
                }
            }
            RevisionSelectionItem::Range(RevisionRange::Asymmetric { start, end }) => {
                linear_ranges.push((start.clone(), end.clone(), not));
            }
            RevisionSelectionItem::Range(RevisionRange::Symmetric { left, right }) => {
                symmetric_ranges.push((left.clone(), right.clone(), not));
            }
        }
    }
    Ok(())
}

pub fn commit_identity_timestamp(raw: &[u8]) -> String {
    // The raw timestamp digit-run (git's `%at`/`%ct`), located git's way so a
    // broken email still yields the date and a whitespace-only date yields the
    // empty string rather than a misparse.
    sley_core::split_ident_line(raw)
        .and_then(|fields| fields.date)
        .map(|date| String::from_utf8_lossy(date).into_owned())
        .unwrap_or_default()
}

pub fn commit_identity_timestamp_i64(raw: &[u8]) -> Result<i64> {
    // git parses the committer date once at `parse_commit` time and stores
    // `commit->date == 0` when it is missing or unparsable; downstream ordering,
    // age filtering, and `--timestamp` output all see that epoch sentinel rather
    // than failing. Mirror that: never error, just fall back to 0.
    Ok(commit_identity_timestamp(raw).parse::<i64>().unwrap_or(0))
}
