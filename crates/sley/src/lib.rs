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
//! **Heddle / embedder surface** (feature `remote`, on by default):
//!
//! * [`Repository::config_snapshot`] — full git config with `include` /
//!   `includeIf` / `hasconfig:` resolution.
//! * [`Repository::init_mirror`] — bare mirror defaults (`+refs/*:refs/*`,
//!   `mirror = true` on `origin`).
//! * [`Repository::copy_reachable_from`] — pack-based object transfer.
//! * [`Repository::remote`] / [`remote::RemoteContext`] — URL rewriting and
//!   [`sley_remote`] fetch/push/clone/ls-remote orchestration (HTTP v2 fetch,
//!   SSH, bundle fetch, thin-pack push).
//! * [`notes`] — git notes read/write for round-trip fidelity.
//! * [`Repository::capabilities`] / [`Repository::transport_capabilities`] —
//!   capability probes before calling in.
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

mod capabilities;
mod config_edit;
mod diff;
mod index_io;
mod notes_repo;
mod objects;
mod refs;
mod remote_edit;
mod status_plan;

#[cfg(feature = "remote")]
pub mod remote;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sley_object::{Commit, EncodedObject, ObjectType, Tag, Tree, TreeBuilder};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter, install_reachable_pack};
use sley_refs::{FileRefStore, RefTarget};
use sley_rev::ResolvedTreePath;
use sley_sequencer::create_annotated_tag;

/// Git notes read/write ([`sley_notes`]).
pub mod notes {
    pub use sley_notes::*;
}

/// Re-exports of the underlying plumbing crates for callers that need direct
/// access to the engine. Everything reachable through [`Repository`] is built
/// from these, and they remain available for the operations the facade does not
/// (yet) wrap.
pub mod plumbing {
    pub use sley_config;
    pub use sley_core;
    pub use sley_diff_merge;
    pub use sley_formats;
    pub use sley_grep;
    pub use sley_index;
    pub use sley_notes;
    pub use sley_object;
    pub use sley_odb;
    pub use sley_pretty;
    pub use sley_refs;
    #[cfg(feature = "remote")]
    pub use sley_remote;
    pub use sley_rev;
    pub use sley_sequencer;
    pub use sley_worktree;
}

// The most frequently used plumbing types are also re-exported at the crate root
// so the common path (`use sley::{Repository, ObjectId, ...}`) stays short.
pub use sley_config::GitConfig;
pub use sley_core::{
    BString, FullName, GitError, GitTime, MissingObjectContext, MissingObjectKind, NotFoundKind,
    ObjectFormat, ObjectId, Result, Signature,
};
pub use sley_diff_merge::{DiffNameStatusOptions, NameStatusEntry};
pub use sley_grep as grep;
pub use sley_index::{Index, IndexEntry, Stage as IndexStage};
pub use sley_object::{
    Commit as CommitObject, ObjectType as GitObjectType, Tag as TagObject, Tree as TreeObject,
};
pub use sley_object::{EntryKind, TreeBuilder as TreeEditor};
pub use sley_odb::FileObjectDatabase as ObjectDatabase;
pub use sley_pretty as pretty;
pub use sley_refs::{
    FileRefStore as RefStore, RefDeleteError, RefPrecondition, RefTarget as ReferenceTarget,
};
pub use sley_sequencer::TagCreate;
pub use sley_worktree::{
    AtomicMetadataWriteOptions, AtomicMetadataWriteResult, IndexStatProbe, IndexStatProbeCache,
    ShortStatusEntry, ShortStatusOptions, ShortStatusRow, StatusUntrackedMode, StreamControl,
    WorktreeEntryState, write_metadata_file_atomic,
};

pub use capabilities::RepositoryCapabilities;
pub use config_edit::{
    ConfigEdit, ConfigEditError, ConfigEditPlan, ConfigEditScope, ConfigRemote, ConfigSectionEntry,
    ConfigSectionId, ConfigSnapshot, ConfigSource, ConfigStackOptions, ConfigStackView,
    ConfigValue, RemoteConfig, RemoteConfigRefusal, RemoteConfigRemove, RemoteConfigSet,
    RemoteConfigSnapshot, RemoteConfigSource, RemoteConfigValue, WorktreeConfig,
};
pub use index_io::{IndexError, IndexWriteError, IndexWriteOptions, IndexWriteResult};
pub use objects::{BlobFetchOptions, BlobStore, LoadedObject};
pub use refs::{
    DeleteRef, RefBatchChange, RefChange, RefChangeResult, RefConflict, RefDeleteExpected,
    RefUpdateOptions, ReflogMessage,
};
pub use status_plan::{StatusCacheKey, StatusPlan, StatusPlanBuilder};

/// A resolved reference: its full name plus the target it points at.
///
/// `target` is the immediate target as stored on disk (a direct [`ObjectId`] or
/// a symbolic pointer to another ref name), while [`Reference::peeled_oid`]
/// follows symbolic chains to the final object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The full reference name, e.g. `refs/heads/main` or `HEAD`.
    pub name: FullName,
    /// The reference's immediate target.
    pub target: RefTarget,
}

impl Reference {
    /// Return this reference's immediate direct object id.
    ///
    /// This intentionally does not follow symbolic refs and does not peel
    /// annotated tags. Use [`Repository::require_reference`] first when absence
    /// is a hard error, then choose explicit symbolic resolution or object
    /// peeling at the call site.
    pub fn direct_target(&self) -> Result<ObjectId> {
        match &self.target {
            RefTarget::Direct(oid) => Ok(*oid),
            RefTarget::Symbolic(target) => Err(GitError::InvalidFormat(format!(
                "reference {} is symbolic to {target}",
                self.name
            ))),
        }
    }

    /// Borrow this reference's immediate on-disk target.
    pub fn immediate_target(&self) -> &RefTarget {
        &self.target
    }

    /// The object id this reference resolves to, if it is (or chains to) a
    /// direct reference.
    pub fn peeled_oid(&self, repo: &Repository) -> Result<Option<ObjectId>> {
        match &self.target {
            RefTarget::Direct(oid) => Ok(Some(*oid)),
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
    pub symbolic_target: Option<FullName>,
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
            .as_ref()
            .map(FullName::as_str)
            .and_then(|name| name.strip_prefix("refs/heads/"))
    }
}

/// Repository open behavior for callers that need to distinguish discovery from
/// exact git-directory opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    exact_path: bool,
    bare: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            exact_path: false,
            bare: false,
        }
    }

    /// When true, `path` must be the git directory itself; no parent discovery is
    /// performed. When false, `path` is treated like [`Repository::discover`].
    pub fn exact_path(mut self, exact_path: bool) -> Self {
        self.exact_path = exact_path;
        self
    }

    /// Require the opened repository to be bare.
    pub fn bare(mut self, bare: bool) -> Self {
        self.bare = bare;
        self
    }

    pub fn is_exact_path(self) -> bool {
        self.exact_path
    }

    pub fn requires_bare(self) -> bool {
        self.bare
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// An ergonomic handle to a git repository.
///
/// Construct one with [`Repository::open`] (when you already know the git
/// directory), [`Repository::discover`] (to search upward from a working-tree
/// path), or [`Repository::init`] / [`Repository::init_bare`] (to create a new
/// repository). The handle is cheap to clone and shares a session-scoped object
/// database ([`Repository::objects`]) whose read caches survive across calls
/// until [`Repository::refresh_objects`] (automatic after `fetch` / pack copy).
#[derive(Debug, Clone)]
pub struct Repository {
    git_dir: PathBuf,
    common_dir: PathBuf,
    format: ObjectFormat,
    objects: Arc<FileObjectDatabase>,
}

impl PartialEq for Repository {
    fn eq(&self, other: &Self) -> bool {
        self.git_dir == other.git_dir
            && self.common_dir == other.common_dir
            && self.format == other.format
    }
}

impl Eq for Repository {}

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
            return Err(GitError::repository_not_found(format!(
                "not a git repository: {}",
                git_dir.display()
            )));
        }
        Self::from_git_dir(git_dir)
    }

    /// Open a repository with explicit discovery and bare-worktree policy.
    ///
    /// Use `OpenOptions::new().exact_path(true).bare(true)` for scratch bare
    /// directories where silently discovering a parent checkout would be wrong.
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self> {
        let repo = if options.exact_path {
            Self::open(path)
        } else {
            Self::discover(path)
        }?;
        if options.bare && repo.workdir().is_some() {
            return Err(GitError::InvalidFormat(format!(
                "repository is not bare: {}",
                repo.git_dir.display()
            )));
        }
        Ok(repo)
    }

    /// Open exactly `git_dir` as a bare repository, never discovering a parent.
    pub fn open_exact_bare(git_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(git_dir, OpenOptions::new().exact_path(true).bare(true))
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
        let objects = Arc::new(FileObjectDatabase::from_git_dir(&common_dir, format));
        Ok(Self {
            git_dir,
            common_dir,
            format,
            objects,
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

    /// The working-tree root, or `None` for a bare repository.
    ///
    /// Resolution follows git's repository-intrinsic rules: a `core.worktree`
    /// override, then a linked worktree's recorded location, then the parent of
    /// the `.git` directory for an ordinary checkout. It does *not* consult the
    /// process-level `GIT_WORK_TREE`/`GIT_DIR` overrides (those belong to a CLI
    /// front-end, not a library handle). A working tree that is configured but
    /// cannot be resolved on disk (e.g. a `core.worktree` pointing at a missing
    /// path) is reported as `None` rather than erroring.
    pub fn workdir(&self) -> Option<PathBuf> {
        sley_worktree::worktree_root_for_git_dir(&self.git_dir)
            .ok()
            .flatten()
    }

    /// Whether this repository is shallow — created or fetched with a depth
    /// limit, so a `shallow` file records its grafted history boundaries.
    pub fn is_shallow(&self) -> bool {
        sley_worktree::is_shallow_repository(&self.git_dir)
    }

    /// Return short-status entries for this repository's working tree using
    /// sley's default status options.
    ///
    /// Bare repositories have no working tree and return
    /// [`GitError::Unsupported`].
    pub fn short_status(&self) -> Result<Vec<ShortStatusEntry>> {
        self.short_status_with_options(ShortStatusOptions::default())
    }

    /// Stream short-status entries for this repository's working tree using
    /// sley's default status options.
    ///
    /// Bare repositories have no working tree and return
    /// [`GitError::Unsupported`].
    pub fn stream_short_status<F>(&self, emit: F) -> Result<()>
    where
        F: for<'a> FnMut(ShortStatusRow<'a>) -> Result<StreamControl>,
    {
        self.stream_short_status_with_options(ShortStatusOptions::default(), emit)
    }

    /// Return short-status entries for this repository's working tree.
    ///
    /// This facade collects entries from [`sley_worktree::stream_short_status_with_options`].
    pub fn short_status_with_options(
        &self,
        options: ShortStatusOptions,
    ) -> Result<Vec<ShortStatusEntry>> {
        let mut entries = Vec::new();
        self.stream_short_status_with_options(options, |entry| {
            entries.push(entry.to_owned_entry());
            Ok(StreamControl::Continue)
        })?;
        Ok(entries)
    }

    /// Stream short-status entries for this repository's working tree.
    pub fn stream_short_status_with_options<F>(
        &self,
        options: ShortStatusOptions,
        emit: F,
    ) -> Result<()>
    where
        F: for<'a> FnMut(ShortStatusRow<'a>) -> Result<StreamControl>,
    {
        let workdir = self.workdir().ok_or_else(|| {
            GitError::Unsupported("short status requires a repository worktree".into())
        })?;
        sley_worktree::stream_short_status_with_options(
            &workdir,
            &self.git_dir,
            self.format,
            options,
            emit,
        )
    }

    /// Count short-status entries for this repository's working tree.
    pub fn short_status_count_with_options(&self, options: ShortStatusOptions) -> Result<usize> {
        let workdir = self.workdir().ok_or_else(|| {
            GitError::Unsupported("short status requires a repository worktree".into())
        })?;
        sley_worktree::short_status_count_with_options(
            &workdir,
            &self.git_dir,
            self.format,
            options,
        )
    }

    /// Compare one tracked entry to this repository's worktree, using the same
    /// racy-clean/stat-cache rules as [`Repository::short_status_with_options`].
    pub fn worktree_entry_state(
        &self,
        path: impl AsRef<Path>,
        expected_oid: &ObjectId,
        expected_mode: u32,
        index_probe: Option<&IndexStatProbe>,
    ) -> Result<WorktreeEntryState> {
        let workdir = self.workdir().ok_or_else(|| {
            GitError::Unsupported("worktree entry state requires a repository worktree".into())
        })?;
        sley_worktree::worktree_entry_state(
            &workdir,
            &self.git_dir,
            self.format,
            path,
            expected_oid,
            expected_mode,
            index_probe,
        )
    }

    /// The names of the configured remotes (`[remote "<name>"]` sections),
    /// sorted alphabetically with duplicates collapsed — the order `git remote`
    /// lists them in.
    ///
    /// Remotes are read from the *effective* configuration (see
    /// [`Repository::config_snapshot`]), so a remote defined in an included file
    /// is included.
    pub fn remote_names(&self) -> Result<Vec<String>> {
        Ok(sley_config::remotes::remote_names(&self.config_snapshot()?))
    }

    /// The repository's object format (`sha1` or `sha256`), read from
    /// `extensions.objectformat`.
    pub fn object_format(&self) -> ObjectFormat {
        self.format
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
        Ok(config.get(section, subsection, key).map(str::to_owned))
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
            .as_ref()
            .map(FullName::as_str)
            .and_then(|target| target.strip_prefix("refs/heads/"))
            .map(str::to_string)
    }

    /// Resolve `HEAD`: its symbolic branch target (if any) and the commit it
    /// points at (if the branch exists).
    pub fn head(&self) -> Result<Head> {
        let refs = self.references();
        match refs.read_ref("HEAD")? {
            None => Err(GitError::reference_not_found("HEAD is missing")),
            Some(RefTarget::Direct(oid)) => Ok(Head {
                symbolic_target: None,
                oid: Some(oid),
            }),
            Some(RefTarget::Symbolic(name)) => {
                let symbolic_target = FullName::new(&name)?;
                let oid = self.resolve_symbolic(&name)?;
                Ok(Head {
                    symbolic_target: Some(symbolic_target),
                    oid,
                })
            }
        }
    }

    /// Look up a reference by full name (e.g. `refs/heads/main`, `refs/tags/v1`,
    /// or `HEAD`), returning `None` if it does not exist.
    pub fn find_reference(&self, name: &str) -> Result<Option<Reference>> {
        let name = FullName::new(name)?;
        let refs = self.references();
        Ok(refs
            .read_ref(name.as_str())?
            .map(|target| Reference { name, target }))
    }

    /// Return whether `name` exists in the ref backend without resolving its
    /// target or reading the object it names.
    pub fn reference_exists(&self, name: &str) -> Result<bool> {
        self.references().raw_ref_exists(name)
    }

    /// Look up a reference that must exist.
    pub fn require_reference(&self, name: &str) -> Result<Reference> {
        self.find_reference(name)?
            .ok_or_else(|| GitError::reference_not_found(name))
    }

    /// Peel annotated tags until the referenced non-tag object is reached.
    ///
    /// This does not resolve symbolic refs. Use [`Reference::direct_target`] or
    /// [`Repository::head`] first so the call site is explicit about symbolic
    /// reference resolution versus object graph peeling.
    pub fn peel_to_object_oid(&self, oid: ObjectId) -> Result<ObjectId> {
        const MAX_TAG_DEPTH: usize = 1024;
        let mut current = oid;
        for _ in 0..MAX_TAG_DEPTH {
            let object = self.read_object(&current).map_err(|err| {
                expect_missing_object_kind(
                    err,
                    current,
                    MissingObjectKind::Object,
                    MissingObjectContext::Traversal,
                )
            })?;
            if object.object_type != ObjectType::Tag {
                return Ok(current);
            }
            let tag = Tag::parse(self.format, &object.body)?;
            current = tag.object;
        }
        Err(GitError::InvalidObject(format!(
            "annotated tag chain too deep starting at {oid}"
        )))
    }

    /// Peel an object id to the commit it ultimately names.
    pub fn peel_to_commit_oid(&self, oid: ObjectId) -> Result<ObjectId> {
        sley_rev::peel_to_commit(self.objects.as_ref(), self.format, &oid).map_err(|err| {
            expect_missing_object_kind(
                err,
                oid,
                MissingObjectKind::Commit,
                MissingObjectContext::Traversal,
            )
        })
    }

    /// Resolve a revision specification (anything `git rev-parse` accepts:
    /// branch/tag names, abbreviated or full object ids, `HEAD~2`, `<rev>:<path>`,
    /// `@{u}`, etc.) to a concrete [`ObjectId`].
    pub fn rev_parse(&self, spec: &str) -> Result<ObjectId> {
        sley_rev::resolve_revision(&self.git_dir, self.format, spec)
    }

    /// Resolve `<rev>:<path>` to the tree entry it names within `<rev>`'s tree.
    ///
    /// `rev` is peeled to a tree (commit, tag, or tree ids all work) and `path`
    /// is walked component by component. An empty `path` resolves to the tree
    /// itself.
    pub fn resolve_path(&self, rev: &str, path: &str) -> Result<ResolvedTreePath> {
        sley_rev::resolve_rev_path_entry(
            &self.git_dir,
            self.format,
            self.objects.as_ref(),
            rev,
            path,
        )
    }

    /// Write an annotated tag object, returning its id.
    ///
    /// This creates only the tag *object*; updating `refs/tags/<name>` is the
    /// caller's responsibility (see [`Repository::apply_ref_changes`]).
    pub fn write_annotated_tag(&self, tag: TagCreate) -> Result<ObjectId> {
        let mut objects = self.objects_mut();
        create_annotated_tag(&mut objects, tag)
    }

    /// Copy objects reachable from `roots` out of `other` into this repository.
    ///
    /// Uses a pack-based transfer ([`sley_odb::build_reachable_pack`] on the
    /// source, [`sley_odb::install_raw_pack`] on the destination) for
    /// performance. Semantics:
    ///
    /// * Only *objects* are copied; refs in `other` are not updated here.
    /// * The transitive closure of each root is included (commits bring in
    ///   their trees, blobs, tags, and parent commits).
    /// * Objects already present in this repository are skipped by the pack
    ///   installer (ids are unchanged).
    /// * Both repositories must use the same [`ObjectFormat`]; mismatches error.
    /// * When nothing new is reachable, this is a no-op (`Ok(())`).
    pub fn copy_reachable_from(&self, other: &Repository, roots: &[ObjectId]) -> Result<()> {
        if self.format != other.format {
            return Err(GitError::InvalidObjectId(format!(
                "object format mismatch: destination uses {}, source uses {}",
                self.format.name(),
                other.format.name()
            )));
        }
        install_reachable_pack(
            other.objects().as_ref(),
            self.objects().as_ref(),
            self.format,
            roots.iter().copied(),
        )?;
        self.refresh_objects();
        Ok(())
    }

    /// Read a raw object (any type) from the object database.
    pub fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        ObjectReader::read_object(self.objects.as_ref(), oid)
    }

    /// Read a commit object, parsing it into a [`Commit`]. Returns an error if
    /// `oid` does not name a commit.
    pub fn read_commit(&self, oid: &ObjectId) -> Result<Commit> {
        let object = self.read_object(oid).map_err(|err| {
            expect_missing_object_kind(
                err,
                *oid,
                MissingObjectKind::Commit,
                MissingObjectContext::Read,
            )
        })?;
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
        let object = self.read_object(oid).map_err(|err| {
            expect_missing_object_kind(
                err,
                *oid,
                MissingObjectKind::Tree,
                MissingObjectContext::Read,
            )
        })?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::InvalidObject(format!(
                "object {oid} is a {}, not a tree",
                object.object_type.as_str()
            )));
        }
        Tree::parse(self.format, &object.body)
    }

    /// Read an annotated tag object, parsing it into a [`Tag`]. Returns an error
    /// if `oid` does not name a tag.
    pub fn read_tag(&self, oid: &ObjectId) -> Result<Tag> {
        let object = self.read_object(oid).map_err(|err| {
            expect_missing_object_kind(
                err,
                *oid,
                MissingObjectKind::Tag,
                MissingObjectContext::Read,
            )
        })?;
        if object.object_type != ObjectType::Tag {
            return Err(GitError::InvalidObject(format!(
                "object {oid} is a {}, not a tag",
                object.object_type.as_str()
            )));
        }
        Tag::parse(self.format, &object.body)
    }

    /// Read a commit and return the typed [`Signature`] parse-view of its
    /// `author` line, or `None` if the stored ident is malformed.
    ///
    /// Convenience for `repo.read_commit(oid)?.author_signature()`: the raw
    /// `author` bytes are unchanged, and the returned signature re-serializes
    /// byte-identically to them (see [`Signature::to_ident_bytes`]).
    pub fn read_commit_author(&self, oid: &ObjectId) -> Result<Option<Signature>> {
        Ok(self.read_commit(oid)?.author_signature())
    }

    /// Read a commit and return the typed [`Signature`] parse-view of its
    /// `committer` line, or `None` if the stored ident is malformed. The raw
    /// bytes are untouched; see [`Repository::read_commit_author`].
    pub fn read_commit_committer(&self, oid: &ObjectId) -> Result<Option<Signature>> {
        Ok(self.read_commit(oid)?.committer_signature())
    }

    /// Read an annotated tag and return the typed [`Signature`] parse-view of
    /// its `tagger` line, or `None` if the tag has no tagger or the stored ident
    /// is malformed. The raw bytes are untouched; see
    /// [`Repository::read_commit_author`].
    pub fn read_tag_tagger(&self, oid: &ObjectId) -> Result<Option<Signature>> {
        Ok(self.read_tag(oid)?.tagger_signature())
    }

    /// Write a raw object (any type) to the object database as a loose object,
    /// returning its id. The bytes are stored verbatim, so writing an object
    /// that originated from another repository preserves its id exactly.
    pub fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        let odb = self.objects_mut();
        odb.write_object(object)
    }

    /// Write `body` as a raw object of `object_type`, preserving the bytes
    /// exactly and returning the resulting object id.
    pub fn write_raw_object(
        &self,
        object_type: ObjectType,
        body: impl Into<Vec<u8>>,
    ) -> Result<ObjectId> {
        self.write_object(EncodedObject::new(object_type, body))
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
        sley_worktree::index_from_tree(self.objects.as_ref(), self.format, tree_oid)
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

fn expect_missing_object_kind(
    err: GitError,
    oid: ObjectId,
    expected: MissingObjectKind,
    context: MissingObjectContext,
) -> GitError {
    match err.not_found_kind() {
        Some(NotFoundKind::Object { .. }) => {
            GitError::object_kind_not_found_in(oid, expected, context)
        }
        _ => err,
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
    Err(GitError::repository_not_found(format!(
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
        let db = repo.objects_mut();

        let blob_oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .expect("write blob");

        let tree = Tree {
            entries: vec![sley_object::TreeEntry {
                mode: 0o100644,
                name: BString::from(b"hello.txt"),
                oid: blob_oid,
            }],
        };
        let tree_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("write tree");

        let commit = Commit {
            tree: tree_oid,
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
            commit_oid,
            b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            b"commit (initial): initial".to_vec(),
        )
        .expect("create main branch");

        commit_oid
    }

    fn seed_empty_tree_commit(repo: &Repository) -> ObjectId {
        let db = repo.objects_mut();
        let commit = Commit {
            tree: ObjectId::empty_tree(repo.object_format()),
            parents: Vec::new(),
            author: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            committer: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            encoding: None,
            message: b"empty tree\n".to_vec(),
        };
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .expect("write empty tree commit")
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
    fn open_exact_bare_never_discovers_parent_repo() {
        let temp = TempDir::new();
        Repository::init(temp.path()).expect("init parent");
        let scratch = temp.path().join("nested").join("scratch.git");
        fs::create_dir_all(&scratch).expect("create scratch path");

        Repository::open_exact_bare(&scratch).expect_err("exact open must not discover parent");
        let discovered = Repository::discover(&scratch).expect("discover parent repo");
        assert_eq!(discovered.git_dir(), temp.path().join(".git"));

        let bare = TempDir::new();
        let bare_repo = Repository::init_bare(bare.path()).expect("init bare");
        let exact = Repository::open_exact_bare(bare.path()).expect("open exact bare");
        assert_eq!(exact.git_dir(), bare_repo.git_dir());
    }

    #[test]
    fn head_is_unborn_after_init() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let head = repo.head().expect("head");
        assert_eq!(
            head.symbolic_target.as_ref().map(FullName::as_str),
            Some("refs/heads/main")
        );
        assert_eq!(head.oid, None);
        assert!(head.is_unborn());
        assert!(!head.is_detached());
        assert_eq!(head.branch_name(), Some("main"));
    }

    #[test]
    fn reference_helpers_keep_direct_tag_target_separate_from_peeling() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);
        let tag = Tag {
            object: commit_oid,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Tester <test@example.com> 1700000001 +0000".to_vec()),
            message: b"release\n".to_vec(),
            raw_body: None,
        };
        let tag_oid = repo
            .write_object(EncodedObject::new(ObjectType::Tag, tag.write()))
            .expect("write tag");
        repo.apply_ref_changes(&[
            RefChange::new("refs/tags/v1.0", RefTarget::Direct(tag_oid)).expect("valid tag ref")
        ])
        .expect("write tag ref");

        assert!(
            repo.reference_exists("refs/tags/v1.0")
                .expect("exists check")
        );
        let tag_ref = repo
            .require_reference("refs/tags/v1.0")
            .expect("require tag ref");
        assert_eq!(tag_ref.direct_target().expect("direct target"), tag_oid);
        assert_eq!(
            repo.peel_to_object_oid(tag_oid).expect("peel object"),
            commit_oid
        );
        assert_eq!(
            repo.peel_to_commit_oid(tag_oid).expect("peel commit"),
            commit_oid
        );
    }

    #[test]
    fn blob_boundary_and_status_plan_are_embedder_facing_facades() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let blob_oid = repo.write_blob(b"payload").expect("write blob");

        let bytes = repo
            .blobs()
            .read_or_fetch_blocking(blob_oid, BlobFetchOptions::from_remote("origin"))
            .expect("read local blob");
        assert_eq!(bytes, b"payload");

        let missing = ObjectId::null(repo.object_format());
        let err = repo
            .blobs()
            .read_or_fetch_blocking(missing, BlobFetchOptions::from_remote("origin"))
            .expect_err("missing blob");
        match err.not_found_kind() {
            Some(NotFoundKind::Object { oid, kind, context }) => {
                assert_eq!(*oid, missing);
                assert_eq!(*kind, MissingObjectKind::Blob);
                assert_eq!(*context, Some(MissingObjectContext::RemoteBoundary));
            }
            other => panic!("expected typed missing blob, got {other:?}"),
        }

        let status = repo
            .status_plan()
            .include_untracked(false)
            .reuse_index_cache("health")
            .build()
            .expect("status plan");
        assert_eq!(
            status.cache_key().map(StatusCacheKey::as_str),
            Some("health")
        );
        assert!(status.collect().expect("collect status").is_empty());
    }

    #[test]
    fn head_resolves_after_commit() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);

        let head = repo.head().expect("head");
        assert_eq!(
            head.symbolic_target.as_ref().map(FullName::as_str),
            Some("refs/heads/main")
        );
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
        let blob = repo.read_object(&tree.entries[0].oid).expect("read blob");
        assert_eq!(blob.object_type, ObjectType::Blob);
        assert_eq!(blob.body, b"hello\n");
    }

    #[test]
    fn read_tree_accepts_implied_empty_tree_without_stored_object() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let empty = ObjectId::empty_tree(repo.object_format());

        let object = repo.read_object(&empty).expect("read implied empty tree");
        assert_eq!(object.object_type, ObjectType::Tree);
        assert!(object.body.is_empty());

        let tree = repo.read_tree(&empty).expect("parse implied empty tree");
        assert!(tree.entries.is_empty());
    }

    #[test]
    fn missing_object_errors_expose_oid_and_expected_kind() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let missing = ObjectId::from_hex(
            repo.object_format(),
            "1111111111111111111111111111111111111111",
        )
        .expect("valid oid");

        let raw_err = repo.read_object(&missing).expect_err("raw missing object");
        let raw_kind = raw_err.not_found_kind().expect("typed not found");
        assert_eq!(raw_kind.object_id(), Some(missing));
        assert_eq!(
            raw_kind.missing_object_kind(),
            Some(MissingObjectKind::Object)
        );
        assert_eq!(
            raw_kind.missing_object_context(),
            Some(MissingObjectContext::Read)
        );

        let commit_err = repo
            .read_commit(&missing)
            .expect_err("typed missing commit");
        let commit_kind = commit_err.not_found_kind().expect("typed not found");
        assert_eq!(commit_kind.object_id(), Some(missing));
        assert_eq!(
            commit_kind.missing_object_kind(),
            Some(MissingObjectKind::Commit)
        );
        assert_eq!(
            commit_kind.missing_object_context(),
            Some(MissingObjectContext::Read)
        );
    }

    #[test]
    fn read_commit_accepts_encoded_non_utf8_commit() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let tree = ObjectId::empty_tree(repo.object_format());
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author J\xF6rg <j@example.invalid> 0 +0000\n");
        body.extend_from_slice(b"committer M\xFCller <m@example.invalid> 1 +0000\n");
        body.extend_from_slice(b"encoding ISO-8859-1\n\ncaf\xE9\n");
        let oid = repo
            .write_raw_object(ObjectType::Commit, body)
            .expect("write raw commit");

        let commit = repo.read_commit(&oid).expect("read non-utf8 commit");
        assert_eq!(commit.author, b"J\xF6rg <j@example.invalid> 0 +0000");
        assert_eq!(commit.committer, b"M\xFCller <m@example.invalid> 1 +0000");
        assert_eq!(commit.encoding.as_deref(), Some(&b"ISO-8859-1"[..]));
        assert_eq!(commit.message, b"caf\xE9\n");
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
        assert_eq!(branch.target, RefTarget::Direct(commit_oid));
        assert_eq!(branch.peeled_oid(&repo).expect("peel"), Some(commit_oid));

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
        let err =
            Repository::discover(temp.path()).expect_err("discovering outside any repo must fail");
        assert!(matches!(err, GitError::NotFound(_)));
    }

    #[test]
    fn open_rejects_non_git_directory() {
        let temp = TempDir::new();
        let err = Repository::open(temp.path()).expect_err("opening a non-git directory must fail");
        assert!(matches!(err, GitError::NotFound(_)));
    }

    #[test]
    fn config_round_trips_and_reports_format() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let config = repo.config().expect("config");
        // init writes core.repositoryformatversion and core.bare.
        assert_eq!(config.get("core", None, "bare"), Some("false"));
        assert_eq!(
            config.repository_object_format().expect("format"),
            ObjectFormat::Sha1
        );
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
        assert_eq!(
            repo.config_string("user", "missing").expect("missing"),
            None
        );

        // Subsections are honoured.
        assert_eq!(
            repo.config_string_subsection("remote", Some("origin"), "url")
                .expect("url"),
            Some("https://example.invalid/x.git".to_string())
        );
    }

    #[test]
    fn workdir_is_parent_for_non_bare_repo() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        // The non-bare layout's working tree is the parent of `.git`, returned
        // verbatim (so it matches the path init was handed).
        assert_eq!(repo.workdir(), Some(temp.path().to_path_buf()));
    }

    #[test]
    fn workdir_is_none_for_bare_repo() {
        let temp = TempDir::new();
        let repo = Repository::init_bare(temp.path()).expect("init bare");
        assert_eq!(repo.workdir(), None);
    }

    #[test]
    fn workdir_honours_core_worktree_override() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        // Point core.worktree at a real sibling directory.
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("create worktree dir");
        let config_path = repo.git_dir().join("config");
        let mut contents = fs::read(&config_path).expect("read config");
        contents.extend_from_slice(
            format!("[core]\n\tworktree = {}\n", elsewhere.display()).as_bytes(),
        );
        fs::write(&config_path, contents).expect("write config");

        // The override wins and is canonicalised.
        assert_eq!(
            repo.workdir(),
            Some(fs::canonicalize(&elsewhere).expect("canon worktree"))
        );
    }

    #[test]
    fn is_shallow_tracks_shallow_file() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        assert!(!repo.is_shallow());
        fs::write(repo.git_dir().join("shallow"), b"").expect("write shallow");
        assert!(repo.is_shallow());
    }

    #[test]
    fn remote_names_lists_configured_remotes_sorted() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        assert_eq!(repo.remote_names().expect("names"), Vec::<String>::new());

        let config_path = repo.common_dir().join("config");
        let mut contents = fs::read(&config_path).expect("read config");
        // Define remotes out of alphabetical order and with a duplicate section.
        contents.extend_from_slice(
            b"[remote \"upstream\"]\n\turl = https://example.invalid/up.git\n\
              [remote \"origin\"]\n\turl = https://example.invalid/o.git\n\
              [remote \"origin\"]\n\tpushurl = https://example.invalid/o-push.git\n",
        );
        fs::write(&config_path, contents).expect("write config");

        assert_eq!(
            repo.remote_names().expect("names"),
            vec!["origin".to_string(), "upstream".to_string()]
        );
    }

    #[test]
    fn plumbing_reexports_are_reachable() {
        // Smoke test that the re-exports compile and resolve to the right types.
        let _format: plumbing::sley_core::ObjectFormat = ObjectFormat::Sha1;
        let _: fn(&[u8]) -> Result<plumbing::sley_config::GitConfig> =
            plumbing::sley_config::GitConfig::parse;
        let _: plumbing::sley_diff_merge::DiffNameStatusOptions =
            plumbing::sley_diff_merge::DiffNameStatusOptions::default();
        let _: fn(&mut plumbing::sley_odb::FileObjectDatabase, TagCreate) -> Result<ObjectId> =
            plumbing::sley_sequencer::create_annotated_tag;
    }

    #[test]
    fn capabilities_reflect_repo_state() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let caps = repo.capabilities();
        assert!(caps.annotated_tags);
        assert!(caps.config_includes);
        assert!(caps.hasconfig_include_if);
        assert!(caps.notes);
        assert!(caps.index);
        assert!(!caps.shallow);
        assert!(!caps.sha256);

        fs::write(repo.git_dir().join("shallow"), b"").expect("shallow");
        assert!(repo.capabilities().shallow);
    }

    #[test]
    fn resolve_path_finds_blob_in_commit() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        seed_commit(&repo);

        let entry = repo
            .resolve_path("HEAD", "hello.txt")
            .expect("resolve path");
        assert_eq!(entry.name, b"hello.txt");
        assert_eq!(entry.object_type, ObjectType::Blob);
        assert!(entry.mode.is_some());
    }

    #[test]
    fn remote_edit_round_trip() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");

        repo.add_remote("origin", "https://example.invalid/o.git")
            .expect("add");
        assert_eq!(
            repo.remote_names().expect("names"),
            vec!["origin".to_string()]
        );
        assert_eq!(
            repo.config_string_subsection("remote", Some("origin"), "url")
                .expect("url"),
            Some("https://example.invalid/o.git".to_string())
        );

        repo.set_remote_url("origin", "https://example.invalid/n.git")
            .expect("set url");
        assert_eq!(
            repo.config_string_subsection("remote", Some("origin"), "url")
                .expect("url"),
            Some("https://example.invalid/n.git".to_string())
        );

        repo.remove_remote("origin").expect("remove");
        assert!(repo.remote_names().expect("names").is_empty());
    }

    #[test]
    fn init_mirror_writes_origin_fetch_and_mirror() {
        let temp = TempDir::new();
        let repo = Repository::init_mirror(temp.path()).expect("init mirror");
        assert_eq!(repo.workdir(), None);
        let config = repo.load_repo_config().expect("config");
        assert_eq!(
            config.get("remote", Some("origin"), "fetch"),
            Some("+refs/*:refs/*")
        );
        assert_eq!(config.get("remote", Some("origin"), "mirror"), Some("true"));
    }

    #[test]
    fn copy_reachable_from_transfers_missing_objects() {
        let source_dir = TempDir::new();
        let dest_dir = TempDir::new();
        let source = Repository::init(source_dir.path()).expect("source");
        let dest = Repository::init(dest_dir.path()).expect("dest");
        let commit_oid = seed_commit(&source);

        dest.copy_reachable_from(&source, std::slice::from_ref(&commit_oid))
            .expect("copy");

        let copied = dest.read_commit(&commit_oid).expect("read copied commit");
        let original = source.read_commit(&commit_oid).expect("read source commit");
        assert_eq!(copied.tree, original.tree);
        assert_eq!(copied.message, original.message);
    }

    #[test]
    fn copy_reachable_from_accepts_implied_empty_tree() {
        let source_dir = TempDir::new();
        let dest_dir = TempDir::new();
        let source = Repository::init(source_dir.path()).expect("source");
        let dest = Repository::init(dest_dir.path()).expect("dest");
        let commit_oid = seed_empty_tree_commit(&source);

        dest.copy_reachable_from(&source, std::slice::from_ref(&commit_oid))
            .expect("copy empty-tree commit");

        let copied = dest.read_commit(&commit_oid).expect("read copied commit");
        assert_eq!(copied.tree, ObjectId::empty_tree(dest.object_format()));
        assert!(
            dest.read_tree(&copied.tree)
                .expect("read tree")
                .entries
                .is_empty()
        );
    }

    #[test]
    fn write_annotated_tag_round_trips() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);

        let tag_oid = repo
            .write_annotated_tag(TagCreate {
                object: commit_oid,
                object_type: ObjectType::Commit,
                name: b"v1".to_vec(),
                tagger: b"Tagger <t@e.com> 1 +0000".to_vec(),
                message: b"release\n".to_vec(),
            })
            .expect("tag");
        let tag = repo.read_tag(&tag_oid).expect("read tag");
        assert_eq!(tag.name, b"v1");
        assert_eq!(tag.object, commit_oid);
    }

    #[test]
    fn diff_name_status_reports_added_file() {
        let temp = TempDir::new();
        let repo = Repository::init(temp.path()).expect("init");
        let commit_oid = seed_commit(&repo);
        let base_tree = repo.read_commit(&commit_oid).expect("commit").tree;

        let mut editor = repo.edit_tree(&base_tree).expect("edit");
        let blob_oid = repo.write_blob(b"new\n").expect("blob");
        editor.upsert("added.txt", sley_object::EntryKind::Blob, blob_oid);
        let new_tree = repo.write_tree(editor).expect("tree");

        let changes = repo.diff_name_status(&base_tree, &new_tree).expect("diff");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, b"added.txt");
    }
}
