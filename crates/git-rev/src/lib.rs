use git_core::{GitError, ObjectId, Result};
use git_formats::{Commit, CommitGraph, Index, ObjectType, Tag, Tree};
use git_odb::{FileObjectDatabase, ObjectPrefixResolution, ObjectReader};
use git_refs::{FileRefStore, PackedRef, RefTarget};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSpec {
    pub raw: String,
}

impl RevisionSpec {
    pub fn parse(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(GitError::InvalidFormat("empty revision spec".into()));
        }
        Ok(Self { raw })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub oid: ObjectId,
    pub parents: Vec<ObjectId>,
    pub commit: Commit,
}

pub fn resolve_revision(
    git_dir: impl AsRef<Path>,
    format: git_core::ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    resolve_revision_with_reader(git_dir, format, &db, rev)
}

pub fn resolve_revision_with_reader<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    rev: &str,
) -> Result<ObjectId> {
    // `:/text` and `:[N:]path` are anchored at the start of the spec; handle them
    // before the `^`/`~` suffix machinery so the leading colon is not mistaken
    // for a normal revision name.
    if let Some(text) = rev.strip_prefix(":/") {
        return search_commit_message_all(git_dir, format, reader, text);
    }
    if let Some(rest) = rev.strip_prefix(':') {
        let (stage, path) = parse_index_stage_path(rest);
        return resolve_index_path(git_dir, format, stage, path);
    }
    // `<rev>:<path>` resolves to the object at `<path>` within `<rev>`'s tree. The
    // colon binds looser than the `^`/`~` navigation suffixes, so an unsuffixed
    // colon here means the whole left side is the revision-ish to peel to a tree.
    if let Some((rev_part, path)) = split_rev_path(rev) {
        return resolve_rev_path(git_dir, format, reader, rev_part, path);
    }
    if let Some((base, suffix)) = split_revision_suffix(rev)? {
        if base.is_empty() {
            return Err(GitError::InvalidFormat(format!(
                "revision {rev} has empty base"
            )));
        }
        let base_oid = resolve_revision_with_reader(git_dir, format, reader, base)?;
        return apply_revision_suffix(git_dir, reader, format, &base_oid, suffix, rev);
    }
    resolve_revision_name(git_dir, format, rev)
}

fn resolve_revision_name(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    rev: &str,
) -> Result<ObjectId> {
    if rev.len() == format.hex_len() && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return ObjectId::from_hex(format, rev);
    }
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    if let Some(oid) = resolve_revision_ref(&refs, rev)? {
        return Ok(oid);
    }
    if rev.len() >= 4
        && rev.len() < format.hex_len()
        && rev.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        match db.resolve_prefix(rev)? {
            ObjectPrefixResolution::Unique(oid) => return Ok(oid),
            ObjectPrefixResolution::Ambiguous(matches) => {
                let mut names = matches
                    .into_iter()
                    .map(|oid| oid.to_string())
                    .collect::<Vec<_>>();
                names.sort();
                return Err(GitError::InvalidObjectId(format!(
                    "short object ID {rev} is ambiguous: {}",
                    names.join(", ")
                )));
            }
            ObjectPrefixResolution::Missing => {}
        }
    }
    Err(GitError::NotFound(format!("revision {rev}")))
}

fn resolve_revision_ref(refs: &FileRefStore, rev: &str) -> Result<Option<ObjectId>> {
    let target = if rev == "HEAD" {
        refs.read_ref("HEAD")?
    } else if rev.starts_with("refs/") {
        refs.read_ref(rev)?
    } else {
        refs.read_ref(&format!("refs/heads/{rev}"))?
            .or(refs.read_ref(&format!("refs/tags/{rev}"))?)
    };
    match target {
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        Some(RefTarget::Symbolic(name)) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionSuffix<'a> {
    Parent(usize),
    FirstParent(usize),
    Peel(PeelKind),
    /// `<rev>^{/text}` — first matching commit in `<rev>`'s first-parent ancestry.
    Search(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeelKind {
    AnyNonTag,
    Object,
    Commit,
    Tree,
    Tag,
}

fn split_revision_suffix(rev: &str) -> Result<Option<(&str, RevisionSuffix<'_>)>> {
    let caret = rev.rfind('^');
    let tilde = rev.rfind('~');
    let Some((op, pos)) = (match (caret, tilde) {
        (Some(caret), Some(tilde)) if caret > tilde => Some(('^', caret)),
        (Some(caret), Some(tilde)) if tilde > caret => Some(('~', tilde)),
        (Some(caret), None) => Some(('^', caret)),
        (None, Some(tilde)) => Some(('~', tilde)),
        (None, None) => None,
        _ => None,
    }) else {
        return Ok(None);
    };
    let (base, suffix) = rev.split_at(pos);
    let suffix = &suffix[1..];
    match op {
        '^' => {
            if let Some(text) = parse_search_suffix(rev, suffix)? {
                return Ok(Some((base, RevisionSuffix::Search(text))));
            }
            let parent = if suffix.is_empty() {
                1
            } else if let Some(kind) = parse_peel_suffix(rev, suffix)? {
                return Ok(Some((base, RevisionSuffix::Peel(kind))));
            } else if suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                parse_revision_count(rev, suffix)?
            } else {
                return Ok(None);
            };
            Ok(Some((base, RevisionSuffix::Parent(parent))))
        }
        '~' => {
            let count = if suffix.is_empty() {
                1
            } else if suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                parse_revision_count(rev, suffix)?
            } else {
                return Ok(None);
            };
            Ok(Some((base, RevisionSuffix::FirstParent(count))))
        }
        _ => Ok(None),
    }
}

fn parse_peel_suffix(rev: &str, suffix: &str) -> Result<Option<PeelKind>> {
    if !suffix.starts_with('{') {
        return Ok(None);
    }
    let Some(kind) = suffix
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
    else {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision peel suffix in {rev}"
        )));
    };
    let kind = match kind {
        "" => PeelKind::AnyNonTag,
        "object" => PeelKind::Object,
        "commit" => PeelKind::Commit,
        "tree" => PeelKind::Tree,
        "tag" => PeelKind::Tag,
        other => {
            return Err(GitError::Unsupported(format!(
                "revision peel suffix ^{{{other}}}"
            )));
        }
    };
    Ok(Some(kind))
}

fn parse_search_suffix<'a>(rev: &str, suffix: &'a str) -> Result<Option<&'a str>> {
    let Some(inner) = suffix.strip_prefix("{/") else {
        return Ok(None);
    };
    let Some(text) = inner.strip_suffix('}') else {
        return Err(GitError::InvalidFormat(format!(
            "invalid revision search suffix in {rev}"
        )));
    };
    Ok(Some(text))
}

fn parse_revision_count(rev: &str, text: &str) -> Result<usize> {
    text.parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid revision suffix in {rev}")))
}

fn apply_revision_suffix<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: git_core::ObjectFormat,
    base: &ObjectId,
    suffix: RevisionSuffix<'_>,
    raw_rev: &str,
) -> Result<ObjectId> {
    match suffix {
        RevisionSuffix::Parent(parent) => {
            if parent == 0 {
                return Err(GitError::InvalidFormat(format!(
                    "invalid zero parent in {raw_rev}"
                )));
            }
            commit_parents_with_graph(git_dir, reader, format, base)?
                .get(parent - 1)
                .cloned()
                .ok_or_else(|| GitError::NotFound(format!("parent {parent} of {base}")))
        }
        RevisionSuffix::FirstParent(count) => {
            let mut current = base.clone();
            for _ in 0..count {
                current = commit_parents_with_graph(git_dir, reader, format, &current)?
                    .first()
                    .cloned()
                    .ok_or_else(|| GitError::NotFound(format!("first parent of {current}")))?;
            }
            Ok(current)
        }
        RevisionSuffix::Peel(kind) => peel_revision(reader, format, base, kind),
        RevisionSuffix::Search(text) => {
            search_commit_message_first_parent(git_dir, reader, format, base, text)
        }
    }
}

fn commit_parents_with_graph<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<ObjectId>> {
    if let Some(parents) = commit_graph_parents_for_oid(git_dir, format, oid)? {
        return Ok(parents);
    }
    commit_parents(reader, format, oid)
}

fn commit_graph_parents_for_oid(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<Vec<ObjectId>>> {
    let path = git_dir.join("objects").join("info").join("commit-graph");
    if !path.exists() {
        return Ok(None);
    }
    let graph = CommitGraph::parse(&fs::read(path)?, format)?;
    let Some(entry) = graph.find(oid) else {
        return Ok(None);
    };
    let mut parents = Vec::with_capacity(entry.parents.len());
    for parent in &entry.parents {
        let parent = usize::try_from(*parent)
            .map_err(|_| GitError::InvalidFormat("commit-graph parent index overflow".into()))?;
        let Some(parent_entry) = graph.commits.get(parent) else {
            return Err(GitError::InvalidFormat(
                "commit-graph parent points past commit table".into(),
            ));
        };
        parents.push(parent_entry.oid.clone());
    }
    Ok(Some(parents))
}

fn commit_parents<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<Vec<ObjectId>> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse(format, &object.body)?.parents)
}

fn peel_revision<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
    kind: PeelKind,
) -> Result<ObjectId> {
    match kind {
        PeelKind::AnyNonTag => peel_tags(reader, format, oid),
        PeelKind::Object => {
            reader.read_object(oid)?;
            Ok(oid.clone())
        }
        PeelKind::Commit => peel_to_commit(reader, format, oid),
        PeelKind::Tree => peel_to_tree(reader, format, oid),
        PeelKind::Tag => {
            let object = reader.read_object(oid)?;
            if object.object_type == ObjectType::Tag {
                Ok(oid.clone())
            } else {
                Err(GitError::InvalidObject(format!(
                    "expected tag {oid}, found {}",
                    object.object_type.as_str()
                )))
            }
        }
    }
}

pub fn peel_tags<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(oid.clone());
    }
    let tag = Tag::parse(format, &object.body)?;
    peel_tags(reader, format, &tag.object)
}

pub fn peel_to_tree<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    match object.object_type {
        ObjectType::Tree => Ok(oid.clone()),
        ObjectType::Commit => Ok(Commit::parse(format, &object.body)?.tree),
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            peel_to_tree(reader, format, &tag.object)
        }
        other => Err(GitError::InvalidObject(format!(
            "expected tree-ish {oid}, found {}",
            other.as_str()
        ))),
    }
}

pub fn peel_to_commit<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = reader.read_object(oid)?;
    match object.object_type {
        ObjectType::Commit => Ok(oid.clone()),
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            peel_to_commit(reader, format, &tag.object)
        }
        other => Err(GitError::InvalidObject(format!(
            "expected commit-ish {oid}, found {}",
            other.as_str()
        ))),
    }
}

pub fn pack_refs_with_auto_peel(
    git_dir: impl AsRef<Path>,
    format: git_core::ObjectFormat,
    prune_loose: bool,
) -> Result<Vec<PackedRef>> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    refs.pack_refs_with_peeler(prune_loose, |_, oid| {
        let peeled = peel_tags(&db, format, oid)?;
        if &peeled == oid {
            Ok(None)
        } else {
            Ok(Some(peeled))
        }
    })
}

pub fn parse_commit_parents(format: git_core::ObjectFormat, body: &[u8]) -> Result<Vec<ObjectId>> {
    let text = std::str::from_utf8(body).map_err(|err| GitError::InvalidObject(err.to_string()))?;
    let mut parents = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(hex) = line.strip_prefix("parent ") {
            parents.push(ObjectId::from_hex(format, hex)?);
        }
    }
    Ok(parents)
}

pub fn walk_commits<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    starts: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<CommitRecord>> {
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.into_iter().collect();
    let mut out = Vec::new();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let object = reader.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        let parents = commit.parents.clone();
        pending.extend(parents.iter().cloned());
        out.push(CommitRecord {
            oid,
            parents,
            commit,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// `<rev>:<path>` resolution
// ---------------------------------------------------------------------------

/// Resolve `<rev>:<path>` to the object id of `<path>` within `<rev>`'s tree.
///
/// `rev` is peeled to a tree (so a commit, tag, or tree id all work) and then
/// `path` is walked component by component. The result is the blob id for a
/// file path or the subtree id for a directory path; an empty `path` resolves
/// to the tree itself. Missing components and attempts to descend through a
/// non-tree entry both report a git-style "path '<path>' does not exist in
/// '<rev>'" error.
pub fn resolve_rev_path<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    rev: &str,
    path: &str,
) -> Result<ObjectId> {
    let rev_oid = resolve_revision_with_reader(git_dir, format, reader, rev)?;
    let tree_oid = peel_to_tree(reader, format, &rev_oid)?;
    resolve_tree_path(reader, format, &tree_oid, path)
        .ok_or_else(|| GitError::NotFound(format!("path '{path}' does not exist in '{rev}'")))
}

/// Walk `path` within the tree `tree_oid`, returning the id of the entry it
/// names, or `None` if any component is missing or a component before the last
/// is not a tree. An empty `path` returns `tree_oid` unchanged.
fn resolve_tree_path<R: ObjectReader>(
    reader: &R,
    format: git_core::ObjectFormat,
    tree_oid: &ObjectId,
    path: &str,
) -> Option<ObjectId> {
    let mut current = tree_oid.clone();
    // Split on '/', skipping empty components so leading/trailing/duplicate
    // separators ("a//b", "/a", "dir/") behave the way git's pathspec does.
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if components.is_empty() {
        return Some(current);
    }
    let last = components.len() - 1;
    for (idx, component) in components.iter().enumerate() {
        let object = reader.read_object(&current).ok()?;
        if object.object_type != ObjectType::Tree {
            // Cannot descend through a blob (or anything non-tree).
            return None;
        }
        let tree = Tree::parse(format, &object.body).ok()?;
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.name == component.as_bytes())?;
        if idx == last {
            return Some(entry.oid.clone());
        }
        // Intermediate component must itself be a tree to keep descending.
        if git_formats::tree_entry_object_type(entry.mode) != ObjectType::Tree {
            return None;
        }
        current = entry.oid.clone();
    }
    Some(current)
}

/// Split `<rev>:<path>` into its revision and path halves.
///
/// Returns `None` when the spec is not a rev/path form, i.e. when there is no
/// colon, when the colon is part of a leading `:` index spec (handled
/// elsewhere), or when the left side is empty. The split uses the first colon
/// so paths may themselves contain colons.
fn split_rev_path(rev: &str) -> Option<(&str, &str)> {
    let colon = rev.find(':')?;
    if colon == 0 {
        return None;
    }
    Some((&rev[..colon], &rev[colon + 1..]))
}

// ---------------------------------------------------------------------------
// `:[N:]<path>` index-stage resolution
// ---------------------------------------------------------------------------

/// Parse the portion after a leading `:` into `(stage, path)`.
///
/// `:<path>` selects stage 0; `:N:<path>` (N in 0..=3) selects stage N. When
/// the leading token is not a single 0-3 digit followed by a colon the whole
/// string is treated as a stage-0 path.
fn parse_index_stage_path(rest: &str) -> (u8, &str) {
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && matches!(bytes[0], b'0'..=b'3') {
        return (bytes[0] - b'0', &rest[2..]);
    }
    (0, rest)
}

/// Resolve `path` at `stage` in the on-disk index, returning the recorded blob
/// id. Reports git-style errors for a missing index, a path absent from the
/// index, and a path present only at other stages.
fn resolve_index_path(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    stage: u8,
    path: &str,
) -> Result<ObjectId> {
    let index_path = repository_index_path(git_dir);
    let bytes = match fs::read(&index_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(GitError::NotFound(format!(
                "path '{path}' is not in the index"
            )));
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let index = Index::parse(&bytes, format)?;
    let mut path_exists = false;
    for entry in &index.entries {
        if entry.path != path.as_bytes() {
            continue;
        }
        path_exists = true;
        if index_entry_stage(entry) == stage {
            return Ok(entry.oid.clone());
        }
    }
    if path_exists {
        Err(GitError::NotFound(format!(
            "path '{path}' is in the index, but not at stage {stage}"
        )))
    } else {
        Err(GitError::NotFound(format!(
            "path '{path}' is not in the index"
        )))
    }
}

/// Extract the merge stage (0-3) from an index entry's flags (bits 12-13).
fn index_entry_stage(entry: &git_formats::IndexEntry) -> u8 {
    ((entry.flags >> 12) & 0x3) as u8
}

/// Locate the index file, honoring `GIT_INDEX_FILE` like the rest of git.
fn repository_index_path(git_dir: &Path) -> PathBuf {
    std::env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.join("index"))
}

// ---------------------------------------------------------------------------
// Commit-message search (`:/text` and `<rev>^{/text}`)
// ---------------------------------------------------------------------------
//
// Matching is a plain substring test against the raw commit message; this crate
// has no regex dependency, so `:/text` and `^{/text}` find commits whose
// message *contains* `text` rather than matching it as a POSIX regular
// expression. An empty pattern matches the most recent candidate, mirroring
// git's "return the youngest commit" behavior for `:/`.

/// `:/text` — newest commit (across all refs) whose message contains `text`.
///
/// "Newest" is approximated by committer timestamp, falling back to the order
/// commits are discovered when timestamps are unavailable, which matches git's
/// observable behavior for the common case.
fn search_commit_message_all<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    text: &str,
) -> Result<ObjectId> {
    let starts = all_ref_commit_starts(git_dir, format, reader)?;
    let mut best: Option<(i64, ObjectId)> = None;
    for record in walk_commits(reader, format, starts)? {
        if !commit_message_contains(&record.commit, text) {
            continue;
        }
        let when = commit_committer_time(&record.commit).unwrap_or(i64::MIN);
        if best
            .as_ref()
            .is_none_or(|(best_when, _)| when >= *best_when)
        {
            best = Some((when, record.oid));
        }
    }
    best.map(|(_, oid)| oid)
        .ok_or_else(|| GitError::NotFound(format!("no commit matching ':/{text}'")))
}

/// `<rev>^{/text}` — first commit reachable from `base` along the first-parent
/// chain whose message contains `text`.
fn search_commit_message_first_parent<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: git_core::ObjectFormat,
    base: &ObjectId,
    text: &str,
) -> Result<ObjectId> {
    let start = peel_to_commit(reader, format, base)?;
    let mut current = Some(start);
    let mut seen = HashSet::new();
    while let Some(oid) = current {
        if !seen.insert(oid.clone()) {
            break;
        }
        let object = reader.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        if commit_message_contains(&commit, text) {
            return Ok(oid);
        }
        current = commit_parents_with_graph(git_dir, reader, format, &oid)?
            .into_iter()
            .next();
    }
    Err(GitError::NotFound(format!(
        "no commit matching '^{{/{text}}}' in first-parent history"
    )))
}

fn commit_message_contains(commit: &Commit, text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    // Search the raw bytes so non-UTF-8 messages still match where possible.
    commit
        .message
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

/// Best-effort committer timestamp (seconds since epoch) from a commit's
/// committer line, used only to order `:/text` candidates.
fn commit_committer_time(commit: &Commit) -> Option<i64> {
    let line = std::str::from_utf8(&commit.committer).ok()?;
    // Format: "Name <email> <seconds> <tz>"; the timestamp is the
    // second-to-last whitespace-separated field.
    let mut fields = line.rsplit(' ');
    let _tz = fields.next()?;
    fields.next()?.parse::<i64>().ok()
}

/// Collect commit starting points from every ref (peeling tags to commits) for
/// a repository-wide `:/text` search.
fn all_ref_commit_starts<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
) -> Result<Vec<ObjectId>> {
    let refs = FileRefStore::new(git_dir.to_path_buf(), format);
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for reference in refs.list_refs()? {
        let oid = match reference.target {
            RefTarget::Direct(oid) => oid,
            RefTarget::Symbolic(_) => continue,
        };
        // Skip refs whose objects (or tag targets) are not present/commit-ish.
        let Ok(commit) = peel_to_commit(reader, format, &oid) else {
            continue;
        };
        if seen.insert(commit.clone()) {
            starts.push(commit);
        }
    }
    Ok(starts)
}

// ---------------------------------------------------------------------------
// Revision ranges (`A..B` and `A...B`)
// ---------------------------------------------------------------------------

/// A parsed revision range expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionRange {
    /// `A..B` — commits reachable from `B` but not from `A`.
    Asymmetric { start: String, end: String },
    /// `A...B` — commits reachable from exactly one of `A`/`B` (symmetric
    /// difference).
    Symmetric { left: String, right: String },
}

/// Parse `A..B` / `A...B` range syntax.
///
/// Returns `None` when `spec` is not a range. An omitted side defaults to
/// `HEAD` (so `..B`, `A..`, `...B`, etc. behave like git). `...` is checked
/// before `..` so the symmetric form is not misread as an asymmetric one. A
/// trailing `..`/`...` with the wrong number of dots (more than two/three) is
/// rejected as a malformed range.
pub fn parse_revision_range(spec: &str) -> Option<RevisionRange> {
    if let Some((left, right)) = spec.split_once("...") {
        if left.contains("..") || right.contains("..") {
            return None;
        }
        return Some(RevisionRange::Symmetric {
            left: default_range_side(left).to_string(),
            right: default_range_side(right).to_string(),
        });
    }
    if let Some((left, right)) = spec.split_once("..") {
        if left.contains("..") || right.contains("..") {
            return None;
        }
        return Some(RevisionRange::Asymmetric {
            start: default_range_side(left).to_string(),
            end: default_range_side(right).to_string(),
        });
    }
    None
}

fn default_range_side(side: &str) -> &str {
    if side.is_empty() {
        "HEAD"
    } else {
        side
    }
}

/// Resolve a parsed range to the set of commit oids it selects.
///
/// `A..B` yields commits reachable from `B` but not `A`; `A...B` yields the
/// symmetric difference (reachable from `A` or `B` but not both). Endpoints are
/// resolved as revisions and peeled to commits before traversal. The returned
/// vector is unordered.
pub fn resolve_revision_range<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    range: &RevisionRange,
) -> Result<Vec<ObjectId>> {
    match range {
        RevisionRange::Asymmetric { start, end } => {
            let start_oid = resolve_range_endpoint(git_dir, format, reader, start)?;
            let end_oid = resolve_range_endpoint(git_dir, format, reader, end)?;
            let excluded = ancestor_set(git_dir, reader, format, &start_oid)?;
            let included = ancestor_set(git_dir, reader, format, &end_oid)?;
            Ok(included
                .into_iter()
                .filter(|oid| !excluded.contains(oid))
                .collect())
        }
        RevisionRange::Symmetric { left, right } => {
            let left_oid = resolve_range_endpoint(git_dir, format, reader, left)?;
            let right_oid = resolve_range_endpoint(git_dir, format, reader, right)?;
            let left_set = ancestor_set(git_dir, reader, format, &left_oid)?;
            let right_set = ancestor_set(git_dir, reader, format, &right_oid)?;
            let mut out = Vec::new();
            for oid in &left_set {
                if !right_set.contains(oid) {
                    out.push(oid.clone());
                }
            }
            for oid in &right_set {
                if !left_set.contains(oid) {
                    out.push(oid.clone());
                }
            }
            Ok(out)
        }
    }
}

fn resolve_range_endpoint<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    rev: &str,
) -> Result<ObjectId> {
    let oid = resolve_revision_with_reader(git_dir, format, reader, rev)?;
    peel_to_commit(reader, format, &oid)
}

/// Compute the set of commits reachable from `start` (inclusive) following all
/// parent edges. Uses the commit-graph for parent lookups when available.
fn ancestor_set<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: git_core::ObjectFormat,
    start: &ObjectId,
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([start.clone()]);
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        for parent in commit_parents_with_graph(git_dir, reader, format, &oid)? {
            pending.push_back(parent);
        }
    }
    Ok(seen)
}

/// Determine whether `ancestor` is reachable from `descendant` via parent
/// edges (an ancestor check). A commit is considered its own ancestor.
pub fn is_ancestor<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    ancestor: &ObjectId,
    descendant: &ObjectId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([descendant.clone()]);
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        for parent in commit_parents_with_graph(git_dir, reader, format, &oid)? {
            if &parent == ancestor {
                return Ok(true);
            }
            pending.push_back(parent);
        }
    }
    Ok(false)
}

/// Compute the merge bases (best common ancestors) of two commits, mirroring
/// the generation-free history walk used elsewhere in the project. Self-contained
/// so callers do not need the CLI's merge-base machinery.
pub fn merge_bases<R: ObjectReader>(
    git_dir: &Path,
    format: git_core::ObjectFormat,
    reader: &R,
    left: &ObjectId,
    right: &ObjectId,
) -> Result<Vec<ObjectId>> {
    let left_depths = ancestor_depths(git_dir, reader, format, left)?;
    let right_depths = ancestor_depths(git_dir, reader, format, right)?;
    let candidates: Vec<ObjectId> = left_depths
        .keys()
        .filter(|oid| right_depths.contains_key(*oid))
        .cloned()
        .collect();
    // Keep only the lowest common ancestors: drop any candidate that has another
    // candidate strictly closer to *both* endpoints (i.e. a descendant common
    // ancestor).
    let mut bases: Vec<ObjectId> = candidates
        .iter()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                other != *candidate
                    && depth_lt(&left_depths, other, candidate)
                    && depth_lt(&right_depths, other, candidate)
            })
        })
        .cloned()
        .collect();
    bases.sort_by_key(|oid| oid.to_hex());
    Ok(bases)
}

fn depth_lt(depths: &HashMap<ObjectId, usize>, a: &ObjectId, b: &ObjectId) -> bool {
    match (depths.get(a), depths.get(b)) {
        (Some(a_depth), Some(b_depth)) => a_depth < b_depth,
        _ => false,
    }
}

/// BFS the ancestry of `start`, recording the shortest distance to each commit.
fn ancestor_depths<R: ObjectReader>(
    git_dir: &Path,
    reader: &R,
    format: git_core::ObjectFormat,
    start: &ObjectId,
) -> Result<HashMap<ObjectId, usize>> {
    let mut depths = HashMap::new();
    let mut pending = VecDeque::from([(start.clone(), 0usize)]);
    while let Some((oid, depth)) = pending.pop_front() {
        if depths.get(&oid).is_some_and(|existing| *existing <= depth) {
            continue;
        }
        depths.insert(oid.clone(), depth);
        for parent in commit_parents_with_graph(git_dir, reader, format, &oid)? {
            pending.push_back((parent, depth + 1));
        }
    }
    Ok(depths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_core::ObjectFormat;
    use git_formats::EncodedObject;
    use git_odb::{ObjectDatabase, ObjectWriter};
    use git_refs::{RefTarget, RefUpdate};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn resolve_revision_reads_symbolic_head_and_tags() {
        let git_dir = temp_git_dir();
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(oid.clone()),
            reflog: None,
        });
        tx.commit().unwrap();
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "HEAD").unwrap(),
            oid
        );
        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, "v1.0").unwrap(),
            oid
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_supports_parent_suffixes() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let base = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let first_parent = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"main\n");
        let second_parent = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"side\n");
        let merge = write_test_commit(
            &mut db,
            tree,
            vec![first_parent.clone(), second_parent.clone()],
            b"merge\n",
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^"))
                .unwrap(),
            first_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}^2"))
                .unwrap(),
            second_parent
        );
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{merge}~2"))
                .unwrap(),
            base
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_supports_abbreviated_loose_object_ids() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"abbrev\n".to_vec()))
            .unwrap();

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &oid.to_hex()[..8]).unwrap(),
            oid
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_prefers_ref_over_abbreviated_object_id() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"abbrev conflict\n".to_vec(),
            ))
            .unwrap();
        let target = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: format!("refs/heads/{}", &object.to_hex()[..4]),
            expected: None,
            new: RefTarget::Direct(target.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        assert_eq!(
            resolve_revision(&git_dir, ObjectFormat::Sha1, &object.to_hex()[..4]).unwrap(),
            target
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_revision_uses_commit_graph_for_parent_suffixes() {
        let git_dir = temp_git_dir();
        fs::create_dir_all(git_dir.join("objects").join("info")).unwrap();
        let parent = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let child = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        fs::write(git_dir.join("HEAD"), format!("{child}\n")).unwrap();
        fs::write(
            git_dir.join("objects").join("info").join("commit-graph"),
            test_commit_graph(ObjectFormat::Sha1, &parent, &child),
        )
        .unwrap();

        struct MissingReader;
        impl ObjectReader for MissingReader {
            fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
                Err(GitError::NotFound(format!(
                    "object reader should not be used for {oid}"
                )))
            }
        }

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &MissingReader, "HEAD^",)
                .unwrap(),
            parent
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn peel_to_tree_handles_commits_and_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            peel_to_tree(&db, ObjectFormat::Sha1, &commit).unwrap(),
            tree
        );
        assert_eq!(peel_to_tree(&db, ObjectFormat::Sha1, &tag).unwrap(), tree);
    }

    #[test]
    fn peel_to_commit_handles_annotated_tags() {
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree, Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            peel_to_commit(&db, ObjectFormat::Sha1, &tag).unwrap(),
            commit
        );
    }

    #[test]
    fn resolve_revision_supports_peel_suffixes() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        db.write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, &format!("{tag}^{{}}"))
                .unwrap(),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{commit}}")
            )
            .unwrap(),
            commit
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tree}}")
            )
            .unwrap(),
            tree
        );
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{tag}^{{tag}}")
            )
            .unwrap(),
            tag
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn pack_refs_with_auto_peel_writes_peeled_tag() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            committer: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            encoding: None,
            message: b"base\n".to_vec(),
        };
        let commit = db
            .write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap();
        let tag = Tag {
            object: commit.clone(),
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        let tag = db
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .unwrap();
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/tags/v1.0".into(),
            expected: None,
            new: RefTarget::Direct(tag.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        let packed = pack_refs_with_auto_peel(&git_dir, ObjectFormat::Sha1, true).unwrap();
        let packed_tag = packed
            .iter()
            .find(|packed| packed.reference.name == "refs/tags/v1.0")
            .unwrap();
        assert_eq!(packed_tag.peeled, Some(commit.clone()));
        assert_eq!(
            refs.read_ref("refs/tags/v1.0").unwrap(),
            Some(RefTarget::Direct(tag))
        );
        assert!(!git_dir.join("refs").join("tags").join("v1.0").exists());
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_rev_path_finds_nested_blob_and_subtree() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .unwrap();
        let sub = write_tree(&mut db, &[(0o100644, b"file.txt", &blob)]);
        let dir = write_tree(&mut db, &[(0o040000, b"sub", &sub)]);
        let root = write_tree(&mut db, &[(0o040000, b"dir", &dir)]);
        let commit = write_test_commit(&mut db, root.clone(), Vec::new(), b"init\n");

        // Nested blob via `<rev>:<path>`.
        assert_eq!(
            resolve_rev_path(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &commit.to_hex(),
                "dir/sub/file.txt"
            )
            .unwrap(),
            blob
        );
        // Subtree path resolves to the subtree id.
        assert_eq!(
            resolve_rev_path(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &commit.to_hex(),
                "dir/sub"
            )
            .unwrap(),
            sub
        );
        // Empty path resolves to the commit's tree.
        assert_eq!(
            resolve_rev_path(&git_dir, ObjectFormat::Sha1, &db, &commit.to_hex(), "").unwrap(),
            root
        );
        // Resolvable through the unified string resolver too.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{commit}:dir/sub/file.txt"),
            )
            .unwrap(),
            blob
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_rev_path_reports_missing_and_non_tree_paths() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let blob = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"root\n".to_vec()))
            .unwrap();
        let root = write_tree(&mut db, &[(0o100644, b"root.txt", &blob)]);
        let commit = write_test_commit(&mut db, root, Vec::new(), b"init\n");

        // Missing path.
        let missing = resolve_rev_path(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &commit.to_hex(),
            "nope.txt",
        )
        .unwrap_err();
        assert!(
            matches!(&missing, GitError::NotFound(msg) if msg.contains("does not exist")),
            "unexpected error: {missing:?}"
        );

        // Descending through a blob is "not a tree" -> reported as not found.
        let not_tree = resolve_rev_path(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &commit.to_hex(),
            "root.txt/x",
        )
        .unwrap_err();
        assert!(
            matches!(&not_tree, GitError::NotFound(msg) if msg.contains("does not exist")),
            "unexpected error: {not_tree:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn resolve_index_path_reads_stage_entries() {
        let git_dir = temp_git_dir();
        let oid_zero = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let oid_two = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        let index = Index {
            version: 2,
            entries: vec![
                test_index_entry(b"file.txt", &oid_zero, 0),
                test_index_entry(b"conflict.txt", &oid_two, 2),
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            git_dir.join("index"),
            index.write(ObjectFormat::Sha1).unwrap(),
        )
        .unwrap();

        // `:path` defaults to stage 0.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &ObjectDatabase::new(ObjectFormat::Sha1),
                ":file.txt",
            )
            .unwrap(),
            oid_zero
        );
        // `:N:path` selects a specific stage.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &ObjectDatabase::new(ObjectFormat::Sha1),
                ":2:conflict.txt",
            )
            .unwrap(),
            oid_two
        );
        // Wrong stage reports a stage-specific error.
        let wrong_stage = resolve_revision_with_reader(
            &git_dir,
            ObjectFormat::Sha1,
            &ObjectDatabase::new(ObjectFormat::Sha1),
            ":1:conflict.txt",
        )
        .unwrap_err();
        assert!(
            matches!(&wrong_stage, GitError::NotFound(msg) if msg.contains("not at stage 1")),
            "unexpected error: {wrong_stage:?}"
        );
        // Unknown path reports "not in the index".
        let unknown = resolve_revision_with_reader(
            &git_dir,
            ObjectFormat::Sha1,
            &ObjectDatabase::new(ObjectFormat::Sha1),
            ":missing.txt",
        )
        .unwrap_err();
        assert!(
            matches!(&unknown, GitError::NotFound(msg) if msg.contains("not in the index")),
            "unexpected error: {unknown:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn search_commit_message_all_finds_matching_commit() {
        let git_dir = temp_git_dir();
        let mut db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = db
            .write_object(EncodedObject::new(ObjectType::Tree, Vec::new()))
            .unwrap();
        let first = write_dated_commit(&mut db, tree.clone(), Vec::new(), b"add feature\n", 1000);
        let second = write_dated_commit(
            &mut db,
            tree.clone(),
            vec![first.clone()],
            b"fix the widget bug\n",
            2000,
        );
        let third = write_dated_commit(
            &mut db,
            tree,
            vec![second.clone()],
            b"unrelated change\n",
            3000,
        );
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(third.clone()),
            reflog: None,
        });
        tx.commit().unwrap();

        assert_eq!(
            resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, ":/widget bug")
                .unwrap(),
            second
        );
        // `^{/regex}` over first-parent history finds the same commit from the tip.
        assert_eq!(
            resolve_revision_with_reader(
                &git_dir,
                ObjectFormat::Sha1,
                &db,
                &format!("{third}^{{/widget bug}}"),
            )
            .unwrap(),
            second
        );
        // No match is an error.
        let miss = resolve_revision_with_reader(&git_dir, ObjectFormat::Sha1, &db, ":/zzznomatch")
            .unwrap_err();
        assert!(
            matches!(miss, GitError::NotFound(_)),
            "unexpected: {miss:?}"
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn parse_revision_range_recognizes_dot_forms() {
        assert_eq!(
            parse_revision_range("a..b"),
            Some(RevisionRange::Asymmetric {
                start: "a".into(),
                end: "b".into(),
            })
        );
        assert_eq!(
            parse_revision_range("a...b"),
            Some(RevisionRange::Symmetric {
                left: "a".into(),
                right: "b".into(),
            })
        );
        assert_eq!(
            parse_revision_range("..b"),
            Some(RevisionRange::Asymmetric {
                start: "HEAD".into(),
                end: "b".into(),
            })
        );
        assert_eq!(
            parse_revision_range("a.."),
            Some(RevisionRange::Asymmetric {
                start: "a".into(),
                end: "HEAD".into(),
            })
        );
        assert_eq!(parse_revision_range("plain"), None);
    }

    #[test]
    fn resolve_revision_range_excludes_ancestors_and_symmetric_difference() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        // base -> a -> b   (left line)
        //   \--> c -> d    (right line)
        let base = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let a = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"a\n");
        let b = write_test_commit(&mut db, tree.clone(), vec![a.clone()], b"b\n");
        let c = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"c\n");
        let d = write_test_commit(&mut db, tree, vec![c.clone()], b"d\n");

        // A..B: reachable from B (a..b line) but not from A (base only here) ->
        // {a, b}; base and earlier are excluded.
        let range = RevisionRange::Asymmetric {
            start: a.to_hex(),
            end: b.to_hex(),
        };
        let mut got = resolve_revision_range(&git_dir, ObjectFormat::Sha1, &db, &range).unwrap();
        got.sort_by(|x, y| x.to_hex().cmp(&y.to_hex()));
        assert_eq!(got, vec![b.clone()]);
        assert!(!got.contains(&a), "A itself is excluded");
        assert!(!got.contains(&base), "A's ancestors are excluded");

        // b...d: symmetric difference excludes the shared `base` while keeping
        // both unique sides {a, b} and {c, d}.
        let sym = RevisionRange::Symmetric {
            left: b.to_hex(),
            right: d.to_hex(),
        };
        let got_sym: HashSet<ObjectId> =
            resolve_revision_range(&git_dir, ObjectFormat::Sha1, &db, &sym)
                .unwrap()
                .into_iter()
                .collect();
        let expected: HashSet<ObjectId> = [a, b, c, d].into_iter().collect();
        assert_eq!(got_sym, expected);
        assert!(!got_sym.contains(&base), "shared base excluded from ...");
        fs::remove_dir_all(git_dir).unwrap();
    }

    #[test]
    fn merge_bases_finds_common_ancestor() {
        let git_dir = temp_git_dir();
        let mut db = ObjectDatabase::new(ObjectFormat::Sha1);
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let base = write_test_commit(&mut db, tree.clone(), Vec::new(), b"base\n");
        let left = write_test_commit(&mut db, tree.clone(), vec![base.clone()], b"left\n");
        let right = write_test_commit(&mut db, tree, vec![base.clone()], b"right\n");
        assert_eq!(
            merge_bases(&git_dir, ObjectFormat::Sha1, &db, &left, &right).unwrap(),
            vec![base]
        );
        fs::remove_dir_all(git_dir).unwrap();
    }

    fn write_test_commit(
        db: &mut ObjectDatabase,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &[u8],
    ) -> ObjectId {
        let commit = Commit {
            tree,
            parents,
            author: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            committer: b"Example User <example@example.invalid> 0 +0000".to_vec(),
            encoding: None,
            message: message.to_vec(),
        };
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap()
    }

    fn write_dated_commit<W: ObjectWriter>(
        db: &mut W,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &[u8],
        when: i64,
    ) -> ObjectId {
        let ident = format!("Example User <example@example.invalid> {when} +0000");
        let commit = Commit {
            tree,
            parents,
            author: ident.clone().into_bytes(),
            committer: ident.into_bytes(),
            encoding: None,
            message: message.to_vec(),
        };
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .unwrap()
    }

    fn write_tree(db: &mut ObjectDatabase, entries: &[(u32, &[u8], &ObjectId)]) -> ObjectId {
        let tree = Tree {
            entries: entries
                .iter()
                .map(|(mode, name, oid)| git_formats::TreeEntry {
                    mode: *mode,
                    name: name.to_vec(),
                    oid: (*oid).clone(),
                })
                .collect(),
        };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .unwrap()
    }

    fn test_index_entry(path: &[u8], oid: &ObjectId, stage: u16) -> git_formats::IndexEntry {
        git_formats::IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid: oid.clone(),
            flags: (stage & 0x3) << 12,
            flags_extended: 0,
            path: path.to_vec(),
        }
    }

    fn temp_git_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "git-rs-rev-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_commit_graph(format: ObjectFormat, parent: &ObjectId, child: &ObjectId) -> Vec<u8> {
        let tree = ObjectId::from_hex(format, "4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
        let mut oidf = vec![0u8; 256 * 4];
        let parent_first = parent.as_bytes()[0] as usize;
        let child_first = child.as_bytes()[0] as usize;
        for idx in 0..256 {
            let count = u32::from(idx >= parent_first) + u32::from(idx >= child_first);
            oidf[idx * 4..idx * 4 + 4].copy_from_slice(&count.to_be_bytes());
        }
        let mut oidl = Vec::new();
        oidl.extend_from_slice(parent.as_bytes());
        oidl.extend_from_slice(child.as_bytes());
        let mut cdat = Vec::new();
        cdat.extend_from_slice(&commit_graph_cdat_entry(
            &tree,
            0x7000_0000,
            0x7000_0000,
            1,
            1,
        ));
        cdat.extend_from_slice(&commit_graph_cdat_entry(&tree, 0, 0x7000_0000, 2, 2));
        commit_graph_file(
            format,
            &[(*b"OIDF", oidf), (*b"OIDL", oidl), (*b"CDAT", cdat)],
        )
    }

    fn commit_graph_cdat_entry(
        tree: &ObjectId,
        parent_one: u32,
        parent_two: u32,
        generation: u32,
        commit_time: u64,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(tree.as_bytes());
        out.extend_from_slice(&parent_one.to_be_bytes());
        out.extend_from_slice(&parent_two.to_be_bytes());
        let high = (generation << 2) | ((commit_time >> 32) as u32 & 0x3);
        out.extend_from_slice(&high.to_be_bytes());
        out.extend_from_slice(&(commit_time as u32).to_be_bytes());
        out
    }

    fn commit_graph_file(format: ObjectFormat, chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let lookup_len = (chunks.len() + 1) * 12;
        let mut out = Vec::new();
        out.extend_from_slice(b"CGPH");
        out.push(1);
        out.push(match format {
            ObjectFormat::Sha1 => 1,
            ObjectFormat::Sha256 => 2,
        });
        out.push(chunks.len() as u8);
        out.push(0);
        let mut offset = (8 + lookup_len) as u64;
        for (id, data) in chunks {
            out.extend_from_slice(id);
            out.extend_from_slice(&offset.to_be_bytes());
            offset += data.len() as u64;
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&offset.to_be_bytes());
        for (_id, data) in chunks {
            out.extend_from_slice(data);
        }
        let checksum = git_core::digest_bytes(format, &out).unwrap();
        out.extend_from_slice(checksum.as_bytes());
        out
    }
}
