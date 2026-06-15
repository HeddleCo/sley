//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

#[derive(Clone, Copy)]
enum RevParsePathFormat {
    Default,
    Absolute,
    Relative,
}

pub(crate) fn cmd_rev_parse(args: &[String]) -> Result<()> {
    if rev_parse_args_need_no_repository(args)? {
        return Ok(());
    }
    let cwd = env::current_dir()?;
    let git_dir = match discover_git_dir(&cwd) {
        Ok(git_dir) => git_dir,
        Err(GitError::NotFound(_)) => {
            if args.is_empty() {
                return Err(GitError::Command("rev-parse requires <rev>...".into()));
            }
            return rev_parse_not_git_repository();
        }
        Err(err) => return Err(err),
    };
    // git's repository setup validates the repository format (version vs
    // extensions) before rev-parse processes any argument; a bare `rev-parse`
    // in a malformed repository must still die (t0001 #60/#62/#64).
    verify_repository_format(&git_dir)?;
    if args.is_empty() {
        return Err(GitError::Command("rev-parse requires <rev>...".into()));
    }
    let format = repository_object_format(&git_dir)?;
    let mut short = None;
    let mut short_revs = 0usize;
    let mut verify = false;
    let mut verified_revs = 0usize;
    let mut quiet = false;
    let mut abbrev_ref = false;
    let mut symbolic_full_name = false;
    let mut path_format = RevParsePathFormat::Default;
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        match arg.as_str() {
            "--" if verify => break,
            "--end-of-options" if verify => {}
            "--git-dir" => println!("{}", display_git_dir(&cwd, &git_dir, path_format)?),
            "--absolute-git-dir" => println!("{}", fs::canonicalize(&git_dir)?.display()),
            "--git-common-dir" => {
                println!("{}", display_git_common_dir(&cwd, &git_dir, path_format)?);
            }
            "--git-path" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_git_path_requires_argument_error)?;
                println!("{}", display_git_path(&cwd, &git_dir, path_format, path)?);
            }
            "--resolve-git-dir" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_resolve_git_dir_requires_argument_error)?;
                println!("{}", resolve_git_dir_arg(&cwd, path)?);
            }
            "--show-toplevel" => {
                if !is_inside_work_tree(&cwd, &git_dir)? {
                    return rev_parse_requires_work_tree();
                }
                let root = worktree_root_for_git_dir(&git_dir)?;
                match path_format {
                    RevParsePathFormat::Default | RevParsePathFormat::Absolute => {
                        println!("{}", root.display());
                    }
                    RevParsePathFormat::Relative => {
                        println!("{}", relative_path_from(&cwd, &root)?)
                    }
                }
            }
            "--show-prefix" => {
                if is_inside_work_tree(&cwd, &git_dir)? {
                    println!("{}", worktree_prefix(&cwd, &git_dir)?);
                } else {
                    println!();
                }
            }
            "--show-cdup" => {
                if is_inside_work_tree(&cwd, &git_dir)? {
                    println!("{}", worktree_cdup(&cwd, &git_dir)?);
                }
            }
            "--show-superproject-working-tree" => {
                if let Some(root) = superproject_working_tree(&git_dir)? {
                    println!("{}", root.display());
                }
            }
            "--show-object-format"
            | "--show-object-format=storage"
            | "--show-object-format=input"
            | "--show-object-format=output" => println!("{}", format.name()),
            "--show-ref-format" => println!("{}", repository_ref_storage_format(&git_dir)?),
            "--local-env-vars" => print_local_env_vars(),
            "--sq-quote" => {
                print_rev_parse_sq_quote(&args[idx + 1..])?;
                break;
            }
            "--path-format=absolute" => path_format = RevParsePathFormat::Absolute,
            "--path-format=relative" => path_format = RevParsePathFormat::Relative,
            "--path-format" => return rev_parse_path_format_requires_argument(),
            "--is-inside-work-tree" => {
                println!("{}", is_inside_work_tree(&cwd, &git_dir)?);
            }
            "--is-inside-git-dir" => println!("{}", is_inside_git_dir(&cwd, &git_dir)?),
            "--is-bare-repository" => println!("{}", is_bare_repository(&git_dir)?),
            "--is-shallow-repository" => println!("{}", is_shallow_repository(&git_dir)),
            "--short" => short = repository_abbrev(&git_dir, format)?,
            "--verify" => verify = true,
            "--quiet" | "-q" => quiet = true,
            "--abbrev-ref" | "--abbrev-ref=strict" | "--abbrev-ref=loose" => abbrev_ref = true,
            "--symbolic-full-name" => symbolic_full_name = true,
            "--bisect" => rev_parse_bisect(&git_dir, format, symbolic_full_name)?,
            value if value.starts_with('-') => {
                if let Some(value) = value.strip_prefix("--short=") {
                    short = Some(parse_abbrev(value)?.max(4));
                    idx += 1;
                    continue;
                }
                // Date-bound options are rewritten the way `git log` consumes
                // them: `--since=`/`--after=` lower-bound the date (an upper bound
                // on age, `--max-age=`), `--before=`/`--until=` do the reverse.
                // The date is parsed to a Unix timestamp; `--max-age=`/`--min-age=`
                // are already in that form and pass through verbatim.
                if let Some(date) = value
                    .strip_prefix("--since=")
                    .or_else(|| value.strip_prefix("--after="))
                {
                    println!("--max-age={}", log_parse_date_cutoff(date)?);
                    idx += 1;
                    continue;
                }
                if let Some(date) = value
                    .strip_prefix("--before=")
                    .or_else(|| value.strip_prefix("--until="))
                {
                    println!("--min-age={}", log_parse_date_cutoff(date)?);
                    idx += 1;
                    continue;
                }
                if value.starts_with("--max-age=") || value.starts_with("--min-age=") {
                    println!("{value}");
                    idx += 1;
                    continue;
                }
                if let Some(value) = value.strip_prefix("--path-format=") {
                    return rev_parse_unknown_path_format(value);
                }
                if let Some(value) = value.strip_prefix("--show-object-format=") {
                    return rev_parse_unknown_show_object_format(value);
                }
                return Err(GitError::Command(format!(
                    "unsupported rev-parse option {value}"
                )));
            }
            rev => {
                if verify {
                    verified_revs += 1;
                    if verified_revs > 1 {
                        return rev_parse_needed_single_revision(quiet);
                    }
                }
                // A leading `^` marks an excluded revision (rev-list's "not this
                // one"). git resolves the remainder exactly like a positive arg
                // and prefixes the rendered output with `^`; the same applies to
                // --abbrev-ref / --symbolic-full-name / --short rendering.
                let (rev, negate) = match rev.strip_prefix('^') {
                    Some(rest) => (rest, true),
                    None => (rev, false),
                };
                if abbrev_ref {
                    let rendered = rev_parse_abbrev_ref(&git_dir, format, rev)?;
                    rev_parse_print_positional(&rendered, negate);
                    idx += 1;
                    continue;
                }
                if symbolic_full_name {
                    if let Some(name) = rev_parse_symbolic_full_name(&git_dir, format, rev)? {
                        rev_parse_print_positional(&name, negate);
                    }
                    idx += 1;
                    continue;
                }
                let oid = match resolve_revision(&git_dir, format, rev) {
                    Ok(oid) => oid,
                    Err(_) if verify && quiet => return Err(GitError::Exit(1)),
                    Err(_) if verify => {
                        return rev_parse_needed_single_revision(false);
                    }
                    Err(err) => return Err(err),
                };
                if let Some(len) = short {
                    short_revs += 1;
                    if short_revs > 1 {
                        return Err(GitError::Command("needed a single revision".into()));
                    }
                    let oid = oid.to_hex();
                    rev_parse_print_positional(&oid[..len.min(oid.len())], negate);
                } else {
                    rev_parse_print_positional(&oid.to_hex(), negate);
                }
            }
        }
        idx += 1;
    }
    if verify && verified_revs != 1 {
        return rev_parse_needed_single_revision(quiet);
    }
    Ok(())
}

fn rev_parse_args_need_no_repository(args: &[String]) -> Result<bool> {
    let cwd = env::current_dir()?;
    let mut idx = 0;
    let mut handled = false;
    while idx < args.len() {
        match args[idx].as_str() {
            "--sq-quote" => {
                print_rev_parse_sq_quote(&args[idx + 1..])?;
                return Ok(true);
            }
            "--local-env-vars" => {
                print_local_env_vars();
                handled = true;
            }
            "--resolve-git-dir" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(rev_parse_resolve_git_dir_requires_argument_error)?;
                println!("{}", resolve_git_dir_arg(&cwd, path)?);
                handled = true;
            }
            _ => return Ok(false),
        }
        idx += 1;
    }
    Ok(handled)
}

fn print_rev_parse_sq_quote(args: &[String]) -> Result<()> {
    let mut stdout = io::stdout();
    for arg in args {
        stdout.write_all(b" '")?;
        for byte in arg.as_bytes() {
            if *byte == b'\'' {
                stdout.write_all(b"'\\''")?;
            } else {
                stdout.write_all(&[*byte])?;
            }
        }
        stdout.write_all(b"'")?;
    }
    stdout.write_all(b"\n")?;
    Ok(())
}

fn print_local_env_vars() {
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ] {
        println!("{name}");
    }
}

fn rev_parse_needed_single_revision(quiet: bool) -> Result<()> {
    if quiet {
        return Err(GitError::Exit(1));
    }
    eprintln!("fatal: Needed a single revision");
    Err(GitError::Exit(128))
}

fn rev_parse_path_format_requires_argument() -> Result<()> {
    eprintln!("fatal: --path-format requires an argument");
    Err(GitError::Exit(128))
}

fn rev_parse_git_path_requires_argument_error() -> GitError {
    eprintln!("fatal: --git-path requires an argument");
    GitError::Exit(128)
}

fn rev_parse_resolve_git_dir_requires_argument_error() -> GitError {
    eprintln!("fatal: --resolve-git-dir requires an argument");
    GitError::Exit(128)
}

fn rev_parse_not_git_repository() -> Result<()> {
    eprintln!("fatal: not a git repository (or any of the parent directories): .git");
    Err(GitError::Exit(128))
}

fn rev_parse_unknown_path_format(value: &str) -> Result<()> {
    eprintln!("fatal: unknown argument to --path-format: {value}");
    Err(GitError::Exit(128))
}

fn rev_parse_unknown_show_object_format(value: &str) -> Result<()> {
    eprintln!("fatal: unknown mode for --show-object-format: {value}");
    Err(GitError::Exit(128))
}

fn rev_parse_not_gitdir(path: &str) -> Result<String> {
    eprintln!("fatal: not a gitdir '{path}'");
    Err(GitError::Exit(128))
}

fn rev_parse_requires_work_tree() -> Result<()> {
    eprintln!("fatal: this operation must be run in a work tree");
    Err(GitError::Exit(128))
}

fn rev_parse_abbrev_ref(git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<String> {
    let store = FileRefStore::new(git_dir, format);
    if rev == "HEAD" {
        return store
            .current_branch()?
            .ok_or_else(|| GitError::reference_not_found("symbolic HEAD"));
    }
    if let Some(name) = rev.strip_prefix("refs/heads/")
        && store.read_ref(rev)?.is_some()
    {
        return Ok(name.into());
    }
    if let Some(name) = rev.strip_prefix("refs/tags/")
        && store.read_ref(rev)?.is_some()
    {
        return Ok(name.into());
    }
    if store.read_ref(&format!("refs/heads/{rev}"))?.is_some() {
        return Ok(rev.into());
    }
    if store.read_ref(&format!("refs/tags/{rev}"))?.is_some() {
        return Ok(rev.into());
    }
    Err(GitError::not_found(format!("revision {rev}")))
}

/// Render a positional rev-parse line, prefixing `^` for an excluded (`^rev`)
/// argument. Mirrors the `^{rendered}` form `rev_parse_bisect` emits for good
/// refs.
fn rev_parse_print_positional(rendered: &str, negate: bool) {
    if negate {
        println!("^{rendered}");
    } else {
        println!("{rendered}");
    }
}

fn rev_parse_bisect(git_dir: &Path, format: ObjectFormat, symbolic_full_name: bool) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    let terms = sley_rev::read_bisect_terms(git_dir)?;
    let emit = |reference: &Ref, negate: bool| -> Result<()> {
        let rendered = if symbolic_full_name {
            reference.name.clone()
        } else {
            match resolve_ref_peeled(&store, &reference.name)? {
                Some(oid) => oid.to_hex(),
                None => return Ok(()),
            }
        };
        rev_parse_print_positional(&rendered, negate);
        Ok(())
    };
    // `list_refs` already returns refs in name order, so a single forward pass
    // per prefix preserves git's sorted output.
    for reference in &refs {
        if terms.is_bad_ref(&reference.name) {
            emit(reference, false)?;
        }
    }
    for reference in &refs {
        if terms.is_good_ref(&reference.name) {
            emit(reference, true)?;
        }
    }
    Ok(())
}

fn worktree_cdup(cwd: &Path, git_dir: &Path) -> Result<String> {
    let prefix = worktree_prefix(cwd, git_dir)?;
    let depth = prefix.split('/').filter(|part| !part.is_empty()).count();
    Ok("../".repeat(depth))
}

fn display_git_dir(cwd: &Path, git_dir: &Path, path_format: RevParsePathFormat) -> Result<String> {
    match path_format {
        RevParsePathFormat::Default => display_git_dir_default(cwd, git_dir),
        RevParsePathFormat::Absolute => Ok(fs::canonicalize(git_dir)?.display().to_string()),
        RevParsePathFormat::Relative => relative_path_from(cwd, git_dir),
    }
}

fn display_git_dir_default(cwd: &Path, git_dir: &Path) -> Result<String> {
    if let Some(git_dir) = explicit_git_dir() {
        return Ok(git_dir.to_string_lossy().into_owned());
    }
    if global_bare() {
        return Ok(fs::canonicalize(git_dir)?.display().to_string());
    }
    if fs::canonicalize(cwd)? == fs::canonicalize(git_dir)? {
        Ok(".".into())
    } else if git_dir.file_name().and_then(|name| name.to_str()) == Some(".git")
        && git_dir.parent() == Some(cwd)
    {
        Ok(".git".into())
    } else {
        Ok(fs::canonicalize(git_dir)?.display().to_string())
    }
}

fn display_git_common_dir(
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
) -> Result<String> {
    match path_format {
        RevParsePathFormat::Default => display_git_common_dir_default(cwd, git_dir),
        RevParsePathFormat::Absolute => {
            Ok(common_git_dir_for_git_dir(git_dir)?.display().to_string())
        }
        RevParsePathFormat::Relative => {
            relative_path_from_absolute(cwd, &common_git_dir_for_git_dir(git_dir)?)
        }
    }
}

fn display_git_common_dir_default(cwd: &Path, git_dir: &Path) -> Result<String> {
    if let Some(git_dir) = explicit_git_dir() {
        return Ok(git_dir.to_string_lossy().into_owned());
    }
    // A linked worktree's git dir (`…/worktrees/<id>`) carries a `commondir`
    // file pointing at the shared repository. git's `--git-common-dir`
    // (DEFAULT_RELATIVE_IF_SHARED) prints that common dir, not the per-worktree
    // git dir, so resolve it before any `.git`-suffix heuristics.
    if git_dir.join("commondir").is_file() {
        return Ok(common_git_dir_for_git_dir(git_dir)?.display().to_string());
    }
    if git_dir.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return display_git_dir_default(cwd, git_dir);
    }
    let cwd = fs::canonicalize(cwd)?;
    let git_dir = fs::canonicalize(git_dir)?;
    if cwd == git_dir {
        return Ok(".".into());
    }
    if cwd.starts_with(&git_dir) {
        return Ok(git_dir.display().to_string());
    }
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    if cwd == fs::canonicalize(worktree_root)? {
        return Ok(".git".into());
    }
    let prefix = worktree_prefix(&cwd, &git_dir)?;
    let depth = prefix.split('/').filter(|part| !part.is_empty()).count();
    Ok(format!("{}.git", "../".repeat(depth)))
}

fn display_git_path(
    cwd: &Path,
    git_dir: &Path,
    path_format: RevParsePathFormat,
    path: &str,
) -> Result<String> {
    if let Some(path) = display_git_path_env_override(cwd, path_format, path)? {
        return Ok(path);
    }
    match path_format {
        RevParsePathFormat::Default => Ok(join_display_path(
            &display_git_common_dir_default(cwd, git_dir)?,
            path,
        )),
        RevParsePathFormat::Absolute => {
            Ok(fs::canonicalize(git_dir)?.join(path).display().to_string())
        }
        RevParsePathFormat::Relative => {
            let target = fs::canonicalize(git_dir)?.join(path);
            relative_path_from_absolute(cwd, &target)
        }
    }
}

fn display_git_path_env_override(
    cwd: &Path,
    path_format: RevParsePathFormat,
    path: &str,
) -> Result<Option<String>> {
    if path == "index"
        && let Some(index) = env::var_os("GIT_INDEX_FILE")
    {
        return display_env_git_path(cwd, path_format, PathBuf::from(index), "");
    }
    let suffix = if path == "objects" {
        Some("")
    } else {
        path.strip_prefix("objects/")
    };
    if let Some(suffix) = suffix
        && let Some(objects) = env::var_os("GIT_OBJECT_DIRECTORY")
    {
        return display_env_git_path(cwd, path_format, PathBuf::from(objects), suffix);
    }
    Ok(None)
}

fn display_env_git_path(
    cwd: &Path,
    path_format: RevParsePathFormat,
    base: PathBuf,
    suffix: &str,
) -> Result<Option<String>> {
    match path_format {
        RevParsePathFormat::Default => {
            let base = base.to_string_lossy();
            Ok(Some(join_display_path(&base, suffix)))
        }
        RevParsePathFormat::Absolute => Ok(Some(
            absolute_env_git_path(cwd, &base, suffix)?
                .display()
                .to_string(),
        )),
        RevParsePathFormat::Relative => {
            let target = absolute_env_git_path(cwd, &base, suffix)?;
            Ok(Some(relative_path_from_absolute(cwd, &target)?))
        }
    }
}

fn absolute_env_git_path(cwd: &Path, base: &Path, suffix: &str) -> Result<PathBuf> {
    let resolved = if base.is_absolute() {
        base.to_path_buf()
    } else {
        cwd.join(base)
    };
    let canonical = if resolved.exists() {
        fs::canonicalize(&resolved)?
    } else if let Some(parent) = resolved.parent() {
        let file_name = resolved
            .file_name()
            .ok_or_else(|| GitError::InvalidPath(resolved.display().to_string()))?;
        fs::canonicalize(parent)?.join(file_name)
    } else {
        resolved
    };
    Ok(if suffix.is_empty() {
        canonical
    } else {
        canonical.join(suffix)
    })
}

fn join_display_path(base: &str, path: &str) -> String {
    if path.is_empty() {
        return base.to_string();
    }
    if base == "." {
        return path.to_string();
    }
    if base.is_empty() {
        return path.to_string();
    }
    format!("{base}/{path}")
}

fn resolve_git_dir_arg(cwd: &Path, path: &str) -> Result<String> {
    let candidate = cwd.join(path);
    if is_git_dir_candidate(&candidate) {
        return Ok(path.to_string());
    }
    if candidate.is_file()
        && let Ok(contents) = fs::read_to_string(&candidate)
        && let Some(target) = contents.trim().strip_prefix("gitdir:")
    {
        let target = target.trim();
        let resolved = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            candidate
                .parent()
                .map(|parent| parent.join(target))
                .unwrap_or_else(|| PathBuf::from(target))
        };
        if is_git_dir_candidate(&resolved) {
            return Ok(target.to_string());
        }
    }
    rev_parse_not_gitdir(path)
}

fn relative_path_from(cwd: &Path, target: &Path) -> Result<String> {
    let cwd = fs::canonicalize(cwd)?;
    let target = fs::canonicalize(target)?;
    relative_path_from_absolute_components(&cwd, &target)
}

fn is_inside_git_dir(cwd: &Path, git_dir: &Path) -> Result<bool> {
    let cwd = fs::canonicalize(cwd)?;
    let git_dir = fs::canonicalize(git_dir)?;
    Ok(cwd.starts_with(git_dir))
}

fn is_inside_work_tree(cwd: &Path, git_dir: &Path) -> Result<bool> {
    if let Some(work_tree) = explicit_work_tree() {
        let root = fs::canonicalize(resolve_cli_path(
            &env::current_dir()?,
            work_tree.to_string_lossy().as_ref(),
        ))?;
        let cwd = fs::canonicalize(cwd)?;
        return Ok(cwd.starts_with(root));
    }
    // A bare repository has no work tree, so we are never inside one. This
    // covers `core.bare = true` set on a `.git`-named directory, which the
    // directory-layout probe below would otherwise treat as having a worktree.
    if is_bare_repository(git_dir)? {
        return Ok(false);
    }
    if worktree_root_for_git_dir(git_dir).is_err() {
        return Ok(false);
    }
    Ok(!is_inside_git_dir(cwd, git_dir)?)
}

fn is_bare_repository(git_dir: &Path) -> Result<bool> {
    if explicit_work_tree().is_some() {
        return Ok(false);
    }
    let config = git_dir.join("config");
    if let Ok(config) = GitConfig::read(config)
        && let Some(bare) = config.get_bool("core", None, "bare")
    {
        return Ok(bare);
    }
    // With `core.bare` unset, git only infers bareness from the directory layout
    // during *discovery* (walking up to find a repo). When the git dir was named
    // explicitly via `--git-dir`/`GIT_DIR`, git applies no name heuristic and
    // defaults to non-bare.
    if explicit_git_dir().is_some() {
        return Ok(false);
    }
    Ok(git_dir.file_name().and_then(|name| name.to_str()) != Some(".git"))
}

fn is_shallow_repository(git_dir: &Path) -> bool {
    sley_worktree::is_shallow_repository(git_dir)
}

/// `check_repository_format_gently`.
fn verify_repository_format(git_dir: &Path) -> Result<()> {
    repository_ref_storage_format(git_dir)?;
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config_path = common_git_dir.join("config");
    let Ok(config) = GitConfig::read(&config_path) else {
        return Ok(());
    };
    let Some(version_value) = config.get("core", None, "repositoryformatversion") else {
        return Ok(());
    };
    let version: i64 = version_value.trim().parse().unwrap_or(0);
    if version > 1 {
        eprintln!("fatal: Expected git repo version <= 1, found {version}");
        return Err(GitError::Exit(128));
    }
    let mut v1_only = Vec::new();
    let mut unknown = Vec::new();
    for section in config.sections.iter().filter(|section| {
        section.name.eq_ignore_ascii_case("extensions") && section.subsection.is_none()
    }) {
        for entry in &section.entries {
            let ext = entry.key.to_ascii_lowercase();
            match ext.as_str() {
                // Extensions git honours even at repository version 0
                // (`handle_extension_v0`).
                "noop" | "preciousobjects" | "partialclone" | "worktreeconfig" => {}
                // v1-only extensions (`handle_extension`).
                "noop-v1"
                | "objectformat"
                | "compatobjectformat"
                | "refstorage"
                | "relativeworktrees"
                | "submodulepathconfig" => v1_only.push(ext),
                _ => unknown.push(ext),
            }
        }
    }
    if version >= 1 && !unknown.is_empty() {
        let plural = if unknown.len() == 1 {
            "extension"
        } else {
            "extensions"
        };
        eprintln!(
            "fatal: unknown repository {plural} found:\n\t{}",
            unknown.join("\n\t")
        );
        return Err(GitError::Exit(128));
    }
    if version == 0 && !v1_only.is_empty() {
        let plural = if v1_only.len() == 1 {
            "extension"
        } else {
            "extensions"
        };
        eprintln!(
            "fatal: repo version is 0, but v1-only {plural} found:\n\t{}",
            v1_only.join("\n\t")
        );
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn repository_ref_storage_format(git_dir: &Path) -> Result<&'static str> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config_path = common_git_dir.join("config");
    let Ok(bytes) = fs::read(&config_path) else {
        return Ok(RefStorageFormat::Files.name());
    };
    let Ok(config) = GitConfig::parse(&bytes) else {
        return Ok(RefStorageFormat::Files.name());
    };
    // Git validates `extensions.refstorage` as the config is read and aborts on
    // the first occurrence whose value is neither "files" nor "reftable" (the
    // check fires per-occurrence in file order, not just on the last-one-wins
    // value). Mirror that: report the bad value plus the physical config line.
    for value in config.get_all("extensions", None, "refStorage") {
        let Some(value) = value else { continue };
        // Git compares the backend name with `strcmp` (case-sensitive): only the
        // exact lowercase `files`/`reftable` are valid; anything else is rejected.
        if value == "files" || value == "reftable" {
            continue;
        }
        eprintln!("error: invalid value for 'extensions.refstorage': '{value}'");
        let line = refstorage_invalid_value_line(&bytes).unwrap_or(0);
        eprintln!(
            "fatal: bad config line {line} in file {}",
            ref_storage_config_display_path(git_dir, &common_git_dir)
        );
        return Err(GitError::Exit(128));
    }
    Ok(match config.get("extensions", None, "refStorage") {
        // Validation above guarantees any surviving value is exactly `files` or
        // `reftable`; only the latter selects the reftable backend.
        Some("reftable") => RefStorageFormat::Reftable.name(),
        _ => RefStorageFormat::Files.name(),
    })
}

fn ref_storage_config_display_path(git_dir: &Path, common_git_dir: &Path) -> String {
    if explicit_git_dir().is_some() {
        return common_git_dir.join("config").display().to_string();
    }
    // Discovery anchors at the worktree toplevel. When the common dir is the
    // toplevel's `.git`, git prints the relative `.git/config`.
    if let Ok(worktree_root) = worktree_root_for_git_dir(git_dir)
        && let Ok(worktree_root) = fs::canonicalize(&worktree_root)
        && common_git_dir == worktree_root.join(".git")
    {
        return Path::new(".git").join("config").display().to_string();
    }
    common_git_dir.join("config").display().to_string()
}

fn refstorage_invalid_value_line(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut in_extensions = false;
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if let Some(header) = line.strip_prefix('[') {
            // A section header opens a new scope. `[extensions]`, the quoted form
            // `[extensions "x"]`, and the dotted form `[extensions.x]` all begin
            // the extensions section (subsection is irrelevant for refstorage).
            let name = header
                .trim_end_matches(']')
                .split([' ', '\t', '.'])
                .next()
                .unwrap_or("")
                .trim();
            in_extensions = name.eq_ignore_ascii_case("extensions");
            continue;
        }
        if !in_extensions {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("refstorage") {
            continue;
        }
        // Strip an inline comment, then surrounding whitespace, to recover the
        // assigned value (git-written configs never quote these tokens).
        let value = value
            .split(['#', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        // Backend names are compared case-sensitively (git uses `strcmp`).
        if value != "files" && value != "reftable" {
            return Some(idx + 1);
        }
    }
    None
}

fn superproject_working_tree(git_dir: &Path) -> Result<Option<PathBuf>> {
    let git_dir = fs::canonicalize(git_dir)?;
    for ancestor in git_dir.ancestors() {
        if ancestor.file_name().and_then(|name| name.to_str()) != Some("modules") {
            continue;
        }
        let Some(super_git_dir) = ancestor.parent() else {
            continue;
        };
        if super_git_dir.file_name().and_then(|name| name.to_str()) == Some(".git")
            && is_git_dir_candidate(super_git_dir)
        {
            return Ok(Some(fs::canonicalize(worktree_root_for_git_dir(
                super_git_dir,
            )?)?));
        }
    }
    Ok(None)
}
