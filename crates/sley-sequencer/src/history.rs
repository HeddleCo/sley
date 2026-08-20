//! Native history-editing operations shared by embedders and the CLI.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, TreeBuilder, TreeEntries};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry};

use crate::{CommitCreate, create_commit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRefScope {
    Branches,
    Head,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTreeEntry {
    pub mode: u32,
    pub oid: ObjectId,
}

#[derive(Debug, Clone)]
pub struct HistorySplitAnalysis {
    pub original_oid: ObjectId,
    pub original: Commit,
    pub parent_tree: ObjectId,
    pub original_tree: ObjectId,
    pub parent_entries: BTreeMap<Vec<u8>, HistoryTreeEntry>,
    pub original_entries: BTreeMap<Vec<u8>, HistoryTreeEntry>,
    pub changed_paths: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySplitSelection {
    pub path: Vec<u8>,
    pub mode: bool,
    pub content: bool,
}

#[derive(Debug, Clone)]
pub struct HistorySplitRequest {
    pub analysis: HistorySplitAnalysis,
    pub split_tree: ObjectId,
    pub first_message: Vec<u8>,
    pub second_message: Vec<u8>,
    pub committer: Vec<u8>,
    pub reflog_message: Vec<u8>,
    pub scope: HistoryRefScope,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySplitOutcome {
    pub first_commit: ObjectId,
    pub second_commit: ObjectId,
    pub updated_refs: Vec<(String, ObjectId, ObjectId)>,
}

#[derive(Debug, Clone)]
pub struct HistoryRewordAnalysis {
    pub original_oid: ObjectId,
    pub original: Commit,
    pub parent_entries: BTreeMap<Vec<u8>, HistoryTreeEntry>,
    pub original_entries: BTreeMap<Vec<u8>, HistoryTreeEntry>,
    pub changed_paths: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct HistoryRewordRequest {
    pub analysis: HistoryRewordAnalysis,
    pub message: Vec<u8>,
    pub committer: Vec<u8>,
    pub reflog_message: Vec<u8>,
    pub scope: HistoryRefScope,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRewordOutcome {
    pub rewritten_commit: ObjectId,
    pub updated_refs: Vec<(String, ObjectId, ObjectId)>,
}

pub fn analyze_history_reword(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    original_oid: ObjectId,
) -> Result<HistoryRewordAnalysis> {
    let original = read_commit(db, format, &original_oid)?;
    let parent_tree = match original.parents.first() {
        Some(parent) => read_commit(db, format, parent)?.tree,
        None => empty_tree_oid(format)?,
    };
    let parent_entries = flatten_tree(db, format, &parent_tree)?;
    let original_entries = flatten_tree(db, format, &original.tree)?;
    let changed_paths = parent_entries
        .keys()
        .chain(original_entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| parent_entries.get(path) != original_entries.get(path))
        .collect();
    Ok(HistoryRewordAnalysis {
        original_oid,
        original,
        parent_entries,
        original_entries,
        changed_paths,
    })
}

pub fn analyze_history_split(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    original_oid: ObjectId,
) -> Result<HistorySplitAnalysis> {
    let original = read_commit(db, format, &original_oid)?;
    if original.parents.len() > 1 {
        return Err(GitError::Command("cannot split up merge commit".into()));
    }
    let parent_tree = match original.parents.first() {
        Some(parent) => read_commit(db, format, parent)?.tree,
        None => empty_tree_oid(format)?,
    };
    let parent_entries = flatten_tree(db, format, &parent_tree)?;
    let original_entries = flatten_tree(db, format, &original.tree)?;
    let changed_paths = parent_entries
        .keys()
        .chain(original_entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| parent_entries.get(path) != original_entries.get(path))
        .collect();
    Ok(HistorySplitAnalysis {
        original_oid,
        original_tree: original.tree,
        original,
        parent_tree,
        parent_entries,
        original_entries,
        changed_paths,
    })
}

pub fn write_history_split_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    analysis: &HistorySplitAnalysis,
    selected: &[HistorySplitSelection],
) -> Result<ObjectId> {
    let mut entries = analysis.parent_entries.clone();
    for selection in selected {
        let before = analysis.parent_entries.get(&selection.path);
        let after = analysis.original_entries.get(&selection.path);
        match (before, after) {
            (_, None) if selection.mode || selection.content => {
                entries.remove(&selection.path);
            }
            (None, Some(entry)) if selection.mode || selection.content => {
                entries.insert(selection.path.clone(), entry.clone());
            }
            (Some(before), Some(after)) => {
                entries.insert(
                    selection.path.clone(),
                    HistoryTreeEntry {
                        mode: if selection.mode {
                            after.mode
                        } else {
                            before.mode
                        },
                        oid: if selection.content {
                            after.oid
                        } else {
                            before.oid
                        },
                    },
                );
            }
            _ => {}
        }
    }
    write_flat_tree(db, format, &entries)
}

/// Validate the ref-selection and descendant graph before prompting or opening
/// an editor. Git rejects an unrelated `--update-refs=head` target and any
/// replay range containing a merge before interactive split selection begins.
pub fn validate_history_split_targets(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    analysis: &HistorySplitAnalysis,
    scope: HistoryRefScope,
) -> Result<()> {
    validate_history_rewrite_targets(git_dir, format, db, analysis.original_oid, scope)
}

pub fn validate_history_reword_targets(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    original_oid: ObjectId,
    scope: HistoryRefScope,
) -> Result<()> {
    validate_history_rewrite_targets(git_dir, format, db, original_oid, scope)
}

fn validate_history_rewrite_targets(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    original_oid: ObjectId,
    scope: HistoryRefScope,
) -> Result<()> {
    let refs = FileRefStore::new(git_dir, format);
    let tips = reference_tips(&refs, scope)?;
    if scope == HistoryRefScope::Head
        && !tips
            .first()
            .is_some_and(|(_, tip)| is_ancestor(db, format, original_oid, *tip).unwrap_or(false))
    {
        return Err(GitError::Command(
            "rewritten commit must be an ancestor of HEAD when using --update-refs=head".into(),
        ));
    }
    for (_, tip) in tips {
        if !is_ancestor(db, format, original_oid, tip)? {
            continue;
        }
        let mut pending = vec![tip];
        let mut seen = HashSet::new();
        while let Some(oid) = pending.pop() {
            if oid == original_oid || !seen.insert(oid) {
                continue;
            }
            let commit = read_commit(db, format, &oid)?;
            let on_rewrite_path = commit
                .parents
                .iter()
                .any(|parent| is_ancestor(db, format, original_oid, *parent).unwrap_or(false));
            if on_rewrite_path && commit.parents.len() > 1 {
                return Err(GitError::Command(
                    "replaying merge commits is not supported yet!".into(),
                ));
            }
            pending.extend(commit.parents);
        }
    }
    Ok(())
}

pub fn execute_history_reword(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    request: HistoryRewordRequest,
) -> Result<HistoryRewordOutcome> {
    validate_history_reword_targets(
        git_dir,
        format,
        db,
        request.analysis.original_oid,
        request.scope,
    )?;

    let mut writer = db.clone();
    let replacement = create_commit(
        &mut writer,
        CommitCreate {
            tree: request.analysis.original.tree,
            parents: request.analysis.original.parents.clone(),
            author: request.analysis.original.author.clone(),
            committer: request.committer.clone(),
            message: request.message,
            encoding: request.analysis.original.encoding.clone(),
            signature: None,
        },
    )?;

    let refs = FileRefStore::new(git_dir, format);
    let tips = reference_tips(&refs, request.scope)?;
    let mut memo = HashMap::from([(request.analysis.original_oid, replacement)]);
    let mut updated = Vec::new();
    let mut rewriter = DescendantRewriter {
        db,
        format,
        writer: &mut writer,
        original: request.analysis.original_oid,
        committer: &request.committer,
        memo: &mut memo,
    };
    for (name, old) in tips {
        let mut visiting = HashSet::new();
        if let Some(new) = rewriter.rewrite(old, &mut visiting)? {
            updated.push((name, old, new));
        }
    }
    if updated.is_empty() {
        return Err(GitError::Command("failed replaying descendants".into()));
    }

    if !request.dry_run {
        let mut transaction = refs.transaction();
        for (name, old, new) in &updated {
            let reflog = refs
                .should_write_reflog_for_update(name, false)?
                .then(|| ReflogEntry {
                    old_oid: *old,
                    new_oid: *new,
                    committer: request.committer.clone(),
                    message: request.reflog_message.clone(),
                });
            transaction.update(RefUpdate {
                name: name.clone(),
                expected: Some(RefTarget::Direct(*old)),
                new: RefTarget::Direct(*new),
                reflog,
            });
        }
        transaction.commit()?;
    }

    Ok(HistoryRewordOutcome {
        rewritten_commit: replacement,
        updated_refs: updated,
    })
}

pub fn execute_history_split(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    request: HistorySplitRequest,
) -> Result<HistorySplitOutcome> {
    if request.split_tree == request.analysis.parent_tree {
        return Err(GitError::Command("split commit is empty".into()));
    }
    if request.split_tree == request.analysis.original_tree {
        return Err(GitError::Command(
            "split commit tree matches original commit".into(),
        ));
    }

    let mut writer = db.clone();
    let first = create_commit(
        &mut writer,
        CommitCreate {
            tree: request.split_tree,
            parents: request.analysis.original.parents.clone(),
            author: request.analysis.original.author.clone(),
            committer: request.committer.clone(),
            message: request.first_message,
            encoding: request.analysis.original.encoding.clone(),
            signature: None,
        },
    )?;
    let second = create_commit(
        &mut writer,
        CommitCreate {
            tree: request.analysis.original_tree,
            parents: vec![first],
            author: request.analysis.original.author.clone(),
            committer: request.committer.clone(),
            message: request.second_message,
            encoding: request.analysis.original.encoding.clone(),
            signature: None,
        },
    )?;

    let refs = FileRefStore::new(git_dir, format);
    validate_history_split_targets(git_dir, format, db, &request.analysis, request.scope)?;
    let tips = reference_tips(&refs, request.scope)?;

    let mut memo = HashMap::from([(request.analysis.original_oid, second)]);
    let mut updated = Vec::new();
    let mut rewriter = DescendantRewriter {
        db,
        format,
        writer: &mut writer,
        original: request.analysis.original_oid,
        committer: &request.committer,
        memo: &mut memo,
    };
    for (name, old) in tips {
        let mut visiting = HashSet::new();
        if let Some(new) = rewriter.rewrite(old, &mut visiting)? {
            updated.push((name, old, new));
        }
    }
    if updated.is_empty() {
        return Err(GitError::Command("failed replaying descendants".into()));
    }

    if !request.dry_run {
        let mut transaction = refs.transaction();
        for (name, old, new) in &updated {
            let reflog = refs
                .should_write_reflog_for_update(name, false)?
                .then(|| ReflogEntry {
                    old_oid: *old,
                    new_oid: *new,
                    committer: request.committer.clone(),
                    message: request.reflog_message.clone(),
                });
            transaction.update(RefUpdate {
                name: name.clone(),
                expected: Some(RefTarget::Direct(*old)),
                new: RefTarget::Direct(*new),
                reflog,
            });
        }
        transaction.commit()?;
    }

    Ok(HistorySplitOutcome {
        first_commit: first,
        second_commit: second,
        updated_refs: updated,
    })
}

fn reference_tips(refs: &FileRefStore, scope: HistoryRefScope) -> Result<Vec<(String, ObjectId)>> {
    let head = refs
        .read_ref("HEAD")?
        .ok_or_else(|| GitError::Command("cannot look up HEAD".into()))?;
    if scope == HistoryRefScope::Head {
        return match head {
            RefTarget::Symbolic(name) => Ok(refs
                .read_ref(&name)?
                .and_then(|target| match target {
                    RefTarget::Direct(oid) => Some((name, oid)),
                    RefTarget::Symbolic(_) => None,
                })
                .into_iter()
                .collect::<Vec<_>>()),
            RefTarget::Direct(oid) => Ok(vec![("HEAD".into(), oid)]),
        };
    }
    let mut tips = refs
        .list_refs_with_prefix("refs/heads/")?
        .into_iter()
        .filter_map(|reference| match reference.target {
            RefTarget::Direct(oid) => Some((reference.name, oid)),
            RefTarget::Symbolic(_) => None,
        })
        .collect::<Vec<_>>();
    if let RefTarget::Direct(oid) = head {
        tips.push(("HEAD".into(), oid));
    }
    tips.sort_by(|left, right| left.0.cmp(&right.0));
    tips.dedup_by(|left, right| left.0 == right.0);
    Ok(tips)
}

struct DescendantRewriter<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    writer: &'a mut FileObjectDatabase,
    original: ObjectId,
    committer: &'a [u8],
    memo: &'a mut HashMap<ObjectId, ObjectId>,
}

impl DescendantRewriter<'_> {
    fn rewrite(
        &mut self,
        oid: ObjectId,
        visiting: &mut HashSet<ObjectId>,
    ) -> Result<Option<ObjectId>> {
        if let Some(rewritten) = self.memo.get(&oid) {
            return Ok(Some(*rewritten));
        }
        if !visiting.insert(oid) {
            return Ok(None);
        }
        let commit = read_commit(self.db, self.format, &oid)?;
        if commit.parents.len() > 1 {
            if commit.parents.iter().any(|parent| {
                is_ancestor(self.db, self.format, self.original, *parent).unwrap_or(false)
            }) {
                return Err(GitError::Command(
                    "replaying merge commits is not supported yet!".into(),
                ));
            }
            return Ok(None);
        }
        let Some(parent) = commit.parents.first().copied() else {
            return Ok(None);
        };
        let Some(new_parent) = self.rewrite(parent, visiting)? else {
            return Ok(None);
        };
        let rewritten = create_commit(
            self.writer,
            CommitCreate {
                tree: commit.tree,
                parents: vec![new_parent],
                author: commit.author,
                committer: self.committer.to_vec(),
                message: commit.message,
                encoding: commit.encoding,
                signature: None,
            },
        )?;
        self.memo.insert(oid, rewritten);
        Ok(Some(rewritten))
    }
}

fn is_ancestor(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    ancestor: ObjectId,
    tip: ObjectId,
) -> Result<bool> {
    let mut pending = vec![tip];
    let mut seen = HashSet::new();
    while let Some(oid) = pending.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if seen.insert(oid) {
            pending.extend(read_commit(db, format, &oid)?.parents);
        }
    }
    Ok(false)
}

fn read_commit(db: &FileObjectDatabase, format: ObjectFormat, oid: &ObjectId) -> Result<Commit> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!("expected commit {oid}")));
    }
    Commit::parse(format, &object.body)
}

fn flatten_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, HistoryTreeEntry>> {
    fn walk(
        db: &FileObjectDatabase,
        format: ObjectFormat,
        tree: &ObjectId,
        prefix: &[u8],
        out: &mut BTreeMap<Vec<u8>, HistoryTreeEntry>,
    ) -> Result<()> {
        if *tree == empty_tree_oid(format)? {
            return Ok(());
        }
        let object = db.read_object(tree)?;
        for entry in TreeEntries::new(format, &object.body) {
            let entry = entry?;
            let mut path = prefix.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.name);
            if entry.mode == 0o040000 {
                walk(db, format, &entry.oid, &path, out)?;
            } else {
                out.insert(
                    path,
                    HistoryTreeEntry {
                        mode: entry.mode,
                        oid: entry.oid,
                    },
                );
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(db, format, tree, b"", &mut out)?;
    Ok(out)
}

#[derive(Default)]
struct TreeNode {
    leaves: BTreeMap<Vec<u8>, HistoryTreeEntry>,
    dirs: BTreeMap<Vec<u8>, TreeNode>,
}

fn write_flat_tree(
    db: &FileObjectDatabase,
    _format: ObjectFormat,
    entries: &BTreeMap<Vec<u8>, HistoryTreeEntry>,
) -> Result<ObjectId> {
    fn insert(node: &mut TreeNode, path: &[u8], entry: HistoryTreeEntry) -> Result<()> {
        if let Some(slash) = path.iter().position(|byte| *byte == b'/') {
            let name = path[..slash].to_vec();
            insert(
                node.dirs.entry(name).or_default(),
                &path[slash + 1..],
                entry,
            )
        } else if path.is_empty() {
            Err(GitError::InvalidPath("empty tree path".into()))
        } else {
            node.leaves.insert(path.to_vec(), entry);
            Ok(())
        }
    }
    fn write_node(db: &mut FileObjectDatabase, node: TreeNode) -> Result<ObjectId> {
        let mut builder = TreeBuilder::new();
        for (name, child) in node.dirs {
            let oid = write_node(db, child)?;
            builder.upsert_raw(name, 0o040000, oid);
        }
        for (name, entry) in node.leaves {
            builder.upsert_raw(name, entry.mode, entry.oid);
        }
        db.write_object(EncodedObject::new(ObjectType::Tree, builder.write()))
    }
    let mut root = TreeNode::default();
    for (path, entry) in entries {
        insert(&mut root, path, entry.clone())?;
    }
    write_node(&mut db.clone(), root)
}

fn empty_tree_oid(format: ObjectFormat) -> Result<ObjectId> {
    EncodedObject::new(ObjectType::Tree, Vec::new()).object_id(format)
}
