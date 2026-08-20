//! `git hash-object`: frame bytes as Git objects, optionally write them, and
//! apply the same worktree-to-blob conversions that Git uses for path-aware
//! hashing.

use sley::plumbing::sley_worktree;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use sley::GitConfig;
use sley::plumbing::sley_object::ObjectType;
use sley::{GitError, ObjectFormat, Repository, Result};

use super::args::{
    GitArgCursor, LongOption, Terminator, option_takes_no_value, switch_requires_value, usage_error,
};
use crate::*;

pub(crate) fn cmd_hash_object(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    HashObjectInvocation::parse(args)?.execute(cli_session)
}

struct HashObjectInvocation {
    object_type: ObjectType,
    format: ObjectFormat,
    explicit_format: bool,
    read_stdin: bool,
    read_stdin_paths: bool,
    allow_no_input: bool,
    filters: HashObjectFilterPolicy,
    literally: bool,
    write: bool,
    paths: Vec<PathBuf>,
}

impl HashObjectInvocation {
    fn parse(args: &[String]) -> Result<Self> {
        let mut invocation = Self {
            object_type: ObjectType::Blob,
            format: ObjectFormat::Sha1,
            explicit_format: false,
            read_stdin: false,
            read_stdin_paths: false,
            allow_no_input: false,
            filters: HashObjectFilterPolicy::Enabled {
                forced: false,
                path: None,
            },
            literally: false,
            write: false,
            paths: Vec::new(),
        };
        let mut positional_only = false;
        let mut args = GitArgCursor::new(args);
        while let Some(arg) = args.next() {
            if positional_only {
                invocation.paths.push(PathBuf::from(arg));
                continue;
            }
            if Terminator::is(arg) {
                positional_only = true;
                invocation.allow_no_input = true;
                continue;
            }
            if let Some(option) = LongOption::parse(arg) {
                if let Some(negated) = option.negated() {
                    match negated.name() {
                        "stdin" => {
                            if negated.has_value() {
                                return option_takes_no_value(negated.option_name());
                            }
                            invocation.read_stdin = false;
                            invocation.allow_no_input = true;
                            continue;
                        }
                        "stdin-paths" => {
                            if negated.has_value() {
                                return option_takes_no_value(negated.option_name());
                            }
                            invocation.read_stdin_paths = false;
                            invocation.allow_no_input = true;
                            continue;
                        }
                        "filters" => {
                            if negated.has_value() {
                                return option_takes_no_value(negated.option_name());
                            }
                            invocation.filters = HashObjectFilterPolicy::Disabled {
                                path: invocation.filters.path().map(PathBuf::from),
                            };
                            continue;
                        }
                        "literally" => {
                            if negated.has_value() {
                                return option_takes_no_value(negated.option_name());
                            }
                            invocation.literally = false;
                            continue;
                        }
                        "path" => {
                            if negated.has_value() {
                                return option_takes_no_value(negated.option_name());
                            }
                            invocation.filters.clear_path();
                            continue;
                        }
                        _ => {}
                    }
                }
                if let Some(path) =
                    args.resolve_value_for(option, "path", || switch_requires_value("path"))?
                {
                    invocation.filters.set_path(PathBuf::from(path.value()));
                    continue;
                }
                if let Some(value) = args.resolve_value_for(option, "object-format", || {
                    switch_requires_value("object-format")
                })? {
                    invocation.set_explicit_format(value.value())?;
                    continue;
                }
                match option.name() {
                    "stdin" => {
                        if option.has_value() {
                            return option_takes_no_value("stdin");
                        }
                        invocation.enable_stdin()?;
                    }
                    "no-stdin" => {
                        if option.has_value() {
                            return option_takes_no_value("no-stdin");
                        }
                        invocation.read_stdin = false;
                        invocation.allow_no_input = true;
                    }
                    "stdin-paths" => {
                        if option.has_value() {
                            return option_takes_no_value("stdin-paths");
                        }
                        invocation.enable_stdin_paths()?;
                    }
                    "no-stdin-paths" => {
                        if option.has_value() {
                            return option_takes_no_value("no-stdin-paths");
                        }
                        invocation.read_stdin_paths = false;
                        invocation.allow_no_input = true;
                    }
                    "filters" => {
                        if option.has_value() {
                            return option_takes_no_value("no-no-filters");
                        }
                        invocation.filters = HashObjectFilterPolicy::Enabled {
                            forced: true,
                            path: invocation.filters.path().map(PathBuf::from),
                        };
                    }
                    "no-filters" => {
                        if option.has_value() {
                            return option_takes_no_value("no-filters");
                        }
                        invocation.filters = HashObjectFilterPolicy::Disabled {
                            path: invocation.filters.path().map(PathBuf::from),
                        };
                    }
                    "literally" => {
                        if option.has_value() {
                            return option_takes_no_value("literally");
                        }
                        invocation.literally = true;
                    }
                    "no-literally" => {
                        if option.has_value() {
                            return option_takes_no_value("no-literally");
                        }
                        invocation.literally = false;
                    }
                    "no-path" => {
                        if option.has_value() {
                            return option_takes_no_value("no-path");
                        }
                        invocation.filters.clear_path();
                    }
                    "path" => {
                        let path = match option.value() {
                            Some(path) => path,
                            None => args.next_required_value(|| switch_requires_value("path"))?,
                        };
                        invocation.filters.set_path(PathBuf::from(path));
                    }
                    "object-format" => {
                        let value = match option.value() {
                            Some(value) => value,
                            None => {
                                args.next_required_value(|| switch_requires_value("object-format"))?
                            }
                        };
                        invocation.set_explicit_format(value)?;
                    }
                    other => return hash_object_unknown_long_option(other),
                }
                continue;
            }
            match arg {
                "-t" => {
                    let value = args.next_required_value(|| switch_requires_value("t"))?;
                    invocation.object_type = value.parse()?;
                }
                value if value.starts_with("-t") && value.len() > 2 => {
                    invocation.object_type = value[2..].parse()?;
                }
                "-w" => invocation.write = true,
                value if value.starts_with('-') => {
                    return hash_object_unknown_short_switch(value);
                }
                value => invocation.paths.push(PathBuf::from(value)),
            }
        }
        invocation.validate()?;
        Ok(invocation)
    }

    fn set_explicit_format(&mut self, value: &str) -> Result<()> {
        self.format = parse_hash_object_format(value)?;
        self.explicit_format = true;
        Ok(())
    }

    fn enable_stdin(&mut self) -> Result<()> {
        if self.read_stdin {
            return usage_error("multiple --stdin options are not allowed");
        }
        if self.read_stdin_paths {
            return usage_error("--stdin and --stdin-paths cannot be used together");
        }
        self.read_stdin = true;
        Ok(())
    }

    fn enable_stdin_paths(&mut self) -> Result<()> {
        if self.read_stdin || self.read_stdin_paths {
            return usage_error("--stdin and --stdin-paths cannot be used together");
        }
        self.read_stdin_paths = true;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.read_stdin_paths && !self.paths.is_empty() {
            return usage_error("cannot pass filenames with --stdin-paths");
        }
        if self.read_stdin_paths && self.filters.path().is_some() {
            return usage_error("cannot use --path with --stdin-paths");
        }
        if self.filters.disabled_path().is_some() {
            return usage_error("cannot use --path with --no-filters");
        }
        Ok(())
    }

    fn execute(mut self, cli_session: &crate::session::CliSession) -> Result<()> {
        if !self.read_stdin && !self.read_stdin_paths && self.paths.is_empty() {
            return Ok(());
        }
        let cwd =
            fs::canonicalize(cli_session.cwd()).unwrap_or_else(|_| cli_session.cwd().to_path_buf());
        // Resolve repository layout, HEAD/include context, effective config,
        // worktree, format, and (when requested) the write store once. A
        // write-only hash-object invocation never enumerates replacement refs;
        // replacements affect reads, not object ids or raw object writes.
        let access = if self.write {
            crate::repository::ObjectAccess::WriteOnly
        } else {
            crate::repository::ObjectAccess::None
        };
        let repository = match crate::RepositoryContext::from_session_with_access(
            cli_session,
            access,
            crate::repository::WorktreePolicy::HashAttributes,
        ) {
            Ok(repository) => Some(repository),
            Err(GitError::NotFound(_)) => None,
            Err(err) => return Err(err),
        };
        let store = if self.write {
            let repository = repository
                .as_ref()
                .ok_or_else(|| GitError::repository_not_found("not a git repository"))?;
            if !self.explicit_format {
                self.format = repository.format();
            }
            Some(repository.repository())
        } else if !self.explicit_format
            && let Some(repository) = repository.as_ref()
        {
            self.format = repository.format();
            None
        } else {
            None
        };
        // Filters are Enabled by default. Without a process-lifetime attribute
        // handle, each path re-parsed repo config and re-read global
        // attributes (sley#25: ~163x slower than git for 200 `--stdin-paths`).
        let filter_context =
            HashObjectFilterContext::new(self.object_type, &self.filters, repository.as_ref())?;
        let stdout = io::stdout();
        let mut stdout = BufWriter::with_capacity(128 * 1024, stdout.lock());
        let mut big_file_threshold_validated = false;
        let result = (|| -> Result<()> {
            if self.read_stdin {
                let mut body = Vec::new();
                io::stdin().read_to_end(&mut body)?;
                self.hash_one(
                    body,
                    &cwd,
                    None,
                    filter_context.as_ref(),
                    store,
                    &mut stdout,
                )?;
            }
            if self.read_stdin_paths {
                let stdin = io::stdin();
                let mut records =
                    crate::commands::stdin_stream::StdinRecordReader::new(stdin.lock(), b'\n');
                while let Some(mut path) = records.read_record()? {
                    if path.is_empty() {
                        continue;
                    }
                    crate::commands::stdin_stream::strip_trailing_cr(&mut path);
                    let path = hash_object_stdin_path(path);
                    ensure_hash_object_big_file_threshold(
                        &mut big_file_threshold_validated,
                        repository.as_ref(),
                    )?;
                    let body = read_hash_object_path(&path)?;
                    self.hash_one(
                        body,
                        &cwd,
                        Some(&path),
                        filter_context.as_ref(),
                        store,
                        &mut stdout,
                    )?;
                    // `--stdin-paths` is a request/response protocol: callers
                    // may wait for this oid before sending the next path. Make
                    // each successful record observable while stdin remains
                    // open. The unconditional flush below still preserves any
                    // earlier responses when a later record fails.
                    stdout.flush()?;
                }
            }
            for path in &self.paths {
                ensure_hash_object_big_file_threshold(
                    &mut big_file_threshold_validated,
                    repository.as_ref(),
                )?;
                let body = read_hash_object_path(path)?;
                self.hash_one(
                    body,
                    &cwd,
                    Some(path),
                    filter_context.as_ref(),
                    store,
                    &mut stdout,
                )?;
            }
            Ok(())
        })();

        // A buffered writer must be flushed even when a later input fails so
        // the object ids for earlier inputs remain observable, as they are in
        // git. A flush failure is itself an output failure and takes precedence;
        // otherwise preserve the result of processing the inputs.
        let flush_result = stdout.flush().map_err(GitError::from);
        flush_result.and(result)
    }

    fn hash_one(
        &self,
        body: Vec<u8>,
        cwd: &Path,
        source_path: Option<&Path>,
        filter_context: Option<&HashObjectFilterContext<'_>>,
        store: Option<&Repository>,
        stdout: &mut dyn Write,
    ) -> Result<()> {
        let body = self.filters.apply(body, cwd, source_path, filter_context)?;
        print_hash_object(
            self.object_type,
            self.format,
            body,
            self.literally,
            store,
            stdout,
        )
    }
}

enum HashObjectFilterPolicy {
    Enabled { forced: bool, path: Option<PathBuf> },
    Disabled { path: Option<PathBuf> },
}

impl HashObjectFilterPolicy {
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Enabled { path, .. } | Self::Disabled { path } => path.as_deref(),
        }
    }

    fn disabled_path(&self) -> Option<&Path> {
        match self {
            Self::Disabled { path: Some(path) } => Some(path),
            _ => None,
        }
    }

    fn clear_path(&mut self) {
        match self {
            Self::Enabled { path, .. } | Self::Disabled { path } => *path = None,
        }
    }

    fn set_path(&mut self, new_path: PathBuf) {
        match self {
            Self::Enabled { path, .. } | Self::Disabled { path } => *path = Some(new_path),
        }
    }

    fn enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    fn forced(&self) -> bool {
        matches!(self, Self::Enabled { forced: true, .. })
    }

    fn apply(
        &self,
        body: Vec<u8>,
        cwd: &Path,
        source_path: Option<&Path>,
        filter_context: Option<&HashObjectFilterContext<'_>>,
    ) -> Result<Vec<u8>> {
        let Some(context) = filter_context else {
            return Ok(body);
        };
        let Some(git_path) =
            hash_object_filter_git_path(cwd, &context.worktree_root, source_path, self)?
        else {
            return Ok(body);
        };
        context.apply_clean_filter(&git_path, &body)
    }
}

struct HashObjectFilterContext<'config> {
    worktree_root: PathBuf,
    config: &'config GitConfig,
    // Process-lifetime attribute handle. `hash-object --stdin-paths` hashes
    // many paths in one process; without this the clean filter re-parsed
    // repo config and re-read global attributes per path (sley#25: ~163x
    // slower than git for 200 paths). In-tree `.gitattributes` are folded
    // per directory chain, matching git — not by walking the whole worktree.
    attributes: sley_worktree::WorktreeAttributes,
}

impl<'config> HashObjectFilterContext<'config> {
    fn new(
        object_type: ObjectType,
        policy: &HashObjectFilterPolicy,
        repository: Option<&'config crate::RepositoryContext>,
    ) -> Result<Option<Self>> {
        if object_type != ObjectType::Blob || !policy.enabled() {
            return Ok(None);
        }
        let Some(repository) = repository else {
            return Ok(None);
        };
        let Ok(worktree_root) = repository.worktree_root() else {
            return Ok(None);
        };
        let worktree_root =
            fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
        let config = repository.config();
        let attributes =
            sley_worktree::WorktreeAttributes::from_worktree_and_common_git_dir_with_config(
                &worktree_root,
                repository.common_git_dir().to_path_buf(),
                config,
            )?;
        Ok(Some(Self {
            worktree_root,
            config,
            attributes,
        }))
    }

    fn apply_clean_filter(&self, path: &[u8], content: &[u8]) -> Result<Vec<u8>> {
        self.attributes
            .apply_clean_filter(self.config, path, content)
    }
}

fn hash_object_filter_git_path(
    cwd: &Path,
    worktree_root: &Path,
    source_path: Option<&Path>,
    policy: &HashObjectFilterPolicy,
) -> Result<Option<Vec<u8>>> {
    if let Some(path) = policy.path() {
        // git: `vpath = prefix_filename(prefix, vpath)` — the `--path` value is a
        // *virtual* attribute-lookup path resolved relative to the cwd (which may
        // sit in a subdirectory of the worktree). It need not name an existing
        // file, so normalize it lexically rather than canonicalizing. A vpath that
        // escapes the worktree matches no in-tree `.gitattributes`, so no filter
        // applies (mirrors the source-path branch's out-of-tree handling).
        return match hash_object_worktree_relative_lexical(cwd, worktree_root, path) {
            Some(relative) => Ok(Some(hash_object_repo_path_bytes(&relative)?)),
            None => Ok(None),
        };
    }
    if let Some(path) = source_path {
        // Ordinary in-tree paths stay on the allocation-light lexical fast
        // path. A source spelling containing `..` needs filesystem-aware
        // resolution, however: `link/../file` traverses through `link` before
        // moving to its parent, so collapsing it lexically can select a
        // different `.gitattributes` chain from the file that was read.
        return match hash_object_worktree_relative_source(cwd, worktree_root, path)? {
            Some(relative) => Ok(Some(hash_object_repo_path_bytes(&relative)?)),
            None => Ok(None),
        };
    }
    if policy.forced() {
        return Ok(Some(Vec::new()));
    }
    Ok(None)
}

/// Resolve a real input path to its worktree-relative attribute path.
///
/// Most inputs can be normalized lexically, avoiding one `realpath` per file on
/// the `--stdin-paths` hot path. Parent components are the exception because
/// filesystem traversal gives them symlink-sensitive semantics.
fn hash_object_worktree_relative_source(
    cwd: &Path,
    worktree_root: &Path,
    path: &Path,
) -> Result<Option<PathBuf>> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let resolved = if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        fs::canonicalize(joined)?
    } else {
        normalize_lexical_path(&joined)
    };
    Ok(resolved
        .strip_prefix(worktree_root)
        .ok()
        .map(Path::to_path_buf))
}

/// Resolve `path` (possibly relative to `cwd`, possibly containing `..`) into a
/// worktree-relative path by lexical normalization, returning `None` when it
/// escapes `worktree_root`. Used for the `--path` virtual attribute path, which
/// git resolves against the subdirectory prefix without requiring the file to
/// exist.
fn hash_object_worktree_relative_lexical(
    cwd: &Path,
    worktree_root: &Path,
    path: &Path,
) -> Option<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = normalize_lexical_path(&joined);
    normalized
        .strip_prefix(worktree_root)
        .ok()
        .map(Path::to_path_buf)
}

fn hash_object_repo_path_bytes(path: &Path) -> Result<Vec<u8>> {
    RepoPathBuf::from_path(path)
        .map(RepoPathBuf::into_bytes)
        .map_err(|err| match err {
            GitError::InvalidPath(_) => {
                GitError::InvalidPath(format!("invalid hash-object path {}", path.display()))
            }
            err => err,
        })
}

fn hash_object_stdin_path(path: Vec<u8>) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(OsString::from_vec(path))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&path).into_owned())
    }
}

fn validate_hash_object_big_file_threshold(config: &GitConfig) -> Result<()> {
    let Some(value) = config.get_entry("core", None, "bigfilethreshold") else {
        return Ok(());
    };
    let value = value.unwrap_or("");
    match crate::sley_config::parse_config_int(value) {
        Some(value) if value >= 0 => Ok(()),
        _ => {
            eprintln!(
                "fatal: bad numeric config value '{value}' for 'core.bigfilethreshold': invalid unit"
            );
            Err(GitError::Exit(128))
        }
    }
}

fn ensure_hash_object_big_file_threshold(
    validated: &mut bool,
    repository: Option<&crate::RepositoryContext>,
) -> Result<()> {
    if *validated {
        return Ok(());
    }
    match repository {
        Some(repository) => validate_hash_object_big_file_threshold(repository.config())?,
        None => {
            // Outside a repository, global and command-scoped config still
            // participates when Git opens a path input.
            let _ = core_big_file_threshold(None)?;
        }
    }
    *validated = true;
    Ok(())
}

fn read_hash_object_path(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    match read_hash_object_file(path) {
        Ok(body) => Ok(body),
        Err(err) => {
            let reason = if err.kind() == io::ErrorKind::NotFound {
                "No such file or directory".to_string()
            } else {
                err.to_string()
            };
            eprintln!(
                "fatal: could not open '{}' for reading: {reason}",
                path.display()
            );
            Err(GitError::Exit(128))
        }
    }
}

fn read_hash_object_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    if let Ok(metadata) = file.metadata()
        && metadata.is_file()
        && metadata.len() > 0
        && let Ok(len) = usize::try_from(metadata.len())
    {
        // Treat the initial stat as the input snapshot, like git does. `Take`
        // avoids both zero-filling the allocation and the extra EOF read that
        // plain `read_to_end` performs. A concurrent shrink is a short read,
        // not a reason to silently hash a different version of the file.
        return read_hash_object_known_size(&mut file, metadata.len(), len);
    }

    // A reported zero length can still describe a non-empty virtual file (for
    // example procfs), and non-regular, unrepresentably large, or unstatable
    // inputs have no usable allocation size. Stream those cases to EOF.
    let mut body = Vec::new();
    file.read_to_end(&mut body)?;
    Ok(body)
}

fn read_hash_object_known_size(
    reader: &mut impl Read,
    sampled_len: u64,
    len: usize,
) -> io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(len);
    let read = reader.take(sampled_len).read_to_end(&mut body)?;
    if read != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short read while hashing object",
        ));
    }
    Ok(body)
}

fn print_hash_object(
    object_type: ObjectType,
    format: ObjectFormat,
    body: Vec<u8>,
    literally: bool,
    store: Option<&Repository>,
    stdout: &mut dyn Write,
) -> Result<()> {
    if !literally {
        super::hash_object_fsck::check_object(object_type, format, &body)?;
    }
    let object = sley_object::EncodedObject::new(object_type, body);
    let oid = if let Some(repository) = store {
        repository.write_object(object)?
    } else {
        object.object_id(format)?
    };
    writeln!(stdout, "{oid}")?;
    Ok(())
}

fn parse_hash_object_format(value: &str) -> Result<ObjectFormat> {
    if value.is_empty() {
        return usage_error("option `object-format' requires a value");
    }
    match value {
        "sha1" => Ok(ObjectFormat::Sha1),
        "sha256" => Ok(ObjectFormat::Sha256),
        other => {
            let message = format!("unknown option `object-format={other}'");
            usage_error(&message)
        }
    }
}

fn hash_object_unknown_long_option<T>(option: &str) -> Result<T> {
    eprintln!("error: unknown option `{option}'");
    Err(GitError::Exit(129))
}

fn hash_object_unknown_short_switch<T>(option: &str) -> Result<T> {
    let tail = option.trim_start_matches('-');
    eprintln!("error: unknown switch `{tail}'");
    Err(GitError::Exit(129))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_invocation_is_allowed() {
        assert!(HashObjectInvocation::parse(&[]).is_ok());
    }

    #[test]
    fn duplicate_stdin_is_exit_129() {
        let args = vec!["--stdin".to_string(), "--stdin".to_string()];
        assert!(matches!(
            HashObjectInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn unknown_long_option_is_exit_129() {
        let args = vec!["--bogus".to_string()];
        assert!(matches!(
            HashObjectInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn unknown_short_switch_is_exit_129() {
        let args = vec!["-x".to_string()];
        assert!(matches!(
            HashObjectInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn object_format_missing_value_is_exit_129() {
        let args = vec!["--object-format".to_string()];
        assert!(matches!(
            HashObjectInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn object_format_empty_value_is_exit_129() {
        let args = vec!["--object-format=".to_string()];
        assert!(matches!(
            HashObjectInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn virtual_path_with_parent_dir_normalizes_lexically() {
        let cwd = Path::new("/worktree");
        let worktree_root = Path::new("/worktree");
        let policy = HashObjectFilterPolicy::Enabled {
            forced: false,
            path: Some(PathBuf::from("dir/../file.txt")),
        };
        let git_path = hash_object_filter_git_path(cwd, worktree_root, None, &policy)
            .expect("resolve in-tree virtual path");
        assert_eq!(git_path, Some(b"file.txt".to_vec()));
    }

    #[test]
    fn known_size_reader_rejects_a_short_read() {
        let mut input = io::Cursor::new(b"short".as_slice());
        let err = read_hash_object_known_size(&mut input, 10, 10)
            .expect_err("sampled size must remain authoritative");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
