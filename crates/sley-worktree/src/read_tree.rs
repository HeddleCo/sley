//! Repository-level index transitions used by `read-tree` and unpack-trees consumers.

use super::*;
use crate::attributes::SparseMatcher;

type LeafMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;

/// One path in a planned read-tree index, including its merge stage and cached
/// working-tree stat when the unpack-trees engine could preserve or refresh it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadTreeEntry {
    /// Canonical Git index mode.
    pub mode: u32,
    /// Object named by this index entry.
    pub oid: ObjectId,
    /// Merge stage, from zero through three.
    pub stage: u8,
    /// Cached worktree metadata preserved or refreshed by unpack-trees.
    pub stat: Option<sley_unpack_trees::StatInfo>,
    /// Explicit unpack-trees sparse state. `None` preserves the current index
    /// flag for legacy index-only projections; worktree transitions set it.
    pub skip_worktree: Option<bool>,
}

impl ReadTreeEntry {
    /// Construct a fresh stage-zero tree entry with no worktree stat cache.
    pub fn stage_zero(mode: u32, oid: ObjectId) -> Self {
        Self {
            mode,
            oid,
            stage: 0,
            stat: None,
            skip_worktree: None,
        }
    }
}

/// How resolved trees are projected onto the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadTreeTransitionMode {
    /// Replace the stage-zero index with the recursive overlay of the trees;
    /// later trees win across both exact and file/directory collisions.
    Overlay,
    /// Overlay the trees below a normalized prefix while retaining the current
    /// stage-zero index outside that prefix. The root sentinel is `/`.
    Prefix(Vec<u8>),
}

/// Inputs to a repository-level read-tree transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadTreeTransitionOptions<'a> {
    /// Index projection strategy.
    pub mode: ReadTreeTransitionMode,
    /// Already-resolved source tree object IDs, in overlay order.
    pub tree_oids: &'a [ObjectId],
}

impl<'a> ReadTreeTransitionOptions<'a> {
    /// Replace the stage-zero index with the recursive overlay of `tree_oids`.
    pub fn overlay(tree_oids: &'a [ObjectId]) -> Self {
        Self {
            mode: ReadTreeTransitionMode::Overlay,
            tree_oids,
        }
    }

    /// Apply `tree_oids` beneath a normalized prefix over the current index.
    pub fn prefix(prefix: Vec<u8>, tree_oids: &'a [ObjectId]) -> Self {
        Self {
            mode: ReadTreeTransitionMode::Prefix(prefix),
            tree_oids,
        }
    }
}

/// Planned index state. Porcelain may apply worktree changes before persisting
/// these entries, or discard the outcome for a dry run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadTreeTransitionOutcome {
    /// Sorted repository-relative paths and their projected index entries.
    pub entries: Vec<(Vec<u8>, ReadTreeEntry)>,
}

/// Typed read-tree transition failure. Repository semantics never select a
/// process exit code; porcelain maps validation errors to its own contract.
#[derive(Debug)]
pub enum ReadTreeTransitionError {
    /// A tree would introduce a protected or structurally invalid index path.
    InvalidPath(Vec<u8>),
    /// A prefix/bind read would overlap an existing stage-zero index path.
    BindOverlap {
        incoming: Vec<u8>,
        existing: Vec<u8>,
    },
    /// Object, index, configuration, or filesystem engine failure.
    Engine(GitError),
}

impl std::fmt::Display for ReadTreeTransitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath(path) => {
                write!(
                    formatter,
                    "invalid path '{}'",
                    String::from_utf8_lossy(path)
                )
            }
            Self::BindOverlap { incoming, existing } => write!(
                formatter,
                "entry '{}' overlaps with '{}'",
                String::from_utf8_lossy(incoming),
                String::from_utf8_lossy(existing)
            ),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ReadTreeTransitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::InvalidPath(_) | Self::BindOverlap { .. } => None,
        }
    }
}

impl From<GitError> for ReadTreeTransitionError {
    fn from(error: GitError) -> Self {
        Self::Engine(error)
    }
}

/// Result type for typed read-tree planning and validation operations.
pub type ReadTreeTransitionResult<T> = std::result::Result<T, ReadTreeTransitionError>;

/// Presentation seam for repository validation failures. The engine controls
/// the verdict and offending bytes; CLI and embedders choose how to surface it.
pub trait ReadTreeDiagnostics {
    fn invalid_path(&mut self, path: &[u8]);
}

/// Diagnostics sink for library callers that only need the typed error.
#[derive(Default)]
pub struct SilentReadTreeDiagnostics;

impl ReadTreeDiagnostics for SilentReadTreeDiagnostics {
    fn invalid_path(&mut self, _path: &[u8]) {}
}

/// Plan an overlay or prefix read from already-resolved tree object IDs.
///
/// Revision parsing remains a porcelain concern. This operation owns the Git
/// index semantics: recursive D/F replacement, protected-path validation, and
/// prefix application over the current stage-zero index.
pub fn plan_read_tree_transition(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    options: ReadTreeTransitionOptions<'_>,
    diagnostics: &mut dyn ReadTreeDiagnostics,
) -> ReadTreeTransitionResult<ReadTreeTransitionOutcome> {
    let rules = ReadTreePathRules::from_config(config);
    let mut merged = match &options.mode {
        ReadTreeTransitionMode::Overlay => LeafMap::new(),
        ReadTreeTransitionMode::Prefix(_) => read_current_index_stage_zero(git_dir, format)?,
    };
    let prefix = match &options.mode {
        ReadTreeTransitionMode::Overlay => &[][..],
        ReadTreeTransitionMode::Prefix(prefix) if prefix == b"/" => &[][..],
        ReadTreeTransitionMode::Prefix(prefix) => prefix.as_slice(),
    };

    for tree_oid in options.tree_oids {
        for (path, value) in sley_diff_merge::flatten_tree(db, format, tree_oid)? {
            let mut full = prefix.to_vec();
            full.extend_from_slice(&path);
            verify_read_tree_path(&full, value.0, rules, diagnostics)?;
            match options.mode {
                ReadTreeTransitionMode::Overlay => overlay_tree_leaf(&mut merged, full, value),
                ReadTreeTransitionMode::Prefix(_) => {
                    if let Some(existing) = bind_overlap_path(&merged, &full) {
                        return Err(ReadTreeTransitionError::BindOverlap {
                            incoming: full,
                            existing,
                        });
                    }
                    merged.insert(full, value);
                }
            }
        }
    }

    Ok(ReadTreeTransitionOutcome {
        entries: merged
            .into_iter()
            .map(|(path, (mode, oid))| (path, ReadTreeEntry::stage_zero(mode, oid)))
            .collect(),
    })
}

fn bind_overlap_path(merged: &LeafMap, incoming: &[u8]) -> Option<Vec<u8>> {
    if merged.contains_key(incoming) {
        return Some(incoming.to_vec());
    }
    for (position, byte) in incoming.iter().enumerate() {
        if *byte == b'/' && merged.contains_key(&incoming[..position]) {
            return Some(incoming[..position].to_vec());
        }
    }
    let mut descendant_prefix = incoming.to_vec();
    descendant_prefix.push(b'/');
    merged
        .range(descendant_prefix.clone()..)
        .find(|(candidate, _)| candidate.starts_with(&descendant_prefix))
        .map(|(candidate, _)| candidate.clone())
}

/// Validate every leaf path in a tree using the same protected-path rules as
/// [`plan_read_tree_transition`]. This lets unpack-trees consumers validate
/// each source without first collapsing the sources into an overlay.
pub fn validate_read_tree_source(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    tree_oid: &ObjectId,
    diagnostics: &mut dyn ReadTreeDiagnostics,
) -> ReadTreeTransitionResult<()> {
    flatten_validated_read_tree_source(db, format, config, tree_oid, diagnostics).map(|_| ())
}

/// Flatten one source tree and validate its paths in a single object walk.
/// Unpack-trees callers can consume the returned tree directly without paying
/// for a second flattening pass after validation.
pub fn flatten_validated_read_tree_source(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    tree_oid: &ObjectId,
    diagnostics: &mut dyn ReadTreeDiagnostics,
) -> ReadTreeTransitionResult<sley_unpack_trees::FlatTree> {
    let rules = ReadTreePathRules::from_config(config);
    let tree = sley_diff_merge::flatten_tree(db, format, tree_oid)?;
    for (path, (mode, _)) in &tree {
        verify_read_tree_path(path, *mode, rules, diagnostics)?;
    }
    Ok(tree)
}

fn overlay_tree_leaf(merged: &mut LeafMap, path: Vec<u8>, value: (u32, ObjectId)) {
    for (position, byte) in path.iter().enumerate() {
        if *byte == b'/' {
            merged.remove(&path[..position]);
        }
    }
    let mut descendant_prefix = path.clone();
    descendant_prefix.push(b'/');
    let descendants = merged
        .range(descendant_prefix.clone()..)
        .take_while(|(candidate, _)| candidate.starts_with(&descendant_prefix))
        .map(|(candidate, _)| candidate.clone())
        .collect::<Vec<_>>();
    for descendant in descendants {
        merged.remove(&descendant);
    }
    merged.insert(path, value);
}

#[derive(Clone, Copy, Debug)]
struct ReadTreePathRules {
    protect_hfs: bool,
    protect_ntfs: bool,
}

impl ReadTreePathRules {
    fn from_config(config: &GitConfig) -> Self {
        Self {
            protect_hfs: config.get_bool("core", None, "protectHFS").unwrap_or(false),
            protect_ntfs: config
                .get_bool("core", None, "protectNTFS")
                .unwrap_or(false),
        }
    }
}

fn verify_read_tree_path(
    path: &[u8],
    mode: u32,
    rules: ReadTreePathRules,
    diagnostics: &mut dyn ReadTreeDiagnostics,
) -> ReadTreeTransitionResult<()> {
    if path.is_empty() || path.contains(&0) {
        return invalid_read_tree_path(path, diagnostics);
    }
    for component in path.split(|&byte| byte == b'/') {
        if component.is_empty()
            || component == b"."
            || component == b".."
            || component.eq_ignore_ascii_case(b".git")
            || (rules.protect_hfs && is_hfs_dotgit(component))
            || (rules.protect_ntfs && is_ntfs_dotgit(component))
        {
            return invalid_read_tree_path(path, diagnostics);
        }
        if mode == 0o120000
            && (component.eq_ignore_ascii_case(b".gitmodules")
                || (rules.protect_hfs && is_hfs_dotgitmodules(component))
                || (rules.protect_ntfs && is_ntfs_dotgitmodules(component)))
        {
            return invalid_read_tree_path(path, diagnostics);
        }
    }
    Ok(())
}

fn invalid_read_tree_path(
    path: &[u8],
    diagnostics: &mut dyn ReadTreeDiagnostics,
) -> ReadTreeTransitionResult<()> {
    diagnostics.invalid_path(path);
    Err(ReadTreeTransitionError::InvalidPath(path.to_vec()))
}

fn is_hfs_dotgit(name: &[u8]) -> bool {
    strip_hfs_ignorable(name).eq_ignore_ascii_case(b".git")
}

fn is_hfs_dotgitmodules(name: &[u8]) -> bool {
    strip_hfs_ignorable(name).eq_ignore_ascii_case(b".gitmodules")
}

fn is_ntfs_dotgit(name: &[u8]) -> bool {
    for segment in name.split(|&byte| byte == b'\\') {
        let stream_name = segment
            .iter()
            .position(|&byte| byte == b':')
            .map_or(segment, |colon| &segment[..colon]);
        let mut end = stream_name.len();
        while end > 0 && matches!(stream_name[end - 1], b'.' | b' ') {
            end -= 1;
        }
        let trimmed = &stream_name[..end];
        if trimmed.eq_ignore_ascii_case(b".git") || trimmed.eq_ignore_ascii_case(b"git~1") {
            return true;
        }
    }
    false
}

fn is_ntfs_dotgitmodules(name: &[u8]) -> bool {
    is_ntfs_dot_name(name, b".gitmodules", b"gitmodules", b"gi7eba")
}

fn is_ntfs_dot_name(name: &[u8], long: &[u8], short_base: &[u8], fallback: &[u8]) -> bool {
    for segment in name.split(|&byte| byte == b'\\') {
        let stream_name = segment
            .iter()
            .position(|&byte| byte == b':')
            .map_or(segment, |colon| &segment[..colon]);
        let mut end = stream_name.len();
        while end > 0 && matches!(stream_name[end - 1], b'.' | b' ') {
            end -= 1;
        }
        let trimmed = &stream_name[..end];
        if trimmed.eq_ignore_ascii_case(long) {
            return true;
        }
        if short_base.len() >= 6
            && trimmed.len() == 8
            && trimmed[..6].eq_ignore_ascii_case(&short_base[..6])
            && trimmed[6] == b'~'
            && matches!(trimmed[7], b'1'..=b'4')
        {
            return true;
        }
        if trimmed.len() == 8 && trimmed[..fallback.len()].eq_ignore_ascii_case(fallback) {
            let mut saw_tilde = false;
            let mut ok = true;
            for byte in trimmed.iter().take(8).skip(fallback.len()) {
                if saw_tilde {
                    ok &= byte.is_ascii_digit();
                } else if *byte == b'~' {
                    saw_tilde = true;
                } else {
                    ok = false;
                }
            }
            if ok && saw_tilde {
                return true;
            }
        }
    }
    false
}

fn strip_hfs_ignorable(name: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(name) else {
        return name.to_vec();
    };
    text.chars()
        .filter(|ch| !is_hfs_ignorable(*ch))
        .collect::<String>()
        .into_bytes()
}

fn is_hfs_ignorable(ch: char) -> bool {
    matches!(
        ch as u32,
        0x200c | 0x200d | 0x200e | 0x200f | 0x202a..=0x202e | 0x206a..=0x206f
        | 0xfeff | 0x00ad | 0x034f | 0x115f | 0x1160 | 0x17b4 | 0x17b5 | 0x2060..=0x2064
    )
}

/// Read the current stage-zero index into path/mode/object form.
pub fn read_current_index_stage_zero(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, (u32, ObjectId)>> {
    let mut out = BTreeMap::new();
    if let Some(mut index) = read_repository_index(git_dir, format)? {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        expand_sparse_index_view(&mut index, &db, format)?;
        for entry in index.entries {
            if index_entry_stage(&entry) == 0 {
                out.insert(entry.path.into_bytes(), (entry.mode, entry.oid));
            }
        }
    }
    Ok(out)
}

/// Project the current stage-zero index into unpack-trees' flat representation.
pub fn read_current_unpack_index(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<sley_unpack_trees::FlatIndex> {
    let mut out = sley_unpack_trees::FlatIndex::new();
    if let Some(index) = read_repository_index(git_dir, format)? {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        for entry in &index.entries {
            if index_entry_stage(entry) != 0 {
                continue;
            }
            if entry.is_sparse_dir() {
                let prefix = entry.path.as_bytes().to_vec();
                out.mark_sparse_directory(prefix.clone());
                for (relative, (mode, oid)) in
                    sley_diff_merge::flatten_tree(&db, format, &entry.oid)?
                {
                    let mut path = prefix.clone();
                    path.extend_from_slice(&relative);
                    out.insert(path, (mode, oid, None));
                }
                continue;
            }
            let path = entry.path.as_bytes().to_vec();
            out.insert(
                path.clone(),
                (
                    entry.mode,
                    entry.oid,
                    Some(stat_info_from_index_entry(entry)),
                ),
            );
            if entry.is_skip_worktree() {
                out.mark_skip_worktree(path);
            }
        }
    }
    Ok(out)
}

/// Apply the repository's active sparse-checkout patterns to every logical
/// path participating in an unpack-trees transition.
///
/// `paths` must be the union of the current logical index and all source-tree
/// leaves so newly introduced out-of-cone entries receive an explicit decision.
/// Returns whether sparse policy is active and should be enabled in the engine.
pub fn configure_active_sparse_checkout_for_unpack_index(
    git_dir: &Path,
    index: &mut sley_unpack_trees::FlatIndex,
    paths: &BTreeSet<Vec<u8>>,
) -> Result<bool> {
    let Some((sparse, mode)) = active_sparse_checkout(git_dir)? else {
        return Ok(false);
    };
    let matcher = SparseMatcher::new(&sparse, mode);
    for path in paths {
        index.set_sparse_checkout_skip(path.clone(), !matcher.includes_file(path));
    }
    Ok(true)
}

/// Snapshot all current index paths, across every stage.
pub fn read_current_index_paths(git_dir: &Path, format: ObjectFormat) -> Result<BTreeSet<Vec<u8>>> {
    let Some(index) = read_repository_index(git_dir, format)? else {
        return Ok(BTreeSet::new());
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut paths = BTreeSet::new();
    for entry in index.entries {
        if entry.is_sparse_dir() {
            let prefix = entry.path.into_bytes();
            for (relative, _) in sley_diff_merge::flatten_tree(&db, format, &entry.oid)? {
                let mut path = prefix.clone();
                path.extend_from_slice(&relative);
                paths.insert(path);
            }
        } else {
            paths.insert(entry.path.into_bytes());
        }
    }
    Ok(paths)
}

fn stat_info_from_index_entry(entry: &IndexEntry) -> sley_unpack_trees::StatInfo {
    sley_unpack_trees::StatInfo {
        ctime_seconds: entry.ctime_seconds,
        ctime_nanoseconds: entry.ctime_nanoseconds,
        mtime_seconds: entry.mtime_seconds,
        mtime_nanoseconds: entry.mtime_nanoseconds,
        dev: entry.dev,
        ino: entry.ino,
        uid: entry.uid,
        gid: entry.gid,
        size: entry.size,
    }
}

fn index_entry_stage(entry: &IndexEntry) -> u8 {
    ((entry.flags >> 12) & 0x3) as u8
}

/// Persist planned read-tree entries while preserving existing skip-worktree
/// bits and rebuilding the cache-tree extension.
pub fn persist_read_tree_entries(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<(Vec<u8>, ReadTreeEntry)>,
) -> Result<()> {
    let entries = project_read_tree_index(git_dir, format, entries)?;
    persist_read_tree_index(git_dir, format, entries, None)
}

/// Persist a checkout/unpack result and retain checkout-safe index extensions.
pub fn persist_checkout_read_tree_entries(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<(Vec<u8>, ReadTreeEntry)>,
    previous: &Index,
) -> Result<()> {
    let entries = project_read_tree_index(git_dir, format, entries)?;
    persist_read_tree_index(git_dir, format, entries, Some(previous))
}

fn project_read_tree_index(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<(Vec<u8>, ReadTreeEntry)>,
) -> Result<Vec<IndexEntry>> {
    let skip_worktree_paths: BTreeSet<Vec<u8>> = read_repository_index(git_dir, format)?
        .map(|index| {
            index
                .entries
                .into_iter()
                .filter(|entry| entry.is_skip_worktree())
                .map(|entry| entry.path.into_bytes())
                .collect()
        })
        .unwrap_or_default();
    let mut index_entries = Vec::with_capacity(entries.len());
    for (path, entry) in entries {
        let name_len = path.len().min(0x0fff) as u16;
        let stage_bits = ((entry.stage as u16) & 0x3) << 12;
        let stat = entry.stat.unwrap_or_default();
        let mut index_entry = IndexEntry {
            ctime_seconds: stat.ctime_seconds,
            ctime_nanoseconds: stat.ctime_nanoseconds,
            mtime_seconds: stat.mtime_seconds,
            mtime_nanoseconds: stat.mtime_nanoseconds,
            dev: stat.dev,
            ino: stat.ino,
            mode: entry.mode,
            uid: stat.uid,
            gid: stat.gid,
            size: stat.size,
            oid: entry.oid,
            flags: name_len | stage_bits,
            flags_extended: 0,
            path: BString::from(path),
        };
        let skip_worktree = entry
            .skip_worktree
            .unwrap_or_else(|| skip_worktree_paths.contains(index_entry.path.as_bytes()));
        if skip_worktree {
            index_entry.set_skip_worktree(true);
        }
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags & 0x3000).cmp(&(right.flags & 0x3000)))
    });
    Ok(index_entries)
}

fn persist_read_tree_index(
    git_dir: &Path,
    format: ObjectFormat,
    entries: Vec<IndexEntry>,
    previous: Option<&Index>,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    refresh_cache_tree(&mut index, &db);
    if let Some(previous) = previous {
        preserve_checkout_index_extensions(previous, &mut index, format)?;
        if (previous.is_sparse() || previous.entries.iter().any(IndexEntry::is_sparse_dir))
            && let Some((sparse, mode)) = active_sparse_checkout(git_dir)?
            && sparse.sparse_index
        {
            let matcher = SparseMatcher::new(&sparse, mode);
            for entry in &mut index.entries {
                if entry.stage() == Stage::Normal {
                    entry.set_skip_worktree(!matcher.includes_file(entry.path.as_bytes()));
                }
            }
            collapse_to_sparse_index(&mut index, &matcher, &db, format)?;
        }
    }
    write_repository_index_ref(git_dir, format, &index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_object::{EncodedObject, ObjectType, Tree, TreeEntry};
    use sley_odb::ObjectWriter;

    #[derive(Default)]
    struct RecordingDiagnostics(Vec<Vec<u8>>);

    impl ReadTreeDiagnostics for RecordingDiagnostics {
        fn invalid_path(&mut self, path: &[u8]) {
            self.0.push(path.to_vec());
        }
    }

    fn temp_repo() -> (tempfile::TempDir, PathBuf, FileObjectDatabase) {
        let root = tempfile::tempdir().expect("create temporary repository");
        let git_dir = root.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object store");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        (root, git_dir, db)
    }

    fn tree(db: &FileObjectDatabase, entries: Vec<(&str, u32, ObjectId)>) -> ObjectId {
        let entries = entries
            .into_iter()
            .map(|(name, mode, oid)| TreeEntry {
                mode,
                name: BString::from(name),
                oid,
            })
            .collect();
        db.write_object(EncodedObject::new(
            ObjectType::Tree,
            Tree { entries }.write(),
        ))
        .expect("write tree")
    }

    fn blob(db: &FileObjectDatabase, body: &[u8]) -> ObjectId {
        db.write_object(EncodedObject::new(ObjectType::Blob, body.to_vec()))
            .expect("write blob")
    }

    #[test]
    fn overlay_replaces_file_directory_collisions_recursively() {
        let (_root, git_dir, db) = temp_repo();
        let one = blob(&db, b"one");
        let two = blob(&db, b"two");
        let subtree = tree(&db, vec![("child", 0o100644, one)]);
        let first = tree(&db, vec![("a", 0o040000, subtree), ("b", 0o100644, one)]);
        let second = tree(&db, vec![("a", 0o100644, two), ("b", 0o040000, subtree)]);
        let mut diagnostics = RecordingDiagnostics::default();
        let outcome = plan_read_tree_transition(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &GitConfig::default(),
            ReadTreeTransitionOptions {
                mode: ReadTreeTransitionMode::Overlay,
                tree_oids: &[first, second],
            },
            &mut diagnostics,
        )
        .expect("plan overlay");
        assert_eq!(outcome.entries.len(), 2);
        assert_eq!(outcome.entries[0].0, b"a");
        assert_eq!(outcome.entries[0].1.oid, two);
        assert_eq!(outcome.entries[1].0, b"b/child");
        assert_eq!(outcome.entries[1].1.oid, one);
        assert!(diagnostics.0.is_empty());
    }

    #[test]
    fn invalid_path_is_reported_through_injected_diagnostics() {
        let (_root, git_dir, db) = temp_repo();
        let content = blob(&db, b"payload");
        let bad = tree(&db, vec![(".git", 0o100644, content)]);
        let mut diagnostics = RecordingDiagnostics::default();
        let error = plan_read_tree_transition(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &GitConfig::default(),
            ReadTreeTransitionOptions {
                mode: ReadTreeTransitionMode::Overlay,
                tree_oids: &[bad],
            },
            &mut diagnostics,
        )
        .expect_err("reject protected path");
        assert!(matches!(
            error,
            ReadTreeTransitionError::InvalidPath(path) if path == b".git"
        ));
        assert_eq!(diagnostics.0, [b".git".to_vec()]);
    }

    #[test]
    fn prefix_transition_retains_index_and_persists_projected_entries() {
        let (_root, git_dir, db) = temp_repo();
        let retained = blob(&db, b"retained");
        persist_read_tree_entries(
            &git_dir,
            ObjectFormat::Sha1,
            vec![(
                b"existing".to_vec(),
                ReadTreeEntry::stage_zero(0o100644, retained),
            )],
        )
        .expect("seed index");
        let added = blob(&db, b"added");
        let source = tree(&db, vec![("new", 0o100644, added)]);
        let mut diagnostics = RecordingDiagnostics::default();
        let outcome = plan_read_tree_transition(
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            &GitConfig::default(),
            ReadTreeTransitionOptions::prefix(b"nested/".to_vec(), &[source]),
            &mut diagnostics,
        )
        .expect("plan prefix transition");
        assert_eq!(
            outcome
                .entries
                .iter()
                .map(|(path, entry)| (path.as_slice(), entry.oid))
                .collect::<Vec<_>>(),
            [(b"existing".as_slice(), retained), (b"nested/new", added)]
        );
        persist_read_tree_entries(&git_dir, ObjectFormat::Sha1, outcome.entries)
            .expect("persist prefix transition");
        let index = read_repository_index(&git_dir, ObjectFormat::Sha1)
            .expect("read index")
            .expect("index exists");
        assert_eq!(
            index
                .entries
                .iter()
                .map(|entry| entry.path.as_bytes())
                .collect::<Vec<_>>(),
            [b"existing".as_slice(), b"nested/new"]
        );
        assert!(diagnostics.0.is_empty());
    }
}
