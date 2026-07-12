//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use crate::*;
use sley::plumbing::{sley_refs, sley_rev, sley_worktree};

use super::commit_graph::commit_graph_commit_time_from_committer;

struct ArchiveExtraFile {
    path: Vec<u8>,
    content: Vec<u8>,
    mode: u32,
}

pub(crate) fn cmd_archive(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut format_name: Option<String> = None;
    let mut prefix = Vec::new();
    let mut output: Option<String> = None;
    let mut remote: Option<String> = None;
    let mut treeish = None;
    let mut pathspecs = Vec::new();
    let mut list = false;
    let mut verbose = false;
    let mut worktree_attributes = false;
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
            "-v" | "--verbose" => verbose = true,
            "--worktree-attributes" => worktree_attributes = true,
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
                        .ok_or_else(|| {
                            GitError::Command("archive --mtime requires a value".into())
                        })?
                        .clone(),
                );
            }
            "--remote" => {
                remote = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("archive --remote requires a value".into())
                        })?
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
            value if value.starts_with("--remote=") => {
                remote = Some(value["--remote=".len()..].to_string());
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
        let config = archive_config_for_list(
            remote.as_deref(),
            cli_session.cwd(),
            cli_session.git_dir().ok().as_deref(),
        )
        .unwrap_or_default();
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        for name in archive_list_formats(&config, remote.is_some()) {
            writeln!(lock, "{name}")?;
        }
        lock.flush()?;
        return Ok(());
    }

    let treeish = treeish.ok_or_else(|| GitError::Command("archive requires a tree-ish".into()))?;
    let cwd = cli_session.cwd().to_path_buf();
    let local_git_dir = cli_session.git_dir().ok();
    let git_dir = if let Some(remote) = remote.as_deref() {
        archive_remote_git_dir(remote, &cwd, local_git_dir.as_deref())?
    } else {
        cli_session.git_dir()?
    };
    let format = repository_object_format(&git_dir)?;
    let db =
        crate::repository::open_object_database(&git_dir, format, cli_session.replace_objects())?;
    // A bare repo has no worktree, so the "current prefix" is empty (we are at
    // the repository root); upstream `git archive` works in a bare repo.
    let current_prefix = if remote.is_some() {
        Vec::new()
    } else {
        match sley_worktree::worktree_root_for_git_dir(&git_dir)? {
            Some(_) => worktree_prefix(cli_session, &cwd, &git_dir)?.into_bytes(),
            None => Vec::new(),
        }
    };
    let pathspecs = match archive_pathspecs_for_current_prefix(
        &current_prefix,
        pathspecs,
        effective_pathspec_flags(cli_session),
    ) {
        Ok(pathspecs) => pathspecs,
        Err(GitError::InvalidPath(message))
            if message.contains("outside the current directory") =>
        {
            eprintln!("fatal: {message}");
            return Err(GitError::Exit(128));
        }
        Err(err) => return Err(err),
    };
    let oid = sley_rev::RevisionResolver::new(&git_dir, format, &db).resolve(&treeish)?;
    let config = read_repo_config(&git_dir)?;
    if remote.is_some()
        && !archive_remote_object_allowed(&git_dir, &db, format, &oid, &treeish, &config)?
    {
        return Err(GitError::Exit(128));
    }
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
    // git dir locates `info/attributes`. `--worktree-attributes` switches this
    // to live worktree attributes, or `attr.tree` for bare repositories. The
    // remaining conversion gap is the lower-level `ident` filter.
    // Format resolution: explicit `--format`, else inferred from the `--output`
    // filename extension, else `tar` (upstream `archive_format_from_filename`).
    let format_name = match format_name {
        Some(name) => name,
        None => output
            .as_deref()
            .and_then(|name| {
                archive_format_from_filename_with_config(name, &config, remote.is_some())
            })
            .unwrap_or_else(|| "tar".to_string()),
    };
    let filter_command = archive_filter_command(&config, &format_name, remote.is_some())?;
    let archive_format = match format_name.as_str() {
        "tar" => ArchiveFormatKind::Tar,
        "zip" => ArchiveFormatKind::Zip,
        // `tgz` and `tar.gz` are the internal-gzip tar filter (git's
        // `internal_gzip_command`): the tar stream wrapped in gzip.
        "tgz" | "tar.gz" if filter_command.is_none() => ArchiveFormatKind::TarGz,
        _ if filter_command.is_some() => ArchiveFormatKind::TarFilter,
        other => {
            return Err(GitError::Command(format!(
                "archive does not support --format={other}"
            )));
        }
    };
    let worktree_root = sley_worktree::worktree_root_for_git_dir(&git_dir)?;
    let attr_root = worktree_root
        .clone()
        .unwrap_or_else(|| git_dir.to_path_buf());
    let archive_process_filter_metadata =
        archive_process_filter_metadata(&git_dir, format, &treeish, &oid);
    let archive_attr_tree_oid = if worktree_attributes && worktree_root.is_none() {
        archive_attr_tree_oid(&git_dir, &db, format, &config)?
    } else {
        None
    };
    let mut convert = if worktree_attributes {
        if let Some(worktree_root) = &worktree_root {
            sley_archive::ArchiveConvert::from_worktree(worktree_root, &config)?
        } else if let Some(attr_tree_oid) = &archive_attr_tree_oid {
            sley_archive::ArchiveConvert::from_tree(
                &attr_root,
                &git_dir,
                &config,
                &db,
                format,
                attr_tree_oid,
            )?
        } else {
            sley_archive::ArchiveConvert::from_tree(
                &attr_root, &git_dir, &config, &db, format, &tree_oid,
            )?
        }
    } else {
        sley_archive::ArchiveConvert::from_tree(
            &attr_root, &git_dir, &config, &db, format, &tree_oid,
        )?
    };
    convert = convert.with_process_filter_metadata(archive_process_filter_metadata);
    // export-subst only runs when archiving a commit (git sets `args->convert`
    // only when a commit is available).
    if let Some(record) = &commit_record {
        let describe_available = std::cell::Cell::new(true);
        let git_dir_ref = &git_dir;
        let db_ref = &db;
        let record_ref = record;
        convert = convert.with_subst(move |fmt| {
            archive_format_subst_for_commit(
                git_dir_ref,
                db_ref,
                format,
                record_ref,
                &describe_available,
                fmt,
            )
        });
    }
    // Text/binary classification for the zip backend, driven by the tree's
    // `diff` userdiff attribute (the same `entry_is_binary` upstream uses). Read
    // attributes from the archived *tree* (not the worktree). The
    // `UserdiffResolver` resolves `diff=<name>` ⇒ `diff.<name>.binary` config and
    // builtin driver flags.
    let diff_attributes = archive_diff_attributes(
        &attr_root,
        &git_dir,
        &db,
        format,
        &tree_oid,
        worktree_attributes
            .then_some(worktree_root.as_deref())
            .flatten(),
        archive_attr_tree_oid.as_ref(),
    )?;
    let userdiff =
        commands::userdiff::UserdiffResolver::with_attributes(None, Some(config.clone()));
    convert = convert
        .with_diff_binary(move |path| archive_diff_binary(&diff_attributes, &userdiff, path));

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
                verbose,
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
                verbose,
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
        ArchiveFormatKind::TarFilter => {
            let options = sley_archive::TarArchiveOptions {
                prefix,
                strip_prefix: current_prefix,
                mtime,
                commit_id,
                pathspecs,
                verbose,
            };
            let command = filter_command.expect("tar filter arm has a command");
            with_archive_writer(output, |writer| {
                let mut tar = Vec::new();
                handle_archive_result(sley_archive::write_tar_archive_full(
                    &mut tar, &db, format, &tree_oid, options, &convert, &extra,
                ))?;
                let filtered = run_archive_filter(&command, &tar)?;
                writer.write_all(&filtered)?;
                Ok(())
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
    TarFilter,
    Zip,
}

fn archive_process_filter_metadata(
    git_dir: &Path,
    format: ObjectFormat,
    treeish: &str,
    oid: &ObjectId,
) -> Vec<(String, String)> {
    let mut metadata = Vec::new();
    if let Ok(Some(refname)) =
        sley_rev::resolve_revision_symbolic_full_name(git_dir, format, treeish)
    {
        metadata.push(("ref".to_string(), refname));
    }
    metadata.push(("treeish".to_string(), oid.to_hex()));
    metadata
}

fn archive_attr_tree_oid(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Option<ObjectId>> {
    let Some(attr_tree) = config.get("attr", None, "tree") else {
        return Ok(None);
    };
    let oid = sley_rev::RevisionResolver::new(git_dir, format, db).resolve(attr_tree)?;
    Ok(Some(sley_rev::peel_to_tree(db, format, &oid)?))
}

enum ArchiveDiffAttributes {
    Tree(sley_worktree::TreeAttributes),
    Worktree(sley_worktree::StandardAttributeMatcher),
}

fn archive_diff_attributes(
    attr_root: &Path,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    worktree_root: Option<&Path>,
    attr_tree_oid: Option<&ObjectId>,
) -> Result<ArchiveDiffAttributes> {
    if let Some(worktree_root) = worktree_root {
        return Ok(ArchiveDiffAttributes::Worktree(
            sley_worktree::StandardAttributeMatcher::from_worktree_root(worktree_root)?,
        ));
    }
    Ok(ArchiveDiffAttributes::Tree(
        sley_worktree::TreeAttributes::from_tree(
            attr_root,
            git_dir,
            db,
            format,
            attr_tree_oid.unwrap_or(tree_oid),
        )?,
    ))
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
fn archive_format_from_filename_with_config(
    filename: &str,
    config: &GitConfig,
    is_remote: bool,
) -> Option<String> {
    let mut formats = archive_list_formats(config, is_remote);
    formats.sort_by_key(|name| std::cmp::Reverse(name.len()));
    formats
        .into_iter()
        .find(|name| archive_match_extension(filename, name))
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

fn archive_config_for_list(
    remote: Option<&str>,
    cwd: &Path,
    local_git_dir: Option<&Path>,
) -> Result<GitConfig> {
    let git_dir = if let Some(remote) = remote {
        archive_remote_git_dir(remote, cwd, local_git_dir)?
    } else {
        local_git_dir
            .map(Path::to_path_buf)
            .ok_or_else(|| GitError::Command("not a git repository".into()))?
    };
    read_repo_config(&git_dir)
}

fn archive_remote_git_dir(
    remote: &str,
    cwd: &Path,
    local_git_dir: Option<&Path>,
) -> Result<PathBuf> {
    let (path, base) = if archive_remote_looks_like_path(remote) {
        (PathBuf::from(remote), cwd.to_path_buf())
    } else {
        let git_dir =
            local_git_dir.ok_or_else(|| GitError::Command(format!("unknown remote: {remote}")))?;
        let config = read_repo_config(git_dir)?;
        let url = config
            .get("remote", Some(remote), "url")
            .ok_or_else(|| GitError::Command(format!("unknown remote: {remote}")))?;
        let base = sley_worktree::worktree_root_for_git_dir(git_dir)?
            .unwrap_or_else(|| git_dir.to_path_buf());
        (PathBuf::from(url), base)
    };
    let repo = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    crate::session::cli_remote_git_dir_from(&repo)
}

fn archive_remote_looks_like_path(remote: &str) -> bool {
    remote == "."
        || remote == ".."
        || remote.starts_with('/')
        || remote.starts_with("./")
        || remote.starts_with("../")
        || remote.contains('/')
}

fn archive_remote_object_allowed(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    treeish: &str,
    config: &GitConfig,
) -> Result<bool> {
    if config
        .get_bool("uploadarchive", None, "allowUnreachable")
        .unwrap_or(false)
    {
        return Ok(true);
    }
    if ObjectId::from_hex(format, treeish).is_ok() {
        return Ok(false);
    }
    let target = match sley_rev::peel_to_commit(db, format, oid) {
        Ok(target) => target,
        Err(_) => return Ok(false),
    };
    let store = sley_refs::FileRefStore::new(git_dir, format);
    let mut roots = Vec::new();
    if let Some(head) = sley_refs::resolve_ref_peeled(&store, "HEAD")? {
        roots.push(head);
    }
    for reference in store.list_refs()? {
        if let sley_refs::RefTarget::Direct(oid) = reference.target {
            roots.push(oid);
        }
    }
    if roots.is_empty() {
        return Ok(false);
    }
    Ok(sley_rev::walk_commits(db, format, roots)?
        .iter()
        .any(|record| record.oid == target))
}

fn archive_list_formats(config: &GitConfig, is_remote: bool) -> Vec<String> {
    let mut formats = vec![
        "tar".to_string(),
        "tgz".to_string(),
        "tar.gz".to_string(),
        "zip".to_string(),
    ];
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("tar") {
            continue;
        }
        let Some(name) = section.subsection.as_deref() else {
            continue;
        };
        let has_command = section
            .entries
            .iter()
            .any(|entry| entry.key.eq_ignore_ascii_case("command"));
        if !has_command {
            continue;
        }
        if is_remote
            && !config
                .get_bool("tar", Some(name), "remote")
                .unwrap_or(false)
        {
            continue;
        }
        if !formats.iter().any(|format| format == name) {
            formats.push(name.to_string());
        }
    }
    formats
}

fn archive_filter_command(
    config: &GitConfig,
    format_name: &str,
    is_remote: bool,
) -> Result<Option<String>> {
    if is_remote {
        let remote_allowed = match format_name {
            "tar" | "tgz" | "tar.gz" | "zip" => config
                .get_bool("tar", Some(format_name), "remote")
                .unwrap_or(true),
            _ => config
                .get_bool("tar", Some(format_name), "remote")
                .unwrap_or(false),
        };
        if !remote_allowed {
            return Err(GitError::Exit(128));
        }
    }
    let Some(command) = config.get("tar", Some(format_name), "command") else {
        return Ok(None);
    };
    if command.is_empty() {
        eprintln!("fatal: empty tar filter command for '{format_name}'");
        return Err(GitError::Exit(128));
    }
    Ok(Some(command.to_string()))
}

fn run_archive_filter(command: &str, input: &[u8]) -> Result<Vec<u8>> {
    let input_path = env::temp_dir().join(format!(
        "sley-archive-filter-{}-{}",
        std::process::id(),
        current_unix_seconds()
    ));
    {
        let mut file = fs::File::create(&input_path)?;
        file.write_all(input)?;
    }
    let input_file = fs::File::open(&input_path)?;
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::from(input_file))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Io(err.to_string()))?;
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Io(err.to_string()))?;
    let _ = fs::remove_file(&input_path);
    if !output.status.success() {
        return Err(GitError::Exit(output.status.code().unwrap_or(128)));
    }
    Ok(output.stdout)
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
    #[cfg(unix)]
    let raw_mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    };
    #[cfg(not(unix))]
    let raw_mode: u32 = 0;
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
    attributes: &ArchiveDiffAttributes,
    userdiff: &commands::userdiff::UserdiffResolver,
    path: &[u8],
) -> Option<bool> {
    match archive_diff_attribute_for_path(attributes, path) {
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

fn archive_diff_attribute_for_path(
    attributes: &ArchiveDiffAttributes,
    path: &[u8],
) -> Option<sley_worktree::AttributeState> {
    match attributes {
        ArchiveDiffAttributes::Tree(attributes) => attributes.diff_attribute_for_path(path),
        ArchiveDiffAttributes::Worktree(attributes) => attributes
            .attributes_for_path(path, &[b"diff".to_vec()], false)
            .into_iter()
            .next()
            .and_then(|check| check.state),
    }
}

fn archive_format_subst_for_commit(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    describe_available: &std::cell::Cell<bool>,
    fmt: &[u8],
) -> Result<Vec<u8>> {
    let describe_ctx = CliLogDescribeContext {
        git_dir,
        db,
        format,
    };
    let describe_adapter = CliLogDescribeAdapter(&describe_ctx);
    let Some(first) = archive_find_describe_atom(fmt, 0) else {
        return archive_render_commit_format(record, fmt, None);
    };
    let mut out = Vec::with_capacity(fmt.len());
    let mut cursor = 0;
    let mut next = Some(first);
    while let Some((start, end)) = next {
        out.extend(archive_render_commit_format(
            record,
            &fmt[cursor..start],
            None,
        )?);
        if describe_available.replace(false) {
            out.extend(archive_render_commit_format(
                record,
                &fmt[start..end],
                Some(&describe_adapter as &dyn LogDescribeLookup),
            )?);
        } else {
            out.extend_from_slice(&fmt[start..end]);
        }
        cursor = end;
        next = archive_find_describe_atom(fmt, cursor);
    }
    out.extend(archive_render_commit_format(record, &fmt[cursor..], None)?);
    Ok(out)
}

fn archive_render_commit_format(
    record: &sley_rev::CommitRecord,
    fmt: &[u8],
    describe: Option<&dyn LogDescribeLookup>,
) -> Result<Vec<u8>> {
    let fmt = String::from_utf8_lossy(fmt);
    let compiled = CompiledLogFormat::compile(&fmt, LogFormatDialect::Log)?;
    let decorations = std::collections::HashMap::new();
    let date_mode = DateMode::Default;
    let mailmap = commands::utility::Mailmap::default();
    let context = LogFormatContext {
        abbrev_len: Some(7),
        decorations: &decorations,
        marker: '>',
        dialect: LogFormatDialect::Log,
        source: None,
        date_mode: &date_mode,
        source_oid: None,
        describe,
        signature: None,
        color: false,
        output_encoding: "UTF-8",
        mailmap: &CliMailmapAdapter(&mailmap),
        use_mailmap: false,
    };
    let mut out = Vec::with_capacity(compiled.estimated_line_capacity());
    emit_compiled_log_format(
        record,
        &compiled,
        &context,
        &mut out,
        0..compiled.tokens.len(),
    )?;
    Ok(out)
}

fn archive_find_describe_atom(fmt: &[u8], mut offset: usize) -> Option<(usize, usize)> {
    let marker = b"%(describe";
    while offset < fmt.len() {
        let relative = fmt[offset..]
            .windows(marker.len())
            .position(|window| window == marker)?;
        let start = offset + relative;
        let after_marker = start + marker.len();
        match fmt.get(after_marker).copied() {
            Some(b')') => return Some((start, after_marker + 1)),
            Some(b':') => {
                let rest = &fmt[after_marker + 1..];
                let close = rest.iter().position(|byte| *byte == b')')?;
                return Some((start, after_marker + 1 + close + 1));
            }
            _ => offset = after_marker,
        }
    }
    None
}

fn handle_archive_result(result: Result<()>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(GitError::InvalidPath(message)) if message.starts_with("pathspec ") => {
            eprintln!("fatal: {message}");
            Err(GitError::Exit(128))
        }
        Err(GitError::InvalidPath(message))
            if message.contains("outside the current directory") =>
        {
            eprintln!("fatal: {message}");
            Err(GitError::Exit(128))
        }
        Err(err) => Err(err),
    }
}

fn archive_pathspecs_for_current_prefix(
    current_prefix: &[u8],
    pathspecs: Vec<Vec<u8>>,
    magic: sley_worktree::PathspecMatchMagic,
) -> Result<Vec<Vec<u8>>> {
    if current_prefix.is_empty() {
        return Ok(pathspecs);
    }
    if pathspecs.is_empty() {
        return Ok(vec![
            current_prefix
                .strip_suffix(b"/")
                .unwrap_or(current_prefix)
                .to_vec(),
        ]);
    }
    let current_components = archive_path_components(current_prefix);
    let mut have_include = false;
    let mut normalized = pathspecs
        .into_iter()
        .map(|pathspec| {
            if pathspec.starts_with(b":") {
                let normalized = archive_normalize_magic_pathspec(current_prefix, &pathspec)?;
                let element =
                    sley_pathspec::PathspecElement::parse(&normalized, magic).map_err(|err| {
                        GitError::InvalidPath(format!("invalid archive pathspec: {err}"))
                    })?;
                have_include |= !element.is_exclude();
                return Ok(normalized);
            }
            let mut components = current_components.clone();
            for component in pathspec.split(|byte| *byte == b'/') {
                match component {
                    b"" | b"." => {}
                    b".." => {
                        components.pop().ok_or_else(|| {
                            GitError::InvalidPath(
                                "pathspec is outside the current directory".into(),
                            )
                        })?;
                    }
                    other => components.push(other.to_vec()),
                }
            }
            if !components.starts_with(&current_components) {
                return Err(GitError::InvalidPath(
                    "pathspec is outside the current directory".into(),
                ));
            }
            have_include = true;
            Ok(components.join(&b'/'))
        })
        .collect::<Result<Vec<_>>>()?;
    if !have_include && normalized.iter().all(|pathspec| pathspec.starts_with(b":")) {
        normalized.insert(
            0,
            current_prefix
                .strip_suffix(b"/")
                .unwrap_or(current_prefix)
                .to_vec(),
        );
    }
    Ok(normalized)
}

fn archive_normalize_magic_pathspec(current_prefix: &[u8], pathspec: &[u8]) -> Result<Vec<u8>> {
    let raw = String::from_utf8_lossy(pathspec);
    let (magic, pattern, top) = split_archive_pathspec_magic_prefix(&raw);
    let base = if top { b"".as_slice() } else { current_prefix };
    let normalized = sley_pathspec::normalize_ls_files_pathspec(base, pattern)?;
    let mut out = magic.as_bytes().to_vec();
    out.extend_from_slice(&normalized);
    Ok(out)
}

fn split_archive_pathspec_magic_prefix(raw: &str) -> (&str, &str, bool) {
    if let Some(after_open) = raw.strip_prefix(":(")
        && let Some(close) = after_open.find(')')
    {
        let magic_end = 2 + close + 1;
        let magic = &raw[..magic_end];
        let body = &after_open[..close];
        let top = body.split(',').any(|word| word == "top");
        return (magic, &raw[magic_end..], top);
    }
    let bytes = raw.as_bytes();
    let mut idx = 1;
    let mut top = false;
    while idx < bytes.len() {
        match bytes[idx] {
            b'!' | b'^' => idx += 1,
            b'/' => {
                top = true;
                idx += 1;
            }
            _ => break,
        }
    }
    (&raw[..idx], &raw[idx..], top)
}

fn archive_path_components(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .map(Vec::from)
        .collect()
}
