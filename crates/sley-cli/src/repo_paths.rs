use sley::{GitError, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::session;

pub(crate) fn common_git_dir_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    if !session::cli_session().is_some_and(|session| session.local_repo_env_hidden())
        && let Some(common_dir) = env::var_os("GIT_COMMON_DIR")
    {
        return Ok(PathBuf::from(common_dir));
    }
    let commondir = git_dir.join("commondir");
    if commondir.is_file() {
        let value = fs::read_to_string(&commondir)?;
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        return fs::canonicalize(common).map_err(|err| GitError::Io(err.to_string()));
    }
    fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()))
}
