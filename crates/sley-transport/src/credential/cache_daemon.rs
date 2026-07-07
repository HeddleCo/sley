//! `git credential-cache--daemon` server.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sley_core::{GitError, Result};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use super::unix_socket::unix_stream_listen;

use super::url::credential_match;
use super::{
    credential_clear_secrets, credential_has_capability, credential_read,
    credential_set_all_capabilities, CredentialOpType, GitCredential, TIME_MAX,
};

pub(crate) struct CacheEntry {
    item: GitCredential,
    expiration: i64,
}

pub fn cmd_credential_cache_daemon(args: &[String]) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = args;
        return Err(GitError::Command(
            "credential-cache--daemon unavailable; no unix socket support".into(),
        ));
    }
    #[cfg(unix)]
    cmd_credential_cache_daemon_unix(args)
}

#[cfg(unix)]
fn cmd_credential_cache_daemon_unix(args: &[String]) -> Result<()> {
    let mut debug = false;
    let mut positional = Vec::new();
    for arg in args {
        if arg == "--debug" {
            debug = true;
        } else if arg.starts_with('-') {
            return Err(GitError::Command(format!(
                "credential-cache--daemon: unsupported option {arg}"
            )));
        } else {
            positional.push(arg.clone());
        }
    }
    let Some(socket_path) = positional.first() else {
        return Err(GitError::Command(
            "credential-cache--daemon requires socket path".into(),
        ));
    };
    if !Path::new(socket_path).is_absolute() {
        return Err(GitError::Command(
            "socket directory must be an absolute path".into(),
        ));
    }
    let socket_file = init_socket_directory(socket_path)?;
    serve_cache(&socket_file, debug)
}

#[cfg(unix)]
fn init_socket_directory(socket_path: &str) -> Result<PathBuf> {
    use std::fs::DirBuilder;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let path = Path::new(socket_path);
    let Some(parent) = path.parent() else {
        return Err(GitError::Command(
            "socket directory must be an absolute path".into(),
        ));
    };
    let Some(socket_file) = path.file_name() else {
        return Err(GitError::Command(
            "socket directory must be an absolute path".into(),
        ));
    };
    if parent.exists() {
        if let Ok(meta) = std::fs::metadata(parent) {
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(GitError::Command(format!(
                    "The permissions on your socket directory are too loose; other\n\
                     users may be able to read your cached credentials. Consider running:\n\
                     \n\
                     \tchmod 0700 {}",
                    parent.display()
                )));
            }
        }
    } else {
        if let Some(grandparent) = parent.parent() {
            std::fs::create_dir_all(grandparent).map_err(|err| GitError::Io(err.to_string()))?;
        }
        DirBuilder::new()
            .mode(0o700)
            .create(parent)
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    let _ = std::env::set_current_dir(parent);
    Ok(PathBuf::from(socket_file))
}

#[cfg(unix)]
struct SocketCleanup<'a> {
    socket_file: &'a Path,
}

#[cfg(unix)]
impl Drop for SocketCleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.socket_file);
    }
}

#[cfg(unix)]
fn unlink_socket(socket_file: &Path) {
    let _ = std::fs::remove_file(socket_file);
}

#[cfg(unix)]
fn serve_cache(socket_file: &Path, _debug: bool) -> Result<()> {
    use std::thread;
    use std::time::{Duration, Instant};

    let _socket_cleanup = SocketCleanup { socket_file };
    let listener =
        unix_stream_listen(socket_file).map_err(|err| GitError::Io(err.to_string()))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| GitError::Io(err.to_string()))?;
    {
        let mut out = io::stdout().lock();
        writeln!(out, "ok").map_err(|err| GitError::Io(err.to_string()))?;
        out.flush().map_err(|err| GitError::Io(err.to_string()))?;
    }
    let mut entries: Vec<CacheEntry> = Vec::new();
    let mut wait_for_entry_until = 0_i64;
    loop {
        let wakeup = check_expirations(&mut entries, &mut wait_for_entry_until);
        if wakeup == 0 {
            break;
        }
        let deadline = Instant::now() + Duration::from_secs(wakeup as u64);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    serve_one_client(stream, socket_file, &mut entries)?;
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    eprintln!("warning: accept failed: {err}");
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

#[cfg(unix)]
fn check_expirations(entries: &mut Vec<CacheEntry>, wait_for_entry_until: &mut i64) -> i64 {
    let current = now();
    if *wait_for_entry_until == 0 {
        *wait_for_entry_until = current + 30;
    }
    let mut i = 0;
    let mut next = TIME_MAX;
    while i < entries.len() {
        if entries[i].expiration <= current {
            credential_clear_secrets(&mut entries[i].item);
            entries.swap_remove(i);
            *wait_for_entry_until = current + 30;
        } else {
            if entries[i].expiration < next {
                next = entries[i].expiration;
            }
            i += 1;
        }
    }
    if entries.is_empty() {
        if *wait_for_entry_until <= current {
            return 0;
        }
        return (*wait_for_entry_until - current).max(1);
    }
    (next - current).max(1)
}

#[cfg(unix)]
fn serve_one_client(
    stream: UnixStream,
    socket_file: &Path,
    entries: &mut Vec<CacheEntry>,
) -> Result<()> {
    let mut reader = io::BufReader::new(
        stream
            .try_clone()
            .map_err(|err| GitError::Io(err.to_string()))?,
    );
    let mut writer = stream;
    let mut credential = GitCredential::default();
    let mut action = String::new();
    let mut timeout = -1_i32;
    if read_request(&mut reader, &mut credential, &mut action, &mut timeout).is_err() {
        return Ok(());
    }
    match action.as_str() {
        "get" => {
            if let Some(entry) = lookup_credential(entries, &credential) {
                write_get_response(&credential, entry, &mut writer)?;
            }
        }
        "exit" => {
            unlink_socket(socket_file);
            std::process::exit(0);
        }
        "erase" => remove_credential(entries, &credential, true),
        "store" => {
            if timeout < 0 {
                eprintln!("warning: cache client didn't specify a timeout");
            } else if (!credential.username.is_some() || !credential.password.is_some())
                && (!credential.authtype.is_some() || !credential.credential.is_some())
            {
                eprintln!("warning: cache client gave us a partial credential");
            } else if credential.ephemeral {
                eprintln!("warning: not storing ephemeral credential");
            } else {
                remove_credential(entries, &credential, false);
                cache_credential(entries, credential, timeout);
            }
        }
        other => eprintln!("warning: cache client sent unknown action: {other}"),
    }
    Ok(())
}

#[cfg(unix)]
fn write_get_response(
    request: &GitCredential,
    entry: &CacheEntry,
    writer: &mut UnixStream,
) -> Result<()> {
    let item = &entry.item;
    writeln!(writer, "capability[]=authtype").map_err(|err| GitError::Io(err.to_string()))?;
    if let Some(username) = &item.username {
        writeln!(writer, "username={username}").map_err(|err| GitError::Io(err.to_string()))?;
    }
    if let Some(password) = &item.password {
        writeln!(writer, "password={password}").map_err(|err| GitError::Io(err.to_string()))?;
    }
    if credential_has_capability(&request.capa_authtype, CredentialOpType::Response) {
        if let Some(authtype) = &item.authtype {
            writeln!(writer, "authtype={authtype}").map_err(|err| GitError::Io(err.to_string()))?;
        }
        if let Some(token) = &item.credential {
            writeln!(writer, "credential={token}").map_err(|err| GitError::Io(err.to_string()))?;
        }
    }
    if item.password_expiry_utc != TIME_MAX {
        writeln!(writer, "password_expiry_utc={}", item.password_expiry_utc)
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    if let Some(token) = &item.oauth_refresh_token {
        writeln!(writer, "oauth_refresh_token={token}")
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_request(
    reader: &mut impl BufRead,
    credential: &mut GitCredential,
    action: &mut String,
    timeout: &mut i32,
) -> Result<()> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let Some(rest) = line.strip_prefix("action=") else {
        return Err(GitError::InvalidFormat(format!(
            "client sent bogus action line: {}",
            line.trim_end()
        )));
    };
    action.push_str(rest.trim_end());
    line.clear();
    reader.read_line(&mut line)?;
    let Some(rest) = line.strip_prefix("timeout=") else {
        return Err(GitError::InvalidFormat(format!(
            "client sent bogus timeout line: {}",
            line.trim_end()
        )));
    };
    *timeout = rest.trim_end().parse().unwrap_or(-1);
    credential_set_all_capabilities(credential, CredentialOpType::Initial);
    credential_read(credential, reader, CredentialOpType::Helper)
}

#[cfg(unix)]
fn cache_credential(entries: &mut Vec<CacheEntry>, credential: GitCredential, timeout: i32) {
    entries.push(CacheEntry {
        item: credential,
        expiration: now() + i64::from(timeout),
    });
}

#[cfg(unix)]
fn lookup_credential<'a>(
    entries: &'a [CacheEntry],
    credential: &GitCredential,
) -> Option<&'a CacheEntry> {
    entries
        .iter()
        .find(|entry| credential_match(credential, &entry.item, false))
}

#[cfg(unix)]
fn remove_credential(entries: &mut Vec<CacheEntry>, credential: &GitCredential, match_password: bool) {
    let current = now();
    for entry in entries.iter_mut() {
        if credential_match(credential, &entry.item, match_password) {
            entry.expiration = current;
        }
    }
}