//! The shared repository setup engine: the env/global-option/config resolution
//! layer that turns the user's cwd + `GIT_DIR`/`GIT_WORK_TREE`/`--git-dir`/
//! `--work-tree`/`core.bare`/`core.worktree`/gitfile inputs into an effective
//! `(git_dir, common_dir, worktree, prefix)` tuple.
//!
//! `sley::Repository::discover` is deliberately repository-*intrinsic* — it never
//! consults the environment, because that resolution "belongs to a CLI layer".
//! This module is that layer, lifted below the seam so embedders share it. It is
//! a faithful port of git's `setup_git_directory_gently` (setup.c), covering the
//! cases t1510 exercises: the eight env/config/gitfile/bare permutations and the
//! relative/absolute, inside/outside-worktree, and chdir-to-toplevel behaviours.
//!
//! Invocation-scoped inputs (`--git-dir` / `GIT_DIR`, `--work-tree` /
//! `GIT_WORK_TREE`, `--bare`, and the invocation cwd) are supplied through the
//! [`SetupEnvironment`] trait so the CLI session and any other embedder can drive
//! the same engine. The `GIT_TRACE_SETUP` observability seam that renders a
//! [`SetupResult`] stays in the CLI layer.

use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, Result};

/// The invocation-scoped setup inputs an embedder supplies to the engine:
/// the current working directory plus git's global-option / environment
/// overrides (`--git-dir` / `GIT_DIR`, `--work-tree` / `GIT_WORK_TREE`, and
/// `--bare`).
pub trait SetupEnvironment {
    /// The invocation's current working directory.
    fn cwd(&self) -> &Path;
    /// The explicit git directory from `--git-dir` / `GIT_DIR`, if any.
    fn explicit_git_dir(&self) -> Option<PathBuf>;
    /// The explicit worktree from `--work-tree` / `GIT_WORK_TREE`, if any.
    fn explicit_work_tree(&self) -> Option<PathBuf>;
    /// Whether `--bare` was requested.
    fn explicit_bare(&self) -> bool;
}

/// Purpose-specific worktree semantics for a repository snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreePolicy {
    /// Normal command setup semantics.
    Command,
    /// Hash-object attribute lookup semantics, based on physical layout.
    HashAttributes,
}

/// The resolved repository layout, in git's trace terms.
pub struct SetupResult {
    /// `repo_get_git_dir`: the textual (often relative) git directory, after
    /// gitfile resolution.
    pub git_dir: String,
    /// `repo_get_common_dir`: the common dir (equal to `git_dir` unless a
    /// `commondir` file or `GIT_COMMON_DIR` redirects it).
    pub common_dir: String,
    /// `repo_get_work_tree`, or `None` for a bare repository / no worktree.
    pub worktree: Option<PathBuf>,
    /// git's post-setup cwd (the worktree top when git chdir'd there, else the
    /// original cwd).
    pub cwd: PathBuf,
    /// The relative path from `cwd` to the user's original cwd, with a trailing
    /// `/`, or `None` (git's `(null)`).
    pub prefix: Option<String>,
    /// Whether `core.bare` and an effective `core.worktree` both apply (git's
    /// `work_tree_config_is_bogus`): a worktree-requiring command must warn
    /// "core.bare and core.worktree do not make sense" and then fail.
    pub worktree_config_bogus: bool,
}

/// Outcome of the upward `.git` discovery walk (git's `enum discovery_result`,
/// limited to the variants we resolve into a [`SetupResult`]).
enum Discovered {
    /// `GIT_DIR`/`--git-dir` was explicit; `git_dir` is the user-given value.
    Explicit { git_dir: String },
    /// `.git` was found by walking up; `dir` is the directory containing it,
    /// `git_dir` is the (relative-to-`dir`) value.
    Found { dir: PathBuf, git_dir: String },
    /// A bare repository was found at `dir` (cwd was inside a git dir).
    Bare { dir: PathBuf },
}

/// Resolve the effective repository layout from the current environment, mirroring
/// git's `setup_git_directory_gently`. `None` means no repository was found (git's
/// `nongit_ok` path) — callers that require a repo surface their own error.
pub fn setup_git_directory<E: SetupEnvironment>(env: &E) -> Option<SetupResult> {
    let cwd = env.cwd().to_path_buf();
    let discovered = discover(env, &cwd)?;
    match discovered {
        Discovered::Explicit { git_dir } => setup_explicit(env, &git_dir, &cwd),
        Discovered::Found { dir, git_dir } => setup_discovered(env, &git_dir, &dir, &cwd),
        Discovered::Bare { dir } => setup_bare(env, &dir, &cwd),
    }
}

/// git's `setup_git_directory_gently_1`: decide explicit vs discovered vs bare.
fn discover<E: SetupEnvironment>(env: &E, cwd: &Path) -> Option<Discovered> {
    // GIT_DIR / --git-dir set explicitly: no discovery, just validation.
    if let Some(git_dir) = env.explicit_git_dir() {
        if git_dir.as_os_str().is_empty() {
            return None;
        }
        return Some(Discovered::Explicit {
            git_dir: git_dir.to_string_lossy().into_owned(),
        });
    }

    // `git --bare`: treat cwd as the (bare) git dir.
    if env.explicit_bare() {
        if super::is_git_dir(cwd) {
            return Some(Discovered::Bare {
                dir: cwd.to_path_buf(),
            });
        }
        return None;
    }

    let one_filesystem = !super::git_env_bool("GIT_DISCOVERY_ACROSS_FILESYSTEM");
    let start_device = if one_filesystem {
        super::device_of(cwd)
    } else {
        None
    };

    let ceilings = super::discovery_ceiling_directories();

    for dir in cwd.ancestors() {
        // GIT_CEILING_DIRECTORIES: stop before entering a listed *proper*
        // ancestor; the starting directory itself is always examined.
        // Empty entries in the env list disable realpath for subsequent ceilings
        // (git's canonicalize_ceiling_entry / t1504 no_resolve cases).
        if dir != cwd
            && ceilings
                .iter()
                .any(|ceiling| ceiling.matches_discovery_candidate(dir))
        {
            return None;
        }

        let dot_git = dir.join(".git");

        // .git file: "gitdir: <path>".
        if dot_git.is_file()
            && let Ok(Some(target)) = super::read_gitdir_link(&dot_git)
            && super::is_git_dir(&target)
        {
            // The user-facing git_dir is the gitfile path itself relative to
            // dir (".git"); repo_set_gitdir resolves it.
            return Some(Discovered::Found {
                dir: dir.to_path_buf(),
                git_dir: ".git".to_string(),
            });
        }

        // .git directory.
        if dot_git.is_dir() && super::is_git_dir(&dot_git) {
            return Some(Discovered::Found {
                dir: dir.to_path_buf(),
                git_dir: ".git".to_string(),
            });
        }

        // bare: dir itself is a git directory.
        if super::is_git_dir(dir) {
            return Some(Discovered::Bare {
                dir: dir.to_path_buf(),
            });
        }

        // Stop at a filesystem boundary unless GIT_DISCOVERY_ACROSS_FILESYSTEM.
        if one_filesystem
            && let Some(parent) = dir.parent()
            && super::device_of(parent) != start_device
        {
            return None;
        }
    }
    None
}

/// git's `setup_explicit_git_dir`. `cwd` is the user's original cwd.
fn setup_explicit<E: SetupEnvironment>(
    env: &E,
    gitdirenv: &str,
    cwd: &Path,
) -> Option<SetupResult> {
    // A `.git` *file* named by GIT_DIR is resolved to its target (git's
    // read_gitfile in setup_explicit_git_dir).
    let gitdir_path = super::resolve_path_from_cwd(cwd, Path::new(gitdirenv));
    let (effective_gitdir_text, gitdir_dir) = if gitdir_path.is_file() {
        match super::read_gitdir_link(&gitdir_path) {
            Ok(Some(target)) => {
                let target = canonicalize_or(&target);
                (path_to_string(&target), target)
            }
            _ => return None,
        }
    } else {
        (gitdirenv.to_string(), gitdir_path)
    };

    if !super::is_git_dir(&gitdir_dir) {
        return None;
    }

    let (is_bare, core_worktree) = read_worktree_config(&gitdir_dir);
    let is_bare = is_bare && !gitdir_dir.join("commondir").is_file();

    let worktree: Option<PathBuf>;

    if let Some(work_tree_env) = env.explicit_work_tree() {
        // #3,#7,...: explicit GIT_WORK_TREE / --work-tree wins.
        let wt = super::resolve_path_from_cwd(cwd, &work_tree_env);
        worktree = Some(canonicalize_or(&wt));
    } else if is_bare {
        // #18, #26: bare, no worktree. If core.worktree is *also* set this is
        // the #22.2/#30 conflict ("core.bare and core.worktree do not make
        // sense"): git warns + marks the work-tree config bogus, then proceeds
        // here with no worktree.
        let bogus = core_worktree.is_some();
        return Some(bare_explicit_result(
            &effective_gitdir_text,
            &gitdir_dir,
            cwd,
            bogus,
        ));
    } else if let Some(core_wt) = core_worktree.as_deref() {
        // #6, #14: core.worktree is relative to the git dir.
        let wt = if Path::new(core_wt).is_absolute() {
            PathBuf::from(core_wt)
        } else {
            gitdir_dir.join(core_wt)
        };
        worktree = Some(canonicalize_or(&wt));
    } else if !git_env_bool_default("GIT_IMPLICIT_WORK_TREE", true) {
        // #16d: GIT_IMPLICIT_WORK_TREE=0, no worktree.
        return Some(bare_explicit_result(
            &effective_gitdir_text,
            &gitdir_dir,
            cwd,
            false,
        ));
    } else {
        // #2, #10: worktree defaults to cwd.
        worktree = Some(canonicalize_or(cwd));
    }

    let worktree = worktree?;
    let cwd_canon = canonicalize_or(cwd);

    // cwd == worktree: keep gitdir textual, no chdir, prefix null.
    if cwd_canon == worktree {
        return Some(SetupResult {
            git_dir: effective_gitdir_text.clone(),
            common_dir: common_dir_for(&effective_gitdir_text, &gitdir_dir),
            worktree: Some(worktree),
            cwd: cwd.to_path_buf(),
            prefix: None,
            worktree_config_bogus: false,
        });
    }

    // cwd inside worktree: git chdir's to worktree top, makes gitdir a realpath,
    // returns the prefix.
    if let Some(prefix) = relative_inside(&worktree, &cwd_canon) {
        // set_git_dir(gitdirenv, 1): realpath the gitdir.
        let abs_gitdir = path_to_string(&canonicalize_or(&gitdir_dir));
        return Some(SetupResult {
            git_dir: abs_gitdir.clone(),
            common_dir: common_dir_for(&abs_gitdir, &gitdir_dir),
            worktree: Some(worktree.clone()),
            cwd: worktree,
            prefix: Some(prefix),
            worktree_config_bogus: false,
        });
    }

    // cwd outside worktree: keep gitdir textual, no chdir, prefix null.
    Some(SetupResult {
        git_dir: effective_gitdir_text.clone(),
        common_dir: common_dir_for(&effective_gitdir_text, &gitdir_dir),
        worktree: Some(worktree),
        cwd: cwd.to_path_buf(),
        prefix: None,
        worktree_config_bogus: false,
    })
}

/// Build the no-worktree result for an explicit GIT_DIR (bare / implicit-wt-off),
/// matching git's `set_git_dir(gitdirenv, 0)` + return NULL.
fn bare_explicit_result(
    gitdir_text: &str,
    gitdir_dir: &Path,
    cwd: &Path,
    worktree_config_bogus: bool,
) -> SetupResult {
    SetupResult {
        git_dir: gitdir_text.to_string(),
        common_dir: common_dir_for(gitdir_text, gitdir_dir),
        worktree: None,
        cwd: cwd.to_path_buf(),
        prefix: None,
        worktree_config_bogus,
    }
}

/// git's `setup_discovered_git_dir`. `dir` is the directory `.git` was found in,
/// `cwd` the user's original cwd. `gitdir` is relative-to-`dir` (".git").
fn setup_discovered<E: SetupEnvironment>(
    env: &E,
    gitdir: &str,
    dir: &Path,
    cwd: &Path,
) -> Option<SetupResult> {
    let gitdir_dir = dir.join(gitdir);
    // The textual git_dir git resolves a gitfile to its target for repo->gitdir;
    // for trace purposes we resolve `.git`-file targets here.
    let (effective_gitdir_text, effective_gitdir_dir) = if gitdir_dir.is_file() {
        match super::read_gitdir_link(&gitdir_dir) {
            Ok(Some(target)) => {
                let target = canonicalize_or(&target);
                (path_to_string(&target), target)
            }
            _ => (gitdir.to_string(), gitdir_dir.clone()),
        }
    } else {
        (gitdir.to_string(), gitdir_dir)
    };

    let has_common_dir = effective_gitdir_dir.join("commondir").is_file();
    let (is_bare, core_worktree) = read_worktree_config(&effective_gitdir_dir);
    let is_bare = is_bare && !has_common_dir;
    let effective_core_worktree = if has_common_dir {
        None
    } else {
        core_worktree.as_deref()
    };

    // --work-tree / GIT_WORK_TREE / core.worktree: defer to explicit handling,
    // but with the *discovered* git dir. git makes the gitdir a realpath when
    // dir != cwd; we pass the resolved git dir text so trace matches.
    if env.explicit_work_tree().is_some() || effective_core_worktree.is_some() {
        return setup_explicit_from_discovered(
            env,
            &effective_gitdir_text,
            &effective_gitdir_dir,
            is_bare,
            effective_core_worktree,
            cwd,
            dir,
        );
    }

    // Bare (core.bare=true), no explicit worktree.
    if is_bare {
        let git_dir = if dir == cwd {
            effective_gitdir_text
        } else {
            // set_git_dir(gitdir, offset != cwd->len): realpath.
            path_to_string(&effective_gitdir_dir)
        };
        return Some(SetupResult {
            git_dir: git_dir.clone(),
            common_dir: common_dir_for(&git_dir, &effective_gitdir_dir),
            worktree: None,
            cwd: cwd.to_path_buf(),
            prefix: None,
            worktree_config_bogus: false,
        });
    }

    // #0, #1, ...: worktree is `dir` (git chdir'd to `dir`), gitdir stays `.git`
    // if equal to DEFAULT_GIT_DIR else realpath'd. git only calls
    // set_git_dir(gitdir, 0) when gitdir != ".git"; here gitdir is ".git" so it
    // keeps the literal — but for the resolved-gitfile case git's repo->gitdir is
    // the target. The trace prints `.git` when the gitfile resolves and we're at
    // top because repo_set_gitdir reads the gitfile and stores the *target* —
    // but the test expects the *gitfile dir's* `.git` literal value only in the
    // non-gitfile case. For the gitfile case the test expects the absolute
    // target. We computed effective_gitdir_text accordingly.
    let git_dir_text = if effective_gitdir_text == ".git" {
        ".git".to_string()
    } else {
        effective_gitdir_text
    };
    let worktree = canonicalize_or(dir);
    let prefix = relative_inside(&worktree, &canonicalize_or(cwd));
    Some(SetupResult {
        git_dir: git_dir_text.clone(),
        common_dir: common_dir_for(&git_dir_text, &effective_gitdir_dir),
        worktree: Some(worktree.clone()),
        cwd: worktree,
        prefix,
        worktree_config_bogus: false,
    })
}

/// The "discovered but worktree/core.worktree present" branch of
/// `setup_discovered_git_dir`, which git re-routes through
/// `setup_explicit_git_dir` with the (realpath'd) discovered git dir. We inline
/// the explicit resolution using the already-known git dir + config.
fn setup_explicit_from_discovered<E: SetupEnvironment>(
    env: &E,
    gitdir_text: &str,
    gitdir_dir: &Path,
    is_bare: bool,
    core_worktree: Option<&str>,
    cwd: &Path,
    dir: &Path,
) -> Option<SetupResult> {
    // git real_pathdup's the discovered git dir when offset != cwd (i.e. the
    // worktree top differs from cwd). Use the absolute git dir text in that case.
    let abs_text = path_to_string(gitdir_dir);
    let gitdir_text_resolved = if dir == cwd {
        gitdir_text.to_string()
    } else {
        abs_text
    };

    let worktree: Option<PathBuf>;
    if let Some(work_tree_env) = env.explicit_work_tree() {
        let wt = super::resolve_path_from_cwd(cwd, &work_tree_env);
        worktree = Some(canonicalize_or(&wt));
    } else if is_bare {
        // #20b/c, #28: core.bare + core.worktree conflict — warn + no worktree.
        return Some(SetupResult {
            git_dir: gitdir_text_resolved.clone(),
            common_dir: common_dir_for(&gitdir_text_resolved, gitdir_dir),
            worktree: None,
            cwd: cwd.to_path_buf(),
            prefix: None,
            worktree_config_bogus: core_worktree.is_some(),
        });
    } else if let Some(core_wt) = core_worktree {
        let wt = if Path::new(core_wt).is_absolute() {
            PathBuf::from(core_wt)
        } else {
            gitdir_dir.join(core_wt)
        };
        worktree = Some(canonicalize_or(&wt));
    } else {
        worktree = Some(canonicalize_or(cwd));
    }

    let worktree = worktree?;
    let cwd_canon = canonicalize_or(cwd);

    if cwd_canon == worktree {
        return Some(SetupResult {
            git_dir: gitdir_text_resolved.clone(),
            common_dir: common_dir_for(&gitdir_text_resolved, gitdir_dir),
            worktree: Some(worktree),
            cwd: cwd.to_path_buf(),
            prefix: None,
            worktree_config_bogus: false,
        });
    }

    if let Some(prefix) = relative_inside(&worktree, &cwd_canon) {
        let abs_gitdir = path_to_string(&canonicalize_or(gitdir_dir));
        return Some(SetupResult {
            git_dir: abs_gitdir.clone(),
            common_dir: common_dir_for(&abs_gitdir, gitdir_dir),
            worktree: Some(worktree.clone()),
            cwd: worktree,
            prefix: Some(prefix),
            worktree_config_bogus: false,
        });
    }

    Some(SetupResult {
        git_dir: gitdir_text_resolved.clone(),
        common_dir: common_dir_for(&gitdir_text_resolved, gitdir_dir),
        worktree: Some(worktree),
        cwd: cwd.to_path_buf(),
        prefix: None,
        worktree_config_bogus: false,
    })
}

/// git's `setup_bare_git_dir`. cwd is inside a git directory; `dir` is the git
/// directory that was found (could equal cwd or be an ancestor).
fn setup_bare<E: SetupEnvironment>(env: &E, dir: &Path, cwd: &Path) -> Option<SetupResult> {
    let (is_bare, core_worktree) = read_worktree_config(dir);

    // --work-tree / GIT_WORK_TREE / core.worktree re-route through explicit
    // setup with the bare git dir (git's setup_bare_git_dir: "if
    // getenv(GIT_WORK_TREE) || git_work_tree_cfg"). A core.worktree gives the
    // otherwise-bare repo a real worktree (#20a).
    if env.explicit_work_tree().is_some() || core_worktree.is_some() {
        let gitdir_text = if dir == cwd {
            ".".to_string()
        } else {
            path_to_string(dir)
        };
        return setup_explicit_from_discovered(
            env,
            &gitdir_text,
            dir,
            is_bare,
            core_worktree.as_deref(),
            cwd,
            dir,
        );
    }
    let _ = is_bare;

    // inside_git_dir: no worktree. git sets git_dir to the (absolute) dir when
    // dir != cwd, else ".".
    let git_dir = if dir == cwd {
        ".".to_string()
    } else {
        path_to_string(dir)
    };
    Some(SetupResult {
        git_dir: git_dir.clone(),
        common_dir: common_dir_for(&git_dir, dir),
        worktree: None,
        cwd: cwd.to_path_buf(),
        prefix: None,
        worktree_config_bogus: false,
    })
}

/// Resolved git directory for this invocation's cwd, honoring `--git-dir` /
/// `GIT_DIR`, gitfiles, and `--bare`. git's session-level git-dir resolution.
pub fn invocation_git_dir<E: SetupEnvironment>(env: &E) -> Result<PathBuf> {
    let cwd = env.cwd().to_path_buf();
    if let Some(git_dir) = env.explicit_git_dir() {
        if git_dir.as_os_str().is_empty() {
            return Err(GitError::repository_not_found("not a git repository"));
        }
        let resolved = super::resolve_path_from_cwd(&cwd, &git_dir);
        if resolved.is_file()
            && let Some(target) = super::read_gitdir_link(&resolved)?
            && super::is_git_dir(&target)
        {
            return fs::canonicalize(target).map_err(|err| GitError::Io(err.to_string()));
        }
        return Ok(resolved);
    }
    if env.explicit_bare() {
        if super::is_git_dir(&cwd) {
            return fs::canonicalize(cwd).map_err(|err| GitError::Io(err.to_string()));
        }
        return Err(GitError::repository_not_found("not a git repository"));
    }
    super::probes::resolve_git_dir_walk_only(cwd)
}

/// Resolve Git's effective worktree policy without requiring one to exist.
///
/// An explicit git directory changes the implicit worktree from the
/// repository-intrinsic `.git` parent to the invocation cwd. Delegate that
/// distinction (including `core.worktree`, `core.bare`, and
/// `GIT_IMPLICIT_WORK_TREE`) to the shared setup engine.
pub fn effective_worktree_for_git_dir<E: SetupEnvironment>(
    env: &E,
    git_dir: &Path,
) -> Result<Option<PathBuf>> {
    if let Some(work_tree) = env.explicit_work_tree() {
        let work_tree = super::resolve_path_from_cwd(env.cwd(), &work_tree);
        return fs::canonicalize(work_tree)
            .map(Some)
            .map_err(|err| GitError::Io(err.to_string()));
    }
    if env.explicit_git_dir().is_some() {
        let setup = setup_git_directory(env).ok_or_else(|| {
            GitError::repository_not_found(format!("not a git repository: {}", git_dir.display()))
        })?;
        return Ok(setup.worktree);
    }
    if env.explicit_bare() {
        return Ok(None);
    }
    if let Some(root) = crate::worktree_root_for_git_dir(git_dir)? {
        return Ok(Some(root));
    }
    // Intrinsic layout only recognizes a worktree when the admin dir is
    // named `.git` (or has a linked-worktree `gitdir`/`commondir` pair).
    // A gitfile that points at a differently-named directory (e.g. the
    // t2105 `gitdir: .real` layout) has a real worktree at the gitfile's
    // parent; fall back to CLI setup discovery so `add` / `commit` work.
    if let Some(setup) = setup_git_directory(env) {
        return Ok(setup.worktree);
    }
    Ok(None)
}

/// Resolve the invocation worktree from already-loaded physical and effective
/// config snapshots without re-opening repository config.
pub fn optional_worktree_from_config<E: SetupEnvironment>(
    env: &E,
    git_dir: &Path,
    setup_config: &GitConfig,
    effective_config: &GitConfig,
    linked_worktree: bool,
    policy: WorktreePolicy,
) -> Result<Option<PathBuf>> {
    if let Some(work_tree) = env.explicit_work_tree() {
        let work_tree = super::resolve_path_from_cwd(env.cwd(), &work_tree);
        return fs::canonicalize(work_tree)
            .map(Some)
            .map_err(|err| GitError::Io(err.to_string()));
    }

    match policy {
        WorktreePolicy::Command => optional_command_worktree_from_config(
            env,
            git_dir,
            setup_config,
            effective_config,
            linked_worktree,
        ),
        WorktreePolicy::HashAttributes => optional_hash_attribute_root_from_config(
            env,
            git_dir,
            setup_config,
            effective_config,
            linked_worktree,
        ),
    }
}

fn optional_command_worktree_from_config<E: SetupEnvironment>(
    env: &E,
    git_dir: &Path,
    setup_config: &GitConfig,
    effective_config: &GitConfig,
    linked_worktree: bool,
) -> Result<Option<PathBuf>> {
    if env.explicit_git_dir().is_some() {
        if effective_config.get_bool("core", None, "bare") == Some(true) && !linked_worktree {
            return Ok(None);
        }
        if let Some(worktree) = effective_config.get("core", None, "worktree") {
            return canonicalize_configured_worktree(git_dir, worktree).map(Some);
        }
        if implicit_worktree_disabled() {
            return Ok(None);
        }
        return canonicalize_cwd(env.cwd()).map(Some);
    }
    if env.explicit_bare() {
        return Ok(None);
    }
    if linked_worktree && let Some(worktree) = linked_worktree_root(git_dir)? {
        return Ok(Some(worktree));
    }
    if setup_config.get_bool("core", None, "bare") == Some(true) {
        return Ok(None);
    }
    if let Some(worktree) = setup_config.get("core", None, "worktree") {
        return canonicalize_configured_worktree(git_dir, worktree).map(Some);
    }
    if git_dir.file_name().and_then(|name| name.to_str()) == Some(".git") {
        return git_dir
            .parent()
            .map(Path::to_path_buf)
            .map(Some)
            .ok_or_else(|| GitError::InvalidPath("git dir has no parent worktree".into()));
    }
    Ok(setup_git_directory(env).and_then(|setup| setup.worktree))
}

fn optional_hash_attribute_root_from_config<E: SetupEnvironment>(
    env: &E,
    git_dir: &Path,
    setup_config: &GitConfig,
    effective_config: &GitConfig,
    linked_worktree: bool,
) -> Result<Option<PathBuf>> {
    // A linked worktree's backlink is intrinsic layout. It wins even when
    // common/effective core.bare is true or implicit worktrees are disabled.
    if linked_worktree && let Some(worktree) = linked_worktree_root(git_dir)? {
        return Ok(Some(worktree));
    }

    let physical_bare = setup_config.get_bool("core", None, "bare") == Some(true);
    if !physical_bare {
        if let Some(worktree) = setup_config.get("core", None, "worktree") {
            return canonicalize_configured_worktree(git_dir, worktree).map(Some);
        }
        if git_dir.file_name().and_then(|name| name.to_str()) == Some(".git") {
            return git_dir
                .parent()
                .map(Path::to_path_buf)
                .map(Some)
                .ok_or_else(|| GitError::InvalidPath("git dir has no parent worktree".into()));
        }

        // Preserve the non-standard gitfile fallback for a physically
        // non-bare admin directory without a normal backlink.
        return Ok(setup_git_directory(env).and_then(|setup| setup.worktree));
    }

    if effective_config.get_bool("core", None, "bare") != Some(false) {
        return Ok(None);
    }
    // For hash-object attributes Git uses cwd here, ignoring config
    // core.worktree and GIT_IMPLICIT_WORK_TREE. An explicit work-tree was
    // already handled by the caller above.
    canonicalize_cwd(env.cwd()).map(Some)
}

fn canonicalize_configured_worktree(git_dir: &Path, worktree: &str) -> Result<PathBuf> {
    let worktree = PathBuf::from(worktree);
    let worktree = if worktree.is_absolute() {
        worktree
    } else {
        git_dir.join(worktree)
    };
    fs::canonicalize(worktree).map_err(|err| GitError::Io(err.to_string()))
}

fn canonicalize_cwd(cwd: &Path) -> Result<PathBuf> {
    fs::canonicalize(cwd).map_err(|err| GitError::Io(err.to_string()))
}

fn implicit_worktree_disabled() -> bool {
    env::var_os("GIT_IMPLICIT_WORK_TREE").is_some_and(|value| {
        matches!(
            value.to_string_lossy().as_ref(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

fn linked_worktree_root(git_dir: &Path) -> Result<Option<PathBuf>> {
    let backlink = git_dir.join("gitdir");
    let Ok(value) = fs::read_to_string(&backlink) else {
        return Ok(None);
    };
    let path = PathBuf::from(value.trim());
    let gitfile = if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    };
    let Some(worktree) = gitfile.parent() else {
        return Ok(None);
    };
    fs::canonicalize(worktree)
        .map(Some)
        .map_err(|err| GitError::Io(err.to_string()))
}

/// Read `core.bare` / `core.worktree` from `<commondir>/config` (and
/// `config.worktree` when `extensions.worktreeConfig` is set), mirroring git's
/// `check_repository_format_gently`. Returns `(is_bare, core_worktree)`.
fn read_worktree_config(gitdir: &Path) -> (bool, Option<String>) {
    let common = common_dir_path(gitdir);
    let config_path = common.join("config");
    let Ok(config) = GitConfig::read(&config_path) else {
        return (false, None);
    };
    let worktree_config = config
        .get_bool("extensions", None, "worktreeConfig")
        .unwrap_or(false);

    let mut is_bare = config.get_bool("core", None, "bare").unwrap_or(false);
    let mut core_worktree = config.get("core", None, "worktree").map(str::to_string);

    if worktree_config {
        // Per-worktree config overrides core.bare/core.worktree.
        let wt_config_path = gitdir.join("config.worktree");
        if let Ok(wt_config) = GitConfig::read(&wt_config_path) {
            if let Some(b) = wt_config.get_bool("core", None, "bare") {
                is_bare = b;
            }
            if let Some(wt) = wt_config.get("core", None, "worktree") {
                core_worktree = Some(wt.to_string());
            }
        }
    }

    (is_bare, core_worktree)
}

/// The common dir for a git dir: follow a `commondir` file (relative to git dir)
/// if present, else the git dir itself.
fn common_dir_path(gitdir: &Path) -> PathBuf {
    if let Some(env) = env::var_os("GIT_COMMON_DIR") {
        return PathBuf::from(env);
    }
    let commondir = gitdir.join("commondir");
    if commondir.is_file()
        && let Ok(value) = fs::read_to_string(&commondir)
    {
        let trimmed = value.trim_end_matches(['\n', '\r']);
        let path = Path::new(trimmed);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            gitdir.join(path)
        };
        return canonicalize_or(&resolved);
    }
    gitdir.to_path_buf()
}

/// The trace's `git_common_dir` value. When `GIT_COMMON_DIR` is set it wins
/// (git's `get_common_dir`); when a `commondir` file is present the realpath'd
/// common dir is used; otherwise the common dir equals the textual git_dir.
fn common_dir_for(git_dir_text: &str, gitdir_dir: &Path) -> String {
    if let Some(env) = env::var_os("GIT_COMMON_DIR") {
        return env.to_string_lossy().into_owned();
    }
    let commondir = gitdir_dir.join("commondir");
    if commondir.is_file()
        && let Ok(value) = fs::read_to_string(&commondir)
    {
        let trimmed = value.trim_end_matches(['\n', '\r']);
        let path = Path::new(trimmed);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            gitdir_dir.join(path)
        };
        return path_to_string(&canonicalize_or(&resolved));
    }
    git_dir_text.to_string()
}

/// A `GIT_*` boolean env var with a configurable default when unset, matching
/// git's `git_env_bool`.
fn git_env_bool_default(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => default,
    }
}

/// Canonicalize a path, falling back to a lexical normalization if the path does
/// not exist (so trace output still has an absolute form).
fn canonicalize_or(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| lexical_abs(path))
}

/// Lexical absolutization: join with cwd if relative, then drop `.`/`..`.
fn lexical_abs(path: &Path) -> PathBuf {
    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in base.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The relative prefix from `worktree` to `cwd` (with trailing `/`) when `cwd`
/// is inside `worktree`, or `None` (git's `(null)`) otherwise. Equal paths yield
/// `None` (prefix is empty → null).
fn relative_inside(worktree: &Path, cwd: &Path) -> Option<String> {
    if worktree == cwd {
        return None;
    }
    let rel = cwd.strip_prefix(worktree).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let mut text = path_to_slash(rel);
    if !text.ends_with('/') {
        text.push('/');
    }
    Some(text)
}

/// Render a path with forward slashes (git uses `/` in trace prefixes).
fn path_to_slash(path: &Path) -> String {
    sley_core::paths::path_to_slash(path)
}

/// A path as a UTF-8-lossy string (git stores paths as bytes; the tested paths
/// are ASCII).
fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
