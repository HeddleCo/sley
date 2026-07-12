//! Init-time config lookup and repository-format bootstrap helpers.

use std::env;
use std::path::{Path, PathBuf};

use sley::plumbing::sley_config;
use sley::{GitConfig, GitError, Result};

use crate::commands::remote::write_repo_config;
use crate::global_options::{
    GlobalConfigOverride, global_config_value, injected_config_parameters,
};

/// Replicate git's implicit-bare determination for `git init --separate-git-dir`.
///
/// git computes `is_bare_repository_cfg = guess_repository_type(git_dir)` only when
/// `--bare` was not given (init-db.c). `git_dir` is `GIT_DIR` when set; otherwise it
/// defaults to `.git`, *unless* `.git` is a gitfile for a linked worktree — i.e. the
/// gitfile's target contains a `commondir` file — in which case git chdir's to the
/// main worktree and inspects the resolved *common* git directory instead. A plain
/// `--separate-git-dir` gitfile (no `commondir`) leaves `git_dir == ".git"`, which is
/// never bare. `guess_repository_type` treats `GIT_DIR=.` / `GIT_DIR=$cwd` and any
/// path not ending in `/.git` as bare, and `.git` / `*/.git` as non-bare; for a bare
/// clone behind a worktree (e.g. `git clone --bare` + `git worktree add`) the common
/// dir is `…/bare.git`, which `guess_repository_type` already reports as bare.

/// Mirror of git's `guess_repository_type()` (builtin/init-db.c): decide whether a
/// git directory path implies a bare repository.

/// git's `default_branch_name_advice` (refs.c, non-WITH_BREAKING_CHANGES build),
/// emitted through `advise_if_enabled(ADVICE_DEFAULT_BRANCH_NAME, ...)` when an
/// unconfigured `git init` falls back to "master".
pub(crate) const DEFAULT_BRANCH_NAME_ADVICE: &str = "Using '{}' as the name for the initial branch. This default branch name\n\
will change to \"main\" in Git 3.0. To configure the initial branch name\n\
to use in all of your new repositories, which will suppress this warning,\n\
call:\n\
\n\
\tgit config --global init.defaultBranch <name>\n\
\n\
Names commonly chosen instead of 'master' are 'main', 'trunk' and\n\
'development'. The just-created branch can be renamed via this command:\n\
\n\
\tgit branch -m <name>\n\
\n\
Disable this message with \"git config set advice.defaultBranchName false\"";

/// Mirror git's `advise_if_enabled` for the unconfigured-default-branch hint:
/// gated on the `GIT_ADVICE` env bool and `advice.defaultBranchName`, rendered
/// line-by-line as `hint: <line>` on stderr, coloured per `color.advice`
/// (advice.c `vadvise`; the hint colour is yellow).

/// Resolve the object format for a *fresh* init, returning the chosen format and
/// whether it was specified explicitly on the command line.
///
/// Mirrors git's `repository_format_configure` precedence: an explicit
/// `--object-format` wins (and a bad value is fatal); otherwise `GIT_DEFAULT_HASH`
/// is consulted (also fatal on a bad value); otherwise the `init.defaultObjectFormat`
/// config default is used (a bad value here only warns and falls back to sha1). The
/// reinitialize-with-different-hash guard is applied later in
/// [`RepositoryBootstrap::init`], once the existing repository format is known.

/// Parse an object-format name the way git's `init` does: an unrecognised value is a
/// `fatal: unknown hash algorithm '<value>'` with exit status 128.

/// Resolve the ref storage format for a *fresh* init, returning the chosen format and
/// whether it was specified explicitly on the command line.
///
/// Mirrors git's `repository_format_configure` precedence: an explicit `--ref-format`
/// wins (and a bad value is fatal); otherwise `GIT_DEFAULT_REF_FORMAT` is consulted
/// (also fatal on a bad value); otherwise the `init.defaultRefFormat` config default is
/// used (a bad value here only warns and falls back to the default), with
/// `feature.experimental` selecting reftable as the last resort. The
/// reinitialize-with-different-format guard is applied later in
/// [`RepositoryBootstrap::init`], once the existing repository format is known.

pub(crate) fn init_config_value(
    key: &str,
    global_config: &[GlobalConfigOverride],
    config_git_dir: Option<&Path>,
) -> Result<Option<String>> {
    if let Some(value) = global_config
        .iter()
        .rev()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
        .map(|entry| entry.value.clone())
    {
        return Ok(Some(value));
    }
    if let Ok(Some(value)) = global_config_value(key) {
        return Ok(Some(value));
    }
    let context = match config_git_dir {
        Some(git_dir) => sley_config::ConfigIncludeContext::new(
            Some(sley_config::git_dir_for_include_context(git_dir)),
            sley_config::repo_current_branch_name(git_dir),
        ),
        None => sley_config::ConfigIncludeContext::new(None, None),
    };
    let mut config = sley_config::load_pre_dispatch_config(config_git_dir, &context)
        .map_err(report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    let base = match env::current_dir() {
        Ok(path) => path,
        Err(_) => PathBuf::from("."),
    };
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &base,
    )
    .map_err(report_config_setup_error)?;
    let (section, entry_key) = key
        .split_once('.')
        .ok_or_else(|| GitError::Command(format!("invalid config key {key}")))?;
    Ok(config.get(section, None, entry_key).map(str::to_owned))
}

/// `init.defaultBranch` from the global/injected config, used by `git clone`
/// when an empty/unborn remote leaves it to name the local default branch.
/// Looked up with no repository context (clone runs before the new repo's config
/// is relevant), so it consults injected `-c` overrides and the global config.
pub(crate) fn clone_init_default_branch_config() -> Result<Option<String>> {
    init_config_value("init.defaultBranch", &[], None)
}

pub(crate) fn clone_init_default_submodule_path_config() -> Result<bool> {
    Ok(
        init_config_value("init.defaultSubmodulePathConfig", &[], None)?
            .as_deref()
            .and_then(parse_config_bool)
            .unwrap_or(false),
    )
}

pub(crate) fn enable_submodule_path_config_extension(git_dir: &Path) -> Result<()> {
    let mut config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    crate::set_config_value(&mut config, "core", None, "repositoryformatversion", "1");
    crate::set_config_value(
        &mut config,
        "extensions",
        None,
        "submodulePathConfig",
        "true",
    );
    write_repo_config(git_dir, &config)
}

pub(crate) fn submodule_path_config_enabled(git_dir: &Path) -> bool {
    GitConfig::read(git_dir.join("config"))
        .ok()
        .and_then(|config| config.get_bool("extensions", None, "submodulePathConfig"))
        .unwrap_or(false)
}

pub(crate) fn report_config_setup_error(err: GitError) -> GitError {
    match err {
        GitError::InvalidFormat(message) => {
            if message == "relative config includes must come from files"
                || message.starts_with("exceeded maximum include depth")
            {
                eprintln!("fatal: {message}");
                return GitError::Exit(128);
            }
            if message
                == "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
            {
                eprintln!("fatal: {message}");
                return GitError::Exit(128);
            }
            if let Some((line, path)) = parse_bad_config_line_with_path(&message) {
                eprintln!("fatal: bad config line {line} in file {path}");
                return GitError::Exit(128);
            }
            if let Some(line) = parse_bad_config_line_without_path(&message) {
                eprintln!("fatal: bad config line {line}");
                return GitError::Exit(128);
            }
            GitError::InvalidFormat(message)
        }
        other => other,
    }
}

pub(crate) fn parse_bad_config_line_with_path(message: &str) -> Option<(&str, &str)> {
    let rest = message.strip_prefix("config line ")?;
    let (line, rest) = rest.split_once(" in file ")?;
    let path = match rest.rsplit_once(':') {
        Some((path, _detail)) => path,
        None => rest,
    };
    Some((line, path))
}

pub(crate) fn parse_bad_config_line_without_path(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("config line ")?;
    let (line, _detail) = rest.split_once(':')?;
    Some(line)
}

pub(crate) fn parse_config_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}
