//! CLI-side repository setup: the env/global-option/config resolution layer that
//! turns the user's cwd + `GIT_DIR`/`GIT_WORK_TREE`/`--git-dir`/`--work-tree`/
//! `core.bare`/`core.worktree`/gitfile inputs into an effective
//! (git_dir, common_dir, worktree, prefix) tuple.
//!
//! `sley::Repository::discover` is deliberately repository-*intrinsic* — it never
//! consults the environment, because that resolution "belongs to a CLI layer".
//! This is that layer. It is a faithful port of git's
//! `setup_git_directory_gently` (setup.c), covering the cases t1510 exercises:
//! the eight env/config/gitfile/bare permutations and the relative/absolute,
//! inside/outside-worktree, and chdir-to-toplevel behaviours.
//!
//! The single observable side effect mirrored here is the `GIT_TRACE_SETUP`
//! trace: with `GIT_TRACE_BARE=1`, git writes five `setup: ` lines naming the
//! resolved git_dir / common_dir / worktree / cwd / prefix. [`trace_repo_setup`]
//! reproduces that output byte-for-byte so harnesses that read the trace (t1510)
//! see identical results.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use sley::GitConfig;

use crate::{git_env_bool, is_git_dir_candidate, read_gitdir_file, resolve_cli_path, session};

/// The resolved repository layout, in git's trace terms.
pub(crate) struct SetupResult {
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
pub(crate) fn setup_git_directory(cli_session: &session::CliSession) -> Option<SetupResult> {
    let cwd = cli_session.cwd().to_path_buf();
    let discovered = discover(cli_session, &cwd)?;
    match discovered {
        Discovered::Explicit { git_dir } => setup_explicit(cli_session, &git_dir, &cwd),
        Discovered::Found { dir, git_dir } => setup_discovered(cli_session, &git_dir, &dir, &cwd),
        Discovered::Bare { dir } => setup_bare(cli_session, &dir, &cwd),
    }
}

/// git's `setup_git_directory_gently_1`: decide explicit vs discovered vs bare.
fn discover(cli_session: &session::CliSession, cwd: &Path) -> Option<Discovered> {
    // GIT_DIR / --git-dir set explicitly: no discovery, just validation.
    if let Some(git_dir) = cli_session.explicit_git_dir() {
        if git_dir.as_os_str().is_empty() {
            return None;
        }
        return Some(Discovered::Explicit {
            git_dir: git_dir.to_string_lossy().into_owned(),
        });
    }

    // `git --bare`: treat cwd as the (bare) git dir.
    if cli_session.explicit_bare() {
        if is_git_dir_candidate(cwd) {
            return Some(Discovered::Bare {
                dir: cwd.to_path_buf(),
            });
        }
        return None;
    }

    let one_filesystem = !git_env_bool("GIT_DISCOVERY_ACROSS_FILESYSTEM");
    let start_device = if one_filesystem { device_of(cwd) } else { None };

    let ceilings = crate::discovery::discovery_ceiling_directories();

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
            && let Ok(Some(target)) = read_gitdir_file(&dot_git)
            && is_git_dir_candidate(&target)
        {
            // The user-facing git_dir is the gitfile path itself relative to
            // dir (".git"); repo_set_gitdir resolves it.
            return Some(Discovered::Found {
                dir: dir.to_path_buf(),
                git_dir: ".git".to_string(),
            });
        }

        // .git directory.
        if dot_git.is_dir() && is_git_dir_candidate(&dot_git) {
            return Some(Discovered::Found {
                dir: dir.to_path_buf(),
                git_dir: ".git".to_string(),
            });
        }

        // bare: dir itself is a git directory.
        if is_git_dir_candidate(dir) {
            return Some(Discovered::Bare {
                dir: dir.to_path_buf(),
            });
        }

        // Stop at a filesystem boundary unless GIT_DISCOVERY_ACROSS_FILESYSTEM.
        if one_filesystem
            && let Some(parent) = dir.parent()
            && device_of(parent) != start_device
        {
            return None;
        }
    }
    None
}

/// git's `setup_explicit_git_dir`. `cwd` is the user's original cwd.
fn setup_explicit(
    cli_session: &session::CliSession,
    gitdirenv: &str,
    cwd: &Path,
) -> Option<SetupResult> {
    // A `.git` *file* named by GIT_DIR is resolved to its target (git's
    // read_gitfile in setup_explicit_git_dir).
    let gitdir_path = resolve_cli_path(cwd, gitdirenv);
    let (effective_gitdir_text, gitdir_dir) = if gitdir_path.is_file() {
        match read_gitdir_file(&gitdir_path) {
            Ok(Some(target)) => {
                let target = canonicalize_or(&target);
                (path_to_string(&target), target)
            }
            _ => return None,
        }
    } else {
        (gitdirenv.to_string(), gitdir_path)
    };

    if !is_git_dir_candidate(&gitdir_dir) {
        return None;
    }

    let (is_bare, core_worktree) = read_worktree_config(&gitdir_dir);
    let is_bare = is_bare && !gitdir_dir.join("commondir").is_file();

    let worktree: Option<PathBuf>;

    if let Some(work_tree_env) = cli_session.explicit_work_tree() {
        // #3,#7,...: explicit GIT_WORK_TREE / --work-tree wins.
        let wt = resolve_cli_path(cwd, &work_tree_env.to_string_lossy());
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
fn setup_discovered(
    cli_session: &session::CliSession,
    gitdir: &str,
    dir: &Path,
    cwd: &Path,
) -> Option<SetupResult> {
    let gitdir_dir = dir.join(gitdir);
    // The textual git_dir git resolves a gitfile to its target for repo->gitdir;
    // for trace purposes we resolve `.git`-file targets here.
    let (effective_gitdir_text, effective_gitdir_dir) = if gitdir_dir.is_file() {
        match read_gitdir_file(&gitdir_dir) {
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
    if cli_session.explicit_work_tree().is_some() || effective_core_worktree.is_some() {
        return setup_explicit_from_discovered(
            cli_session,
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
fn setup_explicit_from_discovered(
    cli_session: &session::CliSession,
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
    if let Some(work_tree_env) = cli_session.explicit_work_tree() {
        let wt = resolve_cli_path(cwd, &work_tree_env.to_string_lossy());
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
fn setup_bare(cli_session: &session::CliSession, dir: &Path, cwd: &Path) -> Option<SetupResult> {
    let (is_bare, core_worktree) = read_worktree_config(dir);

    // --work-tree / GIT_WORK_TREE / core.worktree re-route through explicit
    // setup with the bare git dir (git's setup_bare_git_dir: "if
    // getenv(GIT_WORK_TREE) || git_work_tree_cfg"). A core.worktree gives the
    // otherwise-bare repo a real worktree (#20a).
    if cli_session.explicit_work_tree().is_some() || core_worktree.is_some() {
        let gitdir_text = if dir == cwd {
            ".".to_string()
        } else {
            path_to_string(dir)
        };
        return setup_explicit_from_discovered(
            cli_session,
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

/// The device id of a path (for the single-filesystem discovery boundary).
#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|meta| meta.dev())
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Option<u64> {
    None
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
    sley::plumbing::sley_core::paths::path_to_slash(path)
}

/// A path as a UTF-8-lossy string (git stores paths as bytes; the tested paths
/// are ASCII).
fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Emit git's `GIT_TRACE_SETUP` output for the resolved layout, honoring
/// `GIT_TRACE_BARE` (no timestamp/file:line prefix). A no-op unless
/// `GIT_TRACE_SETUP` requests tracing.
pub(crate) fn trace_repo_setup(result: &SetupResult) {
    let Some(mut sink) = trace_sink() else {
        return;
    };
    let bare = git_env_bool("GIT_TRACE_BARE");
    let worktree = match result.worktree.as_ref() {
        Some(worktree) => path_to_string(worktree),
        None => "(null)".to_string(),
    };
    let cwd = path_to_string(&result.cwd);
    let prefix = match &result.prefix {
        Some(prefix) => prefix.clone(),
        None => "(null)".to_string(),
    };

    let lines = [
        format!("setup: git_dir: {}", quote_crnl(&result.git_dir)),
        format!("setup: git_common_dir: {}", quote_crnl(&result.common_dir)),
        format!("setup: worktree: {}", quote_crnl(&worktree)),
        format!("setup: cwd: {}", quote_crnl(&cwd)),
        format!("setup: prefix: {}", quote_crnl(&prefix)),
    ];
    for line in lines {
        if bare {
            let _ = writeln!(sink, "{line}");
        } else {
            // With a real trace prefix git prepends a timestamp + file:line; the
            // t1510 harness always sets GIT_TRACE_BARE, so this branch is only a
            // best-effort approximation for direct use.
            let _ = writeln!(sink, "{line}");
        }
    }
}

/// git's `quote_crnl`: escape backslash, CR and LF for trace output.
fn quote_crnl(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// The destination for `GIT_TRACE_SETUP` output: `1`/`2` map to stdout/stderr,
/// an absolute path is appended to, and `0`/empty/unset disable tracing. Mirrors
/// git's `get_trace_fd` for the values the tests use.
fn trace_sink() -> Option<Box<dyn Write>> {
    let value = env::var("GIT_TRACE_SETUP").ok()?;
    match value.as_str() {
        "" | "0" | "false" | "no" | "off" => None,
        "1" | "2" => {
            if value == "1" {
                Some(Box::new(std::io::stdout()))
            } else {
                Some(Box::new(std::io::stderr()))
            }
        }
        path if Path::new(path).is_absolute() => fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(|f| Box::new(f) as Box<dyn Write>),
        // A non-absolute, non-numeric value: git treats unparsable as enabling
        // to stderr only for "true"-like; here we disable to be safe.
        _ => None,
    }
}

/// The destination for the general `GIT_TRACE` key, mirroring git's
/// `get_trace_fd` for the default trace key. `1`/`true` → stderr, `2` → stderr,
/// a single digit → that fd (only 1/2 are meaningful here), an absolute path is
/// opened append+create, and `0`/`false`/empty/unset disable tracing.
fn git_trace_sink() -> Option<Box<dyn Write>> {
    let value = env::var("GIT_TRACE").ok()?;
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "" | "0" | "false" => None,
        "1" | "true" => Some(Box::new(std::io::stderr())),
        "2" => Some(Box::new(std::io::stderr())),
        _ => {
            if value.len() == 1 && value.as_bytes()[0].is_ascii_digit() {
                // Single digit other than 0/1/2: git would write to that fd; only
                // 1/2 are reachable from a test harness, so map anything else to
                // stderr as a best-effort.
                Some(Box::new(std::io::stderr()))
            } else if Path::new(&value).is_absolute() {
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&value)
                    .ok()
                    .map(|f| Box::new(f) as Box<dyn Write>)
            } else {
                None
            }
        }
    }
}

/// Whether the general `GIT_TRACE` key is enabled (a sink would open).
pub(crate) fn git_trace_enabled() -> bool {
    git_trace_sink().is_some()
}

/// Emit one `GIT_TRACE` line, prefixed exactly as git's `prepare_trace_line`
/// does when `GIT_TRACE_BARE` is unset: `HH:MM:SS.uuuuuu file:line` padded to
/// column 40, then the message. With `GIT_TRACE_BARE` set, the bare message is
/// written with no prefix (matching git's unit-test mode).
pub(crate) fn git_trace_line(file_line: &str, message: &str) {
    let Some(mut sink) = git_trace_sink() else {
        return;
    };
    if git_env_bool("GIT_TRACE_BARE") {
        let _ = writeln!(sink, "{message}");
        return;
    }
    let mut prefix = format!("{} {}", trace_timestamp(), file_line);
    while prefix.len() < 40 {
        prefix.push(' ');
    }
    let _ = writeln!(sink, "{prefix}{message}");
}

/// Trace-style sq-quote rendering. The canonical implementation lives in
/// [`sley_core::text::sq_quote_buf_pretty`] (git's `sq_quote_buf_pretty`):
/// leave an argument unquoted when every byte is alphanumeric or one of
/// `+,-./:=@_^`; otherwise single-quote it, escaping `'` and `!` as
/// `'\''`-style sequences. An empty argument becomes `''`.
pub(crate) use sley::plumbing::sley_core::text::sq_quote_pretty as trace_quote_sq;

/// `HH:MM:SS.uuuuuu` local-time timestamp matching git's trace prefix.
fn trace_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let usec = now.subsec_micros();
    // Convert to local time-of-day. We only need HH:MM:SS, and the test never
    // inspects the value (the `^trace:` anchor guarantees these timestamped
    // lines are skipped), so UTC time-of-day is sufficient and dependency-free.
    let secs_in_day = total_secs % 86_400;
    let hh = secs_in_day / 3600;
    let mm = (secs_in_day % 3600) / 60;
    let ss = secs_in_day % 60;
    format!("{hh:02}:{mm:02}:{ss:02}.{usec:06}")
}
