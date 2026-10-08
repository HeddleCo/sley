//! Creating worktree entries without following symlinks.
//!
//! Checkout used to build a path string (`root/a/b/file`), `lstat` each
//! parent, replace any symlink it found with a real directory, and then
//! `open(2)` the full path. Every one of those `open` calls followed symlinks
//! again, so a parent swapped for a symlink between the check and the write
//! redirected the write anywhere on the filesystem.
//!
//! [`WorktreeLeaf::open`] instead walks the path one component at a time with
//! `openat(O_DIRECTORY | O_NOFOLLOW)` from a descriptor for the worktree root,
//! creating missing directories with `mkdirat` and replacing a non-directory
//! (file or symlink) in the way with `unlinkat` + `mkdirat`, exactly the D/F
//! transitions git's `create_directories` performs. Every later operation on
//! the leaf (`unlinkat`, `openat(O_CREAT | O_EXCL | O_NOFOLLOW)`,
//! `symlinkat`) is relative to the final parent's descriptor, so no checkout
//! write resolves a symlink the remote planted, whatever happens concurrently.
//!
//! Each opened directory is also compared by `(st_dev, st_ino)` with the
//! repository's `.git` at the worktree root. Name rules (see
//! [`crate::path_safety`]) are a proxy for what a filesystem treats as the
//! same name; the identity check is the filesystem's own answer, so it also
//! catches an alias no name rule anticipated (another case-folding scheme, a
//! different short-name generator) on the filesystem actually written.
//!
//! What is left: the worktree root itself is opened by path, so a symlink at
//! or above the root is followed (it is the caller's choice of directory),
//! and the identity check only knows the `.git` at the worktree root, not a
//! `GIT_DIR` that lives elsewhere inside the worktree under another name.
//! Tracked-path removals use descriptor-relative no-follow walks as well.
//! On non-Unix targets the writer falls back to path-based operations after
//! the same parent checks;
//! the name rules are the protection there.

use super::*;
use crate::path_safety::verify_path_unconditional;

/// A worktree path whose parent directories exist as real directories, ready
/// to have its leaf removed and (re)created.
pub(crate) struct WorktreeLeaf {
    path: PathBuf,
    #[cfg(unix)]
    parent: std::os::fd::OwnedFd,
    #[cfg(unix)]
    name: Vec<u8>,
    #[cfg(unix)]
    dot_git: Option<FileIdentity>,
}

/// How a leaf removal reports refusing to delete the process's cwd.
pub(crate) type CwdRefusal = fn(&Path) -> Result<()>;

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl FileIdentity {
    // `st_dev`/`st_ino` are unsigned on Linux and signed on some BSDs; the
    // bit pattern is what identifies the file, so a wrapping conversion is
    // the faithful one.
    #[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
    fn of(stat: &rustix::fs::Stat) -> Self {
        Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
        }
    }
}

/// Upper bound on create/replace retries for one component, so a concurrent
/// process flipping a name between file and directory cannot spin checkout.
#[cfg(unix)]
const MAX_COMPONENT_ATTEMPTS: usize = 8;

impl WorktreeLeaf {
    /// Validate `git_path` against the effective repository path policy,
    /// then create its parent directories under `worktree_root` without
    /// following symlinks.
    pub(crate) fn open(
        original_cwd: Option<&Path>,
        worktree_root: &Path,
        git_path: &[u8],
        mode: u32,
    ) -> Result<Self> {
        Self::open_with_policy(
            original_cwd,
            worktree_root,
            git_path,
            mode,
            &crate::path_safety::writer_path_policy(worktree_root),
        )
    }

    pub(crate) fn open_with_policy(
        original_cwd: Option<&Path>,
        worktree_root: &Path,
        git_path: &[u8],
        mode: u32,
        policy: &WorktreePathPolicy,
    ) -> Result<Self> {
        policy.verify_path(git_path, mode)?;
        let path = crate::index_io::worktree_path(worktree_root, git_path)?;
        Self::open_validated(original_cwd, worktree_root, git_path, path)
    }

    #[cfg(unix)]
    fn open_validated(
        original_cwd: Option<&Path>,
        worktree_root: &Path,
        git_path: &[u8],
        path: PathBuf,
    ) -> Result<Self> {
        use rustix::fs::{AtFlags, Mode, OFlags};

        let mut dir = rustix::fs::open(
            worktree_root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let dot_git = match rustix::fs::statat(&dir, ".git", AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Some(FileIdentity::of(&stat)),
            Err(_) => None,
        };
        let mut components = git_path.split(|byte| *byte == b'/').peekable();
        let mut current = worktree_root.to_path_buf();
        let mut name = Vec::new();
        while let Some(component) = components.next() {
            current.push(os_str(component));
            if components.peek().is_none() {
                name = component.to_vec();
                break;
            }
            dir = open_or_create_dir(original_cwd, &dir, component, &current, dot_git, git_path)?;
        }
        Ok(Self {
            path,
            parent: dir,
            name,
            dot_git,
        })
    }

    #[cfg(not(unix))]
    fn open_validated(
        original_cwd: Option<&Path>,
        worktree_root: &Path,
        _git_path: &[u8],
        path: PathBuf,
    ) -> Result<Self> {
        let _ = original_cwd;
        prepare_blob_parent_dirs(worktree_root, &path)?;
        Ok(Self { path })
    }

    /// The absolute path of the leaf, for stat-back and diagnostics.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove whatever occupies the leaf: a file, a symlink (never its
    /// target), or a directory subtree. An absent leaf is a no-op. Refuses to
    /// remove the repository's `.git` or the process's original cwd.
    pub(crate) fn remove_existing(
        &self,
        original_cwd: Option<&Path>,
        refuse_cwd: CwdRefusal,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, FileType};
            let stat = match rustix::fs::statat(
                &self.parent,
                self.name.as_slice(),
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => return Ok(()),
                Err(err) => return Err(std::io::Error::from(err).into()),
            };
            self.refuse_dot_git(&stat)?;
            if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                if crate::index_io::path_is_original_cwd(original_cwd, &self.path) {
                    return refuse_cwd(&self.path);
                }
                remove_dir_all_at(&self.parent, &self.name)?;
            } else {
                match rustix::fs::unlinkat(&self.parent, self.name.as_slice(), AtFlags::empty()) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(err) => return Err(std::io::Error::from(err).into()),
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let metadata = match fs::symlink_metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err.into()),
            };
            if metadata.is_dir() {
                if crate::index_io::path_is_original_cwd(original_cwd, &self.path) {
                    return refuse_cwd(&self.path);
                }
                fs::remove_dir_all(&self.path)?;
            } else {
                fs::remove_file(&self.path)?;
            }
            Ok(())
        }
    }

    /// Create the leaf as a new regular file holding `body`. Fails if the
    /// leaf exists (it is never opened through a symlink). Created `0777` for
    /// an executable `mode`, else `0666`, before the umask, like git's
    /// `create_file`; callers adjust the permission bits on the handle.
    pub(crate) fn create_file(&self, body: &[u8], mode: u32) -> Result<fs::File> {
        use std::io::Write as _;
        #[cfg(unix)]
        let mut file = {
            use rustix::fs::{Mode, OFlags};
            let create_mode = if mode & 0o100 != 0 { 0o777 } else { 0o666 };
            let fd = rustix::fs::openat(
                &self.parent,
                self.name.as_slice(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(create_mode),
            )
            .map_err(std::io::Error::from)?;
            fs::File::from(fd)
        };
        #[cfg(not(unix))]
        let mut file = {
            let _ = mode;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.path)?
        };
        file.write_all(body)?;
        Ok(file)
    }

    /// Create the leaf as a symlink to the raw `target` bytes. On targets
    /// without symlinks, write the link text as a regular file, as git does.
    pub(crate) fn create_symlink(&self, target: &[u8]) -> Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::symlinkat(target, &self.parent, self.name.as_slice())
                .map_err(std::io::Error::from)?;
        }
        #[cfg(not(unix))]
        {
            self.create_file(target, 0o100644)?;
        }
        Ok(())
    }

    /// git's `write_entry` type-by-mode switch on a cleared leaf: a symlink
    /// for `0o120000`, else a regular file whose mode is set like
    /// checkout's executable-mode handling.
    pub(crate) fn create_blob_body_or_symlink(
        &self,
        mode: u32,
        body: &[u8],
        link_target: &[u8],
    ) -> Result<()> {
        if (mode & 0o170000) == 0o120000 {
            self.create_symlink(link_target)
        } else {
            let file = self.create_file(body, mode)?;
            set_handle_file_mode(&file, mode)
        }
    }

    /// Ensure the leaf is a directory (a gitlink's submodule directory). An
    /// existing directory is kept. A symlink is kept when `keep_symlink`
    /// (git refuses to replace a symlink with a gitlink directory) and
    /// replaced otherwise; any other non-directory is replaced.
    pub(crate) fn ensure_dir(
        &self,
        original_cwd: Option<&Path>,
        refuse_cwd: CwdRefusal,
        keep_symlink: bool,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, FileType, Mode};
            match rustix::fs::statat(
                &self.parent,
                self.name.as_slice(),
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) => {
                    self.refuse_dot_git(&stat)?;
                    let file_type = FileType::from_raw_mode(stat.st_mode);
                    if file_type == FileType::Directory
                        || (keep_symlink && file_type == FileType::Symlink)
                    {
                        return Ok(());
                    }
                    self.remove_existing(original_cwd, refuse_cwd)?;
                }
                Err(rustix::io::Errno::NOENT) => {}
                Err(err) => return Err(std::io::Error::from(err).into()),
            }
            match rustix::fs::mkdirat(
                &self.parent,
                self.name.as_slice(),
                Mode::from_raw_mode(0o777),
            ) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
                Err(err) => Err(std::io::Error::from(err).into()),
            }
        }
        #[cfg(not(unix))]
        {
            if let Ok(metadata) = fs::symlink_metadata(&self.path) {
                if metadata.is_dir() || (keep_symlink && metadata.file_type().is_symlink()) {
                    return Ok(());
                }
                self.remove_existing(original_cwd, refuse_cwd)?;
            }
            fs::create_dir_all(&self.path)?;
            Ok(())
        }
    }

    #[cfg(unix)]
    fn refuse_dot_git(&self, stat: &rustix::fs::Stat) -> Result<()> {
        if self.dot_git == Some(FileIdentity::of(stat)) {
            return Err(crate::path_safety::invalid_path_error(
                self.path.as_os_str().as_encoded_bytes(),
            ));
        }
        Ok(())
    }
}

/// git's `write_entry` for a blob: replace whatever is at `git_path` with a
/// symlink (`0o120000`, target `link_target`) or a regular file holding
/// `body`, creating parent directories, without following any symlink.
/// Returns the absolute path written.
pub(crate) fn replace_worktree_blob(
    original_cwd: Option<&Path>,
    worktree_root: &Path,
    git_path: &[u8],
    mode: u32,
    body: &[u8],
    link_target: &[u8],
) -> Result<PathBuf> {
    replace_worktree_blob_with_policy(
        original_cwd,
        worktree_root,
        git_path,
        mode,
        body,
        link_target,
        &crate::path_safety::writer_path_policy(worktree_root),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn replace_worktree_blob_with_policy(
    original_cwd: Option<&Path>,
    worktree_root: &Path,
    git_path: &[u8],
    mode: u32,
    body: &[u8],
    link_target: &[u8],
    policy: &WorktreePathPolicy,
) -> Result<PathBuf> {
    let leaf = WorktreeLeaf::open_with_policy(original_cwd, worktree_root, git_path, mode, policy)?;
    leaf.remove_existing(
        original_cwd,
        crate::index_io::refuse_remove_current_working_directory,
    )?;
    leaf.create_blob_body_or_symlink(mode, body, link_target)?;
    Ok(leaf.path)
}

/// [`replace_worktree_blob`] that always writes a regular file (smudged
/// content, merge results), with its permission bits set from `mode`.
pub(crate) fn replace_worktree_file(
    original_cwd: Option<&Path>,
    worktree_root: &Path,
    git_path: &[u8],
    mode: u32,
    body: &[u8],
) -> Result<PathBuf> {
    let leaf = WorktreeLeaf::open(original_cwd, worktree_root, git_path, mode)?;
    leaf.remove_existing(
        original_cwd,
        crate::index_io::refuse_remove_current_working_directory,
    )?;
    let file = leaf.create_file(body, mode)?;
    set_handle_file_mode(&file, mode)?;
    Ok(leaf.path)
}

/// Materialize one worktree entry the way git's `write_entry` does, for
/// callers outside this crate (merge, cherry-pick, apply) that already have
/// the final content: a gitlink (`0o160000`) becomes a directory (an existing
/// directory or symlink is kept), a symlink (`0o120000`) gets `body` as its
/// target, anything else becomes a regular file holding `body`.
///
/// Parent directories are created and the leaf replaced without following
/// any symlink, and `git_path` is refused if it has an empty, `.` or `..`
/// component, names `.git` in any case, or resolves to the repository's
/// `.git`. Returns the absolute path written.
pub fn write_worktree_entry(
    original_cwd: Option<&Path>,
    worktree_root: &Path,
    git_path: &[u8],
    mode: u32,
    body: &[u8],
) -> Result<PathBuf> {
    if sley_index::is_gitlink(mode) {
        let leaf = WorktreeLeaf::open(original_cwd, worktree_root, git_path, mode)?;
        leaf.ensure_dir(
            original_cwd,
            crate::index_io::refuse_remove_current_working_directory,
            true,
        )?;
        return Ok(leaf.path);
    }
    replace_worktree_blob(original_cwd, worktree_root, git_path, mode, body, body)
}

/// chmod an open regular file to match its entry mode, the handle form of
/// checkout's executable-mode handling.
pub(crate) fn set_handle_file_mode(file: &fs::File, entry_mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = match entry_mode {
            0o100755 => 0o755,
            0o100644 => 0o644,
            _ => return Ok(()),
        };
        file.set_permissions(fs::Permissions::from_mode(perms))?;
    }
    #[cfg(not(unix))]
    let _ = (file, entry_mode);
    Ok(())
}

/// Create the ancestor directories of a worktree blob path, removing any
/// regular file or symlink that occupies an ancestor *component* first.
///
/// Mirrors git's `entry.c` `create_directories`: it walks each path component
/// between `worktree_root` and the leaf and, for each, if a non-directory (a
/// regular file or symlink left by a prior tree where `dir` was a FILE) blocks
/// it, unlinks the blocker before `mkdir`. A plain `fs::create_dir_all` fails
/// with `ENOTDIR`/`EEXIST` on such a D/F transition; this is the directory-side
/// of git's force-checkout D/F clearing.
///
/// `worktree_root` itself is never touched. Only components strictly between the
/// root and the leaf are cleared, matching `create_directories`' `base_dir_len`
/// boundary.
#[cfg(not(unix))]
fn prepare_blob_parent_dirs(worktree_root: &Path, file_path: &Path) -> Result<()> {
    let parent = match file_path.parent() {
        Some(parent) => parent,
        None => return Ok(()),
    };
    // Fast path: parent already is a directory (the overwhelmingly common
    // case).  Do not use `Path::is_dir()` here: it follows a symlink.  A
    // checkout of `D/file` with an untracked `D -> elsewhere` must replace the
    // link with a real directory, never write through it into `elsewhere`.
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => return Ok(()),
        Ok(_) => {}
        // `lstat("file/child")` reports ENOTDIR when an earlier component is
        // the D/F blocker we are about to replace. Treat it like an absent
        // descendant and let the root-to-leaf walk remove that blocker.
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        Err(err) => return Err(err.into()),
    }
    // Collect the ancestor chain from worktree_root (exclusive) down to `parent`
    // (inclusive). We can't `create_dir_all` blindly because a non-directory may
    // sit on one of these components; walk them and clear blockers as git does.
    let mut components: Vec<&Path> = Vec::new();
    let mut cursor = Some(parent);
    while let Some(dir) = cursor {
        if dir == worktree_root {
            break;
        }
        components.push(dir);
        cursor = dir.parent();
        if cursor.is_none() {
            break;
        }
    }
    // Walk root → leaf so each parent exists before its child.
    for dir in components.iter().rev() {
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                // A regular file or symlink occupies this component (the prior
                // tree had `dir` as a FILE). Unlink it, then create the dir.
                fs::remove_file(dir)?;
                fs::create_dir(dir)?;
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                fs::create_dir(dir)?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn os_str(bytes: &[u8]) -> &std::ffi::OsStr {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes)
}

/// Open `component` of `dir` as a directory without following a symlink,
/// creating it if missing and replacing a non-directory in the way.
#[cfg(unix)]
fn open_or_create_dir(
    original_cwd: Option<&Path>,
    dir: &std::os::fd::OwnedFd,
    component: &[u8],
    current: &Path,
    dot_git: Option<FileIdentity>,
    git_path: &[u8],
) -> Result<std::os::fd::OwnedFd> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    let refuse = || Err(crate::path_safety::invalid_path_error(git_path));
    for _ in 0..MAX_COMPONENT_ATTEMPTS {
        match rustix::fs::openat(
            dir,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let stat = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
                if dot_git == Some(FileIdentity::of(&stat)) {
                    return refuse();
                }
                return Ok(fd);
            }
            Err(Errno::NOENT) => {}
            // A symlink (ELOOP; EMLINK on FreeBSD) or a non-directory
            // (ENOTDIR) occupies a component that must be a directory: the
            // D/F file->dir transition. Unlink the entry itself, never its
            // target, then create the directory.
            Err(Errno::LOOP | Errno::MLINK | Errno::NOTDIR) => {
                let stat = match rustix::fs::statat(dir, component, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat,
                    Err(Errno::NOENT) => continue,
                    Err(err) => return Err(std::io::Error::from(err).into()),
                };
                if dot_git == Some(FileIdentity::of(&stat)) {
                    return refuse();
                }
                if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                    // Raced into a directory; open it on the next pass.
                    continue;
                }
                if crate::index_io::path_is_original_cwd(original_cwd, current) {
                    crate::index_io::refuse_remove_current_working_directory(current)?;
                    return refuse();
                }
                match rustix::fs::unlinkat(dir, component, AtFlags::empty()) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(err) => return Err(std::io::Error::from(err).into()),
                }
            }
            Err(err) => return Err(std::io::Error::from(err).into()),
        }
        match rustix::fs::mkdirat(dir, component, Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(err) => return Err(std::io::Error::from(err).into()),
        }
    }
    Err(GitError::InvalidPath(format!(
        "'{}': worktree component kept changing while it was created",
        current.display()
    )))
}

/// Remove the directory `name` of `parent` and everything below it without
/// following symlinks: each subdirectory is opened `O_NOFOLLOW` relative to
/// its parent's descriptor, and symlinks are unlinked as entries.
#[cfg(unix)]
fn remove_dir_all_at(parent: &std::os::fd::OwnedFd, name: &[u8]) -> Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    let dir = match rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(dir) => dir,
        Err(Errno::NOENT) => return Ok(()),
        // Became a non-directory (or symlink) meanwhile: unlink the entry.
        Err(Errno::LOOP | Errno::MLINK | Errno::NOTDIR) => {
            return match rustix::fs::unlinkat(parent, name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(err) => Err(std::io::Error::from(err).into()),
            };
        }
        Err(err) => return Err(std::io::Error::from(err).into()),
    };
    let mut children = Vec::new();
    for entry in rustix::fs::Dir::read_from(&dir).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let child = entry.file_name().to_bytes();
        if child == b"." || child == b".." {
            continue;
        }
        children.push((child.to_vec(), entry.file_type()));
    }
    for (child, file_type) in children {
        let is_dir = match file_type {
            FileType::Directory => true,
            FileType::Unknown => {
                match rustix::fs::statat(&dir, child.as_slice(), AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
                    Err(Errno::NOENT) => continue,
                    Err(err) => return Err(std::io::Error::from(err).into()),
                }
            }
            _ => false,
        };
        if is_dir {
            remove_dir_all_at(&dir, &child)?;
        } else {
            match rustix::fs::unlinkat(&dir, child.as_slice(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(err) => return Err(std::io::Error::from(err).into()),
            }
        }
    }
    drop(dir);
    match rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(err) => Err(std::io::Error::from(err).into()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn refuse(_: &Path) -> Result<()> {
        Err(GitError::Transaction("cwd".into()))
    }

    #[test]
    fn removal_and_pruning_stay_relative_to_held_directories() {
        let root = tempfile::tempdir().expect("worktree");
        let outside = tempfile::tempdir().expect("outside directory");
        fs::create_dir(root.path().join("a")).expect("parent");
        fs::write(root.path().join("a/file"), b"tracked").expect("tracked file");
        fs::write(outside.path().join("file"), b"outside").expect("outside file");
        let leaf = ExistingWorktreeLeaf::open(root.path(), b"a/file")
            .expect("open")
            .expect("present parent");
        fs::rename(root.path().join("a"), root.path().join("held")).expect("move parent");
        std::os::unix::fs::symlink(outside.path(), root.path().join("a"))
            .expect("replacement link");
        assert!(leaf.remove(None).expect("remove via descriptor"));
        leaf.prune(None).expect("prune via descriptor");
        assert!(!root.path().join("held/file").exists());
        assert_eq!(
            fs::read(outside.path().join("file")).expect("outside file"),
            b"outside"
        );
    }

    #[test]
    fn pruning_missing_directory_still_removes_empty_real_ancestors() {
        let root = tempfile::tempdir().expect("worktree");
        fs::create_dir(root.path().join("a")).expect("empty ancestor");
        prune_worktree_dirs(None, root.path(), Some(&root.path().join("a/missing")))
            .expect("missing directory is absent");
        assert!(!root.path().join("a").exists());
        assert!(root.path().is_dir());
    }

    #[test]
    fn removal_prunes_empty_real_parents_and_preserves_cwd() {
        let root = tempfile::tempdir().expect("worktree");
        fs::create_dir_all(root.path().join("a/b")).expect("parents");
        fs::write(root.path().join("a/b/file"), b"tracked").expect("tracked file");
        remove_worktree_entry(Some(&root.path().join("a")), root.path(), b"a/b/file")
            .expect("remove");
        assert!(!root.path().join("a/b").exists());
        assert!(root.path().join("a").is_dir());
        fs::write(root.path().join("a/file"), b"tracked").expect("tracked file");
        remove_worktree_entry(None, root.path(), b"a/file").expect("remove and prune");
        assert!(!root.path().join("a").exists());
        assert!(root.path().is_dir());
    }

    #[test]
    fn symlinked_parent_is_replaced_not_followed() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        let outside = base.path().join("outside");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::create_dir(&outside).expect("outside dir");
        std::os::unix::fs::symlink(&outside, root.join("a")).expect("plant symlink");

        let leaf = WorktreeLeaf::open(None, &root, b"a/b/file", 0o100644).expect("open leaf");
        leaf.remove_existing(None, refuse).expect("clear leaf");
        leaf.create_blob_body_or_symlink(0o100644, b"data", b"data")
            .expect("write file");

        assert!(
            fs::symlink_metadata(root.join("a"))
                .expect("lstat a")
                .is_dir()
        );
        assert_eq!(fs::read(root.join("a/b/file")).expect("read"), b"data");
        assert_eq!(fs::read_dir(&outside).expect("list").count(), 0);
    }

    #[test]
    fn leaf_symlink_is_replaced_not_written_through() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        fs::create_dir_all(root.join(".git/hooks")).expect("git dir");
        let target = root.join(".git/hooks/post-checkout");
        std::os::unix::fs::symlink(&target, root.join("hook")).expect("plant symlink");

        let leaf = WorktreeLeaf::open(None, &root, b"hook", 0o100755).expect("open leaf");
        // Without clearing, creation must fail rather than follow the link.
        assert!(leaf.create_file(b"pwned", 0o100755).is_err());
        assert!(!target.exists());
        leaf.remove_existing(None, refuse).expect("clear leaf");
        leaf.create_blob_body_or_symlink(0o100755, b"ok", b"ok")
            .expect("write file");
        assert!(!target.exists());
        assert_eq!(fs::read(root.join("hook")).expect("read"), b"ok");
    }

    #[test]
    fn a_component_that_is_dot_git_by_identity_is_refused() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        fs::create_dir_all(root.join(".git/hooks")).expect("git dir");
        // On a case-insensitive or NTFS volume `.GIT` / `GIT~1` open the
        // same directory as `.git`. Linux test filesystems have no such alias,
        // so bypass the name check and walk `.git` itself: only the identity
        // check stands between the walk and the hooks directory.
        let leaf = WorktreeLeaf::open_validated(
            None,
            &root,
            b".git/hooks/post-checkout",
            root.join(".git/hooks/post-checkout"),
        );
        assert!(leaf.is_err(), "walking through .git must be refused");
        assert!(!root.join(".git/hooks/post-checkout").exists());

        // The `.git` entry itself is refused as a leaf to replace.
        let leaf = WorktreeLeaf::open_validated(None, &root, b".git", root.join(".git"))
            .expect("open root leaf");
        assert!(leaf.remove_existing(None, refuse).is_err());
        assert!(leaf.ensure_dir(None, refuse, false).is_err());
        assert!(root.join(".git/hooks").is_dir());
    }

    #[test]
    fn unconditional_names_are_refused_before_touching_disk() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        for path in [&b".GIT/hooks/x"[..], b"a/../../x", b"./x", b"a//x", b""] {
            assert!(
                WorktreeLeaf::open(None, &root, path, 0o100644).is_err(),
                "{}",
                String::from_utf8_lossy(path)
            );
        }
        assert_eq!(fs::read_dir(&root).expect("list").count(), 1);
    }

    #[test]
    fn remove_dir_all_at_does_not_follow_inner_symlinks() {
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("root");
        let outside = base.path().join("outside");
        fs::create_dir_all(root.join(".git")).expect("git dir");
        fs::create_dir_all(root.join("d/e")).expect("tree");
        fs::create_dir(&outside).expect("outside");
        fs::write(outside.join("keep"), b"keep").expect("outside file");
        std::os::unix::fs::symlink(&outside, root.join("d/e/link")).expect("link");

        let leaf = WorktreeLeaf::open(None, &root, b"d", 0o100644).expect("open leaf");
        leaf.remove_existing(None, refuse).expect("remove subtree");
        assert!(!root.join("d").exists());
        assert_eq!(fs::read(outside.join("keep")).expect("kept"), b"keep");
    }
}

/// An existing path reached one component at a time from a held root handle.
/// Unlike the write walk, a non-directory parent is absent, never replaced.
struct ExistingWorktreeLeaf {
    dirs: Vec<cap_std::fs::Dir>,
    names: Vec<std::ffi::OsString>,
    paths: Vec<PathBuf>,
    path: PathBuf,
    dot_git: Option<same_file::Handle>,
}

impl ExistingWorktreeLeaf {
    fn open(root: &Path, path: &[u8]) -> Result<Option<Self>> {
        use cap_fs_ext::DirExt as _;
        // Sparse-index directory boundaries carry one trailing slash. They
        // name the existing directory itself, not a new tree-entry component.
        let path = path.strip_suffix(b"/").unwrap_or(path);
        verify_path_unconditional(path, 0o100644)?;
        let absolute = crate::index_io::worktree_path(root, path)?;
        let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        let dot_git = dir
            .open_dir_nofollow(".git")
            .ok()
            .and_then(|dir| same_file::Handle::from_file(dir.into_std_file()).ok());
        let names = path
            .split(|byte| *byte == b'/')
            .map(git_name_os_string)
            .collect::<Vec<_>>();
        let mut leaf = Self {
            dirs: vec![dir],
            names,
            paths: vec![root.to_path_buf()],
            path: absolute,
            dot_git,
        };
        for name in leaf.names.iter().take(leaf.names.len().saturating_sub(1)) {
            let parent = leaf.parent()?;
            let dir = match parent.open_dir_nofollow(name) {
                Ok(dir) => dir,
                Err(error) => {
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) || parent.symlink_metadata(name).is_ok_and(|metadata| {
                        !metadata.is_dir() || metadata.file_type().is_symlink()
                    }) {
                        return Ok(None);
                    }
                    return Err(error.into());
                }
            };
            leaf.refuse_dot_git(&dir)?;
            let mut current = leaf
                .paths
                .last()
                .ok_or_else(|| GitError::InvalidPath("missing root".into()))?
                .clone();
            current.push(name);
            leaf.paths.push(current);
            leaf.dirs.push(dir);
        }
        Ok(Some(leaf))
    }

    fn parent(&self) -> Result<&cap_std::fs::Dir> {
        self.dirs
            .last()
            .ok_or_else(|| GitError::InvalidPath("missing worktree parent".into()))
    }

    fn name(&self) -> Result<&std::ffi::OsStr> {
        self.names
            .last()
            .map(std::ffi::OsString::as_os_str)
            .ok_or_else(|| GitError::InvalidPath("missing worktree leaf".into()))
    }

    fn refuse_dot_git(&self, dir: &cap_std::fs::Dir) -> Result<()> {
        if let Some(dot_git) = &self.dot_git {
            let identity = same_file::Handle::from_file(dir.try_clone()?.into_std_file())?;
            if &identity == dot_git {
                return Err(crate::path_safety::invalid_path_error(
                    self.path.as_os_str().as_encoded_bytes(),
                ));
            }
        }
        Ok(())
    }

    fn remove(&self, original_cwd: Option<&Path>) -> Result<bool> {
        use cap_fs_ext::DirExt as _;
        let parent = self.parent()?;
        let name = self.name()?;
        let metadata = match parent.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            self.refuse_dot_git(&parent.open_dir_nofollow(name)?)?;
            if crate::index_io::path_is_original_cwd(original_cwd, &self.path) {
                return Ok(false);
            }
            // Gitlinks and directories with untracked content are never recursive.
            parent.remove_dir(name)
        } else {
            parent.remove_file_or_symlink(name)
        };
        match result {
            Ok(()) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::DirectoryNotEmpty
                        | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn prune(mut self, original_cwd: Option<&Path>) -> Result<()> {
        for position in (1..self.dirs.len()).rev() {
            let path = &self.paths[position];
            if crate::index_io::path_is_original_cwd(original_cwd, path) {
                break;
            }
            // Windows capability handles pin directories against deletion.
            // Release this child before rmdir while retaining its parent;
            // the removal still resolves only one name relative to that parent.
            drop(self.dirs.pop());
            let parent = self.parent()?;
            let name = &self.names[position - 1];
            match parent.remove_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

pub(crate) fn remove_worktree_entry(
    original_cwd: Option<&Path>,
    root: &Path,
    path: &[u8],
) -> Result<()> {
    if let Some(leaf) = ExistingWorktreeLeaf::open(root, path)?
        && leaf.remove(original_cwd)?
    {
        leaf.prune(original_cwd)?;
    }
    Ok(())
}

pub(crate) fn prune_worktree_dirs(
    original_cwd: Option<&Path>,
    root: &Path,
    dir: Option<&Path>,
) -> Result<()> {
    let Some(dir) = dir.filter(|dir| *dir != root) else {
        return Ok(());
    };
    let relative = dir
        .strip_prefix(root)
        .map_err(|_| GitError::InvalidPath(dir.display().to_string()))?;
    if let Some(leaf) = ExistingWorktreeLeaf::open(root, &git_path_bytes(relative))? {
        let can_prune = match leaf.parent()?.symlink_metadata(leaf.name()?) {
            Ok(metadata) => {
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && leaf.remove(original_cwd)?
            }
            // A move may already have removed this directory. Its held real
            // ancestors can still be empty and should be pruned as before.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error.into()),
        };
        if can_prune {
            leaf.prune(original_cwd)?;
        }
    }
    Ok(())
}

/// Compatibility path-only writer: all parents and the leaf are no-follow.
/// Keep the public signature while refusing symlinks rather than truncating
/// their targets. Existing regular files retain the historical overwrite behavior.
pub(crate) fn write_blob_at_path(
    file_path: &Path,
    mode: u32,
    body: &[u8],
    link_target: &[u8],
) -> Result<()> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    use std::io::Write as _;
    let absolute = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(file_path)
    };
    let mut root = PathBuf::new();
    for component in absolute.components() {
        if matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        ) {
            root.push(component.as_os_str());
        } else {
            break;
        }
    }
    let relative = absolute
        .strip_prefix(&root)
        .map_err(|_| GitError::InvalidPath(absolute.display().to_string()))?;
    let leaf = ExistingWorktreeLeaf::open(&root, &git_path_bytes(relative))?
        .ok_or_else(|| GitError::InvalidPath(file_path.display().to_string()))?;
    #[cfg(unix)]
    if mode & 0o170000 == 0o120000 {
        cap_fs_ext::DirExt::symlink(leaf.parent()?, os_str(link_target), leaf.name()?)?;
        return Ok(());
    }
    #[cfg(not(unix))]
    let _ = link_target;
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create(true).follow(FollowSymlinks::No);
    let mut file = leaf.parent()?.open_with(leaf.name()?, &options)?.into_std();
    if !file.metadata()?.is_file() {
        return Err(GitError::InvalidPath(file_path.display().to_string()));
    }
    file.set_len(0)?;
    file.write_all(body)?;
    set_handle_file_mode(&file, mode)
}

fn git_path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

fn git_name_os_string(name: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        std::ffi::OsString::from_vec(name.to_vec())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(name).into_owned().into()
    }
}
