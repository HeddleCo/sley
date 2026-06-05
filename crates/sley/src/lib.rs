//! `git` — the ergonomic facade over the sley engine.
//!
//! Downstream code that wants a "just open a repository and read things" entry
//! point should reach for [`Repository`] rather than wiring the plumbing crates
//! together by hand. A [`Repository`] is a lightweight handle around a resolved
//! git directory: it remembers the `git_dir`, the common directory (for linked
//! worktrees), and the repository's object format, and hands back the
//! underlying plumbing objects ([`sley_odb::FileObjectDatabase`],
//! [`sley_refs::FileRefStore`], [`sley_config::GitConfig`]) on demand.
//!
//! For power users the engine crates are re-exported under [`plumbing`] (and the
//! most common types are re-exported at the crate root), so a single
//! `git = { path = ... }` dependency is enough to reach the whole stack.
//!
//! ```no_run
//! use sley::Repository;
//!
//! # fn main() -> sley::Result<()> {
//! let repo = Repository::discover(".")?;
//! let head = repo.head()?;
//! if let Some(oid) = head.oid {
//!     let commit = repo.read_commit(&oid)?;
//!     let _ = commit.tree;
//! }
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};

use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeBuilder};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget};

/// Re-exports of the underlying plumbing crates for callers that need direct
/// access to the engine. Everything reachable through [`Repository`] is built
/// from these, and they remain available for the operations the facade does not
/// (yet) wrap.
pub mod plumbing {
    pub use sley_config;
    pub use sley_core;
    pub use sley_formats;
    pub use sley_index;
    pub use sley_object;
    pub use sley_odb;
    pub use sley_refs;
    pub use sley_rev;
    pub use sley_worktree;
}

// The most frequently used plumbing types are also re-exported at the crate root
// so the common path (`use sley::{Repository, ObjectId, ...}`) stays short.
pub use sley_config::GitConfig;
pub use sley_core::{GitError, ObjectFormat, ObjectId, Result};
pub use sley_object::{Commit as CommitObject, ObjectType as GitObjectType, Tree as TreeObject};
pub use sley_object::{EntryKind, TreeBuilder as TreeEditor};
pub use sley_index::{Index, IndexEntry, Stage as IndexStage};
pub use sley_odb::FileObjectDatabase as ObjectDatabase;
pub use sley_refs::{FileRefStore as RefStore, RefPrecondition, RefTarget as ReferenceTarget};

/// A resolved reference: its full name plus the target it points at.
///
/// `target` is the immediate target as stored on disk (a direct [`ObjectId`] or
/// a symbolic pointer to another ref name), while [`Reference::peeled_oid`]
/// follows symbolic chains to the final object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The full reference name, e.g. `refs/heads/main` or `HEAD`.
    pub name: String,
    /// The reference's immediate target.
    pub target: RefTarget,
}

impl Reference {
    /// The object id this reference resolves to, if it is (or chains to) a
    /// direct reference.
    pub fn peeled_oid(&self, repo: &Repository) -> Result<Option<ObjectId>> {
        match &self.target {
            RefTarget::Direct(oid) => Ok(Some(oid.clone())),
            RefTarget::Symbolic(name) => repo.resolve_symbolic(name),
        }
    }
}

/// The resolved state of `HEAD`.
///
/// A repository freshly created by `git init` has `HEAD` pointing at a branch
/// that does not exist yet ("unborn"); in that case [`Head::oid`] is `None`
/// while [`Head::symbolic_target`] still names the branch ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// The branch ref `HEAD` symbolically points at (e.g. `refs/heads/main`),
    /// or `None` when `HEAD` is detached (points directly at a commit).
    pub symbolic_target: Option<String>,
    /// The commit `HEAD` resolves to, or `None` for an unborn branch.
    pub oid: Option<ObjectId>,
}

impl Head {
    /// Whether `HEAD` points at a branch that does not exist yet.
    pub fn is_unborn(&self) -> bool {
        self.symbolic_target.is_some() && self.oid.is_none()
    }

    /// Whether `HEAD` points directly at a commit rather than a branch.
    pub fn is_detached(&self) -> bool {
        self.symbolic_target.is_none() && self.oid.is_some()
    }

    /// The short branch name (`refs/heads/<name>` stripped to `<name>`) `HEAD`
    /// points at, if any.
    pub fn branch_name(&self) -> Option<&str> {
        self.symbolic_target
            .as_deref()
            .and_then(|name| name.strip_prefix("refs/heads/"))
    }
}

/// An ergonomic handle to a git repository.
///
/// Construct one with [`Repository::open`] (when you already know the git
/// directory), [`Repository::discover`] (to search upward from a working-tree
/// path), or [`Repository::init`] / [`Repository::init_bare`] (to create a new
/// repository). The handle is cheap to clone and holds no open file handles; it
/// builds the plumbing objects ([`Repository::objects`],
/// [`Repository::references`], [`Repository::config`]) on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    git_dir: PathBuf,
    common_dir: PathBuf,
    format: ObjectFormat,
}

impl Repository {
    /// Open the repository whose git directory is exactly `git_dir`.
    ///
    /// `git_dir` must be a git directory itself (the `.git` directory of a
    /// non-bare repo, or the top level of a bare repo), not a working tree. Use
    /// [`Repository::discover`] to search upward from a working-tree path.
    ///
    /// The path may be a `.git` *file* (a gitlink, as used by linked worktrees
    /// and submodules); its `gitdir:` target is followed.
    pub fn open(git_dir: impl AsRef<Path>) -> Result<Self> {
        let git_dir = resolve_git_dir(git_dir.as_ref())?;
        if !is_git_dir(&git_dir) {
            return Err(GitError::NotFound(format!(
                "not a git repository: {}",
                git_dir.display()
            )));
        }
        Self::from_git_dir(git_dir)
    }

    /// Discover the repository containing `path` by walking up the directory
    /// tree, mirroring git's own discovery (`.git` directory, `.git` gitlink
    /// file, or a bare repository at an ancestor).
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let git_dir = discover_git_dir(path.as_ref())?;
        Self::from_git_dir(git_dir)
    }

    /// Initialize a new non-bare repository rooted at `path` (creating its
    /// `.git` directory) and return a handle to it. Re-initializing an existing
    /// repository is a no-op on already-present files, matching `git init`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_format(path, ObjectFormat::Sha1, false)
    }

    /// Initialize a new bare repository at `path` (the directory becomes the git
    /// directory itself) and return a handle to it.
    pub fn init_bare(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_format(path, ObjectFormat::Sha1, true)
    }

    /// Initialize a new repository at `path` with an explicit object format and
    /// bare-ness.
    pub fn init_with_format(
        path: impl AsRef<Path>,
        format: ObjectFormat,
        bare: bool,
    ) -> Result<Self> {
        let layout = sley_formats::RepositoryLayout::init_at(path, format, bare)?;
        Self::from_git_dir(layout.git_dir)
    }

    fn from_git_dir(git_dir: PathBuf) -> Result<Self> {
        let common_dir = sley_odb::repository_common_dir(&git_dir);
        let format = read_object_format(&common_dir)?;
        Ok(Self {
            git_dir,
            common_dir,
            format,
        })
    }

    /// The repository's git directory (the `.git` directory of a non-bare repo,
    /// or the top level of a bare repo).
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The common directory shared between linked worktrees. For a single
    /// worktree this equals [`Repository::git_dir`]; for a linked worktree it is
    /// the main repository's git directory (as recorded in `commondir`).
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The repository's object format (`sha1` or `sha256`), read from
    /// `extensions.objectformat`.
    pub fn object_format(&self) -> ObjectFormat {
        self.format
    }

    /// The object database for this repository, reading loose and packed
    /// objects (and any alternates).
    pub fn objects(&self) -> FileObjectDatabase {
        FileObjectDatabase::from_git_dir(&self.common_dir, self.format)
    }

    /// The reference store for this repository (loose refs, `packed-refs`, and
    /// reflogs), scoped to this worktree's git directory.
    pub fn references(&self) -> FileRefStore {
        FileRefStore::new(self.git_dir.clone(), self.format)
    }

    /// The repository-level configuration (`<common_dir>/config`).
    ///
    /// Returns an empty [`GitConfig`] if the file is missing, matching the way
    /// git treats an absent repository config.
    pub fn config(&self) -> Result<GitConfig> {
        let path = self.common_dir.join("config");
        match GitConfig::read(&path) {
            Ok(config) => Ok(config),
            Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => Ok(GitConfig::default()),
            Err(err) => Err(err),
        }
    }

    /// The *effective* configuration, merging the system, global, and repository
    /// config files in git's precedence order (repository wins, then global,
    /// then system) with `include`/`includeIf` directives resolved.
    ///
    /// Unlike [`Repository::config`] (repository file only), this is what a
    /// caller wanting git-equivalent lookups — e.g. resolving `user.name` from
    /// `~/.gitconfig` — should use. File discovery honours `$GIT_CONFIG_SYSTEM`,
    /// `$GIT_CONFIG_GLOBAL`, `$XDG_CONFIG_HOME`, `$HOME`, and
    /// `$GIT_CONFIG_NOSYSTEM` exactly as git does; missing files are skipped.
    ///
    /// This does not layer in `-c`/`GIT_CONFIG_COUNT` command-line overrides,
    /// which are process-level and higher precedence than any file.
    pub fn config_snapshot(&self) -> Result<GitConfig> {
        let context = sley_config::ConfigIncludeContext::new(
            Some(self.config_include_git_dir()),
            self.config_include_branch(),
        );
        sley_config::load_effective_config(&self.common_dir, &context)
    }

    /// Look up `section.key` in the effective config (see
    /// [`Repository::config_snapshot`]), returning the highest-precedence value
    /// or `None` if unset. For a subsectioned key use
    /// [`Repository::config_string_subsection`].
    pub fn config_string(&self, section: &str, key: &str) -> Result<Option<String>> {
        self.config_string_subsection(section, None, key)
    }

    /// Look up `section.subsection.key` in the effective config, honouring
    /// subsections (e.g. `remote.origin.url`). `subsection` of `None` reads the
    /// bare section.
    pub fn config_string_subsection(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Result<Option<String>> {
        let config = self.config_snapshot()?;
        Ok(sley_config::config_string(&config, section, subsection, key))
    }

    /// Absolute common git directory used as the `gitdir:` context for
    /// `includeIf` evaluation, falling back to the unmodified path when it
    /// cannot be canonicalised (e.g. it does not yet exist).
    fn config_include_git_dir(&self) -> PathBuf {
        std::fs::canonicalize(&self.common_dir).unwrap_or_else(|_| self.common_dir.clone())
    }

    /// Short branch name from `HEAD` for `includeIf "onbranch:"` evaluation, or
    /// `None` when detached/unborn. Errors are swallowed: a config snapshot must
    /// not fail just because `HEAD` is unreadable.
    fn config_include_branch(&self) -> Option<String> {
        let head = self.head().ok()?;
        head.symbolic_target
            .as_deref()
            .and_then(|target| target.strip_prefix("refs/heads/"))
            .map(str::to_string)
    }

    /// Resolve `HEAD`: its symbolic branch target (if any) and the commit it
    /// points at (if the branch exists).
    pub fn head(&self) -> Result<Head> {
        let refs = self.references();
        match refs.read_ref("HEAD")? {
            None => Err(GitError::NotFound("HEAD is missing".into())),
            Some(RefTarget::Direct(oid)) => Ok(Head {
                symbolic_target: None,
                oid: Some(oid),
            }),
            Some(RefTarget::Symbolic(name)) => {
                let oid = self.resolve_symbolic(&name)?;
                Ok(Head {
                    symbolic_target: Some(name),
                    oid,
                })
            }
        }
    }

    /// Look up a reference by full name (e.g. `refs/heads/main`, `refs/tags/v1`,
    /// or `HEAD`), returning `None` if it does not exist.
    pub fn find_reference(&self, name: &str) -> Result<Option<Reference>> {
        let refs = self.references();
        Ok(refs.read_ref(name)?.map(|target| Reference {
            name: name.to_string(),
            target,
        }))
    }

    /// Resolve a revision specification (anything `git rev-parse` accepts:
    /// branch/tag names, abbreviated or full object ids, `HEAD~2`, `<rev>:<path>`,
    /// `@{u}`, etc.) to a concrete [`ObjectId`].
    pub fn rev_parse(&self, spec: &str) -> Result<ObjectId> {
        sley_rev::resolve_revision(&self.git_dir, self.format, spec)
    }

    /// Read a raw object (any type) from the object database.
    pub fn read_object(&self, oid: &ObjectId) -> Result<EncodedObject> {
        self.objects().read_object(oid)
    }

    /// Read a commit object, parsing it into a [`Commit`]. Returns an error if
    /// `oid` does not name a commit.
    pub fn read_commit(&self, oid: &ObjectId) -> Result<Commit> {
        let object = self.read_object(oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "object {oid} is a {}, not a commit",
                object.object_type.as_str()
            )));
        }
        Commit::parse(self.format, &object.body)
    }

    /// Read a tree object, parsing it into a [`Tree`]. Returns an error if `oid`
    /// does not name a tree.
    pub fn read_tree(&self, oid: &ObjectId) -> Result<Tree> {
        let object = self.read_object(oid)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::InvalidObject(format!(
                "object {oid} is a {}, not a tree",
                object.object_type.as_str()
            )));
        }
        Tree::parse(self.format, &object.body)
    }

    /// Write a raw object (any type) to the object database as a loose object,
    /// returning its id. The bytes are stored verbatim, so writing an object
    /// that originated from another repository preserves its id exactly.
    pub fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        let mut odb = self.objects();
        odb.write_object(object)
    }

    /// Write `bytes` as a blob, returning its id.
    pub fn write_blob(&self, bytes: impl Into<Vec<u8>>) -> Result<ObjectId> {
        self.write_object(EncodedObject::new(ObjectType::Blob, bytes))
    }

    /// Start editing the tree `base` one level deep: returns a [`TreeBuilder`]
    /// seeded with `base`'s entries (empty when `base` is the null or empty
    /// tree). `upsert` entries on it, then write it with
    /// [`Repository::write_tree`].
    pub fn edit_tree(&self, base: &ObjectId) -> Result<TreeBuilder> {
        if base.is_null() || *base == ObjectId::empty_tree(self.format) {
            return Ok(TreeBuilder::new());
        }
        Ok(TreeBuilder::from_tree(self.read_tree(base)?))
    }

    /// Write the tree assembled in `builder` — entries emitted in Git's
    /// canonical order — and return its id.
    pub fn write_tree(&self, builder: TreeBuilder) -> Result<ObjectId> {
        self.write_object(EncodedObject::new(ObjectType::Tree, builder.write()))
    }

    /// Read this repository's index (`.git/index`), returning `None` when the
    /// index file does not exist yet.
    pub fn open_index(&self) -> Result<Option<Index>> {
        sley_worktree::read_repository_index(&self.git_dir, self.format)
    }

    /// Build a fresh index mirroring `tree_oid` (stage-0 entries with a zeroed
    /// stat), the way `git read-tree <tree>` would. Does not touch `.git/index`.
    pub fn index_from_tree(&self, tree_oid: &ObjectId) -> Result<Index> {
        sley_worktree::index_from_tree(&self.objects(), self.format, tree_oid)
    }

    /// Follow a symbolic ref chain (e.g. `HEAD` -> `refs/heads/main`) to the
    /// final object id, returning `None` if the chain ends at a ref that does
    /// not exist (an unborn branch).
    fn resolve_symbolic(&self, name: &str) -> Result<Option<ObjectId>> {
        let refs = self.references();
        // Git refuses to follow symref chains deeper than five hops; mirror that
        // bound so a cycle cannot loop forever.
        const MAX_SYMREF_DEPTH: usize = 5;
        let mut current = name.to_string();
        for _ in 0..MAX_SYMREF_DEPTH {
            match refs.read_ref(&current)? {
                None => return Ok(None),
                Some(RefTarget::Direct(oid)) => return Ok(Some(oid)),
                Some(RefTarget::Symbolic(next)) => current = next,
            }
        }
        Err(GitError::InvalidFormat(format!(
            "symbolic reference chain too deep starting at {name}"
        )))
    }
}

/// Read the object format recorded in a git directory's config, defaulting to
/// SHA-1 (as git does) when the config is absent or omits the extension.
fn read_object_format(common_dir: &Path) -> Result<ObjectFormat> {
    let config_path = common_dir.join("config");
    match GitConfig::read(&config_path) {
        Ok(config) => config.repository_object_format(),
        Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => Ok(ObjectFormat::Sha1),
        Err(err) => Err(err),
    }
}

/// Resolve a path that may be either a git directory or a `.git` gitlink file to
/// the real git directory.
fn resolve_git_dir(path: &Path) -> Result<PathBuf> {
    if path.is_file()
        && let Some(target) = read_gitdir_link(path)?
    {
        return Ok(target);
    }
    Ok(path.to_path_buf())
}

/// True if `path` looks like a git directory (has a `HEAD` file and either an
/// `objects` directory or a `commondir` pointer).
fn is_git_dir(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

/// Read a `gitdir: <path>` link file (used by linked worktrees and submodules),
/// returning the absolute target path it points at.
fn read_gitdir_link(path: &Path) -> Result<Option<PathBuf>> {
    let contents = std::fs::read_to_string(path)?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = PathBuf::from(target.trim());
    if target.is_absolute() {
        Ok(Some(target))
    } else {
        let base = path.parent().unwrap_or_else(|| Path::new(""));
        Ok(Some(base.join(target)))
    }
}

/// Walk up from `start` looking for a repository, mirroring git's discovery
/// rules: a `.git` directory, a `.git` gitlink file, or a bare repository.
fn discover_git_dir(start: &Path) -> Result<PathBuf> {
    let start = if start.as_os_str().is_empty() {
        Path::new(".")
    } else {
        start
    };
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    for candidate in absolute.ancestors() {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git);
        }
        if dot_git.is_file()
            && let Some(git_dir) = read_gitdir_link(&dot_git)?
            && is_git_dir(&git_dir)
        {
            return Ok(git_dir);
        }
        if is_git_dir(candidate) {
            return Ok(candidate.to_path_buf());
        }
    }
    Err(GitError::NotFound(format!(
        "not a git repository (or any parent up to {}): {}",
        absolute.display(),
        start.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_odb::ObjectWriter;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A temporary directory that cleans itself up on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sley-facade-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Write a blob, a tree referencing it, and a commit pointing at the tree,
    /// then point `refs/heads/main` at the commit. Returns the commit oid.
    fn seed_commit(repo: &Repository) -> ObjectId {
        let mut db = repo.objects();

        let blob_oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .expect("write blob");

        let tree = Tree {
            entries: vec![sley_object::TreeEntry {
                mode: 0o100644,
                name: b"hello.txt".to_vec(),
                oid: blob_oid.clone(),
            }],
        };
        let tree_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("write tree");

        let commit = Commit {
            tree: tree_oid.clone(),
            parents: Vec::new(),
            author: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            committer: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            encoding: None,
            message: b"initial\n".to_vec(),
        };
        let commit_oid = db
            .write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .expect("write commit");

        let refs = repo.references();
        refs.create_branch(
            "main",
            commit_oid.clone(),
            b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            b"commit (initial): initial".to_vec(),
        )
        .expect("create main branch");

        commit_oid
    }

    #[test]
    fn init_creates_repo_and_open_reads_it_back() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        assert_eq!(repo.git_dir(), temp.path().join(".git"));
        assert_eq!(repo.object_format(), ObjectFormat::Sha1);
        assert!(repo.git_dir().join("HEAD").is_file());

        // Re-open via open() on the .git directory.
        let reopened = Repository::open(temp.path().join(".git")).expect("open");
        assert_eq!(reopened.git_dir(), repo.git_dir());
        assert_eq!(reopened.object_format(), ObjectFormat::Sha1);
    }

    #[test]
    fn init_bare_uses_path_as_git_dir() {
        let temp = TempDir::new();
        let repo = Repository::init_bare(temp.path()).expect("init bare");
        // Bare repo: the path itself is the git dir, no nested .git.
        assert_eq!(repo.git_dir(), temp.path());
        assert!(repo.git_dir().join("HEAD").is_file());
        assert!(repo.git_dir().join("objects").is_dir());

        let reopened = Repository::open(temp.path()).expect("open bare");
        assert_eq!(reopened.git_dir(), temp.path());
    }

    #[test]
    fn head_is_unborn_after_init() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let head = repo.head().expect("head");
        assert_eq!(head.symbolic_target.as_deref(), Some("refs/heads/main"));
        assert_eq!(head.oid, None);
        assert!(head.is_unborn());
        assert!(!head.is_detached());
        assert_eq!(head.branch_name(), Some("main"));
    }

    #[test]
    fn head_resolves_after_commit() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);

        let head = repo.head().expect("head");
        assert_eq!(head.symbolic_target.as_deref(), Some("refs/heads/main"));
        assert_eq!(head.oid.as_ref(), Some(&commit_oid));
        assert!(!head.is_unborn());
        assert_eq!(head.branch_name(), Some("main"));
    }

    #[test]
    fn read_object_commit_and_tree_round_trip() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);

        // Raw object read.
        let raw = repo.read_object(&commit_oid).expect("read object");
        assert_eq!(raw.object_type, ObjectType::Commit);

        // Typed commit read.
        let commit = repo.read_commit(&commit_oid).expect("read commit");
        assert_eq!(commit.message, b"initial\n");
        assert!(commit.parents.is_empty());

        // Typed tree read via the commit's tree.
        let tree = repo.read_tree(&commit.tree).expect("read tree");
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].name, b"hello.txt");

        // The blob under the tree is readable as a raw object.
        let blob = repo
            .read_object(&tree.entries[0].oid)
            .expect("read blob");
        assert_eq!(blob.object_type, ObjectType::Blob);
        assert_eq!(blob.body, b"hello\n");
    }

    #[test]
    fn read_commit_rejects_non_commit() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);
        let commit = repo.read_commit(&commit_oid).expect("read commit");

        // The tree oid is not a commit.
        let err = repo
            .read_commit(&commit.tree)
            .expect_err("reading a tree as a commit must fail");
        assert!(matches!(err, GitError::InvalidObject(_)));
    }

    #[test]
    fn rev_parse_resolves_branch_and_head() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);

        assert_eq!(repo.rev_parse("HEAD").expect("HEAD"), commit_oid);
        assert_eq!(repo.rev_parse("main").expect("main"), commit_oid);
        assert_eq!(
            repo.rev_parse("refs/heads/main").expect("full ref"),
            commit_oid
        );
        // Full hex object id also resolves.
        assert_eq!(
            repo.rev_parse(&commit_oid.to_hex()).expect("hex"),
            commit_oid
        );
    }

    #[test]
    fn find_reference_returns_branch_and_head() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);

        let branch = repo
            .find_reference("refs/heads/main")
            .expect("find branch")
            .expect("branch exists");
        assert_eq!(branch.name, "refs/heads/main");
        assert_eq!(branch.target, RefTarget::Direct(commit_oid.clone()));
        assert_eq!(
            branch.peeled_oid(&repo).expect("peel"),
            Some(commit_oid.clone())
        );

        let head = repo
            .find_reference("HEAD")
            .expect("find head")
            .expect("head exists");
        assert_eq!(head.target, RefTarget::Symbolic("refs/heads/main".into()));
        // Peeling HEAD follows the symbolic chain to the commit.
        assert_eq!(head.peeled_oid(&repo).expect("peel head"), Some(commit_oid));

        // A missing ref returns None rather than erroring.
        assert!(
            repo.find_reference("refs/heads/missing")
                .expect("missing lookup")
                .is_none()
        );
    }

    #[test]
    fn discover_finds_repo_from_nested_subdirectory() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let nested = temp.path().join("a").join("b").join("c");
        fs::create_dir_all(&nested).expect("nested dirs");

        let discovered = Repository::discover(&nested).expect("discover");
        // Both should point at the same .git after canonicalization.
        assert_eq!(
            fs::canonicalize(discovered.git_dir()).expect("canon discovered"),
            fs::canonicalize(repo.git_dir()).expect("canon repo")
        );
    }

    #[test]
    fn discover_errors_outside_any_repo() {
        let temp = TempDir::new();
        // temp dir is not inside a repo (it lives directly under the system tmp
        // dir, which is not a git working tree).
        let err = Repository::discover(temp.path())
            .expect_err("discovering outside any repo must fail");
        assert!(matches!(err, GitError::NotFound(_)));
    }

    #[test]
    fn open_rejects_non_git_directory() {
        let temp = TempDir::new();
        let err = Repository::open(temp.path())
            .expect_err("opening a non-git directory must fail");
        assert!(matches!(err, GitError::NotFound(_)));
    }

    #[test]
    fn config_round_trips_and_reports_format() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let config = repo.config().expect("config");
        // init writes core.repositoryformatversion and core.bare.
        assert_eq!(config.get("core", None, "bare"), Some("false"));
        assert_eq!(config.repository_object_format().expect("format"), ObjectFormat::Sha1);
    }

    #[test]
    fn sha256_repository_round_trips() {
        let temp = TempDir::new();
        let repo = Repository::init_with_format(temp.path(), ObjectFormat::Sha256, false)
            .expect("init sha256");
        assert_eq!(repo.object_format(), ObjectFormat::Sha256);

        // Re-open and confirm the format is read back from config.
        let reopened = Repository::open(temp.path().join(".git")).expect("open");
        assert_eq!(reopened.object_format(), ObjectFormat::Sha256);

        let commit_oid = seed_commit(&repo);
        assert_eq!(commit_oid.format(), ObjectFormat::Sha256);
        assert_eq!(repo.rev_parse("HEAD").expect("HEAD"), commit_oid);
    }

    #[test]
    fn config_snapshot_reads_repository_layer_via_helpers() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        // Append identity + a subsectioned remote to the repository config. The
        // repository layer is the highest-precedence file layer, so these win
        // over any global/system config the test machine might have, keeping the
        // assertions hermetic. (End-to-end global fallback is covered by the
        // CLI's subprocess interop test.)
        let config_path = repo.common_dir().join("config");
        let mut contents = fs::read(&config_path).expect("read config");
        contents.extend_from_slice(
            b"[user]\n\tname = Snapshot Person\n\temail = snap@example.invalid\n\
              [remote \"origin\"]\n\turl = https://example.invalid/x.git\n",
        );
        fs::write(&config_path, contents).expect("write config");

        // config_snapshot returns the merged effective config.
        let snapshot = repo.config_snapshot().expect("snapshot");
        assert_eq!(snapshot.get("user", None, "name"), Some("Snapshot Person"));

        // config_string is the convenience wrapper.
        assert_eq!(
            repo.config_string("user", "name").expect("name"),
            Some("Snapshot Person".to_string())
        );
        assert_eq!(
            repo.config_string("user", "email").expect("email"),
            Some("snap@example.invalid".to_string())
        );
        assert_eq!(repo.config_string("user", "missing").expect("missing"), None);

        // Subsections are honoured.
        assert_eq!(
            repo.config_string_subsection("remote", Some("origin"), "url")
                .expect("url"),
            Some("https://example.invalid/x.git".to_string())
        );
    }

    #[test]
    fn plumbing_reexports_are_reachable() {
        // Smoke test that the re-exports compile and resolve to the right types.
        let _format: plumbing::sley_core::ObjectFormat = ObjectFormat::Sha1;
        let _: fn(&[u8]) -> Result<plumbing::sley_config::GitConfig> = plumbing::sley_config::GitConfig::parse;
    }
}
