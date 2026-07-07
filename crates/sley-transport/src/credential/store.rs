//! `git credential-store` — read/write `~/.git-credentials`.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use sley_core::{GitError, Result};

use super::url::{
    credential_from_url_gently, credential_match, percent_encode, EncodeMode,
};
use super::{credential_read, CredentialOpType, GitCredential};

pub struct CredentialStoreOptions {
    pub file: Option<PathBuf>,
}

pub fn cmd_credential_store(args: &[String]) -> Result<()> {
    let mut file: Option<PathBuf> = None;
    let mut positional = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--file" {
            idx += 1;
            let Some(path) = args.get(idx) else {
                return Err(GitError::Command("credential-store --file requires a value".into()));
            };
            file = Some(PathBuf::from(path));
        } else if let Some(path) = arg.strip_prefix("--file=") {
            file = Some(PathBuf::from(path));
        } else if arg.starts_with('-') {
            return Err(GitError::Command(format!(
                "credential-store: unsupported option {arg}"
            )));
        } else {
            positional.push(arg.clone());
        }
        idx += 1;
    }
    if positional.len() != 1 {
        return Err(GitError::Command(
            "usage: git credential-store [<options>] <action>".into(),
        ));
    }
    let op = &positional[0];
    let files = store_file_list(file)?;
    if files.is_empty() {
        return Err(GitError::Command(
            "unable to set up default path; use --file".into(),
        ));
    }
    let mut credential = GitCredential::default();
    credential_read(&mut credential, &mut io::stdin().lock(), CredentialOpType::Helper)?;
    match op.as_str() {
        "get" => lookup_credential(&files, &mut credential)?,
        "erase" => remove_credential(&files, &credential)?,
        "store" => store_credential(&files, &credential)?,
        _ => {}
    }
    Ok(())
}

fn store_file_list(file: Option<PathBuf>) -> Result<Vec<PathBuf>> {
    if let Some(file) = file {
        return Ok(vec![file]);
    }
    let mut files = Vec::new();
    if let Some(home) = home_dir() {
        files.push(home.join(".git-credentials"));
    }
    if let Some(path) = xdg_config_home() {
        files.push(path.join("credentials"));
    }
    Ok(files)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn xdg_config_home() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path).join("git"));
    }
    home_dir().map(|home| home.join(".config").join("git"))
}

fn lookup_credential(files: &[PathBuf], credential: &mut GitCredential) -> Result<()> {
    for file in files {
        if parse_credential_file(file, credential, true, None, false)? {
            return Ok(());
        }
    }
    Ok(())
}

fn remove_credential(files: &[PathBuf], credential: &GitCredential) -> Result<()> {
    if credential.protocol.is_none()
        && credential.host.is_none()
        && credential.path.is_none()
        && credential.username.is_none()
    {
        return Ok(());
    }
    for file in files {
        if file.exists() {
            rewrite_credential_file(file, credential, None, true)?;
        }
    }
    Ok(())
}

fn store_credential(files: &[PathBuf], credential: &GitCredential) -> Result<()> {
    if credential.protocol.is_none()
        || (credential.host.is_none() && credential.path.is_none())
        || credential.username.is_none()
        || credential.password.is_none()
    {
        return Ok(());
    }
    for file in files {
        if file.exists() {
            let line = store_line(credential)?;
            rewrite_credential_file(file, credential, Some(line), false)?;
            return Ok(());
        }
    }
    if let Some(file) = files.first() {
        let line = store_line(credential)?;
        rewrite_credential_file(file, credential, Some(line), false)?;
    }
    Ok(())
}

fn store_line(credential: &GitCredential) -> Result<String> {
    let protocol = credential
        .protocol
        .as_deref()
        .ok_or_else(|| GitError::InvalidFormat("credential store missing protocol".into()))?;
    let mut out = format!("{protocol}://");
    if let Some(username) = credential.username.as_deref() {
        percent_encode(username, EncodeMode::Unreserved, &mut out);
    }
    out.push(':');
    if let Some(password) = credential.password.as_deref() {
        percent_encode(password, EncodeMode::Unreserved, &mut out);
    }
    out.push('@');
    if let Some(host) = credential.host.as_deref() {
        percent_encode(host, EncodeMode::Unreserved, &mut out);
    }
    if let Some(path) = credential.path.as_deref() {
        out.push('/');
        percent_encode(path, EncodeMode::StorePath, &mut out);
    }
    Ok(out)
}

fn parse_credential_file(
    path: &Path,
    credential: &GitCredential,
    want_match: bool,
    mut other_cb: Option<&mut dyn FnMut(&str) -> Result<()>>,
    match_password: bool,
) -> Result<bool> {
    let Ok(file) = fs::File::open(path) else {
        return Ok(false);
    };
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        if credential_from_url_gently(&line, true, true).is_ok() {
            let entry = credential_from_url_gently(&line, true, true)?;
            if entry.username.is_some()
                && entry.password.is_some()
                && credential_match(credential, &entry, match_password)
            {
                if want_match {
                    print_entry(&entry)?;
                    return Ok(true);
                }
                return Ok(true);
            }
        }
        if let Some(cb) = other_cb.as_deref_mut() {
            cb(&line)?;
        }
        line.clear();
    }
    Ok(false)
}

fn print_entry(entry: &GitCredential) -> Result<()> {
    if let Some(username) = &entry.username {
        writeln!(io::stdout(), "username={username}")?;
    }
    if let Some(password) = &entry.password {
        writeln!(io::stdout(), "password={password}")?;
    }
    Ok(())
}

fn rewrite_credential_file(
    path: &Path,
    credential: &GitCredential,
    extra: Option<String>,
    match_password: bool,
) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|err| GitError::Io(err.to_string()))?;
    }
    let mut out = Vec::new();
    if let Some(extra) = extra {
        out.extend_from_slice(extra.as_bytes());
        out.push(b'\n');
    }
    let mut sink = |line: &str| -> Result<()> {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
        Ok(())
    };
    parse_credential_file(path, credential, false, Some(&mut sink), match_password)?;
    fs::write(path, out).map_err(|err| GitError::Io(err.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(path, perms);
        }
    }
    Ok(())
}