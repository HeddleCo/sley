//! Git-style command alias resolution before dispatch.
//!
//! Mirrors upstream git's `alias.c`: non-built-in command names are looked up in
//! the `alias.*` config namespace, expanded (simple or shell `!` aliases), and
//! re-dispatched with a recursion limit.

use sley_config::{ConfigIncludeContext, GitConfig};
use sley_core::{GitError, Result};
use std::env;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::commands::remote_cmds::repo_current_branch_name;
use crate::{
    common_git_dir_for_git_dir, discover_git_dir, global_config_value, worktree_root_for_git_dir,
};

pub(crate) const MAX_ALIAS_DEPTH: usize = 20;

/// Result of resolving a user command name against `alias.*` config.
pub(crate) enum AliasExpansion {
    /// No alias defined for this command name.
    None,
    /// Expand to a new argv prefix (remaining CLI args are appended by the caller).
    Args(Vec<String>),
    /// Run a shell command (`!`-prefixed alias); remaining CLI args are appended.
    Shell(String),
}

/// Whether `command` is a built-in git command (built-ins are never aliased).
pub(crate) fn is_builtin_command(command: &str) -> bool {
    matches!(
        command,
        "init"
            | "add"
            | "archive"
            | "branch"
            | "bundle"
            | "hash-object"
            | "index-pack"
            | "cat-file"
            | "checkout"
            | "check-attr"
            | "check-ignore"
            | "check-mailmap"
            | "check-ref-format"
            | "clean"
            | "clone"
            | "config"
            | "count-objects"
            | "gc"
            | "maintenance"
            | "repack"
            | "apply"
            | "commit"
            | "commit-graph"
            | "commit-tree"
            | "diff"
            | "fetch"
            | "for-each-ref"
            | "fsck"
            | "get-tar-commit-id"
            | "ls-remote"
            | "ls-files"
            | "ls-tree"
            | "log"
            | "merge"
            | "merge-base"
            | "fmt-merge-msg"
            | "pull"
            | "rebase"
            | "cherry-pick"
            | "revert"
            | "mktree"
            | "multi-pack-index"
            | "mv"
            | "pack-refs"
            | "prune"
            | "prune-packed"
            | "push"
            | "receive-pack"
            | "upload-pack"
            | "write-tree"
            | "worktree"
            | "update-index"
            | "update-ref"
            | "rev-parse"
            | "rev-list"
            | "reflog"
            | "remote"
            | "replace"
            | "rerere"
            | "reset"
            | "restore"
            | "rm"
            | "show-ref"
            | "show-index"
            | "stripspace"
            | "stash"
            | "submodule"
            | "symbolic-ref"
            | "status"
            | "switch"
            | "tag"
            | "testkit"
            | "unpack-file"
            | "update-server-info"
            | "var"
            | "verify-pack"
            | "version"
            | "-v"
            | "--version"
            | "show"
            | "blame"
            | "annotate"
            | "describe"
            | "shortlog"
            | "grep"
            | "notes"
            | "bisect"
            | "sparse-checkout"
            | "format-patch"
            | "am"
            | "read-tree"
            | "checkout-index"
            | "diff-tree"
            | "diff-index"
            | "diff-files"
            | "merge-tree"
            | "merge-file"
            | "name-rev"
            | "show-branch"
            | "verify-commit"
            | "verify-tag"
            | "mktag"
            | "patch-id"
            | "interpret-trailers"
    )
}

/// Look up `alias.<name>` in file config and `-c`/`GIT_CONFIG_*` overrides.
pub(crate) fn expand_alias(command: &str) -> Result<AliasExpansion> {
    let key = format!("alias.{command}");
    if let Some(value) = global_config_value(&key)? {
        return Ok(alias_value_to_expansion(&value));
    }
    let config = load_alias_file_config()?;
    let Some(value) = config.get("alias", None, command) else {
        return Ok(AliasExpansion::None);
    };
    Ok(alias_value_to_expansion(value))
}

fn alias_value_to_expansion(value: &str) -> AliasExpansion {
    if let Some(shell) = value.strip_prefix('!') {
        return AliasExpansion::Shell(shell.to_string());
    }
    AliasExpansion::Args(split_alias_value(value))
}

fn load_alias_file_config() -> Result<GitConfig> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd).ok();
    let common_git_dir = git_dir
        .as_ref()
        .and_then(|dir| common_git_dir_for_git_dir(dir).ok());
    let branch = git_dir
        .as_ref()
        .and_then(|dir| repo_current_branch_name(dir));
    let context = ConfigIncludeContext::new(common_git_dir.clone(), branch);
    sley_config::load_pre_dispatch_config(common_git_dir.as_deref(), &context)
}

/// Execute a `!`-prefixed alias through git's shell path.
pub(crate) fn run_shell_alias(command: &str, extra_args: &[String]) -> Result<()> {
    let shell = match env::var("GIT_SHELL_PATH") {
        Ok(shell) => shell,
        Err(_) => "/bin/sh".into(),
    };
    let mut script = command.to_string();
    for arg in extra_args {
        script.push(' ');
        script.push_str(&shell_escape(arg));
    }
    let mut process = ProcessCommand::new(&shell);
    process.arg("-c").arg(&script);
    configure_shell_alias_worktree_env(&mut process)?;
    // Propagate the effective config-injection parameters (`-c` / `--config-env`
    // folded onto any inherited `GIT_CONFIG_PARAMETERS`) to the subprocess git,
    // exactly as upstream git does by mutating its own env before running the
    // shell alias. This is what makes `git -c x.one=1 <shell-alias-that-runs-git>`
    // see x.one inside the alias's child git.
    if let Some(params) = crate::effective_config_parameters_env() {
        process.env("GIT_CONFIG_PARAMETERS", params);
    }
    let status = process
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        let code = match status.code() {
            Some(code) => code,
            None => 1,
        };
        Err(GitError::Exit(code))
    }
}

fn configure_shell_alias_worktree_env(process: &mut ProcessCommand) -> Result<()> {
    let cwd = env::current_dir()?;
    let Ok(git_dir) = discover_git_dir(&cwd) else {
        return Ok(());
    };
    let Ok(root) = worktree_root_for_git_dir(&git_dir) else {
        return Ok(());
    };
    let root = canonical_or_self(root);
    let cwd = canonical_or_self(cwd);
    let Ok(prefix_path) = cwd.strip_prefix(&root) else {
        return Ok(());
    };
    process.current_dir(&root);
    let prefix = git_prefix_from_path(prefix_path);
    if prefix.is_empty() {
        process.env_remove("GIT_PREFIX");
    } else {
        process.env("GIT_PREFIX", prefix);
    }
    Ok(())
}

fn canonical_or_self(path: PathBuf) -> PathBuf {
    match fs::canonicalize(&path) {
        Ok(canonical) => canonical,
        Err(_) => path,
    }
}

fn git_prefix_from_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", path.to_string_lossy().replace('\\', "/"))
    }
}

/// Split an alias value into words, respecting single- and double-quoted spans.
fn split_alias_value(value: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' => {
                if !current.is_empty() {
                    args.push(mem::take(&mut current));
                }
            }
            '\'' => {
                while let Some(c) = chars.next() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            '"' => {
                while let Some(c) = chars.next() {
                    if c == '"' {
                        break;
                    }
                    current.push(c);
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        return "''".into();
    }
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_alias_value_respects_quotes() {
        assert_eq!(
            split_alias_value("log --pretty=format:'%h %s'"),
            vec!["log", "--pretty=format:%h %s"]
        );
        assert_eq!(split_alias_value("status -s"), vec!["status", "-s"]);
    }

    #[test]
    fn builtin_commands_are_not_aliased() {
        assert!(is_builtin_command("init"));
        assert!(is_builtin_command("config"));
        assert!(!is_builtin_command("aliasedinit"));
        assert!(!is_builtin_command("hello-world"));
    }
}
