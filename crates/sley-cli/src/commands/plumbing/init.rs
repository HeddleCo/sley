//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;
use sley::plumbing::sley_config;

fn init_repo_is_implicitly_bare(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
) -> Result<bool> {
    // Determine the effective git directory git would inspect.
    if let Some(git_dir) = cli_session.explicit_git_dir() {
        return Ok(guess_repository_type(&git_dir, cwd));
    }
    // No GIT_DIR: git_dir defaults to ".git". Only a linked-worktree gitfile (whose
    // target has a `commondir`) redirects the inspection to the common repository;
    // a plain separate-git-dir gitfile does not.
    let dot_git = cwd.join(".git");
    if dot_git.is_file()
        && let Some(target) = read_gitdir_file(&dot_git)?
        && target.join("commondir").is_file()
    {
        let common = cli_session.common_git_dir(&target)?;
        return Ok(guess_repository_type(&common, cwd));
    }
    // Otherwise git_dir is ".git", which guess_repository_type treats as non-bare.
    Ok(false)
}

fn guess_repository_type(git_dir: &Path, cwd: &Path) -> bool {
    // "GIT_DIR=. git init" — and "GIT_DIR=$(pwd) git init" — are always bare.
    if git_dir == Path::new(".") {
        return true;
    }
    if git_dir == cwd {
        return true;
    }
    // "GIT_DIR=.git" or "GIT_DIR=something/.git" is usually NOT bare.
    if git_dir == Path::new(".git") {
        return false;
    }
    if git_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".git")
    {
        return false;
    }
    // Otherwise it is often bare. At this point git is just guessing.
    true
}

pub(crate) fn cmd_init(
    cli_session: &crate::session::CliSession,
    args: &[String],
    global_config: &[GlobalConfigOverride],
) -> Result<()> {
    let session_bare = cli_session.explicit_bare();
    let mut bare = session_bare;
    // git distinguishes an *explicitly requested* bare repo (`--bare`/global
    // `--bare`) from one merely *guessed* from the environment. The former pairs
    // with `--separate-git-dir` as "cannot be used together"; the latter as
    // "incompatible with bare repository". Track the explicit signal separately
    // from the `.git`-suffix path heuristic applied further down.
    let mut bare_explicit = session_bare;
    let mut object_format = None::<String>;
    let mut ref_format = None::<Option<String>>;
    let mut initial_branch = None::<String>;
    let mut initial_branch_explicit = false;
    let mut quiet = false;
    let mut path = PathBuf::from(".");
    let mut path_given = false;
    let mut template = None::<Option<String>>;
    let mut template_config = true;
    let mut separate_git_dir = None::<String>;
    let mut shared_repository = None::<Option<String>>;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--bare" => {
                bare = true;
                bare_explicit = true;
            }
            "-q" | "--quiet" => quiet = true,
            "-s" | "--shared" => shared_repository = Some(Some("group".into())),
            "--no-shared" => shared_repository = Some(None),
            "-b" | "--initial-branch" => {
                initial_branch = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command(format!("{arg} requires a value")))?
                        .to_string(),
                );
                initial_branch_explicit = true;
            }
            "--object-format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-format requires a value".into()))?;
                object_format = Some(value.to_string());
            }
            "--template" => {
                template = Some(Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("--template requires a value".into()))?
                        .to_string(),
                ));
                template_config = true;
            }
            "--no-template" => {
                template = Some(None);
                template_config = false;
            }
            "--separate-git-dir" => {
                separate_git_dir = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("--separate-git-dir requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--no-separate-git-dir" => separate_git_dir = None,
            "--ref-format" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--ref-format requires a value".into()))?;
                ref_format = Some(Some(value.to_string()));
            }
            "--no-ref-format" => ref_format = Some(None),
            value if value.starts_with("--initial-branch=") => {
                initial_branch = Some(
                    value
                        .strip_prefix("--initial-branch=")
                        .ok_or_else(|| {
                            GitError::Command("--initial-branch requires a value".into())
                        })?
                        .to_string(),
                );
                initial_branch_explicit = true;
            }
            value if value.starts_with("--object-format=") => {
                let value = value
                    .strip_prefix("--object-format=")
                    .ok_or_else(|| GitError::Command("--object-format requires a value".into()))?;
                object_format = Some(value.to_string());
            }
            value if value.starts_with("--template=") => {
                template = Some(Some(
                    value
                        .strip_prefix("--template=")
                        .ok_or_else(|| GitError::Command("--template requires a value".into()))?
                        .to_string(),
                ));
                template_config = true;
            }
            value if value.starts_with("--separate-git-dir=") => {
                separate_git_dir = Some(
                    value
                        .strip_prefix("--separate-git-dir=")
                        .ok_or_else(|| {
                            GitError::Command("--separate-git-dir requires a value".into())
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--shared=") => {
                shared_repository = Some(Some(
                    value
                        .strip_prefix("--shared=")
                        .ok_or_else(|| GitError::Command("--shared requires a value".into()))?
                        .to_string(),
                ));
            }
            value if value.starts_with("--ref-format=") => {
                ref_format = Some(Some(
                    value
                        .strip_prefix("--ref-format=")
                        .ok_or_else(|| GitError::Command("--ref-format requires a value".into()))?
                        .to_string(),
                ));
            }
            value => {
                path = PathBuf::from(value);
                path_given = true;
            }
        }
    }

    let cwd = cli_session.cwd().to_path_buf();
    let init_config_git_dir = init_config_git_dir_for_lookup(
        cli_session,
        &cwd,
        &path,
        bare,
        separate_git_dir.as_deref(),
    )?;

    // Mirror refs.c `repo_default_branch_name`: an explicit `--initial-branch`
    // wins; otherwise `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME` (when non-empty),
    // then `init.defaultBranch`, then "master" (which triggers the
    // `advice.defaultBranchName` hint, emitted after a successful fresh init).
    // A name sourced from the env/config default dies with git's
    // `invalid branch name: init.defaultBranch = <name>`; an explicit
    // `--initial-branch` dies with `invalid initial branch name: '<name>'`
    // (init-db.c).
    let mut branch_defaulted = false;
    let initial_branch = match initial_branch {
        Some(branch) => {
            if check_refname_format(&format!("refs/heads/{branch}"), false).is_err() {
                eprintln!("fatal: invalid initial branch name: '{branch}'");
                return Err(GitError::Exit(128));
            }
            branch
        }
        None => {
            let default_name = env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME")
                .ok()
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || {
                        init_config_value(
                            "init.defaultBranch",
                            global_config,
                            init_config_git_dir.as_deref(),
                        )
                    },
                    |name| Ok(Some(name)),
                )?
                .filter(|value| !value.is_empty());
            match default_name {
                Some(name) => {
                    if check_refname_format(&format!("refs/heads/{name}"), false).is_err() {
                        eprintln!("fatal: invalid branch name: init.defaultBranch = {name}");
                        return Err(GitError::Exit(128));
                    }
                    name
                }
                None => {
                    branch_defaulted = true;
                    "master".to_string()
                }
            }
        }
    };

    let worktree = resolve_cli_path(&cwd, path.to_string_lossy().as_ref());
    let separate_git_dir = separate_git_dir.map(|value| resolve_cli_path(&cwd, &value));

    if separate_git_dir.is_some() {
        if bare_explicit {
            // init-db.c: `real_git_dir && is_bare_repository_cfg == 1` where the
            // `1` came from the `--bare` option.
            eprintln!("fatal: options '--bare' and '--separate-git-dir' cannot be used together");
            return Err(GitError::Exit(128));
        }
        // init-db.c later sets `is_bare_repository_cfg = guess_repository_type(git_dir)`
        // when bare was not explicit, then rejects `--separate-git-dir` against an
        // implicitly-bare repository (e.g. `GIT_DIR=.`, or inside a linked worktree
        // whose common repository is bare).
        if init_repo_is_implicitly_bare(cli_session, &cwd)? {
            eprintln!("fatal: --separate-git-dir incompatible with bare repository");
            return Err(GitError::Exit(128));
        }
    }

    // init-db.c: GIT_WORK_TREE (or --work-tree) only makes sense together with
    // GIT_DIR and without an explicit `--bare`. After chdir'ing into the target
    // directory, `--bare` pins GIT_DIR to that directory (overwriting the
    // environment when a directory argument was given); the effective git dir
    // then comes from GIT_DIR and its *string* form drives the bare guess.
    let env_git_dir = cli_session.explicit_git_dir();
    let env_work_tree = cli_session.explicit_work_tree();
    if env_work_tree.is_some() && (bare_explicit || env_git_dir.is_none()) {
        eprintln!(
            "fatal: GIT_WORK_TREE (or --work-tree=<directory>) not allowed without specifying GIT_DIR (or --git-dir=<directory>)"
        );
        return Err(GitError::Exit(128));
    }

    let mut worktree = worktree;
    let mut git_dir_override = None::<PathBuf>;
    let mut core_worktree = None::<String>;
    // Re-initializing from *inside* a linked worktree operates on the shared
    // repository: git's setup discovers the common git dir and the *main*
    // worktree, so `init --separate-git-dir` there relocates the common dir and
    // repoints the main worktree's `.git` (init-db.c works on the discovered
    // repository, not the linked-worktree admin dir). Redirect `worktree` to the
    // main worktree root before bootstrap so `.git` resolves to the common dir.
    if !bare && env_git_dir.is_none() && env_work_tree.is_none() {
        let dot_git = worktree.join(".git");
        if dot_git.is_file()
            && let Some(admin_dir) = read_gitdir_file(&dot_git)?
            && admin_dir.join("commondir").is_file()
        {
            let common = cli_session.common_git_dir(&admin_dir)?;
            if let Some(main_root) = common.parent() {
                worktree = main_root.to_path_buf();
            }
        }
    }
    if bare_explicit {
        // `--bare` without a directory argument leaves an existing GIT_DIR in
        // charge of where the (bare) repository lives.
        if !path_given && let Some(raw) = env_git_dir {
            git_dir_override = Some(resolve_cli_path(&worktree, raw.to_string_lossy().as_ref()));
        }
    } else if let Some(raw) = env_git_dir
        && separate_git_dir.is_none()
        && !bare
    {
        let git_dir_abs = resolve_cli_path(&worktree, raw.to_string_lossy().as_ref());
        if guess_repository_type(&raw, &worktree) {
            match env_work_tree {
                // Guessed-bare git dir + GIT_WORK_TREE: the repository is
                // *non*-bare after all; record `core.worktree` (init-db.c sets
                // the work tree, so `create_default_files` writes it).
                Some(raw_work_tree) => {
                    let work_tree_abs =
                        resolve_cli_path(&worktree, raw_work_tree.to_string_lossy().as_ref());
                    let work_tree_abs = match fs::canonicalize(&work_tree_abs) {
                        Ok(path) => path,
                        Err(_) => work_tree_abs,
                    };
                    if git_dir_abs != work_tree_abs.join(".git") {
                        core_worktree = Some(work_tree_abs.to_string_lossy().into_owned());
                    }
                    git_dir_override = Some(git_dir_abs);
                    worktree = work_tree_abs;
                }
                // Plain guessed-bare GIT_DIR (e.g. `GIT_DIR=dir.git git init`):
                // a bare repository at that directory.
                None => {
                    git_dir_override = Some(git_dir_abs);
                    bare = true;
                }
            }
        } else {
            // Non-bare guess (".git" or "…/.git"): the work tree is the git
            // dir's parent (or the target directory), unless GIT_WORK_TREE
            // overrides it.
            let work_tree_abs = match env_work_tree {
                Some(raw_work_tree) => {
                    let resolved =
                        resolve_cli_path(&worktree, raw_work_tree.to_string_lossy().as_ref());
                    match fs::canonicalize(&resolved) {
                        Ok(path) => path,
                        Err(_) => resolved,
                    }
                }
                None => match git_dir_abs.parent() {
                    Some(parent) => parent.to_path_buf(),
                    None => worktree.clone(),
                },
            };
            if git_dir_abs != work_tree_abs.join(".git") {
                core_worktree = Some(work_tree_abs.to_string_lossy().into_owned());
            }
            git_dir_override = Some(git_dir_abs);
            worktree = work_tree_abs;
        }
    }

    let (object_format, object_format_explicit) =
        resolve_init_object_format(object_format, global_config, init_config_git_dir.as_deref())?;
    let (ref_storage, ref_storage_explicit) =
        resolve_init_ref_storage(ref_format, global_config, init_config_git_dir.as_deref())?;
    let shared_repository = resolve_init_shared_repository(
        shared_repository,
        global_config,
        bare,
        init_config_git_dir.as_deref(),
    )?;
    let shared_repository = match shared_repository {
        Some(value) => sley::plumbing::sley_formats::canonical_shared_repository_value(&value)?,
        None => None,
    };
    let template_dir = resolve_init_template_dir(
        template,
        template_config,
        global_config,
        &cwd,
        init_config_git_dir.as_deref(),
    )?;

    // `GIT_OBJECT_DIRECTORY` (init-db.c → setup.c `create_object_directory`) places
    // the object store outside `$GIT_DIR/objects`. Git chdirs into a directory
    // argument first, so a *relative* value is resolved against that target; with
    // no directory argument it stays relative to the original cwd. Absolute values
    // are used as-is. Only `info/` and `pack/` are created at this path — the
    // default `$GIT_DIR/objects` tree is never materialised (t0001 #103).
    let object_dir = env::var_os("GIT_OBJECT_DIRECTORY")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else if path_given {
                worktree.join(path)
            } else {
                cwd.join(path)
            }
        });

    let layout = RepositoryBootstrap::init(InitOptions {
        worktree,
        git_dir_override,
        core_worktree,
        object_dir,
        object_format,
        object_format_explicit,
        bare,
        initial_branch: initial_branch.clone(),
        template_dir,
        copy_template_config: template_config,
        separate_git_dir,
        shared_repository,
        ref_storage,
        ref_storage_explicit,
    })
    .map_err(|err| match err {
        // Bootstrap reports fatal init failures (e.g. reinitializing with a different
        // object/ref format) as `GitError::Command`; git prints these as `fatal: <msg>`
        // and exits 128.
        GitError::Command(message) => {
            eprintln!("fatal: {message}");
            GitError::Exit(128)
        }
        other => other,
    })?;

    if !layout.reinitialized
        && init_config_bool(
            "init.defaultSubmodulePathConfig",
            global_config,
            init_config_git_dir.as_deref(),
        )? == Some(true)
    {
        crate::enable_submodule_path_config_extension(&layout.git_dir)?;
    }

    if branch_defaulted && !quiet && !layout.reinitialized {
        emit_default_branch_advice(
            &initial_branch,
            global_config,
            init_config_git_dir.as_deref(),
        )?;
    }
    if layout.reinitialized && initial_branch_explicit {
        eprintln!("warning: re-init: ignored --initial-branch={initial_branch}");
    }
    if !quiet {
        let git_dir = fs::canonicalize(&layout.git_dir)?;
        print_init_repository_message(layout.reinitialized, false, &git_dir)?;
    }
    Ok(())
}

/// Emit the translated `Initialized empty Git repository in …` line.
///
/// Uses gettext when `git-compat-i18n` is enabled so upstream tests like
/// `t0204-gettext-reencode-sanity` see Icelandic (and re-encoded ISO-8859-1)
/// output under `LANGUAGE=is`. Falls back to English otherwise.
fn print_init_repository_message(reinitialized: bool, shared: bool, git_dir: &Path) -> Result<()> {
    let path = git_dir.to_string_lossy();
    let slash = if path.ends_with('/') { "" } else { "/" };
    let msgid = match (reinitialized, shared) {
        (false, false) => "Initialized empty Git repository in %s%s\n",
        (false, true) => "Initialized empty shared Git repository in %s%s\n",
        (true, false) => "Reinitialized existing Git repository in %s%s\n",
        (true, true) => "Reinitialized existing shared Git repository in %s%s\n",
    };
    let message = init_gettext_printf(msgid, &[&path, slash]);
    let mut out = std::io::stdout().lock();
    out.write_all(&message)?;
    out.flush()?;
    Ok(())
}

#[cfg(feature = "git-compat-i18n")]
fn init_gettext_printf(msgid: &str, args: &[&str]) -> Vec<u8> {
    sley_i18n::gettext_printf(msgid, args)
}

#[cfg(not(feature = "git-compat-i18n"))]
fn init_gettext_printf(msgid: &str, args: &[&str]) -> Vec<u8> {
    // Minimal English fallback: expand %s and keep UTF-8.
    let mut out = String::new();
    let mut arg_idx = 0;
    let bytes = msgid.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() && bytes[i + 1] == b's' {
            if let Some(arg) = args.get(arg_idx) {
                out.push_str(arg);
                arg_idx += 1;
            }
            i += 2;
            continue;
        }
        let ch = msgid[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out.into_bytes()
}

fn init_config_git_dir_for_lookup(
    cli_session: &crate::session::CliSession,
    cwd: &Path,
    path: &Path,
    bare: bool,
    separate_git_dir: Option<&str>,
) -> Result<Option<PathBuf>> {
    if let Some(raw) = separate_git_dir {
        return Ok(Some(resolve_cli_path(cwd, raw)));
    }
    if let Some(raw) = cli_session.explicit_git_dir() {
        let git_dir = resolve_cli_path(cwd, raw.to_string_lossy().as_ref());
        if git_dir.is_file()
            && let Some(target) = read_gitdir_file(&git_dir)?
        {
            return cli_session.common_git_dir(&target).map(Some);
        }
        return Ok(Some(git_dir));
    }
    let target = resolve_cli_path(cwd, path.to_string_lossy().as_ref());
    if bare {
        Ok(Some(target))
    } else {
        let git_file = target.join(".git");
        if git_file.is_file()
            && let Some(git_dir) = read_gitdir_file(&git_file)?
        {
            return cli_session.common_git_dir(&git_dir).map(Some);
        }
        Ok(Some(git_file))
    }
}

fn emit_default_branch_advice(
    branch: &str,
    global_config: &[GlobalConfigOverride],
    config_git_dir: Option<&Path>,
) -> Result<()> {
    if let Ok(value) = env::var("GIT_ADVICE") {
        let enabled = match parse_config_bool(&value) {
            Some(value) => value,
            None => !value.is_empty(),
        };
        if !enabled {
            return Ok(());
        }
    }
    if init_config_bool("advice.defaultBranchName", global_config, config_git_dir)? == Some(false) {
        return Ok(());
    }
    // `color.advice`: "always" colours unconditionally; "never"/false disables;
    // "auto"/true/unset colour only when stderr is a terminal (color.c
    // `git_config_colorbool` + `want_color_stderr`).
    let colored = match init_config_value("color.advice", global_config, config_git_dir)?.as_deref()
    {
        Some(value) if value.eq_ignore_ascii_case("always") => true,
        Some(value) if value.eq_ignore_ascii_case("never") => false,
        Some(value) if value.eq_ignore_ascii_case("auto") => stderr_is_terminal(),
        Some(value) => match parse_config_bool(value) {
            Some(false) => false,
            _ => stderr_is_terminal(),
        },
        None => stderr_is_terminal(),
    };
    let (color, reset) = if colored {
        ("\x1b[33m", "\x1b[m")
    } else {
        ("", "")
    };
    // The advice body already ends without a trailing newline; the
    // `Disable this message ...` instruction line was appended above with the
    // leading blank line git's `turn_off_instructions` carries.
    let body = DEFAULT_BRANCH_NAME_ADVICE.replacen("{}", branch, 1);
    for line in body.split('\n') {
        let sep = if line.is_empty() { "" } else { " " };
        eprintln!("{color}hint:{sep}{line}{reset}");
    }
    Ok(())
}

fn stderr_is_terminal() -> bool {
    use std::io::IsTerminal;
    io::stderr().is_terminal()
}

/// [`RepositoryBootstrap::init`], once the existing repository format is known.
fn resolve_init_object_format(
    cli_format: Option<String>,
    global_config: &[GlobalConfigOverride],
    config_git_dir: Option<&Path>,
) -> Result<(ObjectFormat, bool)> {
    // git reads the config defaults FIRST (setup.c `read_default_format_config`),
    // so an invalid `init.defaultObjectFormat` warns even when the command line
    // or `GIT_DEFAULT_HASH` ends up choosing the format.
    let config_format =
        match init_config_value("init.defaultObjectFormat", global_config, config_git_dir)? {
            Some(value) => match value.parse::<ObjectFormat>() {
                Ok(format) => Some(format),
                Err(_) => {
                    eprintln!("warning: unknown hash algorithm '{value}'");
                    None
                }
            },
            None => None,
        };
    if let Some(value) = cli_format {
        return Ok((parse_init_object_format(&value)?, true));
    }
    if let Ok(hash) = env::var("GIT_DEFAULT_HASH")
        && !hash.is_empty()
    {
        return Ok((parse_init_object_format(&hash)?, false));
    }
    if let Some(format) = config_format {
        return Ok((format, false));
    }
    Ok((ObjectFormat::Sha1, false))
}

fn parse_init_object_format(value: &str) -> Result<ObjectFormat> {
    value.parse::<ObjectFormat>().map_err(|_| {
        eprintln!("fatal: unknown hash algorithm '{value}'");
        GitError::Exit(128)
    })
}

fn resolve_init_ref_storage(
    cli_ref_format: Option<Option<String>>,
    global_config: &[GlobalConfigOverride],
    config_git_dir: Option<&Path>,
) -> Result<(RefStorageFormat, bool)> {
    // git reads the config defaults FIRST (setup.c `read_default_format_config`),
    // so an invalid `init.defaultRefFormat` warns even when the command line or
    // `GIT_DEFAULT_REF_FORMAT` ends up choosing the format.
    let config_format =
        match init_config_value("init.defaultRefFormat", global_config, config_git_dir)? {
            Some(value) if value.is_empty() => Some(RefStorageFormat::Files),
            Some(value) => match RefStorageFormat::parse(&value) {
                Ok(format) => Some(format),
                Err(_) => {
                    eprintln!("warning: unknown ref storage format '{value}'");
                    None
                }
            },
            None => None,
        };
    if let Some(value) = cli_ref_format {
        let value: &str = value.as_deref().unwrap_or_default();
        return Ok((parse_init_ref_storage(value)?, true));
    }
    if let Ok(value) = env::var("GIT_DEFAULT_REF_FORMAT") {
        return Ok((parse_init_ref_storage(&value)?, false));
    }
    if let Some(format) = config_format {
        return Ok((format, false));
    }
    if init_config_bool("feature.experimental", global_config, config_git_dir)? == Some(true) {
        return Ok((RefStorageFormat::Reftable, false));
    }
    Ok((RefStorageFormat::Files, false))
}

fn parse_init_ref_storage(value: &str) -> Result<RefStorageFormat> {
    RefStorageFormat::parse(value).map_err(|err| match err {
        GitError::Command(message) => {
            eprintln!("fatal: {message}");
            GitError::Exit(128)
        }
        other => other,
    })
}

fn resolve_init_shared_repository(
    cli_shared: Option<Option<String>>,
    global_config: &[GlobalConfigOverride],
    bare: bool,
    config_git_dir: Option<&Path>,
) -> Result<Option<String>> {
    if let Some(value) = cli_shared {
        return Ok(value);
    }
    if bare {
        let existing_repository =
            config_git_dir.is_some_and(|git_dir| git_dir.join("config").is_file());
        if !existing_repository {
            return Ok(None);
        }
    }
    init_config_value("core.sharedRepository", global_config, config_git_dir)
}

fn resolve_init_template_dir(
    cli_template: Option<Option<String>>,
    template_config: bool,
    global_config: &[GlobalConfigOverride],
    cwd: &Path,
    config_git_dir: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let _ = template_config;
    match cli_template {
        Some(None) => Ok(None),
        Some(Some(path)) => {
            if path.is_empty() {
                Ok(Some(PathBuf::new()))
            } else {
                Ok(Some(resolve_cli_path(cwd, &path)))
            }
        }
        None => {
            if let Some(path) =
                init_config_value("init.templatedir", global_config, config_git_dir)?
            {
                let expanded = sley_config::expand_user_path(&path);
                Ok(Some(if expanded.is_absolute() {
                    expanded
                } else {
                    cwd.join(expanded)
                }))
            } else if let Ok(path) = env::var("GIT_TEMPLATE_DIR") {
                if path.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(resolve_cli_path(cwd, &path)))
                }
            } else {
                Ok(None)
            }
        }
    }
}

fn init_config_bool(
    key: &str,
    global_config: &[GlobalConfigOverride],
    config_git_dir: Option<&Path>,
) -> Result<Option<bool>> {
    init_config_value(key, global_config, config_git_dir)
        .map(|value| value.as_deref().and_then(parse_config_bool))
}
