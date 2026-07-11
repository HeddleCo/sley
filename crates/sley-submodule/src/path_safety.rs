//! Submodule worktree path traversal safety.

use sley_core::{GitError, Result};
use std::io;
use std::path::{Component, Path};
use std::{fs, iter::Peekable};

/// Whether any leading component of a repository-relative submodule path is a
/// symbolic link.
///
/// The final component is intentionally excluded: checkout may legitimately
/// replace a tracked symlink with a gitlink directory. Following a symlink in
/// any parent component would escape the intended worktree location and is
/// rejected by callers.
pub fn submodule_path_has_symlink_parent(
    worktree_root: &Path,
    relative_path: &Path,
) -> Result<bool> {
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(GitError::InvalidPath(format!(
            "invalid submodule worktree path {}",
            relative_path.display()
        )));
    }
    let mut components: Peekable<_> = relative_path.components().peekable();
    let mut current = worktree_root.to_path_buf();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            continue;
        };
        if components.peek().is_none() {
            break;
        }
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn allows_final_symlink_but_rejects_symlinked_parent() {
        let root = std::env::temp_dir().join(format!(
            "sley-submodule-path-safety-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("target")).expect("create symlink target");
        std::os::unix::fs::symlink("target", root.join("final")).expect("create final symlink");
        std::os::unix::fs::symlink("target", root.join("parent")).expect("create parent symlink");

        assert!(
            !submodule_path_has_symlink_parent(&root, Path::new("final"))
                .expect("inspect final symlink")
        );
        assert!(
            submodule_path_has_symlink_parent(&root, Path::new("parent/submodule"))
                .expect("inspect symlinked parent")
        );

        fs::remove_dir_all(root).expect("clean fixture");
    }
}
