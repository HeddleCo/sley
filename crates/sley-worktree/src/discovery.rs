//! Process-environment repository discovery (`GIT_DIR`, `GIT_WORK_TREE`,
//! `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`).
//!
//! Shared by the embeddable facade's `open_env` setup path and the hook engine
//! (`sley-hooks`), which must resolve a git directory from the current working
//! directory with git's environment overrides when callers do not supply one.

//! Consolidated repository-discovery cluster:
//!
//! * this module — env-var helpers, walk-up discovery, ceiling/filesystem
//!   boundaries shared by every consumer below;
//! * [`setup`] — the faithful `setup_git_directory_gently` port plus the
//!   invocation worktree-policy quartet;
//! * [`ownership`] — `safe.directory` / `safe.bareRepository` enforcement;
//! * [`probes`] — gitfile classification diagnostics and remote local-path
//!   walk-up resolution.

pub mod ownership;
pub mod probes;
pub mod setup;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sley_core::{GitError, Result};

/// How a repository path should be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryDiscoveryMode {
    /// Search `start` and its ancestors for a worktree or bare repository.
    Ancestors,
    /// Require `start` itself to be a git directory or gitfile.
    Exact,
    /// Resolve a local remote using git's `path`, `path/.git`, and `path.git`
    /// candidate forms without walking into an unrelated parent repository.
    LocalRemote,
}

/// Repository-safety checks applied after a candidate has been validated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepositoryDiscoverySafety {
    /// Enforce protected-config `safe.directory` ownership checks.
    pub safe_directory: bool,
    /// Enforce protected-config `safe.bareRepository` for implicit bare repos.
    pub safe_bare_repository: bool,
}

/// Controls canonical repository discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryDiscoveryOptions {
    /// Whether to resolve one path, walk ancestors, or apply local-remote forms.
    pub mode: RepositoryDiscoveryMode,
    /// Permit an ancestor walk to cross the starting path's filesystem.
    pub across_filesystem: bool,
    /// Honor `GIT_CEILING_DIRECTORIES` during an ancestor walk.
    pub respect_ceiling_directories: bool,
    /// Optional protected-config checks to apply to the validated result.
    pub safety: RepositoryDiscoverySafety,
    /// Report malformed `.git` entries as fatal errors instead of continuing.
    pub strict_gitfile_errors: bool,
}

impl RepositoryDiscoveryOptions {
    /// Intrinsic upward discovery, bounded to the starting filesystem.
    pub const fn ancestors() -> Self {
        Self {
            mode: RepositoryDiscoveryMode::Ancestors,
            across_filesystem: false,
            respect_ceiling_directories: false,
            safety: RepositoryDiscoverySafety {
                safe_directory: false,
                safe_bare_repository: false,
            },
            strict_gitfile_errors: false,
        }
    }

    /// Exact-path validation without parent discovery.
    pub const fn exact() -> Self {
        Self {
            mode: RepositoryDiscoveryMode::Exact,
            ..Self::ancestors()
        }
    }

    /// Local-remote candidate resolution without parent discovery.
    pub const fn local_remote() -> Self {
        Self {
            mode: RepositoryDiscoveryMode::LocalRemote,
            ..Self::ancestors()
        }
    }
}

/// A validated repository location together with the provenance needed by
/// ownership policy and callers that distinguish linked worktrees from bare
/// repositories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepository {
    git_dir: PathBuf,
    common_dir: PathBuf,
    worktree: Option<PathBuf>,
    gitfile: Option<PathBuf>,
    bare: bool,
}

impl DiscoveredRepository {
    /// The validated per-worktree git administration directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Consume the result and return its git administration directory.
    pub fn into_git_dir(self) -> PathBuf {
        self.git_dir
    }

    /// The validated shared directory, following `commondir` when present.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The resolved worktree root, when discovery needed to identify one.
    ///
    /// Metadata-only discovery, such as validating a direct local-remote git
    /// directory, leaves this unset even when [`Self::is_bare`] is false.
    pub fn worktree(&self) -> Option<&Path> {
        self.worktree.as_deref()
    }

    /// The `.git` gitfile used to locate `git_dir`, when applicable.
    pub fn gitfile(&self) -> Option<&Path> {
        self.gitfile.as_deref()
    }

    /// Whether the repository is bare according to its config and layout.
    pub fn is_bare(&self) -> bool {
        self.bare
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    Worktree,
    Bare,
    Exact,
    RemoteGitDir,
}

/// Discover and validate a repository without consulting `GIT_DIR` or
/// `GIT_WORK_TREE`.
///
/// All intrinsic path forms share this engine: `.git` directories, gitfiles,
/// linked-worktree administration directories (including `commondir`), and
/// standalone bare repositories. Environment-aware invocation setup remains in
/// [`setup`], which deliberately has different precedence rules.
pub fn discover_repository(
    start: impl AsRef<Path>,
    options: RepositoryDiscoveryOptions,
) -> Result<DiscoveredRepository> {
    discover_repository_with_device(start.as_ref(), options, device_of)
}

/// Validate one explicit git-directory path without inferring its worktree.
///
/// Invocation setup applies `GIT_WORK_TREE` and other worktree policy after it
/// resolves `GIT_DIR`. Keeping this operation path-only prevents a stale or
/// intentionally overridden `core.worktree` from rejecting an otherwise valid
/// explicit git directory before that policy can run. Gitfiles are followed
/// relative to the file which contains them.
pub fn resolve_exact_git_dir(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.is_file()
        && let Some(target) = read_gitdir_link(path)?
        && is_git_dir(&target)
    {
        return Ok(target);
    }
    if is_git_dir(path) {
        return Ok(path.to_path_buf());
    }
    Err(not_a_repository(path))
}

/// Device-injected form used to test filesystem-boundary behavior without a
/// privileged mount fixture.
fn discover_repository_with_device(
    start: &Path,
    options: RepositoryDiscoveryOptions,
    device_of: impl Fn(&Path) -> Option<u64>,
) -> Result<DiscoveredRepository> {
    match options.mode {
        RepositoryDiscoveryMode::Ancestors => {
            discover_ancestors_with_device(start, options, device_of)
        }
        RepositoryDiscoveryMode::Exact => {
            let found = resolve_candidate(start, CandidateKind::Exact, false, false)?
                .ok_or_else(|| not_a_repository(start))?;
            apply_discovery_safety(found, options.safety)
        }
        RepositoryDiscoveryMode::LocalRemote => {
            let dot_git_suffix = path_with_dot_git_suffix(start);
            let candidates = [
                (start.join(".git"), CandidateKind::Worktree),
                (start.to_path_buf(), CandidateKind::RemoteGitDir),
                (dot_git_suffix.join(".git"), CandidateKind::Worktree),
                (dot_git_suffix, CandidateKind::RemoteGitDir),
            ];
            for (candidate, kind) in candidates {
                if let Some(found) = resolve_candidate(&candidate, kind, true, false)? {
                    return apply_discovery_safety(found, options.safety);
                }
            }
            Err(not_a_repository(start))
        }
    }
}

fn discover_ancestors_with_device(
    start: &Path,
    options: RepositoryDiscoveryOptions,
    device_of: impl Fn(&Path) -> Option<u64>,
) -> Result<DiscoveredRepository> {
    let start = if start.as_os_str().is_empty() {
        Path::new(".")
    } else {
        start
    };
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|err| GitError::Io(err.to_string()))?
            .join(start)
    };
    let start_device = (!options.across_filesystem)
        .then(|| device_of(&absolute))
        .flatten();
    let ceilings = options
        .respect_ceiling_directories
        .then(discovery_ceiling_directories)
        .unwrap_or_default();

    for candidate in absolute.ancestors() {
        if candidate != absolute.as_path()
            && ceilings
                .iter()
                .any(|ceiling| ceiling.matches_discovery_candidate(candidate))
        {
            break;
        }
        if let Some(found) = resolve_candidate(
            &candidate.join(".git"),
            CandidateKind::Worktree,
            false,
            options.strict_gitfile_errors,
        )? {
            return apply_discovery_safety(found, options.safety);
        }
        if let Some(found) = resolve_candidate(candidate, CandidateKind::Bare, false, false)? {
            return apply_discovery_safety(found, options.safety);
        }
        if let (Some(start_device), Some(parent)) = (start_device, candidate.parent())
            && device_of(parent).is_some_and(|device| device != start_device)
        {
            break;
        }
    }
    Err(GitError::repository_not_found(format!(
        "not a git repository (or any parent up to {}): {}",
        absolute.display(),
        start.display()
    )))
}

fn resolve_candidate(
    path: &Path,
    kind: CandidateKind,
    canonicalize_gitfile: bool,
    strict_gitfile_errors: bool,
) -> Result<Option<DiscoveredRepository>> {
    let (git_dir, gitfile) = if strict_gitfile_errors {
        match probes::probe_dot_git(path)? {
            probes::DotGitProbe::Repo {
                git_dir,
                via_gitfile,
            } => (git_dir, via_gitfile.then(|| path.to_path_buf())),
            probes::DotGitProbe::Continue => return Ok(None),
        }
    } else if kind != CandidateKind::Bare && path.is_file() {
        let Some(target) = read_gitdir_link(path)? else {
            return Ok(None);
        };
        if !is_git_dir(&target) {
            return Ok(None);
        }
        let target = if canonicalize_gitfile {
            fs::canonicalize(target).map_err(|err| GitError::Io(err.to_string()))?
        } else {
            target
        };
        (target, Some(path.to_path_buf()))
    } else if is_git_dir(path) {
        (path.to_path_buf(), None)
    } else {
        return Ok(None);
    };
    let common_dir = sley_formats::repository_common_dir(&git_dir, false)?;
    let (worktree, bare) = match kind {
        CandidateKind::Worktree => (path.parent().map(Path::to_path_buf), false),
        CandidateKind::Bare | CandidateKind::Exact => {
            let worktree = if let Some(gitfile) = &gitfile {
                gitfile.parent().map(Path::to_path_buf)
            } else {
                crate::worktree_root_for_git_dir(&git_dir)?
            };
            let bare = worktree.is_none();
            (worktree, bare)
        }
        // A local remote needs only repository metadata. In particular, a
        // direct git-directory candidate must not be rejected because its
        // checkout path is stale or intentionally unavailable.
        CandidateKind::RemoteGitDir => (None, git_dir_is_bare_without_worktree(&git_dir)),
    };
    Ok(Some(DiscoveredRepository {
        git_dir,
        common_dir,
        worktree,
        gitfile,
        bare,
    }))
}

fn git_dir_is_bare_without_worktree(git_dir: &Path) -> bool {
    if git_dir.join("commondir").is_file() {
        return false;
    }
    if let Ok(config) = sley_config::read_repo_config(git_dir, None)
        && let Some(bare) = config.get_bool("core", None, "bare")
    {
        return bare;
    }
    git_dir.file_name().and_then(|name| name.to_str()) != Some(".git")
}

fn apply_discovery_safety(
    found: DiscoveredRepository,
    safety: RepositoryDiscoverySafety,
) -> Result<DiscoveredRepository> {
    if safety.safe_bare_repository
        && found.is_bare()
        && !ownership::is_implicit_bare_repo(&found.git_dir)
    {
        ownership::note_implicit_bare_repository(&found.git_dir)?;
    }
    if safety.safe_directory {
        ownership::ensure_valid_ownership(
            found.worktree.as_deref(),
            &found.git_dir,
            found.gitfile.as_deref(),
        )?;
    }
    Ok(found)
}

fn path_with_dot_git_suffix(path: &Path) -> PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push(".git");
    PathBuf::from(suffixed)
}

fn not_a_repository(path: &Path) -> GitError {
    GitError::repository_not_found(format!("not a git repository: {}", path.display()))
}

/// `GIT_DIR` when set in the process environment.
pub fn environment_git_dir() -> Option<PathBuf> {
    env::var_os("GIT_DIR").map(PathBuf::from)
}

/// `GIT_WORK_TREE` when set in the process environment.
pub fn environment_work_tree() -> Option<PathBuf> {
    env::var_os("GIT_WORK_TREE").map(PathBuf::from)
}

/// Resolve `value` relative to `cwd` when it is not absolute.
pub fn resolve_path_from_cwd(cwd: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        cwd.join(value)
    }
}

/// Walk upward from `start`, honoring `GIT_DIR` when set and discovery ceilings.
pub fn discover_git_dir_respecting_environment(start: &Path) -> Result<PathBuf> {
    if let Some(git_dir) = environment_git_dir() {
        if git_dir.as_os_str().is_empty() {
            return Err(GitError::repository_not_found("not a git repository"));
        }
        return resolve_explicit_git_dir(start, &git_dir);
    }
    discover_git_dir_with_ceilings(start)
}

/// Resolve an explicitly provided `GIT_DIR` (already known to be non-empty)
/// against `start`, following a `gitdir:` gitlink file when applicable.
pub fn resolve_explicit_git_dir(start: &Path, git_dir: &Path) -> Result<PathBuf> {
    let resolved = resolve_path_from_cwd(start, git_dir);
    if resolved.is_file()
        && let Some(target) = read_gitdir_link(&resolved)?
        && is_git_dir(&target)
    {
        return fs::canonicalize(target).map_err(|err| GitError::Io(err.to_string()));
    }
    Ok(resolved)
}

fn discover_git_dir_with_ceilings(start: &Path) -> Result<PathBuf> {
    let mut options = RepositoryDiscoveryOptions::ancestors();
    options.across_filesystem = git_env_bool("GIT_DISCOVERY_ACROSS_FILESYSTEM");
    options.respect_ceiling_directories = true;
    discover_repository(start, options).map(DiscoveredRepository::into_git_dir)
}

/// One `GIT_CEILING_DIRECTORIES` entry after git's `canonicalize_ceiling_entry`
/// processing (empty entries disable realpath for subsequent ceilings).
struct CeilingDirectory {
    path: PathBuf,
    resolved: bool,
}

impl CeilingDirectory {
    fn matches_discovery_candidate(&self, candidate: &Path) -> bool {
        let ceiling = strip_trailing_slashes(&self.path);
        let candidate_raw = strip_trailing_slashes(candidate);
        if ceiling.as_os_str() == candidate_raw.as_os_str() {
            return true;
        }
        if !self.resolved {
            return false;
        }
        match fs::canonicalize(candidate) {
            Ok(canonical) => strip_trailing_slashes(&canonical).as_os_str() == ceiling.as_os_str(),
            Err(_) => false,
        }
    }
}

fn discovery_ceiling_directories() -> Vec<CeilingDirectory> {
    let Ok(value) = env::var("GIT_CEILING_DIRECTORIES") else {
        return Vec::new();
    };
    if value.is_empty() {
        return Vec::new();
    }
    let mut empty_entry_found = false;
    let mut out = Vec::new();
    for entry in value.split(':') {
        if entry.is_empty() {
            empty_entry_found = true;
            continue;
        }
        let path = Path::new(entry);
        if !path.is_absolute() {
            continue;
        }
        if empty_entry_found {
            out.push(CeilingDirectory {
                path: strip_trailing_slashes(path),
                resolved: false,
            });
            continue;
        }
        if let Ok(canonical) = fs::canonicalize(path) {
            out.push(CeilingDirectory {
                path: strip_trailing_slashes(&canonical),
                resolved: true,
            });
        }
    }
    out
}

fn strip_trailing_slashes(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
    } else {
        PathBuf::from(trimmed)
    }
}

fn git_env_bool(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

fn device_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|metadata| metadata.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// True if `path` looks like a git directory (has a `HEAD` file and either an
/// `objects` directory or a `commondir` pointer).
pub fn is_git_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path.join("HEAD"))
        .is_ok_and(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

/// Read a `gitdir: <path>` link file (used by linked worktrees and submodules),
/// returning the absolute target path it points at.
pub fn read_gitdir_link(path: &Path) -> Result<Option<PathBuf>> {
    let contents = fs::read_to_string(path)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_git_dir(path: &Path) {
        fs::create_dir_all(path.join("objects")).expect("create objects");
        fs::write(path.join("HEAD"), b"ref: refs/heads/main\n").expect("write HEAD");
    }

    #[test]
    fn discovers_worktree_and_reports_validated_layout() {
        let temp = TempDir::new().expect("tempdir");
        let git_dir = temp.path().join(".git");
        create_git_dir(&git_dir);
        let nested = temp.path().join("a/b");
        fs::create_dir_all(&nested).expect("nested");

        let found = discover_repository(&nested, RepositoryDiscoveryOptions::ancestors())
            .expect("discover worktree");

        assert_eq!(found.git_dir(), git_dir);
        assert_eq!(found.common_dir(), git_dir);
        assert_eq!(found.worktree(), Some(temp.path()));
        assert_eq!(found.gitfile(), None);
        assert!(!found.is_bare());
    }

    #[test]
    fn follows_gitfile_and_resolves_linked_worktree_common_dir() {
        let temp = TempDir::new().expect("tempdir");
        let common_dir = temp.path().join("main/.git");
        create_git_dir(&common_dir);
        let linked_git_dir = common_dir.join("worktrees/topic");
        fs::create_dir_all(&linked_git_dir).expect("linked admin");
        fs::write(linked_git_dir.join("HEAD"), b"ref: refs/heads/topic\n").expect("linked HEAD");
        fs::write(linked_git_dir.join("commondir"), b"../..\n").expect("commondir");
        let worktree = temp.path().join("topic");
        fs::create_dir_all(&worktree).expect("linked worktree");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", linked_git_dir.display()),
        )
        .expect("gitfile");

        let found = discover_repository(&worktree, RepositoryDiscoveryOptions::ancestors())
            .expect("discover linked worktree");

        assert_eq!(found.git_dir(), linked_git_dir);
        assert_eq!(
            found.common_dir(),
            fs::canonicalize(&common_dir).expect("canonical common dir")
        );
        assert_eq!(found.worktree(), Some(worktree.as_path()));
        assert_eq!(found.gitfile(), Some(worktree.join(".git").as_path()));
        assert!(!found.is_bare());
    }

    #[test]
    fn exact_and_local_remote_modes_validate_bare_and_suffix_forms() {
        let temp = TempDir::new().expect("tempdir");
        let bare = temp.path().join("project.git");
        create_git_dir(&bare);

        let exact =
            discover_repository(&bare, RepositoryDiscoveryOptions::exact()).expect("exact bare");
        assert_eq!(exact.git_dir(), bare);
        assert_eq!(exact.worktree(), None);
        assert!(exact.is_bare());

        let unsuffixed = temp.path().join("project");
        let remote = discover_repository(&unsuffixed, RepositoryDiscoveryOptions::local_remote())
            .expect("dot-git suffix fallback");
        assert_eq!(remote.git_dir(), bare);
        assert!(remote.is_bare());

        let linked = temp.path().join("linked");
        fs::create_dir(&linked).expect("linked worktree");
        fs::write(linked.join(".git"), b"gitdir: ../project.git/./\n").expect("gitfile");
        let remote = discover_repository(&linked, RepositoryDiscoveryOptions::local_remote())
            .expect("local remote gitfile");
        assert_eq!(
            remote.git_dir(),
            fs::canonicalize(&bare).expect("canonical bare")
        );
        assert_eq!(remote.worktree(), Some(linked.as_path()));
        assert!(!remote.is_bare());
    }

    #[test]
    fn exact_git_dir_resolution_does_not_infer_configured_worktree() {
        let temp = TempDir::new().expect("tempdir");
        let git_dir = temp.path().join("repo.git");
        create_git_dir(&git_dir);
        fs::write(
            git_dir.join("config"),
            b"[core]\n\tbare = false\n\tworktree = missing\n",
        )
        .expect("write config");

        assert_eq!(
            resolve_exact_git_dir(&git_dir).expect("resolve explicit git dir"),
            git_dir
        );
    }

    #[test]
    fn local_remote_git_dir_does_not_infer_configured_worktree() {
        let temp = TempDir::new().expect("tempdir");
        let git_dir = temp.path().join("repo.git");
        create_git_dir(&git_dir);
        fs::write(
            git_dir.join("config"),
            b"[core]\n\tbare = false\n\tworktree = missing\n",
        )
        .expect("write config");

        let found = discover_repository(&git_dir, RepositoryDiscoveryOptions::local_remote())
            .expect("resolve local remote git dir");
        assert_eq!(found.git_dir(), git_dir);
        assert_eq!(found.common_dir(), git_dir);
        assert_eq!(found.worktree(), None);
        assert!(!found.is_bare());
    }

    #[test]
    fn filesystem_bound_discovery_never_reports_outer_worktree_sibling() {
        let temp = TempDir::new().expect("tempdir");
        let outer_git_dir = temp.path().join(".git");
        create_git_dir(&outer_git_dir);
        let mounted_worktree = temp.path().join("mounted-worktree");
        let start = mounted_worktree.join("nested");
        let sibling_file = temp.path().join("sibling").join("outside.txt");
        fs::create_dir_all(&start).expect("create discovery start");
        fs::create_dir_all(sibling_file.parent().expect("sibling parent"))
            .expect("create sibling directory");
        fs::write(&sibling_file, b"outside\n").expect("write sibling file");

        let simulated_device = |path: &Path| {
            if path.starts_with(&mounted_worktree) {
                Some(2)
            } else {
                Some(1)
            }
        };
        let mut options = RepositoryDiscoveryOptions::ancestors();
        options.across_filesystem = true;
        let outer = discover_repository_with_device(&start, options, simulated_device)
            .expect("unbounded discovery reaches outer repository");
        let unbounded_paths =
            crate::untracked_paths(temp.path(), outer.git_dir(), sley_core::ObjectFormat::Sha1)
                .expect("walk outer worktree");
        assert!(
            unbounded_paths.contains(&b"sibling/outside.txt".to_vec()),
            "fixture must prove an unbounded discovery includes the outer sibling"
        );

        options.across_filesystem = false;
        let bounded = discover_repository_with_device(&start, options, simulated_device);
        let bounded_paths = match bounded {
            Ok(found) => crate::untracked_paths(
                found.worktree().expect("discovered worktree"),
                found.git_dir(),
                sley_core::ObjectFormat::Sha1,
            )
            .expect("walk discovered worktree"),
            Err(GitError::NotFound(_)) => Vec::new(),
            Err(err) => panic!("unexpected discovery error: {err}"),
        };
        assert!(
            !bounded_paths.contains(&b"sibling/outside.txt".to_vec()),
            "filesystem-bound discovery must never report a sibling outside the mounted worktree"
        );
    }

    #[test]
    fn invalid_dot_git_directory_is_not_a_repository() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("empty dot-git");
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).expect("nested");

        let err = discover_repository(&nested, RepositoryDiscoveryOptions::ancestors())
            .expect_err("empty .git must be ignored");
        assert!(matches!(err, GitError::NotFound(_)));
    }
}
