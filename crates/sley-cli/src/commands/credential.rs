//! `git credential` and credential helper builtins.

use std::env;
use std::fs;
use std::io::{self};
use std::path::{Path, PathBuf};

use sley::plumbing::sley_config;
use sley::{GitError, Result};
use sley_transport::{
    CredentialOpType, GitCredential, cmd_credential_cache as transport_cmd_credential_cache,
    cmd_credential_cache_daemon as transport_cmd_credential_cache_daemon,
    cmd_credential_store as transport_cmd_credential_store, credential_announce_capabilities,
    credential_approve, credential_fill, credential_next_state, credential_read, credential_reject,
    credential_set_all_capabilities, credential_write,
};

use crate::injected_config_parameters;
use crate::repo_paths::common_git_dir_for_git_dir;

pub(crate) fn cmd_credential(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    if args.len() != 1 {
        eprintln!("usage: git credential (fill|approve|reject)");
        return Err(GitError::Exit(129));
    }
    let op = &args[0];
    if op == "capability" {
        let mut credential = GitCredential::default();
        credential_set_all_capabilities(&mut credential, CredentialOpType::Initial);
        credential_announce_capabilities(&credential, &mut io::stdout().lock())?;
        return Ok(());
    }
    let stack = load_credential_config_stack(cli_session)?;
    let mut credential = GitCredential::default();
    credential_read(
        &mut credential,
        &mut io::stdin().lock(),
        CredentialOpType::Initial,
    )?;
    match op.as_str() {
        "fill" => {
            credential_fill(None, Some(&stack), &mut credential, false)?;
            credential_next_state(&mut credential);
            credential_write(
                &credential,
                &mut io::stdout().lock(),
                CredentialOpType::Response,
            )?;
        }
        "approve" => {
            credential_set_all_capabilities(&mut credential, CredentialOpType::Helper);
            credential_approve(None, Some(&stack), &mut credential)?;
        }
        "reject" => {
            credential_set_all_capabilities(&mut credential, CredentialOpType::Helper);
            credential_reject(None, Some(&stack), &mut credential)?;
        }
        _ => {
            eprintln!("usage: git credential (fill|approve|reject)");
            return Err(GitError::Exit(129));
        }
    }
    Ok(())
}

pub(crate) fn cmd_credential_store(args: &[String]) -> Result<()> {
    transport_cmd_credential_store(args)
}

pub(crate) fn cmd_credential_cache(args: &[String]) -> Result<()> {
    transport_cmd_credential_cache(args)
}

pub(crate) fn cmd_credential_cache_daemon(args: &[String]) -> Result<()> {
    transport_cmd_credential_cache_daemon(args)
}

fn load_credential_config_stack(
    cli_session: &crate::session::CliSession,
) -> Result<sley_config::ConfigStack> {
    let context = credential_include_context(cli_session);
    let mut stack = sley_config::ConfigStack::new();
    for (path, scope) in sley_config::default_config_layer_paths() {
        let _ = stack.push_file(&path, scope, true, &context);
    }
    if let Ok(git_dir) = cli_session.git_dir()
        && let Ok(common) = common_git_dir_for_git_dir(&git_dir)
    {
        let local_path = config_display_path(common.join("config"));
        let _ = stack.push_file(&local_path, sley_config::ConfigScope::Local, true, &context);
        if worktree_config_extension_enabled(&common) {
            let worktree_path = config_display_path(git_dir.join("config.worktree"));
            let _ = stack.push_file(
                &worktree_path,
                sley_config::ConfigScope::Worktree,
                true,
                &context,
            );
        }
    }
    if let Ok(parameters) = injected_config_parameters() {
        let _ = stack.push_parameters_with_includes(&parameters, &context);
    }
    Ok(stack)
}

fn credential_include_context(
    cli_session: &crate::session::CliSession,
) -> sley_config::ConfigIncludeContext {
    let cwd = cli_session.cwd();
    let _start = logical_cwd_for_include_context(cwd);
    match cli_session.git_dir() {
        Ok(git_dir) => sley_config::ConfigIncludeContext::new(
            Some(sley_config::git_dir_for_include_context(&git_dir)),
            repo_current_branch_name(&git_dir),
        ),
        Err(_) => sley_config::ConfigIncludeContext::default(),
    }
}

fn logical_cwd_for_include_context(cwd: &Path) -> PathBuf {
    let Some(pwd) = env::var_os("PWD").map(PathBuf::from) else {
        return cwd.to_path_buf();
    };
    if !pwd.is_absolute() {
        return cwd.to_path_buf();
    }
    match (fs::canonicalize(&pwd), fs::canonicalize(cwd)) {
        (Ok(pwd_real), Ok(cwd_real)) if pwd_real == cwd_real => pwd,
        _ => cwd.to_path_buf(),
    }
}

fn repo_current_branch_name(git_dir: &Path) -> Option<String> {
    let head = git_dir.join("HEAD");
    let contents = fs::read_to_string(head).ok()?;
    let reference = contents.trim();
    reference
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

fn worktree_config_extension_enabled(common_git_dir: &Path) -> bool {
    let config_path = common_git_dir.join("config");
    let Ok(bytes) = fs::read(&config_path) else {
        return false;
    };
    let Ok(config) = sley_config::GitConfig::parse(&bytes) else {
        return false;
    };
    config
        .get_bool("extensions", None, "worktreeConfig")
        .unwrap_or(false)
}

fn config_display_path(path: PathBuf) -> PathBuf {
    if let Ok(cwd) = env::current_dir()
        && let Ok(relative) = path.strip_prefix(&cwd)
    {
        return relative.to_path_buf();
    }
    path
}
