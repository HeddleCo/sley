//! Interactive credential prompting via core.askPass / GIT_ASKPASS.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};

use sley_config::{ConfigStack, GitConfig};
use sley_core::{GitError, Result};

use super::GitCredential;
use super::url::{credential_describe, credential_format};

pub(crate) fn credential_getpass(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    credential: &mut GitCredential,
) -> Result<bool> {
    if let Some(interactive) = config_bool(config, stack, "credential", "interactive") {
        if !interactive {
            return Ok(true);
        }
        if let Some(value) = config_string(config, stack, "credential", "interactive")
            && value == "never"
        {
            return Ok(true);
        }
    }
    if credential.username.is_none() {
        credential.username = Some(credential_ask_one(
            "Username", credential, true, config, stack,
        )?);
    }
    if credential.password.is_none() {
        credential.password = Some(credential_ask_one(
            "Password", credential, false, config, stack,
        )?);
    }
    Ok(false)
}

pub(crate) fn die_unable_to_get_password() -> Result<()> {
    Err(GitError::Command("unable to get password from user".into()))
}

fn credential_ask_one(
    what: &str,
    credential: &GitCredential,
    echo: bool,
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
) -> Result<String> {
    let desc = if credential.sanitize_prompt {
        credential_format(credential)
    } else {
        credential_describe(credential)
    };
    let prompt = if desc.is_empty() {
        format!("{what}: ")
    } else {
        format!("{what} for '{desc}': ")
    };
    run_askpass(&prompt, echo, config, stack)
}

fn run_askpass(
    prompt: &str,
    echo: bool,
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
) -> Result<String> {
    if let Some(program) = resolve_askpass_program(config, stack) {
        let mut command = Command::new(program);
        command.arg(prompt);
        if !echo {
            command.env("GIT_TERMINAL_PROMPT", "0");
        }
        return read_prompt_output(&mut command);
    }
    eprint!("{prompt}");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| GitError::Io(err.to_string()))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn resolve_askpass_program(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
) -> Option<OsString> {
    if let Some(program) = std::env::var_os("GIT_ASKPASS").filter(|value| !value.is_empty()) {
        return Some(program);
    }
    if let Some(program) = config_string(config, stack, "core", "askpass") {
        return Some(OsString::from(program));
    }
    std::env::var_os("SSH_ASKPASS").filter(|value| !value.is_empty())
}

fn read_prompt_output(command: &mut Command) -> Result<String> {
    command.stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| GitError::Io(err.to_string()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Io("askpass stdout was not piped".into()))?;
    let mut output = String::new();
    stdout
        .read_to_string(&mut output)
        .map_err(|err| GitError::Io(err.to_string()))?;
    let status = child.wait().map_err(|err| GitError::Io(err.to_string()))?;
    if !status.success() {
        return Err(GitError::Command("askpass failed".into()));
    }
    Ok(output.trim_end_matches(['\n', '\r']).to_string())
}

fn config_bool(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    section: &str,
    key: &str,
) -> Option<bool> {
    if let Some(stack) = stack {
        return stack.get_bool(section, None, key);
    }
    config.and_then(|config| config.get_bool(section, None, key))
}

fn config_string(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    section: &str,
    key: &str,
) -> Option<String> {
    if let Some(stack) = stack {
        return stack
            .get(section, None, key)
            .and_then(|entry| entry.value.clone());
    }
    config
        .and_then(|config| config.get(section, None, key))
        .map(str::to_string)
}
