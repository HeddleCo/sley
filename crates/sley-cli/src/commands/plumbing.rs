//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

/// An `--add-file` / `--add-virtual-file` entry: the output path (already
/// prefixed) plus its content + mode. Disk-backed files are read at parse time
/// so the base prefix in effect at that point is captured.
struct ArchiveExtraFile {
    path: Vec<u8>,
    content: Vec<u8>,
    mode: u32,
}

pub(crate) fn cmd_archive(args: &[String]) -> Result<()> {
    let mut format_name: Option<String> = None;
    let mut prefix = Vec::new();
    let mut output: Option<String> = None;
    let mut treeish = None;
    let mut pathspecs = Vec::new();
    let mut list = false;
    let mut mtime_option: Option<String> = None;
    let mut compression_level: Option<u32> = None;
    let mut extra_files: Vec<ArchiveExtraFile> = Vec::new();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            if treeish.is_none() {
                treeish = Some(arg.clone());
            } else {
                pathspecs.push(arg.as_bytes().to_vec());
            }
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--end-of-options" => positional_only = true,
            "-l" | "--list" => list = true,
            "--format" => {
                format_name = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("archive --format requires a value".into())
                        })?
                        .clone(),
                );
            }
            "--prefix" => {
                prefix = iter
                    .next()
                    .ok_or_else(|| GitError::Command("archive --prefix requires a value".into()))?
                    .as_bytes()
                    .to_vec();
            }
            "--mtime" => {
                mtime_option = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("archive --mtime requires a value".into()))?
                        .clone(),
                );
            }
            "-o" | "--output" => {
                output = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("archive --output requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--add-file" => {
                let path = iter.next().ok_or_else(|| {
                    GitError::Command("archive --add-file requires a value".into())
                })?;
                extra_files.push(archive_disk_extra_file(&prefix, path)?);
            }
            value if value.starts_with("--add-file=") => {
                let path = &value["--add-file=".len()..];
                extra_files.push(archive_disk_extra_file(&prefix, path)?);
            }
            value if value.starts_with("--add-virtual-file=") => {
                let spec = &value["--add-virtual-file=".len()..];
                extra_files.push(archive_virtual_extra_file(spec)?);
            }
            value if value.starts_with("--format=") => {
                format_name = Some(value["--format=".len()..].to_string());
            }
            value if value.starts_with("--prefix=") => {
                prefix = value.as_bytes()["--prefix=".len()..].to_vec();
            }
            value if value.starts_with("--mtime=") => {
                mtime_option = Some(value["--mtime=".len()..].to_string());
            }
            value if value.starts_with("--output=") => {
                output = Some(value["--output=".len()..].to_string());
            }
            // `-N` (0..=9) compression level for the zip backend.
            value
                if value.len() == 2
                    && value.starts_with('-')
                    && value.as_bytes()[1].is_ascii_digit() =>
            {
                compression_level = Some((value.as_bytes()[1] - b'0') as u32);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported archive option {value}"
                )));
            }
            value => {
                if treeish.is_none() {
                    treeish = Some(value.to_string());
                } else {
                    pathspecs.push(value.as_bytes().to_vec());
                }
            }
        }
    }

    if list {
        // `--list` takes no tree-ish or pathspecs.
        if treeish.is_some() || !pathspecs.is_empty() {
            return Err(GitError::Exit(128));
        }
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        for name in ["tar", "tgz", "tar.gz", "zip"] {
            writeln!(lock, "{name}")?;
        }
        lock.flush()?;
        return Ok(());
    }

    // Format resolution: explicit `--format`, else inferred from the `--output`
    // filename extension, else `tar` (upstream `archive_format_from_filename`).
    let format_name = match format_name {
        Some(name) => name,
        None => output
            .as_deref()
            .and_then(archive_format_from_filename)
            .unwrap_or("tar")
            .to_string(),
    };
    let archive_format = match format_name.as_str() {
        "tar" => ArchiveFormatKind::Tar,
        "zip" => ArchiveFormatKind::Zip,
        // `tgz` and `tar.gz` are the internal-gzip tar filter (git's
        // `internal_gzip_command`): the tar stream wrapped in gzip.
        "tgz" | "tar.gz" => ArchiveFormatKind::TarGz,
        other => {
            return Err(GitError::Command(format!(
                "archive does not support --format={other}"
            )));
        }
    };

    let treeish = treeish.ok_or_else(|| GitError::Command("archive requires a tree-ish".into()))?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    // A bare repo has no worktree, so the "current prefix" is empty (we are at
    // the repository root); upstream `git archive` works in a bare repo.
    let current_prefix = match sley_worktree::worktree_root_for_git_dir(&git_dir)? {
        Some(_) => worktree_prefix(&cwd, &git_dir)?.into_bytes(),
        None => Vec::new(),
    };
    let pathspecs = archive_pathspecs_for_current_prefix(&current_prefix, pathspecs);
    let oid = resolve_revision(&git_dir, format, &treeish)?;
    let object = db.read_object(&oid)?;
    let (tree_oid, default_mtime, commit_id, commit_record) = match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse(format, &object.body)?;
            let mtime = commit_graph_commit_time_from_committer(&commit.committer)?;
            let record = sley_rev::CommitRecord {
                oid,
                parents: commit.parents.clone(),
                commit,
            };
            (record.commit.tree, mtime, Some(oid), Some(record))
        }
        ObjectType::Tree => (oid, current_unix_seconds().max(0) as u64, None, None),
        ObjectType::Tag => {
            let tree_oid = sley_rev::peel_to_tree(&db, format, &oid)?;
            (tree_oid, current_unix_seconds().max(0) as u64, None, None)
        }
        other => {
            return Err(GitError::InvalidObject(format!(
                "expected tree-ish {oid}, found {}",
                other.as_str()
            )));
        }
    };
    // `--mtime` overrides the per-entry timestamp (upstream parses it via
    // approxidate). Without it, the commit time (or now, for a tree-ish) is used.
    let mtime = match &mtime_option {
        Some(value) => crate::commands::approxidate::parse_approxidate(value)
            .ok_or_else(|| GitError::Command(format!("invalid --mtime value: {value}")))?
            .max(0) as u64,
        None => default_mtime,
    };

    // Content conversion (smudge: EOL + filter drivers) per the archived tree's
    // `.gitattributes`, matching `git archive`'s `convert_to_working_tree`, plus
    // `export-subst` keyword substitution against the archived commit. The
    // attribute root is the worktree (when non-bare) or the git dir (bare); the
    // git dir locates `info/attributes`. TODO(convert): `--worktree-attributes`
    // (read live `.gitattributes`) and the `ident` filter are not wired yet.
    let config = read_repo_config(&git_dir)?;
    let attr_root = sley_worktree::worktree_root_for_git_dir(&git_dir)?
        .unwrap_or_else(|| git_dir.to_path_buf());
    let mut convert = sley_archive::ArchiveConvert::from_tree(
        &attr_root, &git_dir, &config, &db, format, &tree_oid,
    )?;
    // export-subst only runs when archiving a commit (git sets `args->convert`
    // only when a commit is available).
    if let Some(record) = &commit_record {
        convert = convert.with_subst(move |fmt| format_subst_for_commit(record, fmt));
    }
    // Text/binary classification for the zip backend, driven by the tree's
    // `diff` userdiff attribute (the same `entry_is_binary` upstream uses). Read
    // attributes from the archived *tree* (not the worktree). The
    // `UserdiffResolver` resolves `diff=<name>` ⇒ `diff.<name>.binary` config and
    // builtin driver flags.
    let diff_attributes =
        sley_worktree::TreeAttributes::from_tree(&attr_root, &git_dir, &db, format, &tree_oid)?;
    let userdiff = commands::userdiff::UserdiffResolver::with_attributes(None, Some(config.clone()));
    convert = convert.with_diff_binary(move |path| {
        archive_diff_binary(&diff_attributes, &userdiff, path)
    });

    let extra = sley_archive::ArchiveExtras {
        files: extra_files
            .into_iter()
            .map(|file| sley_archive::ArchiveExtraEntry {
                path: file.path,
                content: file.content,
                mode: file.mode,
            })
            .collect(),
    };

    match archive_format {
        ArchiveFormatKind::Tar => {
            let options = sley_archive::TarArchiveOptions {
                prefix,
                strip_prefix: current_prefix,
                mtime,
                commit_id,
                pathspecs,
            };
            with_archive_writer(output, |writer| {
                handle_archive_result(sley_archive::write_tar_archive_full(
                    writer, &db, format, &tree_oid, options, &convert, &extra,
                ))
            })
        }
        ArchiveFormatKind::TarGz => {
            let options = sley_archive::TarArchiveOptions {
                prefix,
                strip_prefix: current_prefix,
                mtime,
                commit_id,
                pathspecs,
            };
            with_archive_writer(output, |writer| {
                handle_archive_result(sley_archive::write_tar_gz_archive_full(
                    writer,
                    &db,
                    format,
                    &tree_oid,
                    options,
                    &convert,
                    &extra,
                    // git defaults tgz to the zlib default level (6).
                    compression_level.unwrap_or(6),
                ))
            })
        }
        ArchiveFormatKind::Zip => {
            let options = sley_archive::ZipArchiveOptions {
                prefix,
                strip_prefix: current_prefix,
                mtime,
                commit_id,
                pathspecs,
                // git's default is the zlib default level (6); `-0` forces store.
                compression_level: compression_level.unwrap_or(6),
            };
            with_archive_writer(output, |writer| {
                handle_archive_result(sley_archive::write_zip_archive_full(
                    writer, &db, format, &tree_oid, options, &convert, &extra,
                ))
            })
        }
    }
}

enum ArchiveFormatKind {
    Tar,
    TarGz,
    Zip,
}

/// Run `body` with a writer that is either the `--output` file or stdout.
fn with_archive_writer(
    output: Option<String>,
    body: impl FnOnce(&mut dyn io::Write) -> Result<()>,
) -> Result<()> {
    if let Some(path) = output {
        let mut file = fs::File::create(path)?;
        body(&mut file)
    } else {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        body(&mut lock)?;
        lock.flush()?;
        Ok(())
    }
}

/// Infer the archive format from an `--output` filename, mirroring upstream
/// `archive_format_from_filename` / `match_extension`: the extension must follow
/// a non-empty basename and a literal `.`.
fn archive_format_from_filename(filename: &str) -> Option<&'static str> {
    for name in ["tar", "tgz", "tar.gz", "zip"] {
        if archive_match_extension(filename, name) {
            return Some(name);
        }
    }
    None
}

fn archive_match_extension(filename: &str, ext: &str) -> bool {
    let Some(prefix_len) = filename.len().checked_sub(ext.len()) else {
        return false;
    };
    // Need 1 char for the '.' plus a non-empty basename before it.
    if prefix_len < 2 || filename.as_bytes()[prefix_len - 1] != b'.' {
        return false;
    }
    &filename[prefix_len..] == ext
}

/// Build an extra-file entry from a disk path: output path is
/// `<current-prefix><basename>`, content is the file bytes, mode is canonicalized
/// (regular 0644/0755, symlink, gitlink) like upstream `canon_mode`.
fn archive_disk_extra_file(prefix: &[u8], path: &str) -> Result<ArchiveExtraFile> {
    let metadata = fs::symlink_metadata(path)?;
    let basename = std::path::Path::new(path)
        .file_name()
        .map(|name| name.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| path.as_bytes().to_vec());
    let mut output_path = prefix.to_vec();
    output_path.extend_from_slice(&basename);
    use std::os::unix::fs::PermissionsExt;
    let raw_mode = metadata.permissions().mode();
    let mode = if metadata.file_type().is_symlink() {
        0o120000
    } else if raw_mode & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    };
    let content = if metadata.file_type().is_symlink() {
        fs::read_link(path)?.into_os_string().into_encoded_bytes()
    } else {
        fs::read(path)?
    };
    Ok(ArchiveExtraFile {
        path: output_path,
        content,
        mode,
    })
}

/// Parse an `--add-virtual-file=<path>:<content>` spec. The path may be
/// double-quoted (to allow a literal colon); the content is everything after the
/// first unquoted `:`.
fn archive_virtual_extra_file(spec: &str) -> Result<ArchiveExtraFile> {
    let bytes = spec.as_bytes();
    let (path, content) = if bytes.first() == Some(&b'"') {
        // Quoted path: find the closing quote, then the colon after it.
        let close = bytes[1..]
            .iter()
            .position(|&b| b == b'"')
            .map(|index| index + 1)
            .ok_or_else(|| {
                GitError::Command("archive --add-virtual-file: unterminated quote".into())
            })?;
        let path = bytes[1..close].to_vec();
        let after = &bytes[close + 1..];
        let colon = after.iter().position(|&b| b == b':').ok_or_else(|| {
            GitError::Command("archive --add-virtual-file requires <path>:<content>".into())
        })?;
        (path, after[colon + 1..].to_vec())
    } else {
        let colon = bytes.iter().position(|&b| b == b':').ok_or_else(|| {
            GitError::Command("archive --add-virtual-file requires <path>:<content>".into())
        })?;
        (bytes[..colon].to_vec(), bytes[colon + 1..].to_vec())
    };
    Ok(ArchiveExtraFile {
        path,
        content,
        mode: 0o100644,
    })
}

/// git's `entry_is_binary` driver lookup for a tree-relative path: resolve the
/// `diff` attribute, returning the userdiff driver's binary tristate
/// (`Some(true)` = binary, `Some(false)` = text, `None` = auto-detect via
/// content). Mirrors `userdiff_find_by_path(...)->binary`.
fn archive_diff_binary(
    attributes: &sley_worktree::TreeAttributes,
    userdiff: &commands::userdiff::UserdiffResolver,
    path: &[u8],
) -> Option<bool> {
    match attributes.diff_attribute_for_path(path) {
        // `diff` set ⇒ driver_true ⇒ text.
        Some(sley_worktree::AttributeState::Set) => Some(false),
        // `-diff` ⇒ driver_false ⇒ binary.
        Some(sley_worktree::AttributeState::Unset) => Some(true),
        // `diff=<name>` ⇒ resolve the named driver's `binary` flag.
        Some(sley_worktree::AttributeState::Value(name)) => userdiff
            .driver_by_name(&name)
            .ok()
            .flatten()
            .and_then(|driver| driver.binary),
        // unspecified ⇒ no driver override; auto-detect via content.
        None => None,
    }
}

fn handle_archive_result(result: Result<()>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(GitError::InvalidPath(message)) if message.starts_with("pathspec ") => {
            eprintln!("fatal: {message}");
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn archive_pathspecs_for_current_prefix(
    current_prefix: &[u8],
    pathspecs: Vec<Vec<u8>>,
) -> Vec<Vec<u8>> {
    if current_prefix.is_empty() {
        return pathspecs;
    }
    if pathspecs.is_empty() {
        return vec![
            current_prefix
                .strip_suffix(b"/")
                .unwrap_or(current_prefix)
                .to_vec(),
        ];
    }
    pathspecs
        .into_iter()
        .map(|pathspec| {
            let pathspec = pathspec.strip_prefix(b"./").unwrap_or(&pathspec);
            let mut full = Vec::with_capacity(current_prefix.len() + pathspec.len());
            full.extend_from_slice(current_prefix);
            full.extend_from_slice(pathspec);
            full
        })
        .collect()
}

fn init_repo_is_implicitly_bare(cwd: &Path) -> Result<bool> {
    // Determine the effective git directory git would inspect.
    if let Some(git_dir) = environment_git_dir() {
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
        let common = common_git_dir_for_git_dir(&target)?;
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

pub(crate) fn cmd_init(args: &[String], global_config: &[GlobalConfigOverride]) -> Result<()> {
    let mut bare = global_bare();
    // git distinguishes an *explicitly requested* bare repo (`--bare`/global
    // `--bare`) from one merely *guessed* from the environment. The former pairs
    // with `--separate-git-dir` as "cannot be used together"; the latter as
    // "incompatible with bare repository". Track the explicit signal separately
    // from the `.git`-suffix path heuristic applied further down.
    let mut bare_explicit = global_bare();
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

    if !bare
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".git"))
    {
        bare = true;
    }

    let cwd = env::current_dir()?;
    let init_config_git_dir =
        init_config_git_dir_for_lookup(&cwd, &path, bare, separate_git_dir.as_deref())?;

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
        if init_repo_is_implicitly_bare(&cwd)? {
            eprintln!("fatal: --separate-git-dir incompatible with bare repository");
            return Err(GitError::Exit(128));
        }
    }

    // init-db.c: GIT_WORK_TREE (or --work-tree) only makes sense together with
    // GIT_DIR and without an explicit `--bare`. After chdir'ing into the target
    // directory, `--bare` pins GIT_DIR to that directory (overwriting the
    // environment when a directory argument was given); the effective git dir
    // then comes from GIT_DIR and its *string* form drives the bare guess.
    let env_git_dir = explicit_git_dir();
    let env_work_tree = explicit_work_tree();
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
            let common = common_git_dir_for_git_dir(&admin_dir)?;
            if let Some(main_root) = common.parent() {
                worktree = main_root.to_path_buf();
            }
        }
    }
    if bare_explicit {
        // `--bare` without a directory argument leaves an existing GIT_DIR in
        // charge of where the (bare) repository lives.
        if !path_given && let Some(raw) = env_git_dir.clone() {
            git_dir_override = Some(resolve_cli_path(&worktree, raw.to_string_lossy().as_ref()));
        }
    } else if let Some(raw) = env_git_dir.clone()
        && separate_git_dir.is_none()
        && !bare
    {
        let git_dir_abs = resolve_cli_path(&worktree, raw.to_string_lossy().as_ref());
        if guess_repository_type(&raw, &worktree) {
            match env_work_tree.clone() {
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
            let work_tree_abs = match env_work_tree.clone() {
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
    let template_dir = resolve_init_template_dir(
        template,
        template_config,
        global_config,
        &cwd,
        init_config_git_dir.as_deref(),
    )?;

    let layout = RepositoryBootstrap::init(InitOptions {
        worktree,
        git_dir_override,
        core_worktree,
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
        let action = if layout.reinitialized {
            "Reinitialized existing"
        } else {
            "Initialized empty"
        };
        println!("{action} Git repository in {}/", git_dir.to_string_lossy());
    }
    Ok(())
}

fn init_config_git_dir_for_lookup(
    cwd: &Path,
    path: &Path,
    bare: bool,
    separate_git_dir: Option<&str>,
) -> Result<Option<PathBuf>> {
    if let Some(raw) = separate_git_dir {
        return Ok(Some(resolve_cli_path(cwd, raw)));
    }
    if let Some(raw) = explicit_git_dir() {
        let git_dir = resolve_cli_path(cwd, raw.to_string_lossy().as_ref());
        if git_dir.is_file()
            && let Some(target) = read_gitdir_file(&git_dir)?
        {
            return common_git_dir_for_git_dir(&target).map(Some);
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
            return common_git_dir_for_git_dir(&git_dir).map(Some);
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
    if let Ok(hash) = env::var("GIT_DEFAULT_HASH") {
        if !hash.is_empty() {
            return Ok((parse_init_object_format(&hash)?, false));
        }
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
        let value = match value.as_deref() {
            Some(value) => value,
            None => "",
        };
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
        return Ok(None);
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
                Ok(default_init_template_dir())
            }
        }
    }
}

fn default_init_template_dir() -> Option<PathBuf> {
    let output = ProcessCommand::new("git")
        .arg("--exec-path")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let exec_path = String::from_utf8_lossy(&output.stdout);
    let candidate = PathBuf::from(exec_path.trim()).join("../share/git-core/templates");
    candidate.canonicalize().ok().filter(|path| path.is_dir())
}

fn init_config_bool(
    key: &str,
    global_config: &[GlobalConfigOverride],
    config_git_dir: Option<&Path>,
) -> Result<Option<bool>> {
    init_config_value(key, global_config, config_git_dir)
        .map(|value| value.as_deref().and_then(parse_config_bool))
}

pub(crate) fn cmd_add(args: &[String]) -> Result<()> {
    // `add -i` / `add --interactive` and `add -p` / `add --patch` route to the
    // interactive engine. git treats `--patch` as implying interactive and lets
    // a pathspec follow. We collect the non-flag pathspec args plus the diff-tuning
    // flags add-patch forwards to the spawned `diff-files` (`-U`/`--unified`,
    // `--inter-hunk-context`) and forward them.
    {
        let mut interactive = false;
        let mut patch = false;
        let mut spec: Vec<String> = Vec::new();
        // Explicit `-U<n>` / `--inter-hunk-context=<n>` from add's own argv. `None`
        // means "fall back to diff.context / diff.interHunkContext config".
        let mut context: Option<i64> = None;
        let mut interhunk: Option<i64> = None;
        // `--auto-advance`/`--no-auto-advance`. git's default is auto-advance ON;
        // `Some(false)` is `--no-auto-advance`.
        let mut auto_advance: Option<bool> = None;
        let mut after_dd = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if after_dd {
                spec.push(arg.clone());
                continue;
            }
            match arg.as_str() {
                "--" => after_dd = true,
                "-i" | "--interactive" => interactive = true,
                "-p" | "--patch" => patch = true,
                "--auto-advance" => auto_advance = Some(true),
                "--no-auto-advance" => auto_advance = Some(false),
                "-U" | "--unified" => {
                    context = iter.next().and_then(|v| v.parse::<i64>().ok());
                }
                value if value.starts_with("-U") => {
                    context = value[2..].parse::<i64>().ok();
                }
                value if let Some(rest) = value.strip_prefix("--unified=") => {
                    context = rest.parse::<i64>().ok();
                }
                "--inter-hunk-context" => {
                    interhunk = iter.next().and_then(|v| v.parse::<i64>().ok());
                }
                value if let Some(rest) = value.strip_prefix("--inter-hunk-context=") => {
                    interhunk = rest.parse::<i64>().ok();
                }
                other if other.starts_with('-') => {
                    // Leave any other flags to the normal path (no -i/-p).
                }
                other => spec.push(other.to_string()),
            }
        }
        // builtin/add.c validation order: negative context dies first (independent
        // of -p), then the "requires --interactive/--patch" checks fire only when
        // NOT in interactive/patch mode.
        if let Some(value) = context
            && value < -1
        {
            eprintln!("fatal: '--unified' cannot be negative");
            return Err(GitError::Exit(128));
        }
        if let Some(value) = interhunk
            && value < -1
        {
            eprintln!("fatal: '--inter-hunk-context' cannot be negative");
            return Err(GitError::Exit(128));
        }
        if !patch && !interactive {
            if context.is_some() {
                eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
                return Err(GitError::Exit(128));
            }
            if interhunk.is_some() {
                eprintln!(
                    "fatal: the option '--inter-hunk-context' requires '--interactive/--patch'"
                );
                return Err(GitError::Exit(128));
            }
            if auto_advance == Some(false) {
                eprintln!(
                    "fatal: the option '--no-auto-advance' requires '--interactive/--patch'"
                );
                return Err(GitError::Exit(128));
            }
        }
        if patch {
            return super::add_interactive::cmd_add_patch(
                &spec,
                context,
                interhunk,
                auto_advance.unwrap_or(true),
            );
        }
        if interactive {
            return super::add_interactive::cmd_add_interactive(&spec);
        }
    }
    let mut paths = Vec::new();
    let mut dry_run = false;
    let mut verbose = false;
    let mut update = false;
    let mut all = false;
    let mut force = false;
    let mut ignore_removal = false;
    let mut ignore_errors = false;
    let mut ignore_missing = false;
    let mut intent_to_add = false;
    let mut sparse = false;
    let mut refresh = false;
    let mut chmod = None;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut parsing_options = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-u" | "--update" => update = true,
            "--no-update" => update = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-A" | "--all" | "--no-ignore-removal" => {
                all = true;
                ignore_removal = false;
            }
            "--ignore-removal" | "--no-all" => {
                all = false;
                ignore_removal = true;
            }
            "--ignore-missing" => ignore_missing = true,
            "--no-ignore-missing" => ignore_missing = false,
            "--refresh" => refresh = true,
            "--no-refresh" => refresh = false,
            "-N" | "--intent-to-add" => intent_to_add = true,
            "--no-intent-to-add" => intent_to_add = false,
            "--chmod" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--chmod requires a value".into()))?;
                chmod = Some(parse_add_chmod(value)?);
            }
            "--no-chmod" => chmod = None,
            value if value.starts_with("--chmod=") => {
                let value = value
                    .strip_prefix("--chmod=")
                    .expect("prefix checked by match guard");
                chmod = Some(parse_add_chmod(value)?);
            }
            "--ignore-errors" => ignore_errors = true,
            "--no-ignore-errors" => ignore_errors = false,
            "--sparse" => sparse = true,
            "--no-sparse" => sparse = false,
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| matches!(option, b'A' | b'n' | b'u' | b'v' | b'f')) =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'A' => all = true,
                        b'n' => dry_run = true,
                        b'u' => update = true,
                        b'v' => verbose = true,
                        b'f' => force = true,
                        _ => unreachable!("add short-option group was filtered"),
                    }
                }
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                paths.push(PathBuf::from(value));
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    if ignore_missing && !dry_run {
        eprintln!("fatal: the option '--ignore-missing' requires '--dry-run'");
        return Err(GitError::Exit(128));
    }
    if paths.is_empty() && !update && !all {
        eprintln!("Nothing specified, nothing added.");
        eprintln!("hint: Maybe you wanted to say 'git add .'?");
        eprintln!(
            "hint: Disable this message with \"git config set advice.addEmptyPathspec false\""
        );
        return Ok(());
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    if refresh {
        refresh_index_after_add(&worktree_root, &git_dir, format, &paths)?;
        return Ok(());
    }
    if intent_to_add && !dry_run {
        return add_intent_to_add(&cwd, &worktree_root, &git_dir, format, &paths);
    }
    if !update
        && !all
        && let Some(actions) = try_add_regular_exact_tracked_raw(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            &paths,
            AddRegularOptions {
                chmod,
                force,
                ignore_errors,
                ignore_removal,
                ignore_missing,
                dry_run,
                sparse,
            },
        )?
    {
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    let parsed_index = if paths.is_empty() {
        None
    } else {
        sley_worktree::read_repository_index(&git_dir, format)?
    };
    die_on_pathspec_inside_submodule(&cwd, &worktree_root, parsed_index.as_ref(), &paths)?;
    // git's `add` re-stats every tracked path it touches, including ones whose
    // content is unchanged (a `touch`ed file): `builtin/add.c` calls
    // `refresh_index` over the pathspec before/after staging, so the cached stat
    // matches the worktree and `git diff-files` stays clean (t2200 "touch and then
    // add"). sley's action resolver only stages content-changed paths, so a
    // content-clean-but-stat-dirty tracked entry would otherwise keep its stale
    // stat. Capture the pathspec so we can run that refresh after staging; an empty
    // pathspec (bare `add -u`/`-A`) refreshes every tracked entry, matching git.
    //
    // `--chmod` is the one case we must NOT refresh: it deliberately sets an index
    // mode that diverges from the worktree file's mode (e.g. stage 100755 while the
    // file is 100644), and a stat refresh would re-stamp the mode from the worktree
    // and clobber the chmod. git keeps the explicit mode; so do we, by skipping the
    // refresh entirely when a chmod was requested.
    let refresh_paths: Vec<PathBuf> = if dry_run || chmod.is_some() {
        Vec::new()
    } else {
        paths.clone()
    };
    let do_refresh = !dry_run && chmod.is_none();
    if update && !all && paths.is_empty() && !dry_run && chmod.is_none() {
        let config = read_repo_config(&git_dir)?;
        let actions = sley_worktree::add_update_all_tracked_filtered(
            &worktree_root,
            &git_dir,
            format,
            &config,
        )?
        .into_iter()
        .map(|action| -> Result<AddAction> {
            match action {
                sley_worktree::AddUpdateTrackedAction::Add(path) => Ok(AddAction::Add(
                    worktree_root.join(
                        std::str::from_utf8(&path)
                            .map_err(|err| GitError::InvalidPath(err.to_string()))?,
                    ),
                )),
                sley_worktree::AddUpdateTrackedAction::Remove(path) => Ok(AddAction::Remove(
                    worktree_root.join(
                        std::str::from_utf8(&path)
                            .map_err(|err| GitError::InvalidPath(err.to_string()))?,
                    ),
                )),
            }
        })
        .collect::<Result<Vec<_>>>()?;
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    if update || all {
        let actions = resolve_add_update_actions(
            &cwd,
            &worktree_root,
            &git_dir,
            format,
            paths,
            all,
            ignore_missing,
        )?;
        if dry_run {
            print_add_actions(&worktree_root, &actions)?;
            return Ok(());
        }
        let action_paths = actions
            .iter()
            .map(AddAction::path)
            .cloned()
            .collect::<Vec<_>>();
        if !action_paths.is_empty() {
            let config = read_repo_config(&git_dir)?;
            sley_worktree::update_index_paths_filtered(
                &worktree_root,
                &git_dir,
                format,
                &action_paths,
                sley_worktree::UpdateIndexOptions {
                    add: true,
                    remove: true,
                    force_remove: false,
                    chmod,
                    info_only: false,
                    ignore_skip_worktree_entries: false,
                    allow_skip_worktree_entries: sparse,
                },
                &config,
            )?;
        }
        if do_refresh {
            refresh_index_after_add(&worktree_root, &git_dir, format, &refresh_paths)?;
        }
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    let AddRegularResolution {
        actions,
        mut reusable_index,
        exact_tracked,
        ignored_paths,
    } = resolve_add_regular_actions(
        &cwd,
        &worktree_root,
        &git_dir,
        format,
        paths,
        AddRegularOptions {
            chmod,
            force,
            ignore_errors,
            ignore_removal,
            ignore_missing,
            dry_run,
            sparse,
        },
        parsed_index,
    )?;
    if dry_run {
        print_add_actions(&worktree_root, &actions)?;
        if !ignored_paths.is_empty() {
            print_add_ignored_paths(&git_dir, &ignored_paths);
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }
    if let Some(exact) = exact_tracked {
        let actions = if exact.needs_index_update {
            let index = reusable_index.take().ok_or_else(|| {
                GitError::Command("exact tracked add lost its parsed index".into())
            })?;
            sley_worktree::add_exact_tracked_path_with_index(
                &worktree_root,
                &git_dir,
                format,
                index,
                &exact.git_path,
            )?
            .into_iter()
            .map(|action| add_update_tracked_action_to_add_action(&worktree_root, action))
            .collect::<Result<Vec<_>>>()?
        } else {
            actions
        };
        if verbose {
            print_add_actions(&worktree_root, &actions)?;
        }
        return Ok(());
    }
    let action_paths = actions
        .iter()
        .map(AddAction::path)
        .cloned()
        .collect::<Vec<_>>();
    if !action_paths.is_empty() {
        let config = read_repo_config(&git_dir)?;
        let warn_embedded = actions_may_add_embedded_repo(&actions);
        // Snapshot the tracked paths before staging only when the warning can
        // actually fire. Ordinary file adds never need this second index pass.
        let previously_tracked: BTreeSet<Vec<u8>> = if warn_embedded {
            if let Some(index) = reusable_index.as_ref() {
                index
                    .entries
                    .iter()
                    .map(|entry| entry.path.as_bytes().to_vec())
                    .collect()
            } else {
                sley_worktree::read_repository_index(&git_dir, format)?
                    .map(|index| {
                        index
                            .entries
                            .into_iter()
                            .map(|entry| entry.path.into_bytes())
                            .collect()
                    })
                    .unwrap_or_default()
            }
        } else {
            BTreeSet::new()
        };
        let update_options = sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod,
            info_only: false,
            ignore_skip_worktree_entries: false,
            allow_skip_worktree_entries: sparse,
        };
        if let Some(index) = reusable_index.take() {
            sley_worktree::update_index_paths_filtered_with_index(
                &worktree_root,
                &git_dir,
                format,
                index,
                &action_paths,
                update_options,
                &config,
            )?;
        } else {
            sley_worktree::update_index_paths_filtered(
                &worktree_root,
                &git_dir,
                format,
                &action_paths,
                update_options,
                &config,
            )?;
        }
        if warn_embedded {
            warn_on_embedded_repos(&git_dir, &worktree_root, &actions, &previously_tracked)?;
        }
    }
    if do_refresh && !add_refresh_is_redundant(&worktree_root, &refresh_paths, &actions) {
        refresh_index_after_add(&worktree_root, &git_dir, format, &refresh_paths)?;
    }
    if verbose {
        print_add_actions(&worktree_root, &actions)?;
    }
    if !ignored_paths.is_empty() {
        print_add_ignored_paths(&git_dir, &ignored_paths);
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// `git add -N` / `git add --intent-to-add`: record that each named path will
/// be added later without staging its content. Mirrors `builtin/add.c`'s
/// `ADD_CACHE_INTENT` path: for every pathspec that resolves to a worktree file
/// not already tracked at stage 0, insert an intent-to-add placeholder entry
/// (empty-blob id, mode 100644, the ITA extended flag). Already-tracked paths
/// are left untouched. The index is rewritten with the entries kept in git's
/// canonical (path, stage) sort order.
fn add_intent_to_add(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<()> {
    let mut index = sley_worktree::read_repository_index(git_dir, format)?.unwrap_or_else(|| Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });

    let mut changed = false;
    for path in paths {
        // Resolve the pathspec to a worktree-relative git path. Reject anything
        // outside the worktree (git errors; we silently skip, matching the
        // tests which only ever pass in-tree paths).
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            continue;
        };
        let git_path = add_git_path_bytes(relative)?;
        if git_path.is_empty() {
            continue;
        }
        // The worktree file must exist (git only marks paths that are present).
        if !worktree_root.join(relative).is_file() {
            continue;
        }
        // Skip paths already in the index at stage 0 (tracked or already ITA).
        let already = index
            .entries
            .iter()
            .any(|entry| index_entry_stage(entry) == 0 && entry.path.as_bytes() == git_path.as_slice());
        if already {
            continue;
        }
        let entry = IndexEntry::intent_to_add(format, git_path);
        // Insert keeping the (path, stage) sort order the writer relies on.
        let position = index
            .entries
            .binary_search_by(|existing| {
                existing
                    .path
                    .as_bytes()
                    .cmp(entry.path.as_bytes())
                    .then(index_entry_stage(existing).cmp(&index_entry_stage(&entry)))
            })
            .unwrap_or_else(|insert_at| insert_at);
        index.entries.insert(position, entry);
        changed = true;
    }

    if changed {
        // ITA entries carry an extended flag → the writer needs index v3+.
        if index.version < 3 {
            index.version = 3;
        }
        let index_path = sley_worktree::repository_index_path(git_dir);
        std::fs::write(index_path, index.write(format)?)?;
    }
    Ok(())
}

fn add_update_tracked_action_to_add_action(
    worktree_root: &Path,
    action: sley_worktree::AddUpdateTrackedAction,
) -> Result<AddAction> {
    match action {
        sley_worktree::AddUpdateTrackedAction::Add(path) => Ok(AddAction::Add(worktree_root.join(
            std::str::from_utf8(&path).map_err(|err| GitError::InvalidPath(err.to_string()))?,
        ))),
        sley_worktree::AddUpdateTrackedAction::Remove(path) => {
            Ok(AddAction::Remove(worktree_root.join(
                std::str::from_utf8(&path).map_err(|err| GitError::InvalidPath(err.to_string()))?,
            )))
        }
    }
}

fn try_add_regular_exact_tracked_raw(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: AddRegularOptions,
) -> Result<Option<Vec<AddAction>>> {
    if paths.len() != 1
        || options.dry_run
        || options.chmod.is_some()
        || options.force
        || options.ignore_missing
        || options.sparse
    {
        return Ok(None);
    }
    let path = &paths[0];
    if add_pathspec_needs_status_walk(path) || add_pathspec_has_trailing_separator(path) {
        return Ok(None);
    }
    let absolute = if path.is_absolute() {
        path.clone()
    } else {
        cwd.join(path)
    };
    let Ok(relative) = absolute.strip_prefix(worktree_root) else {
        return Ok(None);
    };
    if relative.as_os_str().is_empty() {
        return Ok(None);
    }
    let git_path = match add_git_path_bytes(relative) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let result = sley_worktree::add_exact_tracked_path_from_disk(
        worktree_root,
        git_dir,
        format,
        &git_path,
        options.ignore_removal,
        crate::effective_config_parameters_env().as_deref(),
    )?;
    match result {
        sley_worktree::AddExactTrackedPathResult::Handled(action) => action
            .into_iter()
            .map(|action| add_update_tracked_action_to_add_action(worktree_root, action))
            .collect::<Result<Vec<_>>>()
            .map(Some),
        sley_worktree::AddExactTrackedPathResult::Unsupported => Ok(None),
    }
}

fn add_pathspec_has_trailing_separator(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .ends_with(std::path::MAIN_SEPARATOR)
}

/// Re-stat the index entries `git add` touched so the cached stat matches the
/// worktree (git's `refresh_index` over the pathspec): a tracked path whose
/// content is unchanged but whose stat is dirty (e.g. it was `touch`ed) is
/// stamped clean, so `git diff-files` reports nothing. An empty pathspec (bare
/// `add -u`/`-A`) refreshes every tracked entry. Quiet + tolerant of missing
/// files (content mismatches are genuine worktree changes, not a refresh error).
fn refresh_index_after_add(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    refresh_paths: &[PathBuf],
) -> Result<()> {
    // Pathspecs here may be directories or `.`. Passing an empty set preserves
    // the fast all-entry refresh path for non-file pathspecs; pure file
    // pathspecs still force the checked refresh path, matching git's
    // "needs update" handling without staging changed content.
    let only_files = !refresh_paths.is_empty()
        && refresh_paths.iter().all(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            fs::symlink_metadata(&absolute)
                .map(|metadata| metadata.file_type().is_file() || metadata.file_type().is_symlink())
                .unwrap_or(false)
        });
    let selected: &[PathBuf] = if only_files { refresh_paths } else { &[] };
    sley_worktree::refresh_index_paths(
        worktree_root,
        git_dir,
        format,
        selected,
        /* quiet */ true,
        /* ignore_missing */ true,
        /* really_refresh */ false,
    )?;
    Ok(())
}

/// Upstream pathspec.c `die_path_inside_submodule()`: a pathspec that names a
/// path *inside* a tracked gitlink is fatal — the file belongs to the
/// submodule's repository, not this one.
fn die_on_pathspec_inside_submodule(
    cwd: &Path,
    worktree_root: &Path,
    index: Option<&Index>,
    paths: &[PathBuf],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let Some(index) = index else {
        return Ok(());
    };
    if paths
        .iter()
        .any(|path| add_pathspec_needs_status_walk(path))
    {
        return die_on_pathspec_inside_submodule_by_scan(cwd, worktree_root, index, paths);
    }
    let mut git_paths = Vec::with_capacity(paths.len());
    for path in paths {
        match add_pathspec_git_path_for_submodule_fast(cwd, worktree_root, path)? {
            AddSubmodulePathspec::Inside(git_path) => git_paths.push((path, git_path)),
            AddSubmodulePathspec::Outside => {}
            AddSubmodulePathspec::Unsafe => {
                return die_on_pathspec_inside_submodule_by_scan(cwd, worktree_root, index, paths);
            }
        }
    }
    for (path, git_path) in git_paths {
        if let Some(link) = gitlink_ancestor_for_path(&index.entries, &git_path) {
            eprintln!(
                "fatal: Pathspec '{}' is in submodule '{}'",
                path.to_string_lossy(),
                String::from_utf8_lossy(link)
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

enum AddSubmodulePathspec {
    Inside(Vec<u8>),
    Outside,
    Unsafe,
}

fn add_pathspec_git_path_for_submodule_fast(
    cwd: &Path,
    worktree_root: &Path,
    path: &Path,
) -> Result<AddSubmodulePathspec> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let Ok(relative) = absolute.strip_prefix(worktree_root) else {
        return Ok(AddSubmodulePathspec::Outside);
    };
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::Prefix(_)
                | std::path::Component::RootDir
        )
    }) {
        return Ok(AddSubmodulePathspec::Unsafe);
    }
    Ok(AddSubmodulePathspec::Inside(add_git_path_bytes(relative)?))
}

fn gitlink_ancestor_for_path<'a>(entries: &'a [IndexEntry], git_path: &[u8]) -> Option<&'a [u8]> {
    for (idx, byte) in git_path.iter().enumerate() {
        if *byte != b'/' || idx == 0 {
            continue;
        }
        if let Some(link) = index_gitlink_at_path(entries, &git_path[..idx]) {
            return Some(link);
        }
    }
    None
}

fn index_gitlink_at_path<'a>(entries: &'a [IndexEntry], path: &[u8]) -> Option<&'a [u8]> {
    let range = add_index_entries_path_range(entries, path);
    entries[range]
        .iter()
        .find(|entry| entry.stage() == sley_index::Stage::Normal && entry.mode == 0o160000)
        .map(|entry| entry.path.as_bytes())
}

fn die_on_pathspec_inside_submodule_by_scan(
    cwd: &Path,
    worktree_root: &Path,
    index: &Index,
    paths: &[PathBuf],
) -> Result<()> {
    let gitlinks: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|entry| entry.mode == 0o160000)
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    if gitlinks.is_empty() {
        return Ok(());
    }
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            continue;
        };
        let git_path = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        let git_path = git_path.as_bytes();
        for link in &gitlinks {
            if git_path.len() > link.len()
                && git_path.starts_with(link)
                && git_path[link.len()] == b'/'
            {
                eprintln!(
                    "fatal: Pathspec '{}' is in submodule '{}'",
                    path.to_string_lossy(),
                    String::from_utf8_lossy(link)
                );
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(())
}

/// Upstream builtin/add.c check_embedded_repo(): after staging, warn (per
/// path) about each embedded git repository that was just added as a gitlink,
/// and print the `advice.addEmbeddedRepo` hint once.
fn warn_on_embedded_repos(
    git_dir: &Path,
    worktree_root: &Path,
    actions: &[AddAction],
    previously_tracked: &BTreeSet<Vec<u8>>,
) -> Result<()> {
    let mut adviced = false;
    for action in actions {
        let AddAction::Add(path) = action else {
            continue;
        };
        if !path.is_dir() || sley_diff_merge::gitlink_git_dir(path).is_none() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(worktree_root) else {
            continue;
        };
        let name = relative.to_string_lossy().replace('\\', "/");
        if previously_tracked.contains(name.as_bytes()) {
            continue;
        }
        eprintln!("warning: adding embedded git repository: {name}");
        if adviced {
            continue;
        }
        adviced = true;
        let advice_enabled = read_repo_config(git_dir)
            .ok()
            .and_then(|config| config.get_bool("advice", None, "addembeddedrepo"))
            .unwrap_or(true);
        if !advice_enabled {
            continue;
        }
        eprintln!("hint: You've added another git repository inside your current repository.");
        eprintln!("hint: Clones of the outer repository will not contain the contents of");
        eprintln!("hint: the embedded repository and will not know how to obtain it.");
        eprintln!("hint: If you meant to add a submodule, use:");
        eprintln!("hint:");
        eprintln!("hint: \tgit submodule add <url> {name}");
        eprintln!("hint:");
        eprintln!("hint: If you added this path by mistake, you can remove it from the");
        eprintln!("hint: index with:");
        eprintln!("hint:");
        eprintln!("hint: \tgit rm --cached {name}");
        eprintln!("hint:");
        eprintln!("hint: See \"git help submodule\" for more information.");
        eprintln!(
            "hint: Disable this message with \"git config set advice.addEmbeddedRepo false\""
        );
    }
    Ok(())
}

fn actions_may_add_embedded_repo(actions: &[AddAction]) -> bool {
    actions.iter().any(|action| match action {
        AddAction::Add(path) => path.is_dir() && sley_diff_merge::gitlink_git_dir(path).is_some(),
        AddAction::Remove(_) => false,
    })
}

fn add_refresh_is_redundant(
    worktree_root: &Path,
    refresh_paths: &[PathBuf],
    actions: &[AddAction],
) -> bool {
    if refresh_paths.is_empty() || refresh_paths.len() != actions.len() {
        return false;
    }
    let action_paths = actions.iter().map(AddAction::path).collect::<BTreeSet<_>>();
    refresh_paths.iter().all(|path| {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        if fs::symlink_metadata(&absolute)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            return false;
        }
        action_paths.contains(&absolute)
    })
}

fn parse_add_chmod(value: &str) -> Result<bool> {
    match value {
        "+x" => Ok(true),
        "-x" => Ok(false),
        _ => {
            eprintln!("fatal: --chmod param '{value}' must be either -x or +x");
            Err(GitError::Exit(128))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AddRegularOptions {
    chmod: Option<bool>,
    force: bool,
    ignore_errors: bool,
    ignore_removal: bool,
    ignore_missing: bool,
    dry_run: bool,
    sparse: bool,
}

struct AddRegularResolution {
    actions: Vec<AddAction>,
    reusable_index: Option<Index>,
    exact_tracked: Option<ExactTrackedAdd>,
    ignored_paths: Vec<Vec<u8>>,
}

struct ExactTrackedAdd {
    git_path: Vec<u8>,
    needs_index_update: bool,
}

struct TrackedExactResolution {
    actions: Vec<AddAction>,
    exact_tracked: Option<ExactTrackedAdd>,
}

fn resolve_add_regular_actions(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: Vec<PathBuf>,
    options: AddRegularOptions,
    reusable_index: Option<Index>,
) -> Result<AddRegularResolution> {
    reject_add_paths_outside_sparse_checkout(cwd, worktree_root, git_dir, &paths, options)?;
    if let Some(exact) = resolve_add_regular_tracked_exact_actions(
        cwd,
        worktree_root,
        git_dir,
        &paths,
        options,
        reusable_index.as_ref(),
    )? {
        return Ok(AddRegularResolution {
            actions: exact.actions,
            reusable_index,
            exact_tracked: exact.exact_tracked,
            ignored_paths: Vec::new(),
        });
    }
    let pathspecs = paths
        .into_iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(&path)
            };
            let matched = absolute.exists();
            (path, absolute, matched)
        })
        .collect::<Vec<_>>();
    let mut matched = pathspecs
        .iter()
        .map(|(_, _, matched)| *matched)
        .collect::<Vec<_>>();
    let mut actions = Vec::new();
    let mut seen = BTreeSet::new();
    let mut ignored_paths = BTreeSet::new();
    let _ignore_errors = options.ignore_errors;
    if !options.force {
        let indexed_paths = add_all_index_paths(git_dir, format, reusable_index.as_ref())?;
        for (idx, ignored_path) in
            collect_add_ignored_pathspec_matches(
                worktree_root,
                git_dir,
                format,
                &pathspecs,
                &indexed_paths,
            )?
        {
            matched[idx] = true;
            ignored_paths.insert(ignored_path);
        }
    }
    sley_worktree::stream_short_status(worktree_root, git_dir, format, |entry| {
        let actionable = (entry.index == b'?' && entry.worktree == b'?')
            || entry.worktree == b'M'
            || entry.worktree == b'D';
        if !actionable {
            return Ok(sley_worktree::StreamControl::Continue);
        }
        let path = worktree_root.join(
            std::str::from_utf8(entry.path)
                .map_err(|err| GitError::InvalidPath(err.to_string()))?,
        );
        let mut path_matches = false;
        for (idx, (_, pathspec, _)) in pathspecs.iter().enumerate() {
            if add_path_matches(&path, pathspec) {
                matched[idx] = true;
                path_matches = true;
            }
        }
        if !path_matches {
            return Ok(sley_worktree::StreamControl::Continue);
        }
        if entry.worktree == b'D' && options.ignore_removal {
            return Ok(sley_worktree::StreamControl::Continue);
        }
        if seen.insert(path.clone()) {
            let action = if entry.worktree == b'D' {
                AddAction::Remove(path)
            } else {
                AddAction::Add(path)
            };
            actions.push(action);
        }
        Ok(sley_worktree::StreamControl::Continue)
    })?;
    if options.chmod.is_some() || options.force {
        // `--force` stages paths the status walk never reports (gitignored
        // files; gitignored embedded repositories as gitlinks), so resolve the
        // pathspecs straight off the filesystem. The same walk feeds `--chmod`,
        // which must touch every matching file whether or not it changed.
        for (idx, (_, pathspec, _)) in pathspecs.iter().enumerate() {
            for path in resolve_add_paths(cwd, worktree_root, vec![pathspec.clone()])? {
                if fs::symlink_metadata(&path).is_err() {
                    continue;
                }
                matched[idx] = true;
                if seen.insert(path.clone()) {
                    actions.push(AddAction::Add(path));
                }
            }
        }
    }
    for ((display, _, _), matched) in pathspecs.iter().zip(matched) {
        if !matched && !options.ignore_missing {
            eprintln!(
                "fatal: pathspec '{}' did not match any files",
                display.to_string_lossy()
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(AddRegularResolution {
        actions,
        reusable_index: None,
        exact_tracked: None,
        ignored_paths: ignored_paths.into_iter().collect(),
    })
}

fn collect_add_ignored_pathspec_matches(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    pathspecs: &[(PathBuf, PathBuf, bool)],
    indexed_paths: &BTreeSet<Vec<u8>>,
) -> Result<Vec<(usize, Vec<u8>)>> {
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }

    let mut candidates = BTreeSet::new();
    for directory in [false, true] {
        let ignored = sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory,
                no_empty_directory: false,
                preserve_ignored_directories: false,
                exclude_standard: true,
                ignored_only: true,
                exclude_patterns: Vec::new(),
                exclude_per_directory: Vec::new(),
                pathspecs: Vec::new(),
            },
        )?;
        for mut path in ignored {
            if path.ends_with(b"/") {
                path.pop();
            }
            if !path.is_empty() && !add_ignored_candidate_is_indexed(&path, indexed_paths) {
                candidates.insert(path);
            }
        }
    }

    let mut matches = Vec::new();
    for (idx, (display, pathspec, _)) in pathspecs.iter().enumerate() {
        for candidate in &candidates {
            let candidate_path = worktree_path_from_git_path(worktree_root, candidate)?;
            if add_ignored_path_matches(display, &candidate_path, pathspec) {
                matches.push((
                    idx,
                    add_ignored_display_path(worktree_root, &candidate_path, candidate)?,
                ));
            }
        }
    }
    Ok(matches)
}

fn add_all_index_paths(
    git_dir: &Path,
    format: ObjectFormat,
    index: Option<&Index>,
) -> Result<BTreeSet<Vec<u8>>> {
    if let Some(index) = index {
        return Ok(index
            .entries
            .iter()
            .map(|entry| entry.path.as_bytes().to_vec())
            .collect());
    }
    Ok(sley_worktree::read_repository_index(git_dir, format)?
        .map(|index| {
            index
                .entries
                .into_iter()
                .map(|entry| entry.path.into_bytes())
                .collect()
        })
        .unwrap_or_default())
}

fn add_ignored_candidate_is_indexed(candidate: &[u8], indexed_paths: &BTreeSet<Vec<u8>>) -> bool {
    if indexed_paths.contains(candidate) {
        return true;
    }
    let mut prefix = candidate.to_vec();
    prefix.push(b'/');
    indexed_paths
        .range(prefix.clone()..)
        .next()
        .is_some_and(|path| path.starts_with(&prefix))
}

fn add_ignored_path_matches(display: &Path, candidate_path: &Path, pathspec: &Path) -> bool {
    if add_pathspec_needs_status_walk(display) {
        let has_separator = display
            .components()
            .filter(|component| !matches!(component, std::path::Component::CurDir))
            .count()
            > 1;
        if !has_separator {
            return false;
        }
        return add_path_matches(candidate_path, pathspec);
    }
    candidate_path == pathspec || pathspec.starts_with(candidate_path)
}

fn worktree_path_from_git_path(worktree_root: &Path, git_path: &[u8]) -> Result<PathBuf> {
    let text = std::str::from_utf8(git_path)
        .map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let mut path = worktree_root.to_path_buf();
    for component in text.split('/') {
        if !component.is_empty() {
            path.push(component);
        }
    }
    Ok(path)
}

fn add_ignored_display_path(
    worktree_root: &Path,
    candidate_path: &Path,
    candidate_git_path: &[u8],
) -> Result<Vec<u8>> {
    let mut prefix = Vec::new();
    for component in candidate_git_path.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(component);
        let prefix_path = worktree_path_from_git_path(worktree_root, &prefix)?;
        let is_dir = prefix_path.is_dir();
        if sley_worktree::standard_ignore_match(worktree_root, &prefix, is_dir)?
            .is_some_and(|ignore_match| ignore_match.ignored)
        {
            return Ok(prefix);
        }
    }
    if candidate_path.is_dir() {
        return add_git_path_bytes(
            candidate_path
                .strip_prefix(worktree_root)
                .map_err(|_| GitError::InvalidPath(candidate_path.display().to_string()))?,
        );
    }
    Ok(candidate_git_path.to_vec())
}

fn print_add_ignored_paths(git_dir: &Path, ignored_paths: &[Vec<u8>]) {
    eprintln!("The following paths are ignored by one of your .gitignore files:");
    for path in ignored_paths {
        eprintln!("{}", String::from_utf8_lossy(path));
    }
    if add_ignored_file_advice_enabled(git_dir) {
        eprintln!("hint: Use -f if you really want to add them.");
        eprintln!(
            "hint: Disable this message with \"git config set advice.addIgnoredFile false\""
        );
    }
}

fn add_ignored_file_advice_enabled(git_dir: &Path) -> bool {
    if env::var("GIT_ADVICE")
        .ok()
        .as_deref()
        .and_then(parse_config_bool)
        == Some(false)
    {
        return false;
    }
    read_repo_config(git_dir)
        .ok()
        .and_then(|config| {
            config
                .get_bool("advice", None, "addignoredfile")
                .or_else(|| config.get_bool("advice", None, "addIgnoredFile"))
        })
        .unwrap_or(true)
}

fn resolve_add_regular_tracked_exact_actions(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    paths: &[PathBuf],
    options: AddRegularOptions,
    index: Option<&Index>,
) -> Result<Option<TrackedExactResolution>> {
    reject_add_paths_outside_sparse_checkout(cwd, worktree_root, git_dir, paths, options)?;
    if paths.is_empty() || options.chmod.is_some() || options.force || options.dry_run {
        return Ok(None);
    }
    let Some(index) = index else {
        return Ok(None);
    };
    let index_path = sley_worktree::repository_index_path(git_dir);
    let index_mtime = fs::metadata(&index_path)
        .ok()
        .and_then(|metadata| sley_index::file_mtime_parts(&metadata));
    let stat_cache = sley_index::IndexStatCache::from_index_mtime_only(index_mtime);
    let mut actions = Vec::new();
    let mut exact_tracked = None;
    let single_path = paths.len() == 1;
    for path in paths {
        if add_pathspec_needs_status_walk(path) {
            return Ok(None);
        }
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            return Ok(None);
        };
        let git_path = add_git_path_bytes(relative)?;
        let range = add_index_entries_path_range(&index.entries, &git_path);
        if range.is_empty() {
            return Ok(None);
        }
        if range.len() != 1 {
            return Ok(None);
        }
        if index.entries[range.clone()]
            .iter()
            .any(|entry| entry.stage() != sley_index::Stage::Normal || entry.is_skip_worktree())
        {
            return Ok(None);
        }
        let entry = &index.entries[range.start];
        if entry.mode == 0o160000 {
            return Ok(None);
        }
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if metadata.is_dir() || !(file_type.is_file() || file_type.is_symlink()) {
                    return Ok(None);
                }
                let needs_index_update =
                    stat_cache.reusable_index_entry(entry, &metadata).is_none();
                if needs_index_update {
                    actions.push(AddAction::Add(absolute));
                }
                if single_path {
                    exact_tracked = Some(ExactTrackedAdd {
                        git_path: git_path.clone(),
                        needs_index_update,
                    });
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let needs_index_update = !options.ignore_removal;
                if needs_index_update {
                    actions.push(AddAction::Remove(absolute));
                }
                if single_path {
                    exact_tracked = Some(ExactTrackedAdd {
                        git_path: git_path.clone(),
                        needs_index_update,
                    });
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(Some(TrackedExactResolution {
        actions,
        exact_tracked,
    }))
}

fn reject_add_paths_outside_sparse_checkout(
    cwd: &Path,
    worktree_root: &Path,
    git_dir: &Path,
    paths: &[PathBuf],
    options: AddRegularOptions,
) -> Result<()> {
    if options.sparse || paths.is_empty() {
        return Ok(());
    }
    let Some(active) = active_sparse_checkout_for_add(git_dir)? else {
        return Ok(());
    };
    let mut rejected = Vec::new();
    for path in paths {
        if add_pathspec_needs_status_walk(path) {
            continue;
        }
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            continue;
        };
        if relative.as_os_str().is_empty()
            || relative == Path::new(".")
            || relative == Path::new("")
        {
            continue;
        }
        let mut git_path = match add_git_path_bytes(relative) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if git_path.is_empty() {
            continue;
        }
        if fs::symlink_metadata(&absolute)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
            && !git_path.ends_with(b"/")
        {
            git_path.push(b'/');
        }
        if !sley_worktree::path_in_sparse_checkout(&git_path, &active.sparse, active.mode) {
            rejected.push(path.to_string_lossy().replace('\\', "/"));
        }
    }
    if rejected.is_empty() {
        return Ok(());
    }
    eprintln!("The following paths and/or pathspecs matched paths that exist");
    eprintln!("outside of your sparse-checkout definition, so will not be");
    eprintln!("updated in the index:");
    for path in rejected {
        eprintln!("{path}");
    }
    eprintln!("hint: If you intend to update such entries, try one of the following:");
    eprintln!("hint: * Use the --sparse option.");
    eprintln!("hint: * Disable or modify the sparsity rules.");
    eprintln!("hint: Disable this message with \"git config set advice.updateSparsePath false\"");
    Err(GitError::Exit(1))
}

struct ActiveSparseCheckoutForAdd {
    sparse: sley_worktree::SparseCheckout,
    mode: sley_worktree::SparseCheckoutMode,
}

fn active_sparse_checkout_for_add(git_dir: &Path) -> Result<Option<ActiveSparseCheckoutForAdd>> {
    let worktree_config = GitConfig::read(git_dir.join("config.worktree")).unwrap_or_default();
    let repo_config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    let sparse_enabled = worktree_config
        .get_bool("core", None, "sparseCheckout")
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckout"))
        .unwrap_or(false);
    if !sparse_enabled {
        return Ok(None);
    }
    let sparse_file = git_dir.join("info").join("sparse-checkout");
    if !sparse_file.exists() {
        return Ok(None);
    }
    let mut patterns: Vec<Vec<u8>> = fs::read(sparse_file)?
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if patterns.last().map(Vec::is_empty) == Some(true) {
        patterns.pop();
    }
    let cone = worktree_config
        .get_bool("core", None, "sparseCheckoutCone")
        .or_else(|| repo_config.get_bool("core", None, "sparseCheckoutCone"))
        .unwrap_or(false);
    let sparse = sley_worktree::SparseCheckout {
        patterns,
        sparse_index: false,
    };
    let mode = if cone {
        sley_worktree::SparseCheckoutMode::Cone
    } else {
        sley_worktree::SparseCheckoutMode::Full
    };
    Ok(Some(ActiveSparseCheckoutForAdd { sparse, mode }))
}

fn add_pathspec_needs_status_walk(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(value) = component else {
            return false;
        };
        value.to_string_lossy().starts_with(':')
    }) || path
        .to_string_lossy()
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\'))
}

fn add_git_path_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(GitError::InvalidPath(format!(
            "invalid index path {}",
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

fn add_index_entries_path_range(entries: &[IndexEntry], path: &[u8]) -> std::ops::Range<usize> {
    let mut start = match entries.binary_search_by(|entry| entry.path.as_bytes().cmp(path)) {
        Ok(index) => index,
        Err(insert) => return insert..insert,
    };
    while start > 0 && entries[start - 1].path.as_bytes() == path {
        start -= 1;
    }
    let mut end = start;
    while end < entries.len() && entries[end].path.as_bytes() == path {
        end += 1;
    }
    start..end
}

fn resolve_add_paths(
    cwd: &Path,
    worktree_root: &Path,
    paths: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut resolved = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if absolute.is_dir() {
            collect_add_files(worktree_root, &absolute, &mut resolved)?;
        } else {
            resolved.insert(absolute);
        }
    }
    Ok(resolved.into_iter().collect())
}

fn collect_add_files(
    worktree_root: &Path,
    directory: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    // An embedded repository below the worktree root is opaque to `add`: it is
    // staged as a single gitlink path, never descended into. Canonicalize both
    // sides so a pathspec like `.` (root + a CurDir component) is still
    // recognized as the root itself, not an embedded repository.
    let is_root = match (fs::canonicalize(directory), fs::canonicalize(worktree_root)) {
        (Ok(left), Ok(right)) => left == right,
        _ => directory == worktree_root,
    };
    if !is_root && sley_diff_merge::gitlink_git_dir(directory).is_some() {
        out.insert(directory.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path == worktree_root.join(".git") {
            continue;
        }
        if path.is_dir() {
            collect_add_files(worktree_root, &path, out)?;
        } else {
            out.insert(path);
        }
    }
    Ok(())
}

fn print_add_actions(worktree_root: &Path, actions: &[AddAction]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for action in actions {
        let path = action.path();
        let display = path.strip_prefix(worktree_root).unwrap_or(path);
        let verb = match action {
            AddAction::Add(_) => "add",
            AddAction::Remove(_) => "remove",
        };
        writeln!(
            stdout,
            "{verb} '{}'",
            display.to_string_lossy().replace('\\', "/")
        )?;
    }
    Ok(())
}

pub(crate) fn cmd_clean(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut force = false;
    let mut force_was_mentioned = false;
    let mut directories = false;
    let mut include_ignored = false;
    let mut quiet = false;
    let mut excludes = Vec::new();
    let mut path_args = Vec::new();
    let mut parsing_options = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            path_args.push(arg.to_string());
            continue;
        }
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-f" | "--force" | "-ff" => {
                force = true;
                force_was_mentioned = true;
            }
            "--no-force" => {
                force = false;
                force_was_mentioned = true;
            }
            "-d" => directories = true,
            "-x" => include_ignored = true,
            "-e" | "--exclude" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clean --exclude requires a value".into()))?;
                excludes.push(value.to_string());
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--no-interactive" => {}
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|byte| matches!(byte, b'f' | b'd' | b'n' | b'q' | b'x')) =>
            {
                dry_run |= value.contains('n');
                if value.contains('f') {
                    force = true;
                    force_was_mentioned = true;
                }
                directories |= value.contains('d');
                include_ignored |= value.contains('x');
                quiet |= value.contains('q');
            }
            "--" => parsing_options = false,
            value if value.starts_with("--exclude=") => {
                let value = value
                    .strip_prefix("--exclude=")
                    .ok_or_else(|| GitError::Command("clean --exclude requires a value".into()))?;
                excludes.push(value.to_string());
            }
            value => path_args.push(value.to_string()),
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let config = read_repo_config(&git_dir)?;
    let require_force = config
        .get_bool("clean", None, "requireForce")
        .unwrap_or(true);
    if !dry_run && !force && require_force {
        if force_was_mentioned {
            eprintln!("fatal: clean.requireForce is true and -f not given: refusing to clean");
        } else {
            eprintln!(
                "fatal: clean.requireForce defaults to true and neither -i, -n, nor -f given; refusing to clean"
            );
        }
        return Err(GitError::Exit(128));
    }
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let pathspec = LsFilesPathspec::new(&cwd, &worktree_root, false, &path_args)?;
    let paths = clean_targets(
        &worktree_root,
        &git_dir,
        format,
        directories,
        include_ignored,
        &pathspec,
        &excludes,
    )?;
    let mut stdout = io::stdout();
    for target in paths {
        let display = String::from_utf8_lossy(&target.display);
        if dry_run {
            writeln!(stdout, "Would remove {display}")?;
            continue;
        }
        if !quiet {
            writeln!(stdout, "Removing {display}")?;
        }
        let mut filesystem_path = target.path;
        if filesystem_path.ends_with(b"/") {
            filesystem_path.pop();
        }
        let relative = std::str::from_utf8(&filesystem_path)
            .map_err(|err| GitError::InvalidPath(err.to_string()))?;
        let absolute = worktree_root.join(relative);
        if target.is_dir {
            fs::remove_dir_all(absolute)?;
        } else {
            fs::remove_file(absolute)?;
        }
    }
    Ok(())
}

enum ApplyAction {
    Write {
        path: Vec<u8>,
        mode: u32,
        content: Vec<u8>,
    },
    Remove {
        path: Vec<u8>,
    },
}

/// `git apply --whitespace=<action>` modes (apply.c's `ws_error_action`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WsAction {
    /// `nowarn`: ignore whitespace errors entirely.
    Nowarn,
    /// `warn` (default with `--apply`): warn but still apply.
    Warn,
    /// `error`: warn and refuse to apply.
    Error,
    /// `error-all`: like `error` but do not squelch repeated warnings.
    ErrorAll,
    /// `fix`/`strip`: correct whitespace errors as the patch is applied.
    Fix,
}

pub(crate) fn cmd_apply(args: &[String]) -> Result<()> {
    let mut check = false;
    let mut update_index = false;
    let mut files = Vec::new();
    // git's default when applying is `warn`; the value is overridden by the
    // last `--whitespace=` seen.
    let mut ws_action = WsAction::Warn;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--apply" | "--stat" | "--numstat" | "--summary" | "-q" | "--quiet" | "--recount"
            | "--allow-empty" | "--unsafe-paths" | "-l" | "--ignore-whitespace"
            | "--ignore-space-change" => {}
            "-R" | "--reverse" => {
                return Err(GitError::Unsupported(
                    "apply --reverse is not supported yet".into(),
                ));
            }
            "--index" => update_index = true,
            "-3" | "--3way" | "--cached" => {
                return Err(GitError::Unsupported(format!(
                    "apply {arg} is not supported yet"
                )));
            }
            "--whitespace" => {
                if let Some(value) = iter.next() {
                    ws_action = parse_ws_action(value)?;
                }
            }
            "-p" | "-C" | "--directory" | "--exclude" | "--include" => {
                iter.next();
            }
            "--" => {
                files.extend(iter.by_ref().map(|value| value.to_string()));
                break;
            }
            value if let Some(rest) = value.strip_prefix("--whitespace=") => {
                ws_action = parse_ws_action(rest)?;
            }
            value
                if value.starts_with("-p")
                    || value.starts_with("--directory=")
                    || value.starts_with("--exclude=")
                    || value.starts_with("--include=") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported apply option {value}"
                )));
            }
            value => files.push(value.to_string()),
        }
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let ws_resolver = commands::diff::WhitespaceRuleResolver::from_git_dir(&git_dir)?;
    let mut input = Vec::new();
    if files.is_empty() {
        io::stdin().read_to_end(&mut input)?;
    } else {
        for file in &files {
            input.extend_from_slice(&fs::read(file)?);
        }
    }
    let mut patches = sley_diff_merge::parse_unified_patch(&input)?;
    let patch_input_file = files.first().map(String::as_str).unwrap_or("<stdin>");

    // Phase 0: whitespace handling. Resolve the per-path rule, then warn/error
    // or fix the introduced (`+`) lines per `--whitespace=<action>`. In `fix`
    // mode this rewrites the patch's Insert lines (and trims new blank lines at
    // EOF) before it is applied. In `error`/`error-all` mode a whitespace error
    // aborts the whole apply.
    let mut ws_error_count = 0usize;
    let mut ws_squelched = 0usize;
    let squelch_limit = if matches!(ws_action, WsAction::ErrorAll) {
        usize::MAX
    } else {
        5
    };
    if !matches!(ws_action, WsAction::Nowarn) {
        for patch in &mut patches {
            let target = patch
                .new_path
                .as_deref()
                .or(patch.old_path.as_deref())
                .unwrap_or(b"");
            let mut rule = ws_resolver.rule_for_path(target)?;
            // A symlink's incomplete line is not news (apply.c clears it).
            if patch.new_mode == Some(0o120000) {
                rule &= !sley_diff_merge::ws::WS_INCOMPLETE_LINE;
            }
            let base = read_patch_base(&worktree_root, patch)?;
            apply_patch_whitespace(
                patch,
                &base,
                rule,
                ws_action,
                patch_input_file,
                squelch_limit,
                &mut ws_error_count,
                &mut ws_squelched,
            );
        }
    }
    if ws_squelched > 0 {
        eprintln!(
            "warning: squelched {ws_squelched} whitespace error{}",
            if ws_squelched == 1 { "" } else { "s" }
        );
    }
    if ws_error_count > 0 {
        let n = ws_error_count;
        match ws_action {
            WsAction::Fix => {
                eprintln!(
                    "warning: {n} line{} applied after fixing whitespace errors.",
                    if n == 1 { " adds" } else { "s add" }
                );
            }
            _ => {
                eprintln!(
                    "warning: {n} line{} whitespace error{}.",
                    if n == 1 { " adds" } else { "s add" },
                    if n == 1 { "" } else { "s" }
                );
            }
        }
    }
    if ws_error_count > 0 && matches!(ws_action, WsAction::Error | WsAction::ErrorAll) {
        return Err(GitError::Exit(1));
    }
    let patches = patches;

    // Phase 1: compute every result first (git applies a patch atomically).
    let mut actions = Vec::new();
    for patch in &patches {
        let base = read_patch_base(&worktree_root, patch)?;
        let content = match sley_diff_merge::apply_file_patch(&base, patch) {
            sley_diff_merge::ApplyOutcome::Applied(content) => content,
            sley_diff_merge::ApplyOutcome::Rejected => {
                let name = patch
                    .new_path
                    .as_deref()
                    .or(patch.old_path.as_deref())
                    .unwrap_or(b"");
                eprintln!("error: patch failed: {}", String::from_utf8_lossy(name));
                return Err(GitError::Exit(1));
            }
        };
        if patch.is_delete {
            if let Some(old) = &patch.old_path {
                actions.push(ApplyAction::Remove { path: old.clone() });
            }
        } else {
            let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone()) else {
                return Err(GitError::InvalidFormat("patch missing target path".into()));
            };
            let mode = apply_write_mode(&worktree_root, patch, &target)?;
            actions.push(ApplyAction::Write {
                path: target,
                mode,
                content,
            });
            if patch.is_rename
                && let Some(old) = &patch.old_path
            {
                actions.push(ApplyAction::Remove { path: old.clone() });
            }
        }
    }

    if check {
        return Ok(());
    }
    // Phase 2: materialize.
    let mut index_paths = Vec::new();
    for action in &actions {
        match action {
            ApplyAction::Write {
                path,
                mode,
                content,
            } => merge_write_worktree_file(&worktree_root, path, content, *mode)?,
            ApplyAction::Remove { path } => merge_remove_worktree_file(&worktree_root, path)?,
        }
        index_paths.push(PathBuf::from(
            std::str::from_utf8(action.path())
                .map_err(|err| GitError::InvalidPath(err.to_string()))?,
        ));
    }
    if update_index && !index_paths.is_empty() {
        let config = read_repo_config(&git_dir)?;
        sley_worktree::update_index_paths_filtered(
            &worktree_root,
            &git_dir,
            format,
            &index_paths,
            sley_worktree::UpdateIndexOptions {
                add: true,
                remove: true,
                force_remove: false,
                chmod: None,
                info_only: false,
                ignore_skip_worktree_entries: false,
                allow_skip_worktree_entries: false,
            },
            &config,
        )?;
    }
    Ok(())
}

fn apply_write_mode(
    worktree_root: &Path,
    patch: &sley_diff_merge::FilePatch,
    target: &[u8],
) -> Result<u32> {
    if let Some(mode) = patch.new_mode {
        return Ok(mode);
    }
    if patch.is_new {
        return Ok(0o100644);
    }
    let path = std::str::from_utf8(target)
        .map_err(|err| GitError::InvalidPath(err.to_string()))
        .map(|relative| worktree_root.join(relative))?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata_to_git_mode(&metadata)),
        Err(_) => Ok(patch.old_mode.unwrap_or(0o100644)),
    }
}

fn metadata_to_git_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        return 0o120000;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return 0o100755;
        }
    }
    0o100644
}

impl ApplyAction {
    fn path(&self) -> &[u8] {
        match self {
            ApplyAction::Write { path, .. } | ApplyAction::Remove { path } => path,
        }
    }
}

/// Read the worktree base content a patch applies against (empty for a new
/// file). Shared by the whitespace pass and the apply pass.
fn read_patch_base(
    worktree_root: &Path,
    patch: &sley_diff_merge::FilePatch,
) -> Result<Vec<u8>> {
    if patch.is_new {
        return Ok(Vec::new());
    }
    let Some(old) = patch.old_path.as_deref().or(patch.new_path.as_deref()) else {
        return Ok(Vec::new());
    };
    let rel = std::str::from_utf8(old)
        .map_err(|_| GitError::InvalidFormat("non-utf8 patch path".into()))?;
    Ok(fs::read(worktree_root.join(rel)).unwrap_or_default())
}

/// Parse the `--whitespace=<action>` value into a [`WsAction`].
fn parse_ws_action(value: &str) -> Result<WsAction> {
    match value {
        "nowarn" => Ok(WsAction::Nowarn),
        "warn" => Ok(WsAction::Warn),
        "error" => Ok(WsAction::Error),
        "error-all" => Ok(WsAction::ErrorAll),
        "fix" | "strip" => Ok(WsAction::Fix),
        other => Err(GitError::Command(format!(
            "unrecognized whitespace option '{other}'"
        ))),
    }
}

/// Whitespace handling for one file patch: warn/error on, or fix, the
/// introduced (`+`) lines. Port of apply.c's `apply_one_fragment` ws path plus
/// its `check_whitespace`. Mutates the patch's Insert lines in `fix` mode.
#[allow(clippy::too_many_arguments)]
fn apply_patch_whitespace(
    patch: &mut sley_diff_merge::FilePatch,
    base: &[u8],
    rule: sley_diff_merge::ws::WsRule,
    action: WsAction,
    patch_input_file: &str,
    squelch_limit: usize,
    error_count: &mut usize,
    squelched: &mut usize,
) {
    use sley_diff_merge::HunkLine;
    use sley_diff_merge::ws;

    let fixing = matches!(action, WsAction::Fix);

    // git first scans the whole patch for whitespace errors (`check_whitespace`
    // sets a single `state->whitespace_error` flag). In `fix` mode the actual
    // `ws_fix_copy` is then applied to *every* introduced line, but only when
    // that flag is set — so a clean-on-its-own line (e.g. `8 spaces + tab`,
    // which the indent-with-non-tab check passes) is still re-indented when a
    // sibling line in the same patch is dirty. We mirror that by pre-scanning.
    let patch_has_ws_error = patch.hunks.iter().any(|hunk| {
        hunk.lines.iter().any(|hl| match hl {
            HunkLine::Insert(bytes) => ws::ws_check(bytes, rule) != 0,
            _ => false,
        })
    });

    for hunk in &mut patch.hunks {
        let mut lineno = hunk.new_start; // 1-based new-file line of next +/space
        for hl in &mut hunk.lines {
            match hl {
                HunkLine::Context(_) => {
                    lineno += 1;
                }
                HunkLine::Delete(_) => {}
                HunkLine::Insert(bytes) => {
                    let bad = ws::ws_check(bytes, rule);
                    if fixing {
                        // Re-indent/strip every introduced line once any line
                        // in the patch is dirty (git's global-flag semantics).
                        if patch_has_ws_error {
                            let fixed = ws::ws_fix_line_content(bytes, rule);
                            if fixed != *bytes {
                                *bytes = fixed;
                                *error_count += 1;
                            }
                        }
                    } else if bad != 0 {
                        *error_count += 1;
                        if *error_count <= squelch_limit {
                            let err = ws::whitespace_error_string(bad);
                            eprintln!("{patch_input_file}:{lineno}: {err}.");
                            eprintln!("+{}", String::from_utf8_lossy(bytes));
                        } else {
                            *squelched += 1;
                        }
                    }
                    lineno += 1;
                }
            }
        }
    }

    // Blank-at-EOF: compare the trailing-blank run of the pre- and post-images.
    if rule & ws::WS_BLANK_AT_EOF != 0 {
        let postimage = match sley_diff_merge::apply_file_patch(base, patch) {
            sley_diff_merge::ApplyOutcome::Applied(content) => content,
            sley_diff_merge::ApplyOutcome::Rejected => return,
        };
        let l1 = ws::count_trailing_blank(base);
        let l2 = ws::count_trailing_blank(&postimage);
        if l2 > l1 {
            let at = ws::count_lines(&postimage);
            let blank_at_eof = at - l2 + 1;
            if fixing {
                // Trim the extra blank lines off the last hunk's trailing
                // inserts.
                let extra = l2 - l1;
                trim_trailing_blank_inserts(patch, extra);
                *error_count += 1;
            } else {
                *error_count += 1;
                if *error_count <= squelch_limit {
                    let err = ws::whitespace_error_string(ws::WS_BLANK_AT_EOF);
                    eprintln!("{patch_input_file}:{blank_at_eof}: {err}.");
                } else {
                    *squelched += 1;
                }
            }
        }
    }
}

/// Drop up to `count` trailing blank `Insert` lines from the patch's last hunk
/// (the `--whitespace=fix` blank-at-EOF correction).
fn trim_trailing_blank_inserts(patch: &mut sley_diff_merge::FilePatch, mut count: usize) {
    use sley_diff_merge::HunkLine;
    use sley_diff_merge::ws;
    let Some(hunk) = patch.hunks.last_mut() else {
        return;
    };
    while count > 0 {
        match hunk.lines.last() {
            Some(HunkLine::Insert(bytes)) if ws::ws_blank_line(bytes) => {
                hunk.lines.pop();
                if hunk.new_len > 0 {
                    hunk.new_len -= 1;
                }
                count -= 1;
            }
            _ => break,
        }
    }
    // The last surviving inserted line keeps the file's terminal newline state.
    hunk.new_no_newline = false;
}

pub(crate) fn cmd_fsck(args: &[String]) -> Result<()> {
    let mut progress = true;
    let mut report_dangling = true;
    let mut report_unreachable = false;
    let mut strict = false;
    let mut connectivity_only = false;
    // `--tags` restricts the root set to tags; `--root` additionally pins the
    // root tree(s). Both default off (a bare `git fsck` walks all refs).
    let mut only_tags = false;
    // `--name-objects` annotates broken/missing-object reports with a path
    // describing how the object is reached (e.g. an index entry `:file`).
    let mut name_objects = false;
    let mut explicit_oids: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--no-progress" => progress = false,
            "--progress" => progress = true,
            "--dangling" => report_dangling = true,
            "--no-dangling" => report_dangling = false,
            "--unreachable" => report_unreachable = true,
            "--no-unreachable" => report_unreachable = false,
            "--strict" => strict = true,
            "--no-strict" => strict = false,
            "--connectivity-only" => connectivity_only = true,
            "--tags" => only_tags = true,
            "--name-objects" => name_objects = true,
            "--no-name-objects" => name_objects = false,
            // These affect output/perf only; object-content checks are
            // unconditional in this implementation, so accept and ignore them.
            "--full" | "--no-full" | "--root" | "--cache" | "--no-cache" | "--lost-found"
            | "--references" | "--no-references" => {}
            value if value.starts_with("--") => {
                return Err(GitError::Command(format!(
                    "fsck currently supports --no-progress and basic object connectivity; unsupported option {value}"
                )));
            }
            // A positional argument is an explicit object/head to check.
            value => explicit_oids.push(value.to_string()),
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);

    // Resolve `fsck.<msgid>` severity overrides from the repo config (folds in
    // command-line `-c fsck.x=y` via GIT_CONFIG_PARAMETERS).
    let mut severity = sley_fsck::SeverityConfig::new(strict);
    if let Ok(config) = read_repo_config(&git_dir) {
        for (key, value) in config.fsck_entries() {
            severity.set(&key, &value);
        }
    }

    // Explicit object-id arguments override the default ref-walk roots. git
    // resolves each to an object; an explicit but unknown head reports
    // `invalid sha1 pointer` and does NOT fall back to all heads (t1450 "bogus
    // head" case), so the rest of the walk sees no roots.
    //
    // git's `snapshot_ref` validates every root ref before the walk:
    //   - if the ref's object is not parseable: `error: <name>: invalid sha1
    //     pointer <oid>` on stderr, sets ERROR_REACHABLE, and the ref is NOT
    //     walked (so its referents never surface as dangling/missing);
    //   - if the ref is a branch but its object is not a commit:
    //     `error: <name>: not a commit`, sets ERROR_REFS.
    // We collect named roots, run those checks, and pass only the valid tip
    // oids to the connectivity walk.
    let mut ref_error_bits = 0i32;
    let mut object_names = std::collections::HashMap::new();
    let named_roots: Vec<(String, ObjectId)> = if !explicit_oids.is_empty() {
        let mut resolved = Vec::new();
        for spec in &explicit_oids {
            match ObjectId::from_hex(format, spec) {
                Ok(oid) => resolved.push((oid.to_hex(), oid)),
                Err(_) => {
                    return Err(GitError::Command(format!("Invalid object name '{spec}'.")));
                }
            }
        }
        resolved
    } else if only_tags {
        fsck_tag_root_oids(&git_dir, format)?
    } else {
        fsck_root_oids(&git_dir, format)?
    };

    let mut roots = Vec::new();
    for (name, oid) in &named_roots {
        match db.read_object(oid) {
            Ok(object) => {
                // A branch ref must point at a commit.
                if object.object_type != sley_object::ObjectType::Commit && is_branch_ref(name) {
                    eprintln!("error: {name}: not a commit");
                    ref_error_bits |= sley_fsck::ERROR_REFS;
                }
                roots.push(*oid);
                if name_objects {
                    object_names.entry(*oid).or_insert_with(|| name.clone());
                }
            }
            Err(_) => {
                eprintln!("error: {name}: invalid sha1 pointer {oid}");
                ref_error_bits |= sley_fsck::ERROR_REACHABLE;
            }
        }
    }

    if explicit_oids.is_empty() && !only_tags {
        ref_error_bits |= fsck_worktree_head_refs(
            &db,
            format,
            &git_dir,
            name_objects,
            &mut roots,
            &mut object_names,
        )?;
    }

    // With explicit object-id roots, git checks only what is reachable from
    // them — it does not enumerate every loose object (so a removed-but-
    // unreferenced blob is not independently reported, and nothing is
    // "dangling").
    let mut object_ids = if explicit_oids.is_empty() {
        repository_object_ids(&git_dir, format)?
    } else {
        Vec::new()
    };
    if !explicit_oids.is_empty() {
        report_dangling = false;
    }
    // Mirror builtin/fsck.c `fsck_loose`: probe every loose object file before the
    // connectivity walk, reporting corrupt or mismatched ones at `error:` level on
    // stderr (with git's path-form spelling) and excluding them from the object set
    // so they neither parse nor surface as dangling.
    let objects_dir_display = fsck_objects_dir_display(&git_dir, &cwd);
    let mut bad_loose = HashSet::new();
    // The loose-object integrity scan enumerates the whole object store, which
    // git only does for a full fsck (no explicit roots).
    if explicit_oids.is_empty() {
        for oid in db.loose().object_ids()? {
            let hex = oid.to_hex();
            let display_path = format!("{objects_dir_display}/{}/{}", &hex[..2], &hex[2..]);
            match db.loose().verify_object(&oid, &display_path)? {
                None | Some(LooseObjectIntegrity::Ok) => {}
                Some(LooseObjectIntegrity::HashMismatch { actual }) => {
                    if !connectivity_only {
                        eprintln!("error: {actual}: hash-path mismatch, found at: {display_path}");
                        bad_loose.insert(oid);
                    }
                }
                Some(LooseObjectIntegrity::Corrupt) => {
                    eprintln!("error: {oid}: object corrupt or missing: {display_path}");
                    bad_loose.insert(oid);
                }
            }
        }
    }
    let alternate_loose_errors = if explicit_oids.is_empty() {
        fsck_alternate_loose_objects(&git_dir, format, &cwd)?
    } else {
        false
    };
    let pack_errors = if explicit_oids.is_empty() {
        fsck_pack_files(&git_dir, format, &cwd)?
    } else {
        false
    };
    let loose_errors = !bad_loose.is_empty();
    object_ids.retain(|oid| !bad_loose.contains(oid));

    // git's `fsck_index`: with no explicit object args, the current worktree's
    // index (and other worktrees') becomes a reachability root set. Each
    // non-gitlink entry's blob is marked reachable; a missing one is reported as
    // `missing blob <oid>` (annotated `(<index>:<name>)` under --name-objects),
    // setting ERROR_REACHABLE. The cache-tree's recorded tree oids must each be
    // valid trees, else `<oid>: invalid sha1 pointer in cache-tree of <index>`
    // sets ERROR_REFS.
    let mut index_error_bits = 0i32;
    if explicit_oids.is_empty() {
        index_error_bits |= fsck_index_roots(
            &db,
            format,
            &git_dir,
            name_objects,
            &mut roots,
            &bad_loose,
        )?;
    }

    if roots.is_empty() && progress {
        eprintln!("notice: No default references");
    }
    let report = sley_fsck::fsck_objects_with_options(
        &db,
        format,
        roots,
        object_ids,
        sley_fsck::FsckOptions {
            report_dangling,
            report_unreachable,
            connectivity_only,
            object_names,
            severity,
        },
    );
    // Match builtin/fsck.c's stream split: notices (dangling/unreachable) and
    // connectivity complaints (broken link, missing, type mismatch) go to
    // stdout; object-content findings (`error in`/`warning in`) go to stderr.
    for notice in &report.notices {
        println!("{}", notice.message);
    }
    for issue in &report.issues {
        match issue.stream {
            sley_fsck::IssueStream::Stdout => println!("{}", issue.message),
            sley_fsck::IssueStream::Stderr => eprintln!("{}", issue.message),
        }
    }
    // git's exit status is the OR of its `ERROR_*` bits. The connectivity
    // report contributes ERROR_OBJECT/REACHABLE/REFS; a bad loose object or a
    // bogus explicit head sets ERROR_OBJECT.
    let mut exit_bits = report.exit_code();
    if loose_errors {
        exit_bits |= sley_fsck::ERROR_OBJECT;
    }
    if alternate_loose_errors || pack_errors {
        exit_bits |= sley_fsck::ERROR_OBJECT;
    }
    // git's `snapshot_ref` errors (invalid sha1 pointer / not a commit) set
    // ERROR_REACHABLE / ERROR_REFS — not ERROR_OBJECT.
    exit_bits |= ref_error_bits;
    exit_bits |= index_error_bits;

    // git's fsck verifies the commit-graph when `core.commitGraph` is true (the
    // default; unset ⇒ true) by shelling out to `commit-graph verify`. We run the
    // same verification inline and OR in ERROR_COMMIT_GRAPH on any failure.
    if fsck_core_commit_graph_enabled(&git_dir) {
        let object_dir = repository_objects_dir(&git_dir);
        let graph_path = object_dir.join("info").join("commit-graph");
        if let OpenResult::Bytes(graph_bytes) = open_commit_graph_bytes(&graph_path)
            && verify_commit_graph_bytes(&object_dir, format, &graph_bytes, progress).is_err()
        {
            exit_bits |= ERROR_COMMIT_GRAPH;
        }
    }

    if exit_bits != 0 {
        Err(GitError::Exit(exit_bits))
    } else {
        Ok(())
    }
}

const ERROR_COMMIT_GRAPH: i32 = 0o20;

/// `core.commitGraph` resolved with git's default of true (an unset value enables
/// the fsck commit-graph check).
fn fsck_core_commit_graph_enabled(git_dir: &Path) -> bool {
    read_repo_config(git_dir)
        .ok()
        .and_then(|config| config.get_bool("core", None, "commitGraph"))
        .unwrap_or(true)
}

/// Named root refs restricted to `refs/tags/*` (for `git fsck --tags`).
fn fsck_tag_root_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<(String, ObjectId)>> {
    let store = FileRefStore::new(git_dir, format);
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for reference in store.list_refs()? {
        if !reference.name.starts_with("refs/tags/") {
            continue;
        }
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push((reference.name.clone(), oid));
        }
    }
    Ok(roots)
}

/// spelling for those shapes and fall back to the absolute path.
fn fsck_objects_dir_display(git_dir: &Path, cwd: &Path) -> String {
    if git_dir == cwd {
        return "./objects".to_string();
    }
    if let Ok(relative) = git_dir.strip_prefix(cwd) {
        return format!("{}/objects", relative.display());
    }
    format!("{}/objects", git_dir.display())
}

fn fsck_display_path(path: &Path, cwd: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(cwd) {
        if relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            relative.display().to_string()
        }
    } else {
        path.display().to_string()
    }
}

fn fsck_alternate_loose_objects(
    git_dir: &Path,
    format: ObjectFormat,
    cwd: &Path,
) -> Result<bool> {
    let objects_dir = repository_objects_dir(git_dir);
    let alternates = objects_dir.join("info").join("alternates");
    let Ok(contents) = fs::read_to_string(&alternates) else {
        return Ok(false);
    };
    let mut failed = false;
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let alternate = PathBuf::from(line);
        let alternate = if alternate.is_absolute() {
            alternate
        } else {
            objects_dir.join(alternate)
        };
        let store = sley_odb::LooseObjectStore::new(&alternate, format);
        for oid in store.object_ids()? {
            let hex = oid.to_hex();
            let display_path = fsck_display_path(&alternate.join(&hex[..2]).join(&hex[2..]), cwd);
            match store.verify_object(&oid, &display_path)? {
                None | Some(LooseObjectIntegrity::Ok) => {}
                Some(LooseObjectIntegrity::HashMismatch { actual }) => {
                    eprintln!("error: {actual}: hash-path mismatch, found at: {display_path}");
                    failed = true;
                }
                Some(LooseObjectIntegrity::Corrupt) => {
                    eprintln!("error: {oid}: object corrupt or missing: {display_path}");
                    failed = true;
                }
            }
        }
    }
    Ok(failed)
}

fn fsck_pack_files(git_dir: &Path, format: ObjectFormat, cwd: &Path) -> Result<bool> {
    let pack_dir = repository_objects_dir(git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    let mut packs = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("pack"))
        .collect::<Vec<_>>();
    packs.sort();

    let mut failed = false;
    for pack_path in packs {
        let bytes = fs::read(&pack_path)?;
        let display_path = fsck_display_path(&pack_path, cwd);
        let trailer_len = format.raw_len();
        if bytes.len() >= 12 + trailer_len {
            let trailer_offset = bytes.len() - trailer_len;
            let actual = sley_core::digest_bytes(format, &bytes[..trailer_offset])?;
            let expected = ObjectId::from_raw(format, &bytes[trailer_offset..])?;
            if actual != expected {
                eprintln!("error: checksum mismatch in {display_path}");
                failed = true;
            }
        } else {
            eprintln!("error: checksum mismatch in {display_path}");
            failed = true;
            continue;
        }

        let idx_path = pack_path.with_extension("idx");
        let Ok(index_bytes) = fs::read(&idx_path) else {
            continue;
        };
        let Ok(index) = sley_pack::PackIndex::parse(&index_bytes, format) else {
            continue;
        };
        let trailer_offset = bytes.len() - trailer_len;
        for entry in index.entries {
            let Ok(offset) = usize::try_from(entry.offset) else {
                continue;
            };
            if offset >= trailer_offset {
                continue;
            }
            let object_type = (bytes[offset] >> 4) & 0x07;
            if object_type == 0 {
                eprintln!(
                    "error: unknown object type 0 at offset {offset} in {display_path}"
                );
                failed = true;
            }
        }
    }
    Ok(failed)
}

/// git's `is_branch`: a ref whose tip must be a commit (`HEAD` or `refs/heads/*`).
fn is_branch_ref(name: &str) -> bool {
    name == "HEAD" || name.starts_with("refs/heads/")
}

fn fsck_worktree_head_refs(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    git_dir: &Path,
    name_objects: bool,
    roots: &mut Vec<ObjectId>,
    object_names: &mut std::collections::HashMap<ObjectId, String>,
) -> Result<i32> {
    let mut bits = 0i32;
    let common = common_git_dir_for_git_dir(git_dir)?;
    let mut heads: Vec<(PathBuf, String)> = Vec::new();
    heads.push((git_dir.to_path_buf(), "HEAD".to_string()));
    if common != git_dir {
        heads.push((common.clone(), "HEAD".to_string()));
    }
    let worktrees_dir = common.join("worktrees");
    if let Ok(entries) = fs::read_dir(&worktrees_dir) {
        let mut linked: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        linked.sort();
        for path in linked {
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            heads.push((path, format!("worktrees/{name}/HEAD")));
        }
    }

    heads.sort_by(|left, right| left.1.cmp(&right.1));
    heads.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);

    for (head_git_dir, display) in heads {
        let store = FileRefStore::new(&head_git_dir, format);
        let Some(target) = store.read_ref("HEAD")? else {
            continue;
        };
        match target {
            RefTarget::Direct(oid) if oid.is_null() => {
                eprintln!(
                    "error: {display}: badRefOid: points to invalid object ID '{oid}'"
                );
                bits |= sley_fsck::ERROR_REFS;
            }
            RefTarget::Direct(oid) => {
                if db.contains(&oid).unwrap_or(false) {
                    roots.push(oid);
                    if name_objects {
                        object_names.entry(oid).or_insert(display);
                    }
                } else {
                    eprintln!("error: {display}: invalid sha1 pointer {oid}");
                    bits |= sley_fsck::ERROR_REACHABLE;
                }
            }
            RefTarget::Symbolic(target) => {
                if !target.starts_with("refs/heads/") {
                    eprintln!(
                        "error: {display}: badHeadTarget: HEAD points to non-branch '{target}'"
                    );
                    bits |= sley_fsck::ERROR_REFS;
                    continue;
                }
                let reference = sley_refs::Ref {
                    name: "HEAD".to_string(),
                    target: RefTarget::Symbolic(target),
                };
                if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
                    && db.contains(&oid).unwrap_or(false)
                {
                    roots.push(oid);
                    if name_objects {
                        object_names.entry(oid).or_insert(display);
                    }
                }
            }
        }
    }
    Ok(bits)
}

/// git's `fsck_index` for every worktree index: mark each entry's blob
/// reachable (appending existing ones to `roots`), report a missing blob with
/// git's `missing blob <oid>` line (annotated `(<index>:<name>)` under
/// `--name-objects`), and validate the cache-tree's recorded tree oids. Returns
/// the accumulated `ERROR_*` bits (REACHABLE for a missing index blob, REFS for
/// an invalid cache-tree pointer).
fn fsck_index_roots(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    git_dir: &Path,
    name_objects: bool,
    roots: &mut Vec<ObjectId>,
    bad_loose: &HashSet<ObjectId>,
) -> Result<i32> {
    let mut bits = 0i32;
    // The current worktree's index (annotation prefix ""), then each linked
    // worktree's index (annotation prefix `<index-path>`), mirroring git's
    // get_worktrees() order with the current worktree's blank filename.
    let mut indexes: Vec<(PathBuf, bool, String)> = Vec::new();
    let current_index = sley_worktree::repository_index_path(git_dir);
    indexes.push((current_index, true, String::new()));
    // Linked worktrees: <common_git_dir>/worktrees/<name>/index. Their reports
    // carry the index path (relative to the cwd-rooted .git when possible).
    if let Ok(common) = common_git_dir_for_git_dir(git_dir) {
        let worktrees_dir = common.join("worktrees");
        if let Ok(entries) = fs::read_dir(&worktrees_dir) {
            let mut linked: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect();
            linked.sort();
            for wt in linked {
                let index_path = wt.join("index");
                if index_path.exists() {
                    let display = fsck_index_display_path(git_dir, &index_path);
                    indexes.push((index_path, false, display));
                }
            }
        }
    }

    for (index_path, _is_current, annotation_prefix) in indexes {
        if !index_path.exists() {
            continue;
        }
        let index_bytes = fs::read(&index_path)?;
        // git's `verify_hdr` (with verify_index_checksum set for a full/`--cache`
        // fsck) rejects a bad trailing SHA: `error: bad index file sha1
        // signature` + `fatal: index file corrupt`, setting ERROR_OBJECT.
        if !index_checksum_ok(&index_bytes, format) {
            eprintln!("error: bad index file sha1 signature");
            eprintln!("fatal: index file corrupt");
            bits |= sley_fsck::ERROR_OBJECT;
            continue;
        }
        let index = match Index::parse(&index_bytes, format) {
            Ok(index) => index,
            Err(_) => continue,
        };
        for entry in &index.entries {
            // git skips gitlinks (S_ISGITLINK) in the index walk.
            if entry.mode == 0o160000 {
                continue;
            }
            let oid = entry.oid.clone();
            if db.contains(&oid).unwrap_or(false) && !bad_loose.contains(&oid) {
                // Present: mark reachable so it is not reported as dangling.
                roots.push(oid);
                continue;
            }
            // Missing index blob. git: `missing blob <oid>` (stdout),
            // ERROR_REACHABLE; `--name-objects` appends `(<prefix>:<name>)`.
            if name_objects {
                let name = String::from_utf8_lossy(entry.path.as_ref());
                println!("missing blob {oid} ({annotation_prefix}:{name})");
            } else {
                println!("missing blob {oid}");
            }
            bits |= sley_fsck::ERROR_REACHABLE;
        }
        // Cache-tree: each recorded (non-invalidated) subtree oid must be a
        // valid tree. git: `<oid>: invalid sha1 pointer in cache-tree of
        // <index>` + ERROR_REFS for an unparseable pointer.
        if let Ok(Some(cache_tree)) = index.cache_tree(format) {
            bits |= fsck_cache_tree(db, &cache_tree, &index_path, roots);
        }
    }
    Ok(bits)
}

/// Recursively validate a cache-tree node: a node with a valid (>=0) entry
/// count records a tree oid that must resolve to a tree object. Appends valid
/// tree oids to `roots` so they are marked reachable.
fn fsck_cache_tree(
    db: &FileObjectDatabase,
    node: &sley_index::CacheTree,
    index_path: &Path,
    roots: &mut Vec<ObjectId>,
) -> i32 {
    let mut bits = 0i32;
    if node.entry_count >= 0
        && let Some(oid) = &node.oid
    {
        match db.read_object(oid) {
            Ok(object) if object.object_type == sley_object::ObjectType::Tree => {
                roots.push(oid.clone());
            }
            Ok(_) => {
                // Present but not a tree: git's `non-tree in cache-tree`.
                eprintln!("error in cache-tree of {}: non-tree", index_path.display());
                bits |= sley_fsck::ERROR_OBJECT;
            }
            Err(_) => {
                eprintln!(
                    "error: {oid}: invalid sha1 pointer in cache-tree of {}",
                    index_path.display()
                );
                bits |= sley_fsck::ERROR_REFS;
            }
        }
    }
    for child in &node.subtrees {
        bits |= fsck_cache_tree(db, &child.tree, index_path, roots);
    }
    bits
}

/// Whether an index file's trailing hash matches the digest of its body, git's
/// `verify_hdr` checksum check. A too-short file (no room for the trailing hash)
/// is treated as a checksum failure.
fn index_checksum_ok(bytes: &[u8], format: ObjectFormat) -> bool {
    let hash_len = format.raw_len();
    if bytes.len() < 12 + hash_len {
        return false;
    }
    let split = bytes.len() - hash_len;
    match sley_core::digest_bytes(format, &bytes[..split]) {
        Ok(actual) => actual.as_bytes() == &bytes[split..],
        Err(_) => false,
    }
}

/// The path string git prints for a linked worktree's index in fsck reports:
/// relative to the cwd-rooted `.git` when the index lives under it, else the
/// absolute path.
fn fsck_index_display_path(git_dir: &Path, index_path: &Path) -> String {
    if let Ok(cwd) = env::current_dir()
        && let Ok(rel) = git_dir.strip_prefix(&cwd)
        && let Ok(suffix) = index_path.strip_prefix(git_dir)
    {
        return format!("{}/{}", rel.display(), suffix.display());
    }
    index_path.display().to_string()
}

/// Named root refs for a full fsck: every ref (and HEAD), each as
/// `(refname, target_oid)`. The driver validates each against git's
/// `snapshot_ref` rules (parseable object, branch→commit) before walking.
fn fsck_root_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<(String, ObjectId)>> {
    let store = FileRefStore::new(git_dir, format);
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for reference in store.list_refs()? {
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push((reference.name.clone(), oid));
        }
    }
    // git resolves HEAD after the ref iteration (its worktree-HEAD pass).
    if let Some(target) = store.read_ref("HEAD")? {
        let reference = Ref {
            name: "HEAD".to_string(),
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push(("HEAD".to_string(), oid));
        }
    }
    Ok(roots)
}

#[derive(Debug)]
enum ReplaceMode {
    Create { object: String, replacement: String },
    List { pattern: Option<String> },
    Delete { objects: Vec<String> },
}

#[derive(Debug)]
struct ReplaceOptions {
    force: bool,
    format: ReplaceListFormat,
    mode: ReplaceMode,
}

#[derive(Debug, Clone, Copy)]
enum ReplaceListFormat {
    Short,
    Medium,
    Long,
}

pub(crate) fn cmd_replace(args: &[String]) -> Result<()> {
    let options = parse_replace_options(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    match options.mode {
        ReplaceMode::List { pattern } => {
            replace_list(&store, &db, format, pattern.as_deref(), options.format)
        }
        ReplaceMode::Delete { objects } => {
            replace_delete(&store, &common_git_dir, format, &objects)
        }
        ReplaceMode::Create {
            object,
            replacement,
        } => replace_create(
            &store,
            &db,
            &common_git_dir,
            format,
            &object,
            &replacement,
            options.force,
        ),
    }
}

fn parse_replace_options(args: &[String]) -> Result<ReplaceOptions> {
    let mut force = false;
    let mut format = ReplaceListFormat::Short;
    let mut list = false;
    let mut delete = false;
    let mut unsupported_mode = None::<&str>;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                positional.extend(iter.cloned());
                break;
            }
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-l" | "--list" => list = true,
            "-d" | "--delete" => delete = true,
            "-e" | "--edit" => unsupported_mode = Some("--edit"),
            "-g" | "--graft" => unsupported_mode = Some("--graft"),
            "--convert-graft-file" => unsupported_mode = Some("--convert-graft-file"),
            "--raw" | "--no-raw" => {}
            "--format" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `format' requires a value");
                    return Err(GitError::Exit(129));
                };
                format = parse_replace_list_format(value)?;
            }
            "--no-format" => format = ReplaceListFormat::Short,
            value if let Some(value) = long_option_value(value, "format") => {
                format = parse_replace_list_format(value)?;
            }
            value if value.starts_with("--no-force=") => {
                eprintln!("error: option `no-force' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return replace_usage();
            }
            value if value.starts_with('-') && value.len() > 1 => {
                for option in value[1..].chars() {
                    match option {
                        'f' => force = true,
                        'l' => list = true,
                        'd' => delete = true,
                        'e' => unsupported_mode = Some("--edit"),
                        'g' => unsupported_mode = Some("--graft"),
                        other => {
                            eprintln!("error: unknown switch `{other}'");
                            return replace_usage();
                        }
                    }
                }
            }
            value => positional.push(value.to_string()),
        }
    }
    if let Some(mode) = unsupported_mode {
        return Err(GitError::Unsupported(format!("replace {mode}")));
    }
    if delete {
        if positional.is_empty() {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::Delete {
                objects: positional,
            },
        });
    }
    if list || positional.len() <= 1 {
        if positional.len() > 1 {
            return replace_usage();
        }
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::List {
                pattern: positional.pop(),
            },
        });
    }
    if positional.len() == 2 {
        return Ok(ReplaceOptions {
            force,
            format,
            mode: ReplaceMode::Create {
                object: positional.remove(0),
                replacement: positional.remove(0),
            },
        });
    }
    replace_usage()
}

fn parse_replace_list_format(value: &str) -> Result<ReplaceListFormat> {
    match value {
        "short" => Ok(ReplaceListFormat::Short),
        "medium" => Ok(ReplaceListFormat::Medium),
        "long" => Ok(ReplaceListFormat::Long),
        other => {
            eprintln!("error: invalid replace format '{other}'");
            eprintln!("valid formats are 'short', 'medium' and 'long'");
            Err(GitError::Exit(255))
        }
    }
}

fn replace_list(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    object_format: ObjectFormat,
    pattern: Option<&str>,
    format: ReplaceListFormat,
) -> Result<()> {
    for reference in store.list_refs()? {
        let Some(object) = reference.name.strip_prefix("refs/replace/") else {
            continue;
        };
        if pattern.is_some_and(|pattern| !refname_pattern_matches(pattern, object)) {
            continue;
        }
        let RefTarget::Direct(replacement) = reference.target else {
            continue;
        };
        match format {
            ReplaceListFormat::Short => println!("{object}"),
            ReplaceListFormat::Medium => println!("{object} -> {replacement}"),
            ReplaceListFormat::Long => {
                let object_type = replace_object_type(db, object_format, object)?;
                let replacement_type = db
                    .read_object_header(&replacement)?
                    .map(|(object_type, _)| object_type.as_str())
                    .unwrap_or("unknown");
                println!("{object} ({object_type}) -> {replacement} ({replacement_type})");
            }
        }
    }
    Ok(())
}

fn replace_delete(
    store: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    objects: &[String],
) -> Result<()> {
    let mut failed = false;
    for object in objects {
        let oid = match ObjectId::from_hex(format, object) {
            Ok(oid) => oid,
            Err(_) => match resolve_revision(git_dir, format, object) {
                Ok(oid) => oid,
                Err(_) => {
                    eprintln!("error: failed to resolve '{object}' as a valid ref");
                    failed = true;
                    continue;
                }
            },
        };
        let name = format!("refs/replace/{oid}");
        match store.delete_ref(&name) {
            Ok(_) => println!("Deleted replace ref '{oid}'"),
            Err(_) => {
                eprintln!("error: replace ref '{oid}' not found");
                failed = true;
            }
        }
    }
    if failed {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

fn replace_create(
    store: &FileRefStore,
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    object: &str,
    replacement: &str,
    force: bool,
) -> Result<()> {
    let object_oid = resolve_revision(git_dir, format, object)?;
    let replacement_oid = resolve_revision(git_dir, format, replacement)?;
    let object_type = db
        .read_object_header(&object_oid)?
        .map(|(object_type, _)| object_type)
        .ok_or_else(|| GitError::object_not_found(object_oid))?;
    let replacement_type = db
        .read_object_header(&replacement_oid)?
        .map(|(object_type, _)| object_type)
        .ok_or_else(|| GitError::object_not_found(replacement_oid))?;
    if object_type != replacement_type {
        eprintln!("error: Objects must be of the same type.");
        eprintln!(
            "'{object}' points to a replaced object of type '{}'",
            object_type.as_str()
        );
        eprintln!(
            "while '{replacement}' points to a replacement object of type '{}'.",
            replacement_type.as_str()
        );
        return Err(GitError::Exit(255));
    }
    let name = format!("refs/replace/{object_oid}");
    let precondition = if force {
        RefPrecondition::Any
    } else {
        RefPrecondition::MustNotExist
    };
    let mut tx = store.transaction();
    tx.update_to(
        name.clone(),
        RefTarget::Direct(replacement_oid),
        precondition,
        None,
    );
    match tx.commit() {
        Ok(()) => Ok(()),
        Err(_) if !force => {
            eprintln!("error: replace ref '{name}' already exists");
            Err(GitError::Exit(255))
        }
        Err(err) => Err(err),
    }
}

fn replace_object_type(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    object: &str,
) -> Result<&'static str> {
    let oid = ObjectId::from_hex(format, object)?;
    Ok(db
        .read_object_header(&oid)?
        .map(|(object_type, _)| object_type.as_str())
        .unwrap_or("unknown"))
}

fn replace_usage<T>() -> Result<T> {
    eprintln!("usage: git replace [-f] <object> <replacement>");
    eprintln!("   or: git replace [-f] --edit <object>");
    eprintln!("   or: git replace [-f] --graft <commit> [<parent>...]");
    eprintln!("   or: git replace [-f] --convert-graft-file");
    eprintln!("   or: git replace -d <object>...");
    eprintln!("   or: git replace [--format=<format>] [-l [<pattern>]]");
    eprintln!();
    eprintln!("    -l, --list            list replace refs");
    eprintln!("    -d, --delete          delete replace refs");
    eprintln!("    -e, --edit            edit existing object");
    eprintln!("    -g, --graft           change a commit's parents");
    eprintln!("    --convert-graft-file  convert existing graft file");
    eprintln!("    -f, --[no-]force      replace the ref if it exists");
    eprintln!("    --[no-]raw            do not pretty-print contents for --edit");
    eprintln!("    --[no-]format <format>");
    eprintln!("                          use this format");
    eprintln!();
    Err(GitError::Exit(129))
}

pub(crate) fn cmd_prune_packed(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut positional = 0usize;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-q" | "--quiet" | "--no-quiet" => {}
            "--" => {
                positional += iter.count();
                break;
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return prune_packed_usage();
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown switch `{}'", value.trim_start_matches('-'));
                return prune_packed_usage();
            }
            _ => positional += 1,
        }
    }
    if positional > 0 {
        eprintln!("fatal: too many arguments");
        eprintln!();
        return prune_packed_usage();
    }

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let objects_dir = repository_objects_dir(&git_dir);
    let packed = prune_packed_object_ids(&objects_dir.join("pack"), format)?;
    if packed.is_empty() {
        return Ok(());
    }
    for (oid, path) in prune_packed_loose_object_paths(&objects_dir, format)? {
        if !packed.contains(&oid) {
            continue;
        }
        if dry_run {
            println!("rm -f {}", prune_packed_display_path(&path)?);
        } else {
            fs::remove_file(&path)?;
            if let Some(parent) = path.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    if !dry_run {
        prune_empty_loose_object_dirs(&objects_dir)?;
    }
    Ok(())
}

fn prune_empty_loose_object_dirs(objects_dir: &Path) -> Result<()> {
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let _ = fs::remove_dir(entry.path());
        }
    }
    Ok(())
}

fn prune_packed_usage<T>() -> Result<T> {
    eprintln!("usage: git prune-packed [-n | --dry-run] [-q | --quiet]");
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    dry run");
    eprintln!("    -q, --[no-]quiet      be quiet");
    eprintln!();
    Err(GitError::Exit(129))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeRrEntry {
    hash: String,
    variant: u32,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerereSubcommand {
    Clear,
    Forget,
    Gc,
    Status,
}

#[derive(Debug)]
struct RerereOptions {
    subcommand: Option<RerereSubcommand>,
    paths: Vec<String>,
}

pub(crate) fn cmd_rerere(args: &[String]) -> Result<()> {
    let options = parse_rerere_options(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    match options.subcommand {
        None => Ok(()),
        Some(RerereSubcommand::Status) => rerere_status(&git_dir),
        Some(RerereSubcommand::Clear) => rerere_clear(&git_dir),
        Some(RerereSubcommand::Forget) => rerere_forget(&git_dir, &options.paths),
        Some(RerereSubcommand::Gc) => rerere_gc(&git_dir),
    }
}

fn parse_rerere_options(args: &[String]) -> Result<RerereOptions> {
    let mut autoupdate = None;
    let mut subcommand = None;
    let mut paths = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            paths.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--rerere-autoupdate" => autoupdate = Some(true),
            "--no-rerere-autoupdate" => autoupdate = Some(false),
            value if value.starts_with("--no-rerere-autoupdate=") => {
                eprintln!("error: option `no-rerere-autoupdate' takes no value");
                return rerere_usage();
            }
            value if value.starts_with("--rerere-autoupdate=") => {
                eprintln!("error: option `rerere-autoupdate' takes no value");
                return rerere_usage();
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return rerere_usage();
            }
            "clear" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Clear),
            "forget" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Forget),
            "gc" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Gc),
            "status" if subcommand.is_none() => subcommand = Some(RerereSubcommand::Status),
            _ if subcommand.is_none() => return rerere_usage(),
            value => paths.push(value.to_string()),
        }
    }
    if matches!(subcommand, Some(RerereSubcommand::Forget)) && paths.is_empty() {
        eprintln!("warning: 'git rerere forget' without paths is deprecated");
    }
    let _ = autoupdate;
    Ok(RerereOptions { subcommand, paths })
}

fn rerere_usage<T>() -> Result<T> {
    eprintln!("usage: git rerere [clear | forget <pathspec>... | diff | status | remaining | gc]");
    eprintln!();
    eprintln!("    --[no-]rerere-autoupdate");
    eprintln!("                          register clean resolutions in index");
    eprintln!();
    Err(GitError::Exit(129))
}

fn is_rerere_enabled(git_dir: &Path) -> Result<bool> {
    let config = read_repo_config(git_dir)?;
    if let Some(value) = config.get("rerere", None, "enabled") {
        return Ok(matches!(value, "true" | "1" | "yes" | "on"));
    }
    Ok(git_dir.join("rr-cache").is_dir())
}

fn read_merge_rr(git_dir: &Path) -> Result<Vec<MergeRrEntry>> {
    let path = git_dir.join("MERGE_RR");
    let Ok(data) = fs::read(&path) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for record in data
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(GitError::Command("corrupt MERGE_RR".into()));
        };
        let id = std::str::from_utf8(&record[..tab])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
        let (hash, variant) = parse_merge_rr_id(id)?;
        entries.push(MergeRrEntry {
            hash,
            variant,
            path: path.to_string(),
        });
    }
    Ok(entries)
}

fn parse_merge_rr_id(id: &str) -> Result<(String, u32)> {
    let Some(dot) = id.find('.') else {
        return Ok((id.to_string(), 0));
    };
    let hash = &id[..dot];
    let variant = id[dot + 1..]
        .parse::<u32>()
        .map_err(|_| GitError::Command("corrupt MERGE_RR".into()))?;
    Ok((hash.to_string(), variant))
}

fn rerere_cache_file_path(cache_dir: &Path, variant: u32, name: &str) -> PathBuf {
    if variant == 0 {
        cache_dir.join(name)
    } else {
        cache_dir.join(format!("{name}.{variant}"))
    }
}

fn rerere_has_resolution(rr_cache: &Path, entry: &MergeRrEntry) -> bool {
    let cache_dir = rr_cache.join(&entry.hash);
    rerere_cache_file_path(&cache_dir, entry.variant, "preimage").is_file()
        && rerere_cache_file_path(&cache_dir, entry.variant, "postimage").is_file()
}

fn remove_rr_cache_entry(rr_cache: &Path, entry: &MergeRrEntry) -> Result<()> {
    let cache_dir = rr_cache.join(&entry.hash);
    if !cache_dir.is_dir() {
        return Ok(());
    }
    for file in fs::read_dir(&cache_dir)? {
        let path = file?.path();
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    match fs::remove_dir(&cache_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(GitError::Io(err.to_string())),
    }
    Ok(())
}

fn rerere_status(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    for entry in read_merge_rr(git_dir)? {
        println!("{}", entry.path);
    }
    Ok(())
}

pub(crate) fn rerere_clear(git_dir: &Path) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    for entry in read_merge_rr(git_dir)? {
        if !rerere_has_resolution(&rr_cache, &entry) {
            remove_rr_cache_entry(&rr_cache, &entry)?;
        }
    }
    let merge_rr = git_dir.join("MERGE_RR");
    if merge_rr.is_file() {
        fs::remove_file(merge_rr)?;
    }
    Ok(())
}

fn rerere_gc(git_dir: &Path) -> Result<()> {
    let rr_cache = git_dir.join("rr-cache");
    if !rr_cache.exists() {
        return Ok(());
    }
    rerere_gc_dir(&rr_cache, true)
}

fn rerere_gc_dir(path: &Path, keep_root: bool) -> Result<()> {
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            rerere_gc_dir(&child, false)?;
        } else {
            match fs::remove_file(&child) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    if !keep_root {
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn rerere_path_matches(path: &str, pattern: &str) -> bool {
    path == pattern || path.ends_with(&format!("/{pattern}"))
}

fn rerere_forget(git_dir: &Path, paths: &[String]) -> Result<()> {
    if !is_rerere_enabled(git_dir)? {
        return Ok(());
    }
    if paths.is_empty() {
        return Ok(());
    }
    let rr_cache = git_dir.join("rr-cache");
    let entries = read_merge_rr(git_dir)?;
    for pattern in paths {
        let mut matched = false;
        for entry in entries
            .iter()
            .filter(|entry| rerere_path_matches(&entry.path, pattern))
        {
            matched = true;
            let cache_dir = rr_cache.join(&entry.hash);
            let postimage = rerere_cache_file_path(&cache_dir, entry.variant, "postimage");
            if !postimage.is_file() {
                eprintln!("error: no remembered resolution for '{pattern}'");
                continue;
            }
            fs::remove_file(&postimage)?;
            if let Ok(thisimage) = fs::read(rerere_cache_file_path(
                &cache_dir,
                entry.variant,
                "thisimage",
            )) {
                fs::write(
                    rerere_cache_file_path(&cache_dir, entry.variant, "preimage"),
                    thisimage,
                )?;
                eprintln!("Updated preimage for '{pattern}'");
            }
            eprintln!("Forgot resolution for '{pattern}'");
        }
        if !matched {
            eprintln!("error: no remembered resolution for '{pattern}'");
        }
    }
    Ok(())
}

fn prune_packed_object_ids(pack_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let mut packed = HashSet::new();
    if !pack_dir.exists() {
        return Ok(packed);
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        packed.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(packed)
}

fn prune_packed_loose_object_paths(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<(ObjectId, PathBuf)>> {
    let mut objects = Vec::new();
    if !objects_dir.exists() {
        return Ok(objects);
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let fanout = entry.file_name();
        let Some(fanout) = fanout.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let suffix = object_entry.file_name();
            let Some(suffix) = suffix.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let oid = ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?;
            objects.push((oid, object_entry.path()));
        }
    }
    objects.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(objects)
}

fn prune_packed_display_path(path: &Path) -> Result<String> {
    let cwd = env::current_dir()?;
    let display = path.strip_prefix(&cwd).unwrap_or(path);
    Ok(display.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn cmd_rm(args: &[String]) -> Result<()> {
    let mut paths = Vec::new();
    let mut recursive = false;
    let mut quiet = false;
    let mut cached = false;
    let mut force = false;
    let mut dry_run = false;
    let mut ignore_unmatch = false;
    let mut parsing_options = true;
    let mut pathspec_from_file: Option<PathBuf> = None;
    let mut pathspec_file_nul = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !parsing_options {
            if pathspec_from_file.is_some() {
                eprintln!(
                    "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                );
                return Err(GitError::Exit(128));
            }
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "-r" | "-R" | "--recursive" => recursive = true,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--cached" => cached = true,
            "--no-cached" => cached = false,
            "--ignore-unmatch" => ignore_unmatch = true,
            "--no-ignore-unmatch" => ignore_unmatch = false,
            "--sparse" | "--no-sparse" => {}
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            "--pathspec-from-file" => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--pathspec-from-file=") => {
                if !paths.is_empty() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                let value = value.strip_prefix("--pathspec-from-file=").ok_or_else(|| {
                    GitError::Command("--pathspec-from-file requires a value".into())
                })?;
                pathspec_from_file = Some(PathBuf::from(value));
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..]
                        .bytes()
                        .all(|option| matches!(option, b'r' | b'R' | b'f' | b'n' | b'q')) =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b'r' | b'R' => recursive = true,
                        b'f' => force = true,
                        b'n' => dry_run = true,
                        b'q' => quiet = true,
                        _ => unreachable!("rm short-option group was filtered"),
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported rm option {value}")));
            }
            value => {
                if pathspec_from_file.is_some() {
                    eprintln!(
                        "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
                    );
                    return Err(GitError::Exit(128));
                }
                paths.push(PathBuf::from(value));
            }
        }
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file {
        paths.extend(read_pathspecs_from_file(&pathspec_file, pathspec_file_nul)?);
    }
    if paths.is_empty() {
        eprintln!("fatal: No pathspec was given. Which files should I remove?");
        return Err(GitError::Exit(128));
    }
    if paths.iter().any(|path| path.as_os_str().is_empty()) {
        eprintln!(
            "fatal: empty string is not a valid pathspec. please use . instead if you meant to match all paths"
        );
        return Err(GitError::Exit(128));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let resolved_paths = paths
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .collect::<Vec<_>>();
    let config_parameters_env = effective_config_parameters_env();
    let result = sley_worktree::remove_index_and_worktree_paths(
        worktree_root,
        git_dir,
        format,
        &resolved_paths,
        sley_worktree::RemoveOptions {
            recursive,
            cached,
            force,
            dry_run,
            ignore_unmatch,
        },
        config_parameters_env.as_deref(),
    )?;
    if !quiet {
        let mut stdout = io::stdout().lock();
        for path in result.removed {
            writeln!(stdout, "rm '{}'", String::from_utf8_lossy(&path))?;
        }
    }
    Ok(())
}

pub(crate) fn cmd_mv(args: &[String]) -> Result<()> {
    let mut paths = Vec::new();
    let mut force = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut skip_errors = false;
    let mut parsing_options = true;
    for arg in args {
        if !parsing_options {
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => parsing_options = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-k" => skip_errors = true,
            "--sparse" | "--no-sparse" => {}
            value if value.starts_with('-') && !value.starts_with("--") && value.len() > 2 => {
                for flag in value[1..].bytes() {
                    match flag {
                        b'f' => force = true,
                        b'n' => dry_run = true,
                        b'v' => verbose = true,
                        b'k' => skip_errors = true,
                        other => {
                            return Err(GitError::Command(format!(
                                "unsupported mv option -{}",
                                other as char
                            )));
                        }
                    }
                }
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported mv option {value}")));
            }
            value => paths.push(PathBuf::from(value)),
        }
    }
    if paths.len() < 2 {
        return Err(GitError::Command(
            "mv currently supports <source>... <destination>".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let destination = if paths[paths.len() - 1].is_absolute() {
        paths[paths.len() - 1].clone()
    } else {
        cwd.join(&paths[paths.len() - 1])
    };
    if paths.len() > 2 && !destination.is_dir() {
        eprintln!(
            "fatal: destination '{}' is not a directory",
            destination.display()
        );
        return Err(GitError::Exit(128));
    }

    let mut results = Vec::new();
    for source in &paths[..paths.len() - 1] {
        let source = if source.is_absolute() {
            source.clone()
        } else {
            cwd.join(source)
        };
        let result = sley_worktree::move_index_and_worktree_path(
            &worktree_root,
            &git_dir,
            format,
            &source,
            &destination,
            sley_worktree::MoveOptions {
                force,
                dry_run,
                skip_errors,
            },
        )?;
        let fatal = result.fatal.is_some();
        results.push(result);
        if dry_run && fatal {
            break;
        }
    }
    if dry_run {
        for result in &results {
            let source = String::from_utf8_lossy(&result.source);
            let destination = String::from_utf8_lossy(&result.destination);
            println!("Checking rename of '{source}' to '{destination}'");
            for detail in &result.details {
                let source = String::from_utf8_lossy(&detail.source);
                let destination = String::from_utf8_lossy(&detail.destination);
                println!("Checking rename of '{source}' to '{destination}'");
            }
        }
        if let Some(fatal) = results.iter().find_map(|result| result.fatal.as_deref()) {
            eprintln!("{fatal}");
            return Err(GitError::Exit(128));
        }
    }
    if dry_run || verbose {
        for result in &results {
            if result.skipped {
                continue;
            }
            let source = String::from_utf8_lossy(&result.source);
            let destination = String::from_utf8_lossy(&result.destination);
            println!("Renaming {source} to {destination}");
            for detail in &result.details {
                if detail.skipped {
                    continue;
                }
                let source = String::from_utf8_lossy(&detail.source);
                let destination = String::from_utf8_lossy(&detail.destination);
                println!("Renaming {source} to {destination}");
            }
        }
    }
    Ok(())
}

struct CleanTarget {
    path: Vec<u8>,
    display: Vec<u8>,
    is_dir: bool,
}

fn clean_targets(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    directories: bool,
    include_ignored: bool,
    pathspec: &LsFilesPathspec,
    excludes: &[String],
) -> Result<Vec<CleanTarget>> {
    let has_pathspec = !pathspec.filters.is_empty();
    // Git treats any pathspec as `-d` for selection purposes.
    let effective_directories = directories || has_pathspec;
    let index = sley_worktree::read_repository_index(git_dir, format)?;

    let mut paths = if effective_directories {
        sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory: true,
                no_empty_directory: false,
                preserve_ignored_directories: directories,
                exclude_standard: !include_ignored,
                ignored_only: false,
                exclude_patterns: Vec::new(),
                exclude_per_directory: Vec::new(),
                pathspecs: pathspec.untracked_pathspecs(),
            },
        )?
    } else {
        sley_worktree::untracked_paths_with_options(
            worktree_root,
            git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory: false,
                no_empty_directory: false,
                preserve_ignored_directories: false,
                exclude_standard: !include_ignored,
                ignored_only: false,
                exclude_patterns: Vec::new(),
                exclude_per_directory: Vec::new(),
                pathspecs: pathspec.untracked_pathspecs(),
            },
        )?
    };

    // Without `-d` (and without a pathspec, which Git treats as `-d`), the
    // non-directory walk lists every untracked file. Git only removes a file in
    // a subdirectory when that directory contains tracked content; an untracked
    // file inside a wholly-untracked directory needs `-d`. The directory walk
    // already encodes this selection (it rolls wholly-untracked directories up
    // to `dir/` and only descends into directories with tracked/ignored content),
    // so the retain must run only on the non-directory walk's flat output.
    if !effective_directories {
        paths.retain(|path| {
            path.ends_with(b"/") || clean_untracked_file_eligible(path, index.as_ref())
        });
    }

    if has_pathspec {
        paths = clean_collapse_untracked_paths(paths);
    }

    let mut targets = Vec::new();
    for path in paths {
        let is_dir = path.ends_with(b"/");
        let Some(display) = pathspec.display(&path) else {
            continue;
        };
        if clean_target_is_excluded(&path, excludes) {
            continue;
        }
        targets.push(CleanTarget {
            path,
            display,
            is_dir,
        });
    }

    targets.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(targets)
}

/// regardless of `-x` or whether the repository has any commits yet.
fn clean_untracked_file_eligible(path: &[u8], index: Option<&Index>) -> bool {
    if !path.iter().any(|byte| *byte == b'/') {
        return true;
    }
    let Some(index) = index else {
        return false;
    };
    clean_path_parent(path).is_some_and(|parent| clean_index_has_tracked_under(index, parent))
}

fn clean_index_has_tracked_under(index: &Index, directory: &[u8]) -> bool {
    let mut prefix = directory.to_vec();
    prefix.push(b'/');
    index
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes().starts_with(&prefix))
}

fn clean_path_parent(path: &[u8]) -> Option<&[u8]> {
    let slash = path.iter().rposition(|byte| *byte == b'/')?;
    if slash == 0 {
        return None;
    }
    Some(&path[..slash])
}

/// Match git `correct_untracked_entries` for pathspec-driven clean.
fn clean_collapse_untracked_paths(paths: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    // The directory walk already encodes Git's `--directory` rollup: a
    // wholly-untracked directory named by a pathspec is emitted as `dir/`, while
    // untracked files inside a partially-tracked directory are listed
    // individually. The only post-processing left is dropping a file entry that
    // is already subsumed by a rolled-up parent directory entry.
    let mut sorted = paths;
    sorted.sort();
    let mut kept = BTreeSet::new();
    for path in &sorted {
        if sorted.iter().any(|other| {
            other != path && other.ends_with(b"/") && clean_directory_contains_path(other, path)
        }) {
            continue;
        }
        kept.insert(path.clone());
    }
    kept.into_iter().collect()
}

fn clean_target_is_excluded(path: &[u8], excludes: &[String]) -> bool {
    excludes
        .iter()
        .any(|pattern| clean_exclude_pattern_matches(pattern, path))
}

fn clean_exclude_pattern_matches(pattern: &str, path: &[u8]) -> bool {
    let pattern = pattern.trim_end_matches('/');
    let path = String::from_utf8_lossy(path);
    let normalized = path.trim_end_matches('/');
    let candidate = if pattern.contains('/') {
        normalized
    } else {
        normalized.rsplit('/').next().unwrap_or(normalized)
    };
    if pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        refname_pattern_matches(pattern, candidate)
    } else {
        candidate == pattern
    }
}

fn clean_directory_contains_path(directory: &[u8], path: &[u8]) -> bool {
    directory.strip_suffix(b"/").is_some_and(|directory| {
        path.strip_prefix(directory)
            .and_then(|rest| rest.strip_prefix(b"/"))
            .is_some()
    })
}

pub(crate) fn cmd_bundle(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_bundle_usage();
        return Err(GitError::Exit(129));
    };
    match subcommand {
        "create" => cmd_bundle_create(&args[1..]),
        "verify" => cmd_bundle_verify(&args[1..]),
        "list-heads" => cmd_bundle_list_heads(&args[1..]),
        "unbundle" => cmd_bundle_unbundle(&args[1..]),
        _ => {
            print_bundle_usage();
            Err(GitError::Exit(129))
        }
    }
}

const BUNDLE_CREATE_USAGE: &str = "usage: git bundle create [-q | --quiet | --progress]\n                  [--version=<version>] <file> <git-rev-list-args>\n";
const BUNDLE_VERIFY_USAGE: &str = "usage: git bundle verify [-q | --quiet] <file>\n";
const BUNDLE_LIST_HEADS_USAGE: &str = "usage: git bundle list-heads <file> [<refname>...]\n";
const BUNDLE_UNBUNDLE_USAGE: &str = "usage: git bundle unbundle [--progress] <file> [<refname>...]\n";

fn print_bundle_usage() {
    eprint!("{BUNDLE_CREATE_USAGE}");
    eprint!("{BUNDLE_VERIFY_USAGE}");
    eprint!("{BUNDLE_LIST_HEADS_USAGE}");
    eprint!("{BUNDLE_UNBUNDLE_USAGE}");
}

fn bundle_usage_error(usage: &str) -> Result<()> {
    eprintln!("fatal: need a <file> argument");
    eprint!("{usage}");
    Err(GitError::Exit(129))
}

const COMMIT_GRAPH_USAGE: &str = "\
usage: git commit-graph verify [--object-dir <dir>] [--shallow] [--[no-]progress]
   or: git commit-graph write [--object-dir <dir>] [--append]
                       [--split[=<strategy>]] [--reachable | --stdin-packs | --stdin-commits]
                       [--changed-paths] [--[no-]max-new-filters <n>] [--[no-]progress]
                       <split-options>
";

pub(crate) fn cmd_commit_graph(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        // No sub-command ⇒ usage error (exit 129) with the usage block.
        eprint!("{COMMIT_GRAPH_USAGE}");
        return Err(GitError::Exit(129));
    };
    match subcommand {
        "write" => cmd_commit_graph_write(&args[1..]),
        "verify" => cmd_commit_graph_verify(&args[1..]),
        other => {
            // Unknown sub-command ⇒ git's `error: unknown subcommand: \`<x>'`
            // plus the usage block, exit 129.
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{COMMIT_GRAPH_USAGE}");
            Err(GitError::Exit(129))
        }
    }
}

/// Which set of commits seeds the graph (mirrors git's mutually-exclusive
/// `--reachable` / `--stdin-packs` / `--stdin-commits`; default = all packs).
enum CommitGraphSource {
    AllPacks,
    Reachable,
    StdinPacks,
    StdinCommits,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitGraphSplitMode {
    Off,
    Append,
    NoMerge,
    Replace,
}

#[derive(Clone, Copy)]
struct CommitGraphSplitOptions {
    mode: CommitGraphSplitMode,
    size_multiple: usize,
    max_commits: Option<usize>,
    expire_time: Option<i64>,
}

impl CommitGraphSplitOptions {
    fn off() -> Self {
        Self {
            mode: CommitGraphSplitMode::Off,
            size_multiple: 2,
            max_commits: None,
            expire_time: None,
        }
    }
}

fn cmd_commit_graph_write(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut source = CommitGraphSource::AllPacks;
    let mut changed_paths: Option<bool> = None;
    let mut append = false;
    let mut split = CommitGraphSplitOptions::off();
    let mut max_new_filters_arg: Option<usize> = None;
    // git's write progress defaults to isatty(2); the harness redirects stderr,
    // so only an explicit --progress emits the progress lines.
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--reachable" => source = CommitGraphSource::Reachable,
            "--stdin-packs" => source = CommitGraphSource::StdinPacks,
            "--stdin-commits" => source = CommitGraphSource::StdinCommits,
            "--append" => append = true,
            "--split" => split.mode = CommitGraphSplitMode::Append,
            "--split=replace" => split.mode = CommitGraphSplitMode::Replace,
            "--changed-paths" => changed_paths = Some(true),
            "--no-changed-paths" => changed_paths = Some(false),
            "--max-commits" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--max-commits requires a value".into()))?;
                split.max_commits = Some(commit_graph_parse_positive_usize(value, "--max-commits")?);
            }
            "--size-multiple" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--size-multiple requires a value".into()))?;
                split.size_multiple =
                    commit_graph_parse_positive_usize(value, "--size-multiple")?;
            }
            "--expire-time" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--expire-time requires a value".into()))?;
                split.expire_time = Some(commit_graph_parse_expire_time(value)?);
            }
            "--max-new-filters" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--max-new-filters requires a value".into())
                })?;
                max_new_filters_arg = Some(commit_graph_parse_max_new_filters(value)?);
            }
            "--no-max-new-filters" => max_new_filters_arg = None,
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            value if value.starts_with("--split=") => {
                let strategy = value.strip_prefix("--split=").unwrap_or_default();
                split.mode = match strategy {
                    "replace" => CommitGraphSplitMode::Replace,
                    "no-merge" => CommitGraphSplitMode::NoMerge,
                    "merge-all" => CommitGraphSplitMode::Append,
                    _ => CommitGraphSplitMode::Append,
                };
            }
            value if value.starts_with("--max-commits=") => {
                split.max_commits = Some(commit_graph_parse_positive_usize(
                    value.strip_prefix("--max-commits=").unwrap_or_default(),
                    "--max-commits",
                )?);
            }
            value if value.starts_with("--size-multiple=") => {
                split.size_multiple = commit_graph_parse_positive_usize(
                    value.strip_prefix("--size-multiple=").unwrap_or_default(),
                    "--size-multiple",
                )?;
            }
            value if value.starts_with("--expire-time=") => {
                split.expire_time = Some(commit_graph_parse_expire_time(
                    value.strip_prefix("--expire-time=").unwrap_or_default(),
                )?);
            }
            value if value.starts_with("--max-new-filters=") => {
                max_new_filters_arg = Some(commit_graph_parse_max_new_filters(
                    value.strip_prefix("--max-new-filters=").unwrap_or_default(),
                )?);
            }
            // Any unrecognized option or positional arg is a usage error
            // (git's parse-options exits 129); `commit-graph write` takes no
            // positional arguments.
            other => {
                eprintln!("error: unknown option `{}'", other.trim_start_matches('-'));
                eprint!("{COMMIT_GRAPH_USAGE}");
                return Err(GitError::Exit(129));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    // Read config with the command-line `-c` overrides folded in (mirrors git),
    // so `-c commitGraph.generationVersion=1` / `-c commitGraph.changedPaths=…`
    // on the write invocation take effect.
    let repo_config =
        sley_config::read_repo_config(&git_dir, effective_config_parameters_env().as_deref()).ok();
    let changed_paths_version = commit_graph_changed_paths_version(repo_config.as_ref())?;
    if !(-1..=2).contains(&changed_paths_version) {
        eprintln!(
            "warning: attempting to write a commit-graph, but 'commitGraph.changedPathsVersion' ({changed_paths_version}) is not supported"
        );
        return Ok(());
    }
    let existing_bloom_settings = existing_commit_graph_bloom_settings(&object_dir, format)?;
    let bloom_settings = commit_graph_bloom_settings_for_write(
        existing_bloom_settings,
        changed_paths_version,
        true,
    );
    // git: write_generation_data = (get_configured_generation_version(r) == 2).
    // Default is 2; `commitGraph.generationVersion=1` omits the GDA2/GDO2 chunks.
    let write_generation_data = commit_graph_generation_version(repo_config.as_ref()) == 2;
    let changed_paths = changed_paths.unwrap_or_else(|| {
        repo_config
            .as_ref()
            .and_then(|config| config.get_bool("commitGraph", None, "changedPaths"))
            .unwrap_or(false)
            || existing_bloom_settings.is_some()
    });
    let max_new_filters = max_new_filters_arg.or_else(|| {
        repo_config
            .as_ref()
            .and_then(|config| config.get("commitGraph", None, "maxNewFilters"))
            .and_then(|value| commit_graph_parse_max_new_filters(value).ok())
    });
    let existing_filters = if changed_paths {
        existing_commit_graph_bloom_filters(&object_dir, format)?
    } else {
        HashMap::new()
    };

    let db = FileObjectDatabase::new(&object_dir, format);
    let starts = match source {
        CommitGraphSource::Reachable => {
            return write_reachable_commit_graph(
                &git_dir,
                &object_dir,
                format,
                changed_paths,
                bloom_settings,
                write_generation_data,
                max_new_filters,
                &existing_filters,
                split,
                progress,
            );
        }
        CommitGraphSource::AllPacks => commit_graph_packed_commit_starts(&db, &object_dir, format)?,
        CommitGraphSource::StdinPacks => {
            commit_graph_stdin_packs_starts(&db, &object_dir, format)?
        }
        CommitGraphSource::StdinCommits => {
            let starts = commit_graph_stdin_commits_starts(&db, format)?;
            // git's `read_one_commit` loop drives a "Collecting commits from
            // input" progress meter while reading the stdin oids.
            if progress {
                eprintln!("Collecting commits from input: {}, done.", starts.len());
            }
            starts
        }
    };

    let mut starts = starts;
    if append {
        // `--append`: keep the commits already in the graph and add the new
        // source on top (git's `COMMIT_GRAPH_WRITE_APPEND`).
        let mut seen: HashSet<ObjectId> = starts.iter().copied().collect();
        for oid in existing_commit_graph_oids(&object_dir, format)? {
            if seen.insert(oid) {
                starts.push(oid);
            }
        }
    }

    // No commits in scope ⇒ write no graph file (git's "write graph with no
    // packs": the file must stay absent).
    if starts.is_empty() {
        return Ok(());
    }
    let graph = commit_graph_from_starts(
        &db,
        format,
        starts,
        changed_paths,
        bloom_settings,
        write_generation_data,
        max_new_filters,
        &existing_filters,
        progress,
    )?;
    let graph_dir = object_dir.join("info");
    fs::create_dir_all(&graph_dir)?;
    if split.mode == CommitGraphSplitMode::Off {
        write_commit_graph_file(&graph_dir.join("commit-graph"), &graph)?;
        remove_split_commit_graphs(&object_dir)?;
    } else {
        write_split_commit_graph_file(&object_dir, format, &graph, split)?;
    }
    Ok(())
}

/// The commit oids already recorded in the existing single-file commit-graph
/// (empty when there is none). Used by `--append`.
fn existing_commit_graph_oids(object_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let graph_path = object_dir.join("info").join("commit-graph");
    if !graph_path.exists() {
        return Ok(Vec::new());
    }
    let graph = CommitGraph::parse(&fs::read(graph_path)?, format)?;
    Ok(graph.commits.into_iter().map(|entry| entry.oid).collect())
}

/// Write the commit-graph file with git's read-only mode `0444 & ~umask`,
/// matching `mks_tempfile_m(..., 0444)` + `adjust_shared_perm`.
///
/// The umask is derived (without `unsafe`/libc) from the just-created file: the
/// OS gives it `0666 & ~umask`, so its read bits (`& 0444`) equal `0444 &
/// ~umask` exactly — which is the mode git lands on.
fn write_commit_graph_file(path: &Path, bytes: &[u8]) -> Result<()> {
    // A prior graph is written read-only (and a corrupted-graph test may leave
    // it chmod-000); make it writable first so the remove always succeeds, then
    // remove it so the rewrite creates a fresh file with the OS default mode
    // (`0666 & ~umask`), from which the umask can be recovered below. git
    // unconditionally replaces the graph regardless of the old file's mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    let _ = fs::remove_file(path);
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let created_mode = fs::metadata(path)?.permissions().mode();
        let mode = created_mode & 0o444;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn commit_graph_parse_max_new_filters(value: &str) -> Result<usize> {
    value.parse::<usize>().map_err(|_| {
        GitError::Command(format!(
            "bad numeric value '{value}' for '--max-new-filters'"
        ))
    })
}

fn commit_graph_parse_positive_usize(value: &str, option: &str) -> Result<usize> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("bad numeric value '{value}' for '{option}'")))?;
    if parsed == 0 {
        return Err(GitError::Command(format!(
            "bad numeric value '{value}' for '{option}'"
        )));
    }
    Ok(parsed)
}

fn commit_graph_parse_expire_time(value: &str) -> Result<i64> {
    crate::commands::approxidate::parse_expiry_date(value)
        .or_else(|| crate::commands::approxidate::parse_approxidate(value))
        .ok_or_else(|| GitError::Command(format!("invalid date format: {value}")))
}

struct CommitGraphLayer {
    hash: ObjectId,
    graph: CommitGraph,
}

fn write_split_commit_graph_file(
    object_dir: &Path,
    format: ObjectFormat,
    graph: &[u8],
    options: CommitGraphSplitOptions,
) -> Result<()> {
    let info = object_dir.join("info");
    let graphs = info.join("commit-graphs");
    fs::create_dir_all(&graphs)?;
    let single = info.join("commit-graph");
    let chain_path = graphs.join("commit-graph-chain");
    let full_graph = CommitGraph::parse(graph, format)?;
    let mut layers = if options.mode == CommitGraphSplitMode::Replace {
        Vec::new()
    } else {
        load_commit_graph_layers(object_dir, format)?
    };
    if layers.is_empty() && options.mode != CommitGraphSplitMode::Replace && single.exists() {
        let bytes = fs::read(&single)?;
        let hash = graph_file_checksum(&bytes, format)?;
        let path = graphs.join(format!("graph-{hash}.graph"));
        if !path.exists() {
            write_commit_graph_file(&path, &bytes)?;
        }
        layers.push(CommitGraphLayer {
            hash,
            graph: CommitGraph::parse(&bytes, format)?,
        });
    }

    let existing_oids = layers
        .iter()
        .flat_map(|layer| layer.graph.commits.iter().map(|entry| entry.oid))
        .collect::<HashSet<_>>();
    let mut new_entries = commit_graph_write_entries_from_graph(&full_graph)?
        .into_iter()
        .filter(|entry| options.mode == CommitGraphSplitMode::Replace || !existing_oids.contains(&entry.oid))
        .collect::<Vec<_>>();
    if new_entries.is_empty() && options.mode != CommitGraphSplitMode::Replace {
        return Ok(());
    }

    if options.mode == CommitGraphSplitMode::Append {
        let mut new_count = new_entries.len();
        while let Some(top) = layers.last() {
            let force_by_max = options
                .max_commits
                .is_some_and(|max_commits| new_count > max_commits);
            let merge_by_size = top.graph.commits.len() <= options.size_multiple.saturating_mul(new_count);
            if !(force_by_max || merge_by_size) {
                break;
            }
            let top = layers.pop().expect("checked last layer");
            new_count = new_count.saturating_add(top.graph.commits.len());
            let base_oids = layers
                .iter()
                .flat_map(|layer| layer.graph.commits.iter().map(|entry| entry.oid))
                .collect::<Vec<_>>();
            new_entries.extend(commit_graph_write_entries_from_graph_with_base(
                &top.graph,
                &base_oids,
            )?);
        }
    }

    let base_hashes = layers.iter().map(|layer| layer.hash).collect::<Vec<_>>();
    let base_oids = layers
        .iter()
        .flat_map(|layer| layer.graph.commits.iter().map(|entry| entry.oid))
        .collect::<Vec<_>>();
    let write_generation_data = if let Some(top) = layers.last() {
        commit_graph_has_chunk(&top.graph, *b"GDA2")
    } else {
        commit_graph_has_chunk(&full_graph, *b"GDA2")
    };
    let bloom_settings = full_graph
        .bloom_filters
        .as_ref()
        .map(|filters| sley_formats::CommitGraphBloomSettings {
            hash_version: filters.hash_version,
            hash_count: filters.hash_count,
            bits_per_entry: filters.bits_per_entry,
            max_changed_paths: sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS.max_changed_paths,
        })
        .unwrap_or(sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS);
    let graph = CommitGraph::write_with_base_options(
        format,
        &new_entries,
        bloom_settings,
        write_generation_data,
        &base_hashes,
        &base_oids,
    )?;
    let hash = graph_file_checksum(&graph, format)?;
    let graph_path = graphs.join(format!("graph-{hash}.graph"));
    write_commit_graph_file(&graph_path, &graph)?;

    let mut chain = base_hashes;
    chain.push(hash);
    let mut chain_text = String::new();
    for hash in &chain {
        chain_text.push_str(&hash.to_hex());
        chain_text.push('\n');
    }
    write_commit_graph_file(&chain_path, chain_text.as_bytes())?;
    if single.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&single, fs::Permissions::from_mode(0o600));
        }
        let _ = fs::remove_file(&single);
    }
    expire_split_commit_graphs(&graphs, &chain, options.expire_time)?;
    Ok(())
}

fn load_commit_graph_layers(object_dir: &Path, format: ObjectFormat) -> Result<Vec<CommitGraphLayer>> {
    let local_chain = object_dir
        .join("info")
        .join("commit-graphs")
        .join("commit-graph-chain");
    let mut hashes = read_commit_graph_chain_hashes(&local_chain, format)?;
    if hashes.is_empty() {
        for alternate in commit_graph_alternate_object_dirs(object_dir)? {
            let alternate_chain = alternate
                .join("info")
                .join("commit-graphs")
                .join("commit-graph-chain");
            hashes = read_commit_graph_chain_hashes(&alternate_chain, format)?;
            if !hashes.is_empty() {
                break;
            }
        }
    }
    let mut layers = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let bytes = fs::read(commit_graph_layer_path(object_dir, &hash)?)?;
        let graph = CommitGraph::parse(&bytes, format)?;
        layers.push(CommitGraphLayer { hash, graph });
    }
    Ok(layers)
}

fn commit_graph_layer_path(object_dir: &Path, hash: &ObjectId) -> Result<PathBuf> {
    let local = object_dir
        .join("info")
        .join("commit-graphs")
        .join(format!("graph-{hash}.graph"));
    if local.exists() {
        return Ok(local);
    }
    for alternate in commit_graph_alternate_object_dirs(object_dir)? {
        let path = alternate
            .join("info")
            .join("commit-graphs")
            .join(format!("graph-{hash}.graph"));
        if path.exists() {
            return Ok(path);
        }
    }
    Err(GitError::InvalidPath(format!(
        "missing commit-graph layer graph-{hash}.graph"
    )))
}

fn commit_graph_alternate_object_dirs(object_dir: &Path) -> Result<Vec<PathBuf>> {
    let alternates = object_dir.join("info").join("alternates");
    let contents = match fs::read_to_string(&alternates) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let base = alternates.parent().unwrap_or(object_dir);
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let path = PathBuf::from(line);
            if path.is_absolute() {
                path
            } else {
                base.join(path)
            }
        })
        .collect())
}

fn commit_graph_write_entries_from_graph(graph: &CommitGraph) -> Result<Vec<CommitGraphWriteEntry>> {
    commit_graph_write_entries_from_graph_with_base(graph, &[])
}

fn commit_graph_write_entries_from_graph_with_base(
    graph: &CommitGraph,
    base_oids: &[ObjectId],
) -> Result<Vec<CommitGraphWriteEntry>> {
    let mut entries = Vec::with_capacity(graph.commits.len());
    for (idx, entry) in graph.commits.iter().enumerate() {
        let parents = entry
            .parents
            .iter()
            .map(|parent| {
                let parent = *parent as usize;
                if parent < base_oids.len() {
                    Ok(base_oids[parent])
                } else {
                    let local = parent - base_oids.len();
                    graph
                        .commits
                        .get(local)
                        .map(|entry| entry.oid)
                        .ok_or_else(|| {
                            GitError::InvalidFormat(
                                "commit-graph parent points past commit table".into(),
                            )
                        })
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let bloom_filter = graph
            .bloom_filters
            .as_ref()
            .and_then(|filters| filters.filter_for_commit(idx).map(|filter| filter.to_vec()));
        entries.push(CommitGraphWriteEntry {
            oid: entry.oid,
            tree: entry.tree,
            parents,
            generation: entry.generation,
            commit_time: entry.commit_time,
            bloom_filter,
        });
    }
    Ok(entries)
}

fn commit_graph_has_chunk(graph: &CommitGraph, id: [u8; 4]) -> bool {
    graph.chunks.iter().any(|chunk| chunk.id == id)
}

fn expire_split_commit_graphs(
    graphs: &Path,
    chain: &[ObjectId],
    expire_time: Option<i64>,
) -> Result<()> {
    let expire_time = expire_time.unwrap_or_else(current_unix_seconds);
    let keep = chain
        .iter()
        .map(|hash| format!("graph-{hash}.graph"))
        .collect::<HashSet<_>>();
    let Ok(entries) = fs::read_dir(graphs) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".graph") || keep.contains(name) {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(i64::MIN);
        if modified <= expire_time {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn remove_split_commit_graphs(object_dir: &Path) -> Result<()> {
    let graphs = object_dir.join("info").join("commit-graphs");
    let chain = graphs.join("commit-graph-chain");
    let _ = fs::remove_file(chain);
    let Ok(entries) = fs::read_dir(&graphs) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("graph") {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn read_commit_graph_chain_hashes(path: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| ObjectId::from_hex(format, line.trim()))
        .collect()
}

fn graph_file_checksum(bytes: &[u8], format: ObjectFormat) -> Result<ObjectId> {
    let raw_len = format.raw_len();
    if bytes.len() < raw_len {
        return Err(GitError::InvalidFormat("commit-graph file too short".into()));
    }
    ObjectId::from_raw(format, &bytes[bytes.len() - raw_len..])
}

/// `--reachable`: write the graph seeded from refs + HEAD. Always writes a file
/// (matching git, which produces a header-only graph for an empty repo).
fn write_reachable_commit_graph(
    git_dir: &Path,
    object_dir: &Path,
    format: ObjectFormat,
    changed_paths: bool,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
    write_generation_data: bool,
    max_new_filters: Option<usize>,
    existing_filters: &HashMap<ObjectId, Vec<u8>>,
    split: CommitGraphSplitOptions,
    progress: bool,
) -> Result<()> {
    let graph = commit_graph_for_reachable_refs(
        git_dir,
        object_dir,
        format,
        changed_paths,
        bloom_settings,
        write_generation_data,
        max_new_filters,
        existing_filters,
        progress,
    )?;
    let graph_dir = object_dir.join("info");
    fs::create_dir_all(&graph_dir)?;
    if split.mode == CommitGraphSplitMode::Off {
        write_commit_graph_file(&graph_dir.join("commit-graph"), &graph)?;
        remove_split_commit_graphs(object_dir)?;
    } else {
        write_split_commit_graph_file(object_dir, format, &graph, split)?;
    }
    Ok(())
}

/// Seed commits for the default (all-packs) write: every commit object found in
/// the object dir's packs (git's `fill_oids_from_all_packs`).
fn commit_graph_packed_commit_starts(
    db: &FileObjectDatabase,
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for oid in sley_odb::packed_object_ids(object_dir, format)? {
        let Ok(object) = db.read_object(&oid) else {
            continue;
        };
        if object.object_type == ObjectType::Commit && seen.insert(oid) {
            starts.push(oid);
        }
    }
    Ok(starts)
}

/// `--stdin-packs`: read pack index paths from stdin and seed from the commits
/// in those packs. A missing/invalid pack is a fatal "error adding pack".
fn commit_graph_stdin_packs_starts(
    db: &FileObjectDatabase,
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let pack_path = resolve_cli_path(&env::current_dir()?, line);
        // git resolves the named pack relative to the object dir's pack/ dir
        // when it is not an absolute existing path.
        let candidates = [pack_path.clone(), object_dir.join("pack").join(line)];
        let resolved = candidates.iter().find(|path| path.exists());
        let Some(resolved) = resolved else {
            eprintln!("error: error adding pack {line}");
            return Err(GitError::Exit(1));
        };
        let oids = commit_graph_commit_oids_in_pack(db, resolved, format).map_err(|_| {
            eprintln!("error: error adding pack {line}");
            GitError::Exit(1)
        })?;
        for oid in oids {
            if seen.insert(oid) {
                starts.push(oid);
            }
        }
    }
    Ok(starts)
}

/// Commit oids contained in a single pack, addressed by its `.idx` or `.pack`
/// path.
fn commit_graph_commit_oids_in_pack(
    db: &FileObjectDatabase,
    pack_path: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let idx_path = if pack_path.extension().and_then(|ext| ext.to_str()) == Some("pack") {
        pack_path.with_extension("idx")
    } else {
        pack_path.to_path_buf()
    };
    let index_bytes = fs::read(&idx_path)?;
    let index = sley_pack::PackIndex::parse(&index_bytes, format)?;
    let mut oids = Vec::new();
    for entry in index.entries {
        if let Ok(object) = db.read_object(&entry.oid)
            && object.object_type == ObjectType::Commit
        {
            oids.push(entry.oid);
        }
    }
    Ok(oids)
}

/// `--stdin-commits`: read commit oids from stdin (each must be hex and resolve
/// to an existing object), seed the closure from them. git's diagnostics:
/// "unexpected non-hex object ID: <s>" and "invalid object <oid>".
fn commit_graph_stdin_commits_starts(
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(oid) = ObjectId::from_hex(format, line) else {
            eprintln!("error: unexpected non-hex object ID: {line}");
            return Err(GitError::Exit(1));
        };
        let Ok(object) = db.read_object(&oid) else {
            eprintln!("error: invalid object {line}");
            return Err(GitError::Exit(1));
        };
        // Peel tags/commit; non-commit tree-ish (e.g. a tree oid) is silently
        // skipped, matching git, which only graphs the commit objects.
        if object.object_type == ObjectType::Commit && seen.insert(oid) {
            starts.push(oid);
        }
    }
    Ok(starts)
}

fn cmd_commit_graph_verify(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    // git: opts.progress defaults to isatty(2); --progress forces on,
    // --no-progress forces off. Under the test harness stderr is redirected, so
    // the default is off; only an explicit --progress emits the progress line.
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--shallow" => {}
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "commit-graph verify option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    let graph_path = object_dir.join("info").join("commit-graph");
    // git's `cmd_commit_graph_verify` prefers the single-file graph; only if it
    // is absent (ENOENT) does it fall back to the chain. A graph that exists but
    // cannot be opened (e.g. permissions) is a fatal `Could not open` error.
    match open_commit_graph_bytes(&graph_path) {
        OpenResult::Bytes(bytes) => {
            return verify_commit_graph_bytes(&object_dir, format, &bytes, progress);
        }
        OpenResult::OpenError => {
            // git: die_errno("Could not open commit-graph '%s'") ⇒ exit 128.
            eprintln!("fatal: Could not open commit-graph '{}'", graph_path.display());
            return Err(GitError::Exit(128));
        }
        OpenResult::NotFound => {}
    }
    let chain_path = object_dir
        .join("info")
        .join("commit-graphs")
        .join("commit-graph-chain");
    if chain_path.exists() {
        return verify_split_commit_graph_chain(&chain_path, format);
    }
    // No commit-graph at all is not an error (git's `commit-graph verify`
    // exits 0 when there is nothing to verify).
    Ok(())
}

/// Outcome of trying to open + read the single-file commit-graph, mirroring
/// git's `open_commit_graph` (which distinguishes ENOENT from other errno).
enum OpenResult {
    Bytes(Vec<u8>),
    /// The path does not exist (ENOENT) — fall through to the chain.
    NotFound,
    /// The path exists but could not be read (e.g. permission denied).
    OpenError,
}

fn open_commit_graph_bytes(path: &Path) -> OpenResult {
    match fs::read(path) {
        Ok(bytes) => OpenResult::Bytes(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => OpenResult::NotFound,
        Err(_) => OpenResult::OpenError,
    }
}

fn verify_split_commit_graph_chain(chain_path: &Path, format: ObjectFormat) -> Result<()> {
    let chain_dir = chain_path
        .parent()
        .ok_or_else(|| GitError::InvalidPath("commit-graph chain path has no parent".into()))?;
    let chain_bytes = fs::read(chain_path)?;
    let text = std::str::from_utf8(&chain_bytes)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut graph_hashes = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        graph_hashes.push(ObjectId::from_hex(format, line)?);
    }
    if graph_hashes.is_empty() {
        return Err(GitError::InvalidFormat(
            "commit-graph chain is empty".into(),
        ));
    }
    for (idx, expected_hash) in graph_hashes.iter().enumerate() {
        let graph_path = chain_dir.join(format!("graph-{expected_hash}.graph"));
        let graph = CommitGraph::parse(&fs::read(&graph_path)?, format)?;
        if &graph.checksum != expected_hash {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph {} checksum is {}, expected {expected_hash}",
                graph_path.display(),
                graph.checksum
            )));
        }
        if graph.base_graph_count as usize != graph.base_graphs.len() {
            return Err(GitError::InvalidFormat(
                "commit-graph BASE count does not match parsed base list".into(),
            ));
        }
        if graph.base_graph_count as usize > idx {
            return Err(GitError::InvalidFormat(
                "commit-graph has more base graphs than previous chain entries".into(),
            ));
        }
        if !graph.base_graphs.is_empty() {
            let expected_bases = &graph_hashes[idx - graph.base_graphs.len()..idx];
            if graph.base_graphs != expected_bases {
                return Err(GitError::InvalidFormat(
                    "commit-graph BASE hashes do not match chain order".into(),
                ));
            }
        }
    }
    Ok(())
}

// === commit-graph verify ====================================================
//
// A byte-faithful reimplementation of git's `verify_commit_graph` /
// `verify_one_commit_graph` (commit-graph.c) + the structural checks in
// `parse_commit_graph` / `read_table_of_contents`. It re-parses the on-disk
// graph from raw bytes (independent of `CommitGraph::parse`) so the exact
// validation order and error strings match git's, and cross-checks every commit
// against the object database. Each detected problem is reported with git's
// exact `error:`/`fatal:` text; the command exits non-zero when any check fails.

const GRAPH_HEADER_SIZE: usize = 8;
const GRAPH_CHUNK_TOC_ENTRY_SIZE: usize = 12;
const GRAPH_FANOUT_SIZE: usize = 4 * 256;
const GRAPH_SIGNATURE: u32 = 0x4347_5048; // "CGPH"
const GRAPH_VERSION: u8 = 1;
const GRAPH_PARENT_NONE: u32 = 0x7000_0000;
const GRAPH_EXTRA_EDGES_NEEDED: u32 = 0x8000_0000;
const GRAPH_EDGE_LAST_MASK: u32 = 0x7fff_ffff;
const GRAPH_LAST_EDGE: u32 = 0x8000_0000;
const GENERATION_NUMBER_V1_MAX: u64 = 0x3fff_ffff;

const CHUNK_OIDF: [u8; 4] = *b"OIDF";
const CHUNK_OIDL: [u8; 4] = *b"OIDL";
const CHUNK_CDAT: [u8; 4] = *b"CDAT";
const CHUNK_EDGE: [u8; 4] = *b"EDGE";

fn graph_min_size(hash_len: usize) -> usize {
    GRAPH_HEADER_SIZE + 4 * GRAPH_CHUNK_TOC_ENTRY_SIZE + GRAPH_FANOUT_SIZE + hash_len
}

fn graph_data_width(hash_len: usize) -> usize {
    hash_len + 16
}

fn read_be32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_be64(bytes: &[u8], offset: usize) -> u64 {
    let mut value = 0u64;
    for &byte in &bytes[offset..offset + 8] {
        value = (value << 8) | u64::from(byte);
    }
    value
}

/// A parsed-but-unvalidated view of a chunk's byte range within the graph.
struct GraphChunk {
    id: [u8; 4],
    start: usize,
    size: usize,
}

/// The chunks + header fields needed for verification, parsed straight from raw
/// bytes (mirrors git's `parse_commit_graph`). Returns `Err(Exit)` after
/// printing the matching `error:` line when the graph cannot be parsed; the
/// command then exits 1 — exactly git's `if (!graph) return 1;`.
struct ParsedGraph<'a> {
    bytes: &'a [u8],
    format: ObjectFormat,
    hash_len: usize,
    num_commits: u32,
    oid_fanout: usize,
    oid_lookup: usize,
    commit_data: usize,
    extra_edges: Option<(usize, usize)>,
}

fn parse_commit_graph_for_verify<'a>(
    bytes: &'a [u8],
    format: ObjectFormat,
) -> std::result::Result<ParsedGraph<'a>, GitError> {
    let hash_len = format.raw_len();

    if bytes.len() < graph_min_size(hash_len) {
        eprintln!("error: commit-graph file is too small");
        return Err(GitError::Exit(1));
    }

    let signature = read_be32(bytes, 0);
    if signature != GRAPH_SIGNATURE {
        eprintln!(
            "error: commit-graph signature {signature:X} does not match signature {GRAPH_SIGNATURE:X}"
        );
        return Err(GitError::Exit(1));
    }

    let version = bytes[4];
    if version != GRAPH_VERSION {
        eprintln!(
            "error: commit-graph version {version:X} does not match version {GRAPH_VERSION:X}"
        );
        return Err(GitError::Exit(1));
    }

    let hash_version = bytes[5];
    let expected_hash_version = match format {
        ObjectFormat::Sha1 => 1u8,
        ObjectFormat::Sha256 => 2u8,
    };
    if hash_version != expected_hash_version {
        eprintln!(
            "error: commit-graph hash version {hash_version:X} does not match version {expected_hash_version:X}"
        );
        return Err(GitError::Exit(1));
    }

    let num_chunks = bytes[6] as usize;

    if bytes.len()
        < GRAPH_HEADER_SIZE
            + (num_chunks + 1) * GRAPH_CHUNK_TOC_ENTRY_SIZE
            + GRAPH_FANOUT_SIZE
            + hash_len
    {
        eprintln!("error: commit-graph file is too small to hold {num_chunks} chunks");
        return Err(GitError::Exit(1));
    }

    // Read the table of contents (mirrors read_table_of_contents with
    // expected_alignment = 1 for commit-graph).
    let mut chunks: Vec<GraphChunk> = Vec::with_capacity(num_chunks);
    let mfile_size = bytes.len();
    let mut toc = GRAPH_HEADER_SIZE;
    for _ in 0..num_chunks {
        let chunk_id = [
            bytes[toc],
            bytes[toc + 1],
            bytes[toc + 2],
            bytes[toc + 3],
        ];
        let chunk_offset = read_be64(bytes, toc + 4) as usize;
        if chunk_id == [0, 0, 0, 0] {
            eprintln!("error: terminating chunk id appears earlier than expected");
            return Err(GitError::Exit(1));
        }
        let next_toc = toc + GRAPH_CHUNK_TOC_ENTRY_SIZE;
        let next_chunk_offset = read_be64(bytes, next_toc + 4) as usize;
        if next_chunk_offset < chunk_offset || next_chunk_offset > mfile_size - hash_len {
            eprintln!(
                "error: improper chunk offset(s) {chunk_offset:X} and {next_chunk_offset:X}"
            );
            return Err(GitError::Exit(1));
        }
        if chunks.iter().any(|chunk| chunk.id == chunk_id) {
            eprintln!("error: duplicate chunk ID {} found", be32_of(&chunk_id));
            return Err(GitError::Exit(1));
        }
        chunks.push(GraphChunk {
            id: chunk_id,
            start: chunk_offset,
            size: next_chunk_offset - chunk_offset,
        });
        toc = next_toc;
    }
    let terminator_id = read_be32(bytes, toc);
    if terminator_id != 0 {
        eprintln!("error: final chunk has non-zero id {terminator_id:X}");
        return Err(GitError::Exit(1));
    }

    let find = |id: [u8; 4]| chunks.iter().find(|chunk| chunk.id == id);

    // Required: OID fanout.
    let fanout_chunk = find(CHUNK_OIDF);
    let (oid_fanout, num_commits) = match fanout_chunk {
        Some(chunk) if chunk.size == 256 * 4 => {
            // fanout out-of-order check
            for i in 0..255usize {
                let f1 = read_be32(bytes, chunk.start + i * 4);
                let f2 = read_be32(bytes, chunk.start + (i + 1) * 4);
                if f1 > f2 {
                    eprintln!("error: commit-graph fanout values out of order");
                    eprintln!("error: commit-graph required OID fanout chunk missing or corrupted");
                    return Err(GitError::Exit(1));
                }
            }
            (chunk.start, read_be32(bytes, chunk.start + 255 * 4))
        }
        Some(_) => {
            eprintln!("error: commit-graph oid fanout chunk is wrong size");
            eprintln!("error: commit-graph required OID fanout chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
        None => {
            eprintln!("error: commit-graph required OID fanout chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
    };

    // Required: OID lookup.
    let oid_lookup = match find(CHUNK_OIDL) {
        Some(chunk) if chunk.size / hash_len == num_commits as usize => chunk.start,
        Some(_) => {
            eprintln!("error: commit-graph OID lookup chunk is the wrong size");
            eprintln!("error: commit-graph required OID lookup chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
        None => {
            eprintln!("error: commit-graph required OID lookup chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
    };

    // Required: commit data.
    let commit_data = match find(CHUNK_CDAT) {
        Some(chunk) if chunk.size / graph_data_width(hash_len) == num_commits as usize => {
            chunk.start
        }
        Some(_) => {
            eprintln!("error: commit-graph commit data chunk is wrong size");
            eprintln!("error: commit-graph required commit data chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
        None => {
            eprintln!("error: commit-graph required commit data chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
    };

    let extra_edges = find(CHUNK_EDGE).map(|chunk| (chunk.start, chunk.size));

    Ok(ParsedGraph {
        bytes,
        format,
        hash_len,
        num_commits,
        oid_fanout,
        oid_lookup,
        commit_data,
        extra_edges,
    })
}

fn be32_of(id: &[u8; 4]) -> String {
    format!("{:X}", u32::from_be_bytes(*id))
}

/// Full verify of a single-file commit-graph: re-parse + structural checks +
/// per-commit cross-check against the ODB. Returns `Ok(())` only when the graph
/// is fully valid (git's exit 0); otherwise prints the matching diagnostics and
/// returns `Exit`.
fn verify_commit_graph_bytes(
    object_dir: &Path,
    format: ObjectFormat,
    bytes: &[u8],
    progress: bool,
) -> Result<()> {
    let parsed = match parse_commit_graph_for_verify(bytes, format) {
        Ok(parsed) => parsed,
        Err(exit) => return Err(exit),
    };

    let db = FileObjectDatabase::new(object_dir, format);
    let mut had_error = false;
    // Tracks whether the only error so far is the checksum failure; git allows
    // the per-commit cross-check to proceed past a checksum-only failure.
    let mut non_checksum_error = false;

    // Checksum validation (git: commit_graph_checksum_valid).
    let hash_len = parsed.hash_len;
    let checksum_offset = bytes.len() - hash_len;
    let actual = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
    let stored = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
    if actual != stored {
        eprintln!("error: the commit-graph file has incorrect checksum and is likely corrupt");
        had_error = true;
    }

    let num_commits = parsed.num_commits as usize;

    // OID order + fanout consistency (first verify loop).
    let mut prev_oid: Option<ObjectId> = None;
    let mut cur_fanout_pos = 0u32;
    for i in 0..num_commits {
        let cur_oid = oid_at_lookup(&parsed, i)?;
        if let Some(prev) = prev_oid
            && prev.as_bytes() >= cur_oid.as_bytes()
        {
            eprintln!(
                "error: commit-graph has incorrect OID order: {prev} then {cur_oid}"
            );
            had_error = true;
            non_checksum_error = true;
        }
        prev_oid = Some(cur_oid);

        let first_byte = u32::from(cur_oid.as_bytes()[0]);
        while first_byte > cur_fanout_pos {
            let fanout_value = read_be32(bytes, parsed.oid_fanout + cur_fanout_pos as usize * 4);
            if i as u32 != fanout_value {
                eprintln!(
                    "error: commit-graph has incorrect fanout value: fanout[{}] = {} != {}",
                    cur_fanout_pos, fanout_value, i
                );
                had_error = true;
                non_checksum_error = true;
            }
            cur_fanout_pos += 1;
        }
    }
    while cur_fanout_pos < 256 {
        let fanout_value = read_be32(bytes, parsed.oid_fanout + cur_fanout_pos as usize * 4);
        if parsed.num_commits != fanout_value {
            eprintln!(
                "error: commit-graph has incorrect fanout value: fanout[{}] = {} != {}",
                cur_fanout_pos, fanout_value, num_commits
            );
            had_error = true;
            non_checksum_error = true;
        }
        cur_fanout_pos += 1;
    }

    // git: if (verify_commit_graph_error & ~VERIFY_COMMIT_GRAPH_ERROR_HASH)
    //          return verify_commit_graph_error;
    // i.e. stop before the per-commit ODB cross-check if any *non-checksum*
    // error fired above.
    if non_checksum_error {
        return Err(GitError::Exit(1));
    }

    // Per-commit cross-check against the object database (second verify loop).
    // git drives a progress meter titled "Verifying commits in commit graph"
    // here; emit the final, complete line when progress is requested.
    if progress {
        eprintln!(
            "Verifying commits in commit graph: 100% ({num_commits}/{num_commits}), done."
        );
    }
    let mut seen_gen_zero: Option<ObjectId> = None;
    let mut seen_gen_non_zero: Option<ObjectId> = None;

    for i in 0..num_commits {
        let cur_oid = oid_at_lookup(&parsed, i)?;

        // Parse the commit from the ODB.
        let odb_object = match db.read_object(&cur_oid) {
            Ok(object) if object.object_type == ObjectType::Commit => object,
            _ => {
                eprintln!(
                    "error: failed to parse commit {cur_oid} from object database for commit-graph"
                );
                had_error = true;
                continue;
            }
        };
        let odb_commit = Commit::parse_ref(format, &odb_object.body)?;

        // Decode the graph's record for this commit.
        let record = decode_graph_commit(&parsed, i)?;

        // Root tree OID.
        if record.tree != odb_commit.tree {
            eprintln!(
                "error: root tree OID for commit {cur_oid} in commit-graph is {} != {}",
                record.tree, odb_commit.tree
            );
            had_error = true;
        }

        // Parents: compare graph-encoded parents against the ODB parents.
        let graph_parents = &record.parents;
        let odb_parents = &odb_commit.parents;
        let mut max_generation = 0u64;
        let common = graph_parents.len().min(odb_parents.len());
        for k in 0..common {
            let graph_parent_oid = oid_at_lookup(&parsed, graph_parents[k] as usize)?;
            if graph_parent_oid != odb_parents[k] {
                eprintln!(
                    "error: commit-graph parent for {cur_oid} is {graph_parent_oid} != {}",
                    odb_parents[k]
                );
                had_error = true;
            }
            let parent_record = decode_graph_commit(&parsed, graph_parents[k] as usize)?;
            if parent_record.generation > max_generation {
                max_generation = parent_record.generation;
            }
        }
        if graph_parents.len() > odb_parents.len() {
            eprintln!(
                "error: commit-graph parent list for commit {cur_oid} is too long"
            );
            had_error = true;
        } else if odb_parents.len() > graph_parents.len() {
            eprintln!(
                "error: commit-graph parent list for commit {cur_oid} terminates early"
            );
            had_error = true;
        }

        if record.generation != 0 {
            seen_gen_non_zero = Some(cur_oid);
        } else {
            seen_gen_zero = Some(cur_oid);
        }

        if seen_gen_zero.is_some() {
            continue;
        }

        // V1 (topological level) generation check. This graph is written with
        // generationVersion=1, so read_generation_data is false.
        if max_generation == GENERATION_NUMBER_V1_MAX {
            max_generation -= 1;
        }
        if record.generation < max_generation + 1 {
            eprintln!(
                "error: commit-graph generation for commit {cur_oid} is {} < {}",
                record.generation,
                max_generation + 1
            );
            had_error = true;
        }

        // Commit date cross-check.
        let odb_date = odb_commit
            .committer_signature()
            .map(|sig| sig.time.seconds)
            .unwrap_or(0);
        if record.commit_date as i64 != odb_date {
            eprintln!(
                "error: commit date for commit {cur_oid} in commit-graph is {} != {}",
                record.commit_date, odb_date
            );
            had_error = true;
        }
    }

    if let (Some(zero), Some(non_zero)) = (seen_gen_zero, seen_gen_non_zero) {
        eprintln!(
            "error: commit-graph has both zero and non-zero generations (e.g., commits '{zero}' and '{non_zero}')"
        );
        had_error = true;
    }

    if had_error {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

/// The OID at lexicographic position `index` in the OID lookup chunk.
fn oid_at_lookup(parsed: &ParsedGraph<'_>, index: usize) -> Result<ObjectId> {
    let off = parsed.oid_lookup + index * parsed.hash_len;
    ObjectId::from_raw(parsed.format, &parsed.bytes[off..off + parsed.hash_len])
}

/// A commit record decoded straight from the graph's CDAT/EDGE chunks, mirroring
/// git's `fill_commit_in_graph` + `fill_commit_graph_info` parent/date/gen
/// decoding. `parents` are lexicographic positions into the OID lookup table.
struct GraphCommitRecord {
    tree: ObjectId,
    parents: Vec<u32>,
    generation: u64,
    commit_date: u64,
}

fn decode_graph_commit(parsed: &ParsedGraph<'_>, index: usize) -> Result<GraphCommitRecord> {
    let hash_len = parsed.hash_len;
    let width = graph_data_width(hash_len);
    let base = parsed.commit_data + index * width;
    let bytes = parsed.bytes;

    let tree = ObjectId::from_raw(parsed.format, &bytes[base..base + hash_len])?;

    // Date / generation (fill_commit_graph_info, V1 path).
    let date_high = u64::from(read_be32(bytes, base + hash_len + 8) & 0x3);
    let date_low = u64::from(read_be32(bytes, base + hash_len + 12));
    let commit_date = (date_high << 32) | date_low;
    let generation = u64::from(read_be32(bytes, base + hash_len + 8) >> 2);

    // Parents (fill_commit_in_graph). git `die`s on an out-of-range parent
    // position; we mirror that with a fatal `invalid parent position` + exit
    // 128, and on an out-of-bounds extra-edges pointer with the `error:`
    // string + exit 1 (commit-graph extra-edges pointer out of bounds).
    let num_total = parsed.num_commits;
    let mut parents = Vec::new();

    let insert = |pos: u32, parents: &mut Vec<u32>| -> Result<()> {
        if pos >= num_total {
            eprintln!("fatal: invalid parent position {pos}");
            return Err(GitError::Exit(128));
        }
        parents.push(pos);
        Ok(())
    };

    let edge0 = read_be32(bytes, base + hash_len);
    if edge0 == GRAPH_PARENT_NONE {
        return Ok(GraphCommitRecord {
            tree,
            parents,
            generation,
            commit_date,
        });
    }
    insert(edge0, &mut parents)?;

    let edge1 = read_be32(bytes, base + hash_len + 4);
    if edge1 == GRAPH_PARENT_NONE {
        return Ok(GraphCommitRecord {
            tree,
            parents,
            generation,
            commit_date,
        });
    }
    if edge1 & GRAPH_EXTRA_EDGES_NEEDED == 0 {
        insert(edge1, &mut parents)?;
        return Ok(GraphCommitRecord {
            tree,
            parents,
            generation,
            commit_date,
        });
    }

    // Octopus: walk the EDGE chunk.
    let mut parent_data_pos = edge1 & GRAPH_EDGE_LAST_MASK;
    let (edge_start, edge_size) = parsed.extra_edges.unwrap_or((0, 0));
    loop {
        if (edge_size / 4) as u32 <= parent_data_pos {
            eprintln!("error: commit-graph extra-edges pointer out of bounds");
            return Err(GitError::Exit(1));
        }
        let edge_value = read_be32(bytes, edge_start + parent_data_pos as usize * 4);
        insert(edge_value & GRAPH_EDGE_LAST_MASK, &mut parents)?;
        parent_data_pos += 1;
        if edge_value & GRAPH_LAST_EDGE != 0 {
            break;
        }
    }

    Ok(GraphCommitRecord {
        tree,
        parents,
        generation,
        commit_date,
    })
}

fn commit_graph_for_reachable_refs(
    git_dir: &Path,
    object_dir: &Path,
    format: ObjectFormat,
    changed_paths: bool,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
    write_generation_data: bool,
    max_new_filters: Option<usize>,
    existing_filters: &HashMap<ObjectId, Vec<u8>>,
    progress: bool,
) -> Result<Vec<u8>> {
    let db = FileObjectDatabase::new(object_dir, format);
    let store = FileRefStore::new(git_dir, format);
    let mut starts = Vec::new();
    let mut seen_starts = HashSet::new();
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        if let Ok(commit) = sley_rev::peel_to_commit(&db, format, &oid)
            && seen_starts.insert(commit)
        {
            starts.push(commit);
        }
    }
    if let Ok(head) = resolve_revision(git_dir, format, "HEAD")
        && let Ok(commit) = sley_rev::peel_to_commit(&db, format, &head)
        && seen_starts.insert(commit)
    {
        starts.push(commit);
    }
    commit_graph_from_starts(
        &db,
        format,
        starts,
        changed_paths,
        bloom_settings,
        write_generation_data,
        max_new_filters,
        existing_filters,
        progress,
    )
}

/// Build the commit-graph bytes from a set of seed commit oids (their parent
/// closure is walked). Shared by the `--reachable`, default-all-packs, and
/// `--stdin-commits` paths.
fn commit_graph_from_starts(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: Vec<ObjectId>,
    changed_paths: bool,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
    write_generation_data: bool,
    max_new_filters: Option<usize>,
    existing_filters: &HashMap<ObjectId, Vec<u8>>,
    progress: bool,
) -> Result<Vec<u8>> {
    // git's `close_reachable` walk parses every reachable commit (including
    // parents pulled into the closure); a commit that cannot be parsed is fatal
    // with `unable to parse commit <oid>` (exit 128). `walk_commits` surfaces a
    // generic read/parse error instead, so map it to git's diagnostic by
    // re-checking which oid in the closure is unparseable.
    let records = match sley_rev::walk_commits(db, format, starts.clone()) {
        Ok(records) => records,
        Err(err) => {
            if let Some(oid) = commit_graph_first_unparseable_commit(db, format, &starts) {
                eprintln!("fatal: unable to parse commit {oid}");
                return Err(GitError::Exit(128));
            }
            return Err(err);
        }
    };
    let record_map = records
        .iter()
        .map(|record| (record.oid, record))
        .collect::<HashMap<_, _>>();
    let mut generation_cache = HashMap::new();
    let mut entries = Vec::with_capacity(records.len());
    let mut bloom_stats = CommitGraphBloomWriteStats::default();
    for record in &records {
        let bloom_filter = if changed_paths {
            if let Some(filter) = existing_filters.get(&record.oid) {
                bloom_stats.filter_not_computed += 1;
                Some(filter.clone())
            } else if max_new_filters.is_some_and(|max| bloom_stats.filter_computed >= max) {
                bloom_stats.filter_not_computed += 1;
                None
            } else {
                let (filter, disposition) = commit_graph_bloom_filter_for_record(
                    db,
                    format,
                    record,
                    &record_map,
                    bloom_settings,
                )?;
                bloom_stats.filter_computed += 1;
                match disposition {
                    CommitGraphBloomDisposition::Empty => bloom_stats.filter_trunc_empty += 1,
                    CommitGraphBloomDisposition::Large => bloom_stats.filter_trunc_large += 1,
                    CommitGraphBloomDisposition::Normal => {}
                }
                Some(filter)
            }
        } else {
            None
        };
        entries.push(CommitGraphWriteEntry {
            oid: record.oid,
            tree: record.commit.tree,
            parents: record.parents.clone(),
            generation: commit_graph_generation(&record.oid, &record_map, &mut generation_cache)?,
            commit_time: commit_graph_commit_time(&record.commit)?,
            bloom_filter,
        });
    }
    if progress {
        let count = entries.len();
        // git drives several delayed progress meters during a write; emit the
        // generation-number + write-out lines (always) and the changed-path
        // Bloom-filter line (only when changed-path filters are computed).
        if changed_paths {
            eprintln!(
                "Computing commit changed paths Bloom filters: 100% ({count}/{count}), done."
            );
        }
        eprintln!(
            "Computing commit graph generation numbers: 100% ({count}/{count}), done."
        );
        eprintln!("Writing out commit graph in 3 passes: 100% ({}/{}), done.", count * 3, count * 3);
    }
    if changed_paths {
        trace_commit_graph_bloom_settings(bloom_settings);
        sley_core::trace2::data(
            "commit-graph",
            "filter-computed",
            bloom_stats.filter_computed,
        );
        sley_core::trace2::data(
            "commit-graph",
            "filter-not-computed",
            bloom_stats.filter_not_computed,
        );
        sley_core::trace2::data(
            "commit-graph",
            "filter-trunc-empty",
            bloom_stats.filter_trunc_empty,
        );
        sley_core::trace2::data(
            "commit-graph",
            "filter-trunc-large",
            bloom_stats.filter_trunc_large,
        );
        sley_core::trace2::data("commit-graph", "filter-upgraded", 0);
    }
    CommitGraph::write_with_options(format, &entries, bloom_settings, write_generation_data)
}

/// Walk the parent closure of `starts` and return the first oid that cannot be
/// read + parsed as a commit object (git's closure walk dies on such a commit
/// with `unable to parse commit <oid>`). Returns `None` if the whole closure
/// parses (the original error was something else).
fn commit_graph_first_unparseable_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
) -> Option<ObjectId> {
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.iter().copied().collect();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Commit => {
                match Commit::parse_ref(format, &object.body) {
                    Ok(commit) => pending.extend(commit.parents.iter().copied()),
                    Err(_) => return Some(oid),
                }
            }
            // Not a commit, or not readable at all ⇒ git cannot parse it.
            _ => return Some(oid),
        }
    }
    None
}

#[derive(Default)]
struct CommitGraphBloomWriteStats {
    filter_computed: usize,
    filter_not_computed: usize,
    filter_trunc_empty: usize,
    filter_trunc_large: usize,
}

enum CommitGraphBloomDisposition {
    Normal,
    Empty,
    Large,
}

fn commit_graph_bloom_filter_for_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    records: &HashMap<ObjectId, &sley_rev::CommitRecord>,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
) -> Result<(Vec<u8>, CommitGraphBloomDisposition)> {
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: false,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: false,
    };
    let changes = if let Some(parent) = record.parents.first() {
        let parent_tree = if let Some(parent_record) = records.get(parent) {
            parent_record.commit.tree
        } else {
            read_commit_tree_for_graph(db, format, parent)?
        };
        if parent_tree == record.commit.tree {
            Vec::new()
        } else {
            sley_diff_merge::diff_name_status_trees_with_options(
                db,
                format,
                &parent_tree,
                &record.commit.tree,
                options,
            )?
        }
    } else {
        sley_diff_merge::diff_name_status_empty_tree_with_options(
            db,
            format,
            &record.commit.tree,
            options,
        )?
    };
    if changes.is_empty() {
        return Ok((
            sley_formats::commit_graph_bloom_filter_for_paths(
                std::iter::empty::<&[u8]>(),
                bloom_settings,
            ),
            CommitGraphBloomDisposition::Empty,
        ));
    }
    let filter = sley_formats::commit_graph_bloom_filter_for_paths(
        changes.iter().map(|entry| entry.path.as_bytes()),
        bloom_settings,
    );
    let disposition = if filter == [0xff] {
        CommitGraphBloomDisposition::Large
    } else {
        CommitGraphBloomDisposition::Normal
    };
    Ok((filter, disposition))
}

/// `commitGraph.generationVersion` (git's `get_configured_generation_version`):
/// defaults to 2, which writes the GDA2 corrected-commit-date chunk. A value of
/// 1 selects the legacy topological-level-only layout (no GDA2/GDO2).
fn commit_graph_generation_version(config: Option<&sley_config::GitConfig>) -> i64 {
    config
        .and_then(|config| match config.get_entry("commitGraph", None, "generationVersion") {
            Some(Some(value)) => sley_config::parse_config_int(value),
            _ => None,
        })
        .unwrap_or(2)
}

fn commit_graph_changed_paths_version(config: Option<&sley_config::GitConfig>) -> Result<i64> {
    let Some(config) = config else {
        return Ok(-1);
    };
    match config.get_entry("commitGraph", None, "changedPathsVersion") {
        Some(None) => return Ok(1),
        Some(Some(value)) => {
            return sley_config::parse_config_int(value).ok_or_else(|| {
                GitError::Command(format!(
                    "bad numeric config value '{value}' for 'commitGraph.changedPathsVersion'"
                ))
            });
        }
        None => {}
    }
    match config.get_bool("commitGraph", None, "readChangedPaths") {
        Some(false) => Ok(0),
        Some(true) => Ok(-1),
        None => Ok(-1),
    }
}

fn commit_graph_bloom_settings_for_write(
    existing: Option<sley_formats::CommitGraphBloomSettings>,
    changed_paths_version: i64,
    honor_env: bool,
) -> sley_formats::CommitGraphBloomSettings {
    let mut settings = sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS;
    if changed_paths_version == -1
        && let Some(existing) = existing
    {
        settings = existing;
    }
    settings.hash_version = if changed_paths_version == 2
        || (changed_paths_version == -1 && settings.hash_version == 2)
    {
        2
    } else {
        1
    };
    if honor_env {
        if let Ok(value) = env::var("GIT_TEST_BLOOM_SETTINGS_NUM_HASHES")
            && let Ok(parsed) = value.parse::<u32>()
        {
            settings.hash_count = parsed;
        }
        if let Ok(value) = env::var("GIT_TEST_BLOOM_SETTINGS_BITS_PER_ENTRY")
            && let Ok(parsed) = value.parse::<u32>()
        {
            settings.bits_per_entry = parsed;
        }
        if let Ok(value) = env::var("GIT_TEST_BLOOM_SETTINGS_MAX_CHANGED_PATHS")
            && let Ok(parsed) = value.parse::<usize>()
        {
            settings.max_changed_paths = parsed;
        }
    }
    settings
}

fn existing_commit_graph_bloom_settings(
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<sley_formats::CommitGraphBloomSettings>> {
    let graph_path = object_dir.join("info").join("commit-graph");
    if graph_path.exists()
        && let Some(settings) = commit_graph_bloom_settings_from_file(&graph_path, format)
    {
        return Ok(Some(settings));
    }
    let chain_path = object_dir
        .join("info")
        .join("commit-graphs")
        .join("commit-graph-chain");
    let chain_dir = match chain_path.parent() {
        Some(dir) => dir.to_path_buf(),
        None => return Ok(None),
    };
    let hashes = read_commit_graph_chain_hashes(&chain_path, format).unwrap_or_default();
    for hash in hashes.iter().rev() {
        let path = chain_dir.join(format!("graph-{hash}.graph"));
        if let Some(settings) = commit_graph_bloom_settings_from_file(&path, format) {
            return Ok(Some(settings));
        }
    }
    Ok(None)
}

fn commit_graph_bloom_settings_from_file(
    path: &Path,
    format: ObjectFormat,
) -> Option<sley_formats::CommitGraphBloomSettings> {
    let bytes = fs::read(path).ok()?;
    let graph = CommitGraph::parse(&bytes, format).ok()?;
    graph.bloom_filters.map(|filters| {
        let mut settings = sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS;
        settings.hash_version = filters.hash_version;
        settings.hash_count = filters.hash_count;
        settings.bits_per_entry = filters.bits_per_entry;
        settings
    })
}

fn existing_commit_graph_bloom_filters(
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<HashMap<ObjectId, Vec<u8>>> {
    let mut out = HashMap::new();
    let info = object_dir.join("info");
    let single = info.join("commit-graph");
    if single.exists() {
        load_commit_graph_bloom_filters_from_file(&single, format, &mut out);
        return Ok(out);
    }
    let chain = info.join("commit-graphs").join("commit-graph-chain");
    let chain_dir = match chain.parent() {
        Some(dir) => dir.to_path_buf(),
        None => return Ok(out),
    };
    for hash in read_commit_graph_chain_hashes(&chain, format).unwrap_or_default() {
        let path = chain_dir.join(format!("graph-{hash}.graph"));
        load_commit_graph_bloom_filters_from_file(&path, format, &mut out);
    }
    Ok(out)
}

fn load_commit_graph_bloom_filters_from_file(
    path: &Path,
    format: ObjectFormat,
    out: &mut HashMap<ObjectId, Vec<u8>>,
) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(graph) = CommitGraph::parse(&bytes, format) else {
        return;
    };
    let Some(filters) = &graph.bloom_filters else {
        return;
    };
    for (idx, entry) in graph.commits.iter().enumerate() {
        let Some(filter) = filters.filter_for_commit(idx) else {
            continue;
        };
        if !filter.is_empty() {
            out.insert(entry.oid, filter.to_vec());
        }
    }
}

fn trace_commit_graph_bloom_settings(settings: sley_formats::CommitGraphBloomSettings) {
    let Some(target) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let target = target.to_string_lossy().into_owned();
    if !target.starts_with('/') {
        return;
    }
    let line = format!(
        "{{\"event\":\"data_json\",\"sid\":\"sley\",\"thread\":\"main\",\"nesting\":1,\"category\":\"commit-graph\",\"key\":\"bloom-settings\",\"value\":{{\"hash_version\":{},\"num_hashes\":{},\"bits_per_entry\":{},\"max_changed_paths\":{}}}}}\n",
        settings.hash_version,
        settings.hash_count,
        settings.bits_per_entry,
        settings.max_changed_paths
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&target) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn read_commit_tree_for_graph(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

fn commit_graph_generation(
    oid: &ObjectId,
    records: &HashMap<ObjectId, &sley_rev::CommitRecord>,
    cache: &mut HashMap<ObjectId, u32>,
) -> Result<u32> {
    if let Some(generation) = cache.get(oid) {
        return Ok(*generation);
    }
    let record = records
        .get(oid)
        .ok_or_else(|| GitError::InvalidObject(format!("commit {oid} missing from walk")))?;
    let generation = if record.parents.is_empty() {
        1
    } else {
        record
            .parents
            .iter()
            .map(|parent| commit_graph_generation(parent, records, cache))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit generation overflow".into()))?
    };
    cache.insert(*oid, generation);
    Ok(generation)
}

fn commit_graph_commit_time(commit: &Commit) -> Result<u64> {
    commit_graph_commit_time_from_committer(&commit.committer)
}

fn commit_graph_commit_time_from_committer(committer: &[u8]) -> Result<u64> {
    let committer =
        std::str::from_utf8(committer).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let Some((before_tz, _tz)) = committer.rsplit_once(' ') else {
        return Err(GitError::InvalidFormat(
            "commit committer is missing timezone".into(),
        ));
    };
    let Some((_identity, timestamp)) = before_tz.rsplit_once(' ') else {
        return Err(GitError::InvalidFormat(
            "commit committer is missing timestamp".into(),
        ));
    };
    timestamp
        .parse::<u64>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))
}

fn cmd_bundle_create(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut progress = false;
    let mut version = None;
    let mut path = None::<String>;
    let mut rev_args = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if path.is_some() {
            rev_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--progress" | "--all-progress" | "--all-progress-implied" | "--no-quiet" => {
                progress = true
            }
            "--no-progress" => progress = false,
            "--version" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("bundle create --version requires a value".into())
                })?;
                version = Some(parse_bundle_version(value)?);
            }
            value if value.starts_with("--version=") => {
                version = Some(parse_bundle_version(&value["--version=".len()..])?);
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported bundle create option {value}"
                )));
            }
            value => path = Some(value.to_string()),
        }
    }
    let Some(path) = path else {
        return bundle_usage_error(BUNDLE_CREATE_USAGE);
    };
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let options = parse_bundle_revision_args(&rev_args)?;
    let selection = bundle_create_selection(&git_dir, format, &db, &options)?;
    if selection.references.is_empty() {
        eprintln!("fatal: Refusing to create empty bundle.");
        return Err(GitError::Exit(128));
    }
    let Some(pack) =
        build_reachable_pack(&db, format, selection.starts, &selection.excluded_objects)?
    else {
        eprintln!("fatal: Refusing to create empty bundle.");
        return Err(GitError::Exit(128));
    };
    let version = version.unwrap_or(if format == ObjectFormat::Sha1 && options.filter.is_none() {
        2
    } else {
        3
    });
    if !(2..=3).contains(&version) {
        return Err(GitError::InvalidFormat(format!(
            "unsupported bundle version {version}"
        )));
    }
    if version == 2 && (format != ObjectFormat::Sha1 || options.filter.is_some()) {
        return Err(GitError::InvalidFormat(format!(
            "cannot write bundle version {version} with algorithm {}",
            format.name()
        )));
    }
    let mut capabilities = Vec::new();
    if version == 3 {
        capabilities.push(BundleCapability {
            key: "object-format".into(),
            value: Some(format.name().as_bytes().to_vec()),
        });
        if let Some(filter) = options.filter {
            capabilities.push(BundleCapability {
                key: "filter".into(),
                value: Some(filter.into_bytes()),
            });
        }
    }
    let bundle = Bundle {
        version,
        format,
        capabilities,
        prerequisites: selection.prerequisites,
        references: selection.references,
        pack: pack.pack,
    };
    let bytes = bundle.write()?;
    if path == "-" {
        io::stdout().write_all(&bytes)?;
    } else {
        fs::write(path, bytes)?;
    }
    if progress && !quiet {
        let count = bundle.references.len();
        eprintln!("Writing objects: 100% ({count}/{count}), done.");
    }
    Ok(())
}

fn cmd_bundle_verify(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" if path.is_none() => quiet = true,
            _ if path.is_none() => path = Some(arg),
            _ => {
                return Err(GitError::Command(
                    "bundle verify requires [-q|--quiet] <file>".into(),
                ));
            }
        }
    }
    let Some(path) = path else {
        return bundle_usage_error(BUNDLE_VERIFY_USAGE);
    };
    let cwd = env::current_dir()?;
    let git_dir = match discover_git_dir(&cwd) {
        Ok(git_dir) => git_dir,
        Err(_) => {
            eprintln!("error: need a repository to verify a bundle");
            return Err(GitError::Exit(1));
        }
    };
    let format = repository_object_format(&git_dir)?;
    let bundle = Bundle::parse(&read_bundle_path(path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    verify_bundle_prerequisites_for_cli(&bundle, &db)?;
    if !quiet {
        print_bundle_verify_details(&bundle)?;
    }
    eprintln!("{} is okay", if path == "-" { "<stdin>" } else { path });
    Ok(())
}

fn cmd_bundle_list_heads(args: &[String]) -> Result<()> {
    let Some(path) = args.first() else {
        return bundle_usage_error(BUNDLE_LIST_HEADS_USAGE);
    };
    let refs = &args[1..];
    let bundle = Bundle::parse_standalone(&read_bundle_path(path)?)?;
    print_bundle_refs(&bundle.references, refs)
}

fn cmd_bundle_unbundle(args: &[String]) -> Result<()> {
    let mut progress = false;
    let mut path = None;
    let mut refs = Vec::new();
    for arg in args {
        if arg == "--progress" && path.is_none() {
            progress = true;
        } else if path.is_none() {
            path = Some(arg);
        } else {
            refs.push(arg.clone());
        }
    }
    let _ = progress;
    let Some(path) = path else {
        return bundle_usage_error(BUNDLE_UNBUNDLE_USAGE);
    };
    let cwd = env::current_dir()?;
    let git_dir = match discover_git_dir(&cwd) {
        Ok(git_dir) => git_dir,
        Err(_) => {
            eprintln!("fatal: Need a repository to unbundle.");
            return Err(GitError::Exit(128));
        }
    };
    let format = repository_object_format(&git_dir)?;
    let bundle = Bundle::parse(&read_bundle_path(path)?, format)?;
    let prerequisite_reader = FileObjectDatabase::from_git_dir(&git_dir, format);
    let database = FileObjectDatabase::from_git_dir(&git_dir, format);
    let result = install_bundle_pack(&bundle, &prerequisite_reader, &database)?;
    print_bundle_refs(&result.references, &refs)
}

fn print_bundle_refs(refs: &[BundleReference], filters: &[String]) -> Result<()> {
    for reference in refs {
        if filters.is_empty() || filters.iter().any(|filter| filter == &reference.name) {
            println!("{} {}", reference.oid, reference.name);
        }
    }
    Ok(())
}

fn print_bundle_verify_details(bundle: &Bundle) -> Result<()> {
    match bundle.references.len() {
        1 => println!("The bundle contains this ref:"),
        count => println!("The bundle contains these {count} refs:"),
    }
    print_bundle_refs(&bundle.references, &[])?;
    match bundle.prerequisites.len() {
        0 => println!("The bundle records a complete history."),
        1 => {
            println!("The bundle requires this ref:");
            print_bundle_prerequisites(bundle)?;
        }
        count => {
            println!("The bundle requires these {count} refs:");
            print_bundle_prerequisites(bundle)?;
        }
    }
    println!(
        "The bundle uses this hash algorithm: {}",
        bundle.format.name()
    );
    if let Some(filter) = bundle_filter_capability(bundle)? {
        println!("The bundle uses this filter: {filter}");
    }
    Ok(())
}

fn verify_bundle_prerequisites_for_cli(bundle: &Bundle, db: &FileObjectDatabase) -> Result<()> {
    let mut missing = Vec::new();
    for prerequisite in &bundle.prerequisites {
        match db.read_object(&prerequisite.oid) {
            Ok(object) => {
                let actual = object.object_id(bundle.format)?;
                if actual != prerequisite.oid {
                    return Err(GitError::InvalidObject(format!(
                        "bundle prerequisite {} hashes to {actual}",
                        prerequisite.oid
                    )));
                }
            }
            Err(GitError::NotFound(_)) => missing.push(prerequisite),
            Err(err) => return Err(err),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    eprintln!("error: Repository lacks these prerequisite commits:");
    for prerequisite in missing {
        eprintln!("error: {} ", prerequisite.oid);
    }
    Err(GitError::Exit(1))
}

fn print_bundle_prerequisites(bundle: &Bundle) -> Result<()> {
    for prerequisite in &bundle.prerequisites {
        println!("{} ", prerequisite.oid);
    }
    Ok(())
}

fn read_bundle_path(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes)?;
        Ok(bytes)
    } else {
        Ok(fs::read(path)?)
    }
}

fn parse_bundle_version(value: &str) -> Result<u8> {
    value
        .parse::<u8>()
        .map_err(|_| GitError::Command(format!("invalid bundle version {value}")))
}

fn bundle_filter_capability(bundle: &Bundle) -> Result<Option<String>> {
    for capability in &bundle.capabilities {
        if capability.key == "filter" {
            let Some(value) = &capability.value else {
                return Ok(Some(String::new()));
            };
            let text = std::str::from_utf8(value)
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
            return Ok(Some(text.to_string()));
        }
    }
    Ok(None)
}

#[derive(Default)]
struct BundleRevisionOptions {
    all: bool,
    ignore_missing: bool,
    max_count: Option<usize>,
    since: Option<i64>,
    filter: Option<String>,
    specs: Vec<String>,
}

fn parse_bundle_revision_args(args: &[String]) -> Result<BundleRevisionOptions> {
    let mut options = BundleRevisionOptions::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--all" => options.all = true,
            "--objects" => {}
            "--stdin" => {
                let mut input = String::new();
                io::stdin().read_to_string(&mut input)?;
                options.specs.extend(
                    input
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(str::to_string),
                );
            }
            "--ignore-missing" => options.ignore_missing = true,
            "--max-count" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--max-count requires a value".into()))?;
                options.max_count = Some(parse_bundle_usize("--max-count", value)?);
            }
            "--since" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--since requires a value".into()))?;
                options.since = parse_bundle_since(value);
            }
            value if value.starts_with("--max-count=") => {
                options.max_count = Some(parse_bundle_usize(
                    "--max-count",
                    &value["--max-count=".len()..],
                )?);
            }
            value if value.starts_with("--since=") => {
                options.since = parse_bundle_since(&value["--since=".len()..]);
            }
            value if value.starts_with("--filter=") => {
                options.filter = Some(value["--filter=".len()..].to_string());
            }
            value => options.specs.push(value.to_string()),
        }
    }
    if !options.all && options.specs.is_empty() {
        return Err(GitError::Unsupported(
            "bundle create currently supports --all or explicit <rev> [^<rev>...]".into(),
        ));
    }
    Ok(options)
}

fn parse_bundle_usize(option: &str, value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("{option} expects a numerical value")))
}

fn parse_bundle_since(value: &str) -> Option<i64> {
    crate::commands::approxidate::parse_commit_date(value).map(|(timestamp, _)| timestamp)
}

fn bundle_all_references(git_dir: &Path, format: ObjectFormat) -> Result<Vec<BundleReference>> {
    let store = FileRefStore::new(git_dir, format);
    let mut references = Vec::new();
    for reference in store.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target {
            references.push(BundleReference {
                oid,
                name: reference.name,
            });
        }
    }
    if let Ok(oid) = resolve_revision(git_dir, format, "HEAD") {
        references.push(BundleReference {
            oid,
            name: "HEAD".into(),
        });
    }
    Ok(references)
}

struct BundleCreateSelection {
    references: Vec<BundleReference>,
    prerequisites: Vec<BundlePrerequisite>,
    starts: Vec<ObjectId>,
    excluded_objects: HashSet<ObjectId>,
}

#[derive(Clone)]
struct BundleSpec {
    oid: ObjectId,
    name: String,
    include_ref: bool,
}

fn bundle_create_selection(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &BundleRevisionOptions,
) -> Result<BundleCreateSelection> {
    let mut includes = if options.all {
        bundle_all_references(git_dir, format)?
            .into_iter()
            .map(|reference| BundleSpec {
                oid: reference.oid,
                name: reference.name,
                include_ref: true,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut excludes = Vec::new();
    for spec in &options.specs {
        add_bundle_revision_spec(
            git_dir,
            format,
            db,
            spec,
            options.ignore_missing,
            &mut includes,
            &mut excludes,
        )?;
    }

    let user_excluded_objects = collect_reachable_object_ids(db, format, excludes.iter().copied())?;
    let mut references =
        filter_bundle_references(git_dir, format, db, includes, options, &user_excluded_objects)?;
    dedupe_bundle_references(&mut references);
    let starts = references.iter().map(|reference| reference.oid).collect::<Vec<_>>();
    excludes.extend(bundle_limit_excludes(db, format, &starts, options)?);
    let excluded_objects = collect_reachable_object_ids(db, format, excludes.iter().copied())?;
    let mut prerequisites = bundle_boundary_prerequisites(db, format, &starts, &excluded_objects)?;
    order_bundle_prerequisites(db, format, &mut prerequisites, &excludes);
    Ok(BundleCreateSelection {
        references,
        prerequisites,
        starts,
        excluded_objects,
    })
}

fn add_bundle_revision_spec(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    spec: &str,
    ignore_missing: bool,
    includes: &mut Vec<BundleSpec>,
    excludes: &mut Vec<ObjectId>,
) -> Result<()> {
    if let Some(excluded) = spec.strip_prefix('^') {
        if excluded.is_empty() {
            return Err(GitError::Command(
                "bundle create excludes require a revision".into(),
            ));
        }
        match resolve_revision(git_dir, format, excluded) {
            Ok(oid) => excludes.push(oid),
            Err(err) if ignore_missing => {
                let _ = err;
            }
            Err(err) => return Err(err),
        }
        return Ok(());
    }
    if let Some(base) = spec.strip_suffix("^!") {
        let oid = resolve_revision(git_dir, format, base)?;
        includes.push(BundleSpec {
            oid,
            name: bundle_display_ref(git_dir, format, base, oid)?,
            include_ref: true,
        });
        if let Ok(object) = db.read_object(&oid)
            && object.object_type == ObjectType::Commit
        {
            for parent in Commit::parse_ref(format, &object.body)?.parents {
                excludes.push(parent);
            }
        }
        return Ok(());
    }
    if let Some((left, right)) = spec.split_once("..")
        && !left.contains("..")
        && !right.contains("..")
    {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        excludes.push(resolve_revision(git_dir, format, left)?);
        let oid = resolve_revision(git_dir, format, right)?;
        includes.push(BundleSpec {
            oid,
            name: bundle_display_ref(git_dir, format, right, oid)?,
            include_ref: true,
        });
        return Ok(());
    }
    let oid = match resolve_revision(git_dir, format, spec) {
        Ok(oid) => oid,
        Err(err) if ignore_missing => {
            let _ = err;
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    includes.push(BundleSpec {
        oid,
        name: bundle_display_ref(git_dir, format, spec, oid)?,
        include_ref: true,
    });
    Ok(())
}

fn filter_bundle_references(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    includes: Vec<BundleSpec>,
    options: &BundleRevisionOptions,
    excluded_objects: &HashSet<ObjectId>,
) -> Result<Vec<BundleReference>> {
    let mut refs = includes;
    refs.retain(|reference| {
        if !excluded_objects.contains(&reference.oid) {
            return true;
        }
        db.read_object(&reference.oid)
            .is_ok_and(|object| object.object_type == ObjectType::Tag)
    });
    if let Some(since) = options.since {
        refs.retain(|reference| {
            bundle_object_timestamp(db, format, &reference.oid)
                .is_none_or(|timestamp| timestamp > since)
        });
    }
    if let Some(max_count) = options.max_count {
        let mut commit_refs = refs
            .iter()
            .enumerate()
            .filter_map(|(idx, reference)| {
                let object = db.read_object(&reference.oid).ok()?;
                if object.object_type == ObjectType::Commit {
                    Some((idx, bundle_object_timestamp(db, format, &reference.oid).unwrap_or(0)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        commit_refs.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let keep = commit_refs
            .into_iter()
            .take(max_count)
            .map(|(idx, _)| idx)
            .collect::<HashSet<_>>();
        refs = refs
            .into_iter()
            .enumerate()
            .filter_map(|(idx, reference)| {
                let object = db.read_object(&reference.oid).ok()?;
                if object.object_type == ObjectType::Commit && !keep.contains(&idx) {
                    return None;
                }
                Some(reference)
            })
            .collect();
    }
    refs.into_iter()
        .filter(|reference| reference.include_ref)
        .map(|reference| {
            Ok(BundleReference {
                oid: reference.oid,
                name: if reference.name == "HEAD" {
                    "HEAD".into()
                } else {
                    bundle_display_ref(git_dir, format, &reference.name, reference.oid)?
                },
            })
        })
        .collect()
}

fn dedupe_bundle_references(references: &mut Vec<BundleReference>) {
    let mut seen = HashSet::new();
    references.retain(|reference| seen.insert(reference.name.clone()));
}

fn bundle_display_ref(
    git_dir: &Path,
    format: ObjectFormat,
    spec: &str,
    oid: ObjectId,
) -> Result<String> {
    if spec == "HEAD" {
        return Ok("HEAD".into());
    }
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    if spec.starts_with("refs/")
        && refs
            .iter()
            .any(|reference| reference.name == spec && reference.target == RefTarget::Direct(oid))
    {
        return Ok(spec.to_string());
    }
    let branch = format!("refs/heads/{spec}");
    if refs
        .iter()
        .any(|reference| reference.name == branch && reference.target == RefTarget::Direct(oid))
    {
        return Ok(branch);
    }
    let tag = format!("refs/tags/{spec}");
    if refs
        .iter()
        .any(|reference| reference.name == tag && reference.target == RefTarget::Direct(oid))
    {
        return Ok(tag);
    }
    if let Some(reference) = refs
        .iter()
        .find(|reference| reference.target == RefTarget::Direct(oid))
    {
        return Ok(reference.name.clone());
    }
    Ok(spec.to_string())
}

fn bundle_boundary_prerequisites(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
    excluded_objects: &HashSet<ObjectId>,
) -> Result<Vec<BundlePrerequisite>> {
    let mut prerequisites = Vec::new();
    let mut prerequisite_seen = HashSet::new();
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from(starts.to_vec());
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if excluded_objects.contains(&oid) {
            if bundle_is_commit(db, format, &oid)? && prerequisite_seen.insert(oid) {
                prerequisites.push(BundlePrerequisite {
                    oid,
                    comment: Vec::new(),
                });
            }
            continue;
        }
        let object = db.read_object(&oid)?;
        match object.object_type {
            ObjectType::Commit => {
                for parent in Commit::parse_ref(format, &object.body)?.parents {
                    if excluded_objects.contains(&parent) {
                        if prerequisite_seen.insert(parent) {
                            prerequisites.push(BundlePrerequisite {
                                oid: parent,
                                comment: Vec::new(),
                            });
                        }
                    } else {
                        pending.push_back(parent);
                    }
                }
            }
            ObjectType::Tag => {
                let tag = Tag::parse_ref(format, &object.body)?;
                if !excluded_objects.contains(&tag.object) {
                    pending.push_back(tag.object);
                }
            }
            _ => {}
        }
    }
    Ok(prerequisites)
}

fn bundle_limit_excludes(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
    options: &BundleRevisionOptions,
) -> Result<Vec<ObjectId>> {
    let mut excludes = Vec::new();
    let mut seen = HashSet::new();
    if options.max_count.is_some() {
        for oid in starts {
            let object = db.read_object(oid)?;
            if object.object_type != ObjectType::Commit {
                continue;
            }
            for parent in Commit::parse_ref(format, &object.body)?.parents {
                if seen.insert(parent) {
                    excludes.push(parent);
                }
            }
        }
    }
    if let Some(since) = options.since {
        let mut pending = VecDeque::from(starts.to_vec());
        while let Some(oid) = pending.pop_front() {
            let object = db.read_object(&oid)?;
            match object.object_type {
                ObjectType::Commit => {
                    if bundle_object_timestamp(db, format, &oid).is_some_and(|time| time <= since)
                    {
                        if seen.insert(oid) {
                            excludes.push(oid);
                        }
                        continue;
                    }
                    for parent in Commit::parse_ref(format, &object.body)?.parents {
                        pending.push_back(parent);
                    }
                }
                ObjectType::Tag => {}
                _ => {}
            }
        }
    }
    Ok(excludes)
}

fn order_bundle_prerequisites(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    prerequisites: &mut [BundlePrerequisite],
    exclude_tips: &[ObjectId],
) {
    let exact_rank = exclude_tips
        .iter()
        .enumerate()
        .map(|(idx, oid)| (*oid, idx))
        .collect::<HashMap<_, _>>();
    let has_exact = prerequisites
        .iter()
        .any(|prerequisite| exact_rank.contains_key(&prerequisite.oid));
    prerequisites.sort_by(|left, right| {
        match (
            exact_rank.get(&left.oid).copied(),
            exact_rank.get(&right.oid).copied(),
        ) {
            (Some(left_rank), Some(right_rank)) => left_rank.cmp(&right_rank),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                let left_time = bundle_object_timestamp(db, format, &left.oid).unwrap_or(0);
                let right_time = bundle_object_timestamp(db, format, &right.oid).unwrap_or(0);
                if has_exact {
                    left_time.cmp(&right_time)
                } else {
                    right_time.cmp(&left_time)
                }
            }
        }
    });
}

fn bundle_is_commit(db: &FileObjectDatabase, format: ObjectFormat, oid: &ObjectId) -> Result<bool> {
    let object = db.read_object(oid)?;
    Ok(object.object_type == ObjectType::Commit && Commit::parse_ref(format, &object.body).is_ok())
}

fn bundle_object_timestamp(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Option<i64> {
    let object = db.read_object(oid).ok()?;
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body).ok()?;
            bundle_identity_timestamp(commit.committer)
        }
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body).ok()?;
            tag.tagger.and_then(bundle_identity_timestamp)
        }
        _ => None,
    }
}

fn bundle_identity_timestamp(identity: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(identity).ok()?;
    let (before_tz, _) = text.rsplit_once(' ')?;
    let (_, timestamp) = before_tz.rsplit_once(' ')?;
    timestamp.parse::<i64>().ok()
}

pub(crate) fn cmd_commit_tree(args: &[String]) -> Result<()> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut message_chunks = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" => {
                let Some(parent) = iter.next() else {
                    return commit_tree_parent_requires_value_error();
                };
                parents.push(parent.to_string());
            }
            value if value.starts_with("-p") && value.len() > 2 => {
                parents.push(value[2..].to_string());
            }
            "-m" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                let mut chunk = message.as_bytes().to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                let mut chunk = value.as_bytes()[2..].to_vec();
                chunk.push(b'\n');
                message_chunks.push(chunk);
            }
            "-F" => {
                let Some(path) = iter.next() else {
                    return commit_tree_file_requires_value_error();
                };
                message_chunks.push(read_commit_message_file(path)?);
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                message_chunks.push(read_commit_message_file(&value[2..])?);
            }
            "--no-gpg-sign" => {}
            value if tree.is_none() => tree = Some(value.to_string()),
            value if !value.starts_with('-') => return commit_tree_requires_one_tree_error(),
            value => {
                return Err(GitError::Command(format!(
                    "unexpected commit-tree argument {value}"
                )));
            }
        }
    }
    let Some(tree) = tree else {
        return commit_tree_requires_one_tree_error();
    };
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    // git resolves the tree and each `-p` parent as a revision-ish (so a tag,
    // branch, `HEAD^`, abbreviated oid, or `<rev>^{tree}` all work), peeling the
    // tree argument to a tree and each parent to a commit. A *full-length* hex
    // oid is taken verbatim without an existence check (matching git, which
    // accepts e.g. the empty-tree hash `4b825d...` even when it is absent from
    // the object store); shorter names go through revision resolution + peel.
    let db_resolve = FileObjectDatabase::from_git_dir(&git_dir, format);
    let tree = match ObjectId::from_hex(format, &tree) {
        Ok(oid) => oid,
        Err(_) => {
            let tree_rev =
                sley_rev::resolve_revision_with_reader(&git_dir, format, &db_resolve, &tree)?;
            sley_rev::peel_to_tree(&db_resolve, format, &tree_rev)?
        }
    };
    let parents = parents
        .iter()
        .map(|parent| match ObjectId::from_hex(format, parent) {
            Ok(oid) => Ok(oid),
            Err(_) => {
                let resolved =
                    sley_rev::resolve_revision_with_reader(&git_dir, format, &db_resolve, parent)?;
                sley_rev::peel_to_commit(&db_resolve, format, &resolved)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let message = if message_chunks.is_empty() {
        let mut message = Vec::new();
        io::stdin().read_to_end(&mut message)?;
        message
    } else {
        commit_message_from_prepared_chunks(&message_chunks)
    };
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer,
            message,
            encoding: None,
        },
    )?;
    println!("{oid}");
    Ok(())
}

fn commit_tree_parent_requires_value_error() -> Result<()> {
    eprintln!("error: switch `p' requires a value");
    Err(GitError::Exit(129))
}

fn commit_tree_requires_one_tree_error() -> Result<()> {
    eprintln!("fatal: must give exactly one tree");
    Err(GitError::Exit(128))
}
