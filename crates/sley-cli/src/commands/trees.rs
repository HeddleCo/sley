//! Tree-construction plumbing commands.

use sley::plumbing::sley_worktree;
use std::env;

use sley::{GitError, Result};

use crate::repository_object_format;

pub(crate) fn cmd_write_tree(args: &[String]) -> Result<()> {
    let mut missing_ok = false;
    let mut prefix = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--missing-ok" => missing_ok = true,
            "--no-missing-ok" => missing_ok = false,
            "--no-prefix" => prefix = None,
            "--prefix" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--prefix requires a value".into()));
                };
                prefix = Some(value.as_bytes().to_vec());
            }
            value => {
                if let Some(value) = value.strip_prefix("--prefix=") {
                    prefix = Some(value.as_bytes().to_vec());
                    idx += 1;
                    continue;
                }
                return Err(GitError::Command(format!(
                    "unsupported write-tree option {value}"
                )));
            }
        }
        idx += 1;
    }
    let git_dir = crate::session::cli_git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let oid = sley_worktree::write_tree_from_index_with_options(
        git_dir,
        format,
        sley_worktree::WriteTreeOptions { missing_ok, prefix },
    )?;
    println!("{oid}");
    Ok(())
}
