//! Byte-preserving repository-relative path helpers.

use std::ffi::OsStr;
use std::path::{Component, Path};

use sley::{GitError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RepoPathBuf(Vec<u8>);

impl RepoPathBuf {
    pub(crate) fn from_path(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => {
                    if !bytes.is_empty() {
                        bytes.push(b'/');
                    }
                    bytes.extend_from_slice(os_str_bytes(value).as_ref());
                }
                Component::CurDir | Component::RootDir => {}
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(GitError::InvalidPath(format!(
                        "invalid repository path {}",
                        path.display()
                    )));
                }
            }
        }
        Ok(Self(bytes))
    }

    #[cfg(test)]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes()
}

#[cfg(not(unix))]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_normalizes_to_slash_separated_bytes() {
        let path = RepoPathBuf::from_path(Path::new("src//lib.rs")).expect("repo path");
        assert_eq!(path.as_bytes(), b"src/lib.rs");
    }

    #[cfg(windows)]
    #[test]
    fn repo_path_normalizes_windows_separators() {
        let path = RepoPathBuf::from_path(Path::new(r"src\lib.rs")).expect("repo path");
        assert_eq!(path.as_bytes(), b"src/lib.rs");
    }

    #[test]
    fn repo_path_ignores_root_and_current_components() {
        let path = RepoPathBuf::from_path(Path::new("/./src/./lib.rs")).expect("repo path");
        assert_eq!(path.as_bytes(), b"src/lib.rs");

        let current = RepoPathBuf::from_path(Path::new(".")).expect("repo path");
        assert_eq!(current.as_bytes(), b"");
    }

    #[test]
    fn repo_path_rejects_parent_components() {
        assert!(RepoPathBuf::from_path(Path::new("../outside")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn repo_path_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"src/\xff-name"));
        let path = RepoPathBuf::from_path(path).expect("repo path");
        assert_eq!(path.as_bytes(), b"src/\xff-name");
    }

    #[cfg(windows)]
    #[test]
    fn repo_path_rejects_prefix_components() {
        assert!(RepoPathBuf::from_path(Path::new(r"C:\repo\file")).is_err());
    }
}
