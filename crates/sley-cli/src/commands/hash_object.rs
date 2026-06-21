//! `git hash-object`: frame bytes as Git objects, optionally write them, and
//! apply the same worktree-to-blob conversions that Git uses for path-aware
//! hashing.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sley::plumbing::sley_config::GitConfig;
use sley::plumbing::sley_core::{GitError, ObjectFormat, Result};
use sley::plumbing::sley_object::ObjectType;
use sley::plumbing::sley_odb::LooseObjectStore;

use super::args::{
    GitArgCursor, LongOption, Terminator, option_takes_no_value, switch_requires_value, usage_error,
};
use crate::*;

pub(crate) fn cmd_hash_object(args: &[String]) -> Result<()> {
    HashObjectInvocation::parse(args)?.execute()
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

    fn execute(mut self) -> Result<()> {
        if !self.read_stdin && !self.read_stdin_paths && self.paths.is_empty() {
            return Ok(());
        }
        let cwd = env::current_dir()?;
        let repo_git_dir = discover_git_dir(&cwd).ok();
        let _big_file_threshold = core_big_file_threshold(repo_git_dir.as_deref())?;
        let mut store = None;
        if self.write {
            let git_dir = repo_git_dir
                .as_ref()
                .ok_or_else(|| GitError::repository_not_found("not a git repository"))?;
            let repo_format = repository_object_format(git_dir)?;
            if !self.explicit_format {
                self.format = repo_format;
            }
            store = Some(LooseObjectStore::from_git_dir(git_dir, self.format));
        } else if !self.explicit_format
            && let Some(git_dir) = repo_git_dir.as_ref()
        {
            self.format = repository_object_format(git_dir)?;
        }
        // Caching the worktree `.gitattributes` chain pays for itself whenever
        // more than one path is hashed in this process — `--stdin-paths` (many,
        // count unknown up front) OR multiple positional paths. Gating only on
        // `--stdin-paths` regressed `hash-object -w file1 file2 …` back to the
        // per-path attribute re-walk this cache was added to avoid (sley#25).
        let cache_attributes = self.read_stdin_paths || self.paths.len() > 1;
        let filter_context = HashObjectFilterContext::new(
            self.object_type,
            &self.filters,
            repo_git_dir.as_deref(),
            cache_attributes,
        )?;
        let mut stdout = io::stdout().lock();

        if self.read_stdin {
            let mut body = Vec::new();
            io::stdin().read_to_end(&mut body)?;
            self.hash_one(
                body,
                &cwd,
                None,
                filter_context.as_ref(),
                store.as_mut(),
                &mut stdout,
            )?;
        }
        if self.read_stdin_paths {
            crate::commands::stdin_stream::stream_stdin_records(
                b'\n',
                &mut stdout,
                |mut path, stdout| {
                    if path.is_empty() {
                        return Ok(());
                    }
                    crate::commands::stdin_stream::strip_trailing_cr(&mut path);
                    let path = String::from_utf8_lossy(&path);
                    let path = Path::new(path.as_ref());
                    let body = read_hash_object_path(path)?;
                    self.hash_one(
                        body,
                        &cwd,
                        Some(path),
                        filter_context.as_ref(),
                        store.as_mut(),
                        stdout,
                    )?;
                    Ok(())
                },
            )?;
        }
        for path in &self.paths {
            let body = read_hash_object_path(path)?;
            self.hash_one(
                body,
                &cwd,
                Some(path),
                filter_context.as_ref(),
                store.as_mut(),
                &mut stdout,
            )?;
        }
        Ok(())
    }

    fn hash_one(
        &self,
        body: Vec<u8>,
        cwd: &Path,
        source_path: Option<&Path>,
        filter_context: Option<&HashObjectFilterContext>,
        store: Option<&mut LooseObjectStore>,
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
        filter_context: Option<&HashObjectFilterContext>,
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

struct HashObjectFilterContext {
    git_dir: PathBuf,
    worktree_root: PathBuf,
    config: GitConfig,
    // The worktree's `.gitattributes` chain, scanned once. `hash-object
    // --stdin-paths` hashes many paths in one process; without this the clean
    // filter re-walked the entire worktree and re-read every `.gitattributes`
    // per path (sley#25: ~163x slower than git for 200 paths).
    attributes: Option<sley::plumbing::sley_worktree::WorktreeAttributes>,
}

impl HashObjectFilterContext {
    fn new(
        object_type: ObjectType,
        policy: &HashObjectFilterPolicy,
        git_dir: Option<&Path>,
        cache_attributes: bool,
    ) -> Result<Option<Self>> {
        if object_type != ObjectType::Blob || !policy.enabled() {
            return Ok(None);
        }
        let Some(git_dir) = git_dir else {
            return Ok(None);
        };
        let Ok(worktree_root) = worktree_root_for_git_dir(git_dir) else {
            return Ok(None);
        };
        let attributes = cache_attributes
            .then(|| sley::plumbing::sley_worktree::WorktreeAttributes::from_worktree_root(&worktree_root))
            .transpose()?;
        Ok(Some(Self {
            git_dir: git_dir.to_path_buf(),
            worktree_root,
            config: read_repo_config(git_dir)?,
            attributes,
        }))
    }

    fn apply_clean_filter(&self, path: &[u8], content: &[u8]) -> Result<Vec<u8>> {
        match &self.attributes {
            Some(attributes) => attributes.apply_clean_filter(&self.config, path, content),
            None => sley::plumbing::sley_worktree::apply_clean_filter(
                &self.worktree_root,
                &self.git_dir,
                &self.config,
                path,
                content,
            ),
        }
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
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let absolute = fs::canonicalize(&absolute).unwrap_or(absolute);
        let Ok(relative) = absolute.strip_prefix(worktree_root) else {
            return Ok(None);
        };
        return Ok(Some(hash_object_repo_path_bytes(relative)?));
    }
    if policy.forced() {
        return Ok(Some(Vec::new()));
    }
    Ok(None)
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

fn read_hash_object_path(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    match fs::read(path) {
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

fn print_hash_object(
    object_type: ObjectType,
    format: ObjectFormat,
    body: Vec<u8>,
    literally: bool,
    store: Option<&mut LooseObjectStore>,
    stdout: &mut dyn Write,
) -> Result<()> {
    if !literally {
        super::hash_object_fsck::check_object(object_type, format, &body)?;
    }
    let object = sley::plumbing::sley_object::EncodedObject::new(object_type, body);
    let oid = if let Some(store) = store {
        store.write_object(object)?
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
}
