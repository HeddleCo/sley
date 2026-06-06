//! Byte-preserving repository-relative path helpers.

use std::path::{Component, Path};

use sley_core::{GitError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RepoPathBuf(Vec<u8>);

impl RepoPathBuf {
    pub(crate) fn from_path(path: &Path) -> Result<Self> {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(GitError::InvalidPath(format!(
                "invalid repository path {}",
                path.display()
            )));
        }
        let text = path
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        Ok(Self(text.into_bytes()))
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_normalizes_to_slash_separated_bytes() {
        let path = RepoPathBuf::from_path(Path::new("src/lib.rs")).expect("repo path");
        assert_eq!(path.into_bytes(), b"src/lib.rs");
    }

    #[test]
    fn repo_path_rejects_parent_components() {
        assert!(RepoPathBuf::from_path(Path::new("../outside")).is_err());
    }
}
