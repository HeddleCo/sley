//! `git hash-object`: frame bytes as Git objects, optionally write them, and
//! apply the same worktree-to-blob conversions that Git uses for path-aware
//! hashing.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, Result};
use sley_object::{Commit, ObjectType, Tag};
use sley_odb::LooseObjectStore;

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
                    GitError::Command("--object-format requires a value".into())
                })? {
                    invocation.set_format(value.value().parse()?);
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
                            None => args.next_required_value(|| {
                                GitError::Command("--object-format requires a value".into())
                            })?,
                        };
                        invocation.set_format(value.parse()?);
                    }
                    _ => invocation.paths.push(PathBuf::from(arg)),
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
                value => invocation.paths.push(PathBuf::from(value)),
            }
        }
        invocation.validate()?;
        Ok(invocation)
    }

    fn set_format(&mut self, format: ObjectFormat) {
        self.format = format;
        self.explicit_format = true;
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
        if !self.read_stdin && !self.read_stdin_paths && self.paths.is_empty() {
            if self.allow_no_input {
                return Ok(());
            }
            return Err(GitError::Command(
                "hash-object requires --stdin or a path".into(),
            ));
        }
        Ok(())
    }

    fn execute(mut self) -> Result<()> {
        if !self.read_stdin && !self.read_stdin_paths && self.paths.is_empty() {
            return Ok(());
        }
        let cwd = env::current_dir()?;
        let repo_git_dir = discover_git_dir(&cwd).ok();
        let mut store = None;
        if self.write {
            let git_dir = repo_git_dir
                .as_ref()
                .ok_or_else(|| GitError::NotFound("not a git repository".into()))?;
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
        let filter_context =
            HashObjectFilterContext::new(self.object_type, &self.filters, repo_git_dir.as_deref())?;

        if self.read_stdin {
            let mut body = Vec::new();
            io::stdin().read_to_end(&mut body)?;
            self.hash_one(body, &cwd, None, filter_context.as_ref(), store.as_mut())?;
        }
        if self.read_stdin_paths {
            let mut body = Vec::new();
            io::stdin().read_to_end(&mut body)?;
            for path in body.split(|byte| *byte == b'\n') {
                if path.is_empty() {
                    continue;
                }
                let path = path.strip_suffix(b"\r").unwrap_or(path);
                let path = String::from_utf8_lossy(path);
                let path = Path::new(path.as_ref());
                let body = read_hash_object_path(path)?;
                self.hash_one(
                    body,
                    &cwd,
                    Some(path),
                    filter_context.as_ref(),
                    store.as_mut(),
                )?;
            }
        }
        for path in &self.paths {
            let body = read_hash_object_path(path)?;
            self.hash_one(
                body,
                &cwd,
                Some(path),
                filter_context.as_ref(),
                store.as_mut(),
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
    ) -> Result<()> {
        let body = self.filters.apply(body, cwd, source_path, filter_context)?;
        print_hash_object(self.object_type, self.format, body, self.literally, store)
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
        sley_worktree::apply_clean_filter(
            &context.worktree_root,
            &context.git_dir,
            &context.config,
            &git_path,
            &body,
        )
    }
}

struct HashObjectFilterContext {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    config: GitConfig,
}

impl HashObjectFilterContext {
    fn new(
        object_type: ObjectType,
        policy: &HashObjectFilterPolicy,
        git_dir: Option<&Path>,
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
        Ok(Some(Self {
            worktree_root,
            git_dir: git_dir.to_path_buf(),
            config: read_repo_config(git_dir)?,
        }))
    }
}

fn hash_object_filter_git_path(
    cwd: &Path,
    worktree_root: &Path,
    source_path: Option<&Path>,
    policy: &HashObjectFilterPolicy,
) -> Result<Option<Vec<u8>>> {
    if let Some(path) = policy.path() {
        return Ok(Some(hash_object_repo_path_bytes(path)?));
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
) -> Result<()> {
    if !literally {
        validate_hash_object_body(object_type, format, &body)?;
    }
    let object = sley_object::EncodedObject::new(object_type, body);
    let oid = if let Some(store) = store {
        store.write_object(object)?
    } else {
        object.object_id(format)?
    };
    println!("{oid}");
    Ok(())
}

fn validate_hash_object_body(
    object_type: ObjectType,
    format: ObjectFormat,
    body: &[u8],
) -> Result<()> {
    match object_type {
        ObjectType::Blob => Ok(()),
        ObjectType::Tree => validate_tree_body(format, body),
        ObjectType::Commit => Commit::parse(format, body).map(|_| ()),
        ObjectType::Tag => Tag::parse(format, body).map(|_| ()),
    }
}

fn validate_tree_body(format: ObjectFormat, body: &[u8]) -> Result<()> {
    let mut offset = 0usize;
    let mut names = HashSet::new();
    while offset < body.len() {
        let mode_start = offset;
        while body.get(offset).copied() != Some(b' ') {
            offset += 1;
            if offset >= body.len() {
                return Err(GitError::InvalidFormat("unterminated tree mode".into()));
            }
        }
        let mode_text = std::str::from_utf8(&body[mode_start..offset])
            .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        let mode = u32::from_str_radix(mode_text, 8)
            .map_err(|_| GitError::InvalidFormat("invalid tree mode".into()))?;
        if !matches!(mode, 0o040000 | 0o100644 | 0o100755 | 0o120000 | 0o160000) {
            return Err(GitError::InvalidObject("invalid tree mode".into()));
        }
        offset += 1;
        let name_start = offset;
        while body.get(offset).copied() != Some(0) {
            offset += 1;
            if offset >= body.len() {
                return Err(GitError::InvalidFormat("unterminated tree path".into()));
            }
        }
        if offset == name_start {
            return Err(GitError::InvalidObject("empty tree path".into()));
        }
        let name = body[name_start..offset].to_vec();
        if !names.insert(name) {
            return Err(GitError::InvalidObject("duplicateEntries".into()));
        }
        offset += 1;
        let oid_end = offset
            .checked_add(format.raw_len())
            .ok_or_else(|| GitError::InvalidFormat("tree oid overflow".into()))?;
        if oid_end > body.len() {
            return Err(GitError::InvalidFormat("truncated tree object id".into()));
        }
        let _ = sley_core::ObjectId::from_raw(format, &body[offset..oid_end])?;
        offset = oid_end;
    }
    Ok(())
}
