use git_core::{GitError, ObjectFormat, ObjectId, RepoPath, Result};
use git_formats::{Commit, EncodedObject, Index, ObjectType, Tree};
use git_odb::{FileObjectDatabase, ObjectReader};
use git_refs::{FileRefStore, RefTarget};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAlgorithm {
    Myers,
    Minimal,
    Patience,
    Histogram,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Add { path: RepoPath },
    Delete { path: RepoPath },
    Modify { path: RepoPath },
    Rename { old: RepoPath, new: RepoPath },
    Copy { source: RepoPath, dest: RepoPath },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: RepoPath,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameStatus {
    Added,
    Deleted,
    Modified,
    Renamed(u8),
    Copied(u8),
}

impl NameStatus {
    pub const fn code(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Renamed(_) => 'R',
            Self::Copied(_) => 'C',
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Renamed(score) => format!("R{score}"),
            Self::Copied(score) => format!("C{score}"),
            _ => self.code().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusEntry {
    pub status: NameStatus,
    pub path: Vec<u8>,
    pub old_path: Option<Vec<u8>>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub old_oid: Option<ObjectId>,
    pub new_oid: Option<ObjectId>,
}

impl NameStatusEntry {
    pub fn line(&self) -> String {
        if let Some(old_path) = &self.old_path {
            format!(
                "{}\t{}\t{}",
                self.status.label(),
                String::from_utf8_lossy(old_path),
                String::from_utf8_lossy(&self.path)
            )
        } else {
            format!(
                "{}\t{}",
                self.status.label(),
                String::from_utf8_lossy(&self.path)
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffNameStatusOptions {
    pub detect_renames: bool,
    pub detect_copies: bool,
    pub find_copies_harder: bool,
}

impl Default for DiffNameStatusOptions {
    fn default() -> Self {
        Self {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
        }
    }
}

pub fn diff_name_status_head_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_head_worktree_with_options(
        worktree_root,
        git_dir,
        format,
        DiffNameStatusOptions::default(),
    )
}

pub fn diff_name_status_head_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if format != ObjectFormat::Sha1 {
        return Err(GitError::Unsupported(
            "diff --name-status currently reads sha1 repositories".into(),
        ));
    }
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let index = read_index_entries(git_dir)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    let changes =
        diff_name_status_maps(&head, &worktree, head.keys().chain(index.keys()), options)?;
    Ok(mark_unstaged_worktree_oids_unresolved(
        changes, &index, &worktree,
    ))
}

pub fn diff_name_status_head_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_head_index_with_options(git_dir, format, DiffNameStatusOptions::default())
}

pub fn diff_name_status_head_index_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if format != ObjectFormat::Sha1 {
        return Err(GitError::Unsupported(
            "diff --cached currently reads sha1 repositories".into(),
        ));
    }
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let index = read_index_entries(git_dir)?;
    diff_name_status_maps(&head, &index, head.keys().chain(index.keys()), options)
}

pub fn diff_name_status_index_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_index_worktree_with_options(
        worktree_root,
        git_dir,
        format,
        DiffNameStatusOptions::default(),
    )
}

pub fn diff_name_status_index_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if format != ObjectFormat::Sha1 {
        return Err(GitError::Unsupported(
            "diff currently reads sha1 repositories".into(),
        ));
    }
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index = read_index_entries(git_dir)?;
    let worktree = worktree_entries(worktree_root, git_dir, format)?;
    diff_name_status_maps(&index, &worktree, index.keys(), options)
}

fn diff_name_status_maps<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let mut paths = BTreeSet::new();
    paths.extend(candidate_paths.cloned());

    let mut changes = Vec::new();
    for path in paths {
        let left = left_entries.get(&path);
        let right = right_entries.get(&path);
        let status = match (left, right) {
            (None, Some(_)) => Some(NameStatus::Added),
            (Some(_), None) => Some(NameStatus::Deleted),
            (Some(left), Some(right)) if left != right => Some(NameStatus::Modified),
            _ => None,
        };
        if let Some(status) = status {
            changes.push(NameStatusEntry {
                status,
                path,
                old_path: None,
                old_mode: left.map(|entry| entry.mode),
                new_mode: right.map(|entry| entry.mode),
                old_oid: left.map(|entry| entry.oid.clone()),
                new_oid: right.map(|entry| entry.oid.clone()),
            });
        }
    }
    if options.detect_renames {
        changes = detect_exact_renames(changes, left_entries, right_entries);
    }
    if options.detect_copies {
        changes = detect_exact_copies(
            changes,
            left_entries,
            right_entries,
            options.find_copies_harder,
        );
    }
    Ok(changes)
}

fn detect_exact_renames(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
) -> Vec<NameStatusEntry> {
    let added = changes
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.status == NameStatus::Added)
        .map(|(idx, entry)| (idx, entry.path.clone()))
        .collect::<Vec<_>>();
    let deleted = changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Deleted)
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let mut consumed = BTreeSet::new();
    let mut renamed_old_paths = BTreeSet::new();
    let mut result = Vec::new();

    for old_path in deleted {
        let Some(left) = left_entries.get(&old_path) else {
            continue;
        };
        if let Some((idx, new_path)) = added.iter().find(|(idx, new_path)| {
            !consumed.contains(idx)
                && right_entries
                    .get(new_path)
                    .is_some_and(|right| right.oid == left.oid)
        }) {
            consumed.insert(*idx);
            renamed_old_paths.insert(old_path.clone());
            let right = right_entries.get(new_path);
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(100),
                path: new_path.clone(),
                old_path: Some(old_path),
                old_mode: Some(left.mode),
                new_mode: right.map(|entry| entry.mode),
                old_oid: Some(left.oid.clone()),
                new_oid: right.map(|entry| entry.oid.clone()),
            });
        }
    }

    for (idx, entry) in changes.into_iter().enumerate() {
        if entry.status == NameStatus::Added && consumed.contains(&idx) {
            continue;
        }
        if entry.status == NameStatus::Deleted && renamed_old_paths.contains(&entry.path) {
            continue;
        }
        result.push(entry);
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

fn detect_exact_copies(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    find_copies_harder: bool,
) -> Vec<NameStatusEntry> {
    let changed_sources = changes
        .iter()
        .filter(|entry| matches!(entry.status, NameStatus::Deleted | NameStatus::Modified))
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let source_paths = left_entries
        .keys()
        .filter(|path| find_copies_harder || changed_sources.contains(*path))
        .cloned()
        .collect::<Vec<_>>();

    let mut result = Vec::new();
    for entry in changes {
        if entry.status != NameStatus::Added {
            result.push(entry);
            continue;
        }
        let Some(right) = right_entries.get(&entry.path) else {
            result.push(entry);
            continue;
        };
        if let Some(old_path) = source_paths.iter().find(|old_path| {
            old_path.as_slice() != entry.path.as_slice()
                && left_entries
                    .get(*old_path)
                    .is_some_and(|left| left.oid == right.oid)
        }) {
            result.push(NameStatusEntry {
                status: NameStatus::Copied(100),
                path: entry.path,
                old_path: Some(old_path.clone()),
                old_mode: left_entries.get(old_path).map(|entry| entry.mode),
                new_mode: entry.new_mode,
                old_oid: left_entries.get(old_path).map(|entry| entry.oid.clone()),
                new_oid: entry.new_oid,
            });
        } else {
            result.push(entry);
        }
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

fn diff_entry_sort_path(entry: &NameStatusEntry) -> &[u8] {
    if matches!(entry.status, NameStatus::Copied(_)) {
        &entry.path
    } else {
        entry.old_path.as_deref().unwrap_or(&entry.path)
    }
}

fn mark_unstaged_worktree_oids_unresolved(
    changes: Vec<NameStatusEntry>,
    index_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    worktree_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
) -> Vec<NameStatusEntry> {
    changes
        .into_iter()
        .map(|mut entry| {
            let worktree_entry = worktree_entries.get(&entry.path);
            if worktree_entry != index_entries.get(&entry.path) {
                entry.new_oid = None;
            }
            entry
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedEntry {
    mode: u32,
    oid: ObjectId,
}

fn read_index_entries(git_dir: &Path) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let index_path = git_dir.join("index");
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = Index::parse_v2_sha1(&fs::read(index_path)?)?;
    Ok(index
        .entries
        .into_iter()
        .map(|entry| {
            (
                entry.path,
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            )
        })
        .collect())
}

fn head_tree_entries(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let refs = FileRefStore::new(git_dir, format);
    let Some(head) = refs.read_ref("HEAD")? else {
        return Ok(BTreeMap::new());
    };
    let commit_oid = match head {
        RefTarget::Direct(oid) => Some(oid),
        RefTarget::Symbolic(name) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
    };
    let Some(commit_oid) = commit_oid else {
        return Ok(BTreeMap::new());
    };
    let object = db.read_object(&commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "HEAD {commit_oid} is not a commit"
        )));
    }
    let commit = Commit::parse(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, &commit.tree, Vec::new(), &mut entries)?;
    Ok(entries)
}

fn collect_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let tree = Tree::parse(format, &object.body)?;
    for entry in tree.entries {
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        if entry.mode == 0o040000 {
            collect_tree_entries(db, format, &entry.oid, path, entries)?;
        } else {
            entries.insert(
                path,
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            );
        }
    }
    Ok(())
}

fn worktree_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    collect_worktree_entries(worktree_root, git_dir, worktree_root, format, &mut entries)?;
    Ok(entries)
}

fn collect_worktree_entries(
    root: &Path,
    git_dir: &Path,
    dir: &Path,
    format: ObjectFormat,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    if dir == git_dir {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == git_dir {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_worktree_entries(root, git_dir, &path, format, entries)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            let git_path = git_path_bytes(relative)?;
            let body = fs::read(&path)?;
            let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
            entries.insert(
                git_path,
                TrackedEntry {
                    mode: file_mode(&metadata),
                    oid,
                },
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

fn git_path_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(GitError::InvalidPath(format!(
            "invalid diff path {}",
            path.display()
        )));
    }
    Ok(path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_formats::RepositoryLayout;
    use git_odb::ObjectWriter;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn name_status_reports_added_from_index() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false).unwrap();
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .unwrap();
        let index = Index {
            version: 2,
            entries: vec![git_formats::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                size: 6,
                oid,
                flags: "hello.txt".len() as u16,
                flags_extended: 0,
                path: b"hello.txt".to_vec(),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(layout.git_dir.join("index"), index.write_v2_sha1().unwrap()).unwrap();
        fs::write(root.join("hello.txt"), b"hello\n").unwrap();
        let changes =
            diff_name_status_head_worktree(&root, &layout.git_dir, ObjectFormat::Sha1).unwrap();
        assert_eq!(changes[0].line(), "A\thello.txt");
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "git-rs-diff-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
