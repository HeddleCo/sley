//! `git credential-cache` client.

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sley_core::{GitError, Result};

use super::unix_socket::unix_stream_connect;

use super::{
    CredentialOpType, GitCredential, credential_announce_capabilities,
    credential_set_all_capabilities,
};

const FLAG_SPAWN: u8 = 0x1;
const FLAG_RELAY: u8 = 0x2;

pub fn cmd_credential_cache(args: &[String]) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = args;
        return Err(GitError::Command(
            "credential-cache unavailable; no unix socket support".into(),
        ));
    }
    #[cfg(unix)]
    cmd_credential_cache_unix(args)
}

#[cfg(unix)]
fn cmd_credential_cache_unix(args: &[String]) -> Result<()> {
    let mut socket_path: Option<PathBuf> = None;
    let mut timeout = 900_i32;
    let mut positional = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--timeout" {
            idx += 1;
            let Some(value) = args.get(idx) else {
                return Err(GitError::Command(
                    "credential-cache --timeout requires a value".into(),
                ));
            };
            timeout = value
                .parse()
                .map_err(|_| GitError::Command(format!("invalid timeout: {value}")))?;
        } else if let Some(value) = arg.strip_prefix("--timeout=") {
            timeout = value
                .parse()
                .map_err(|_| GitError::Command(format!("invalid timeout: {value}")))?;
        } else if arg == "--socket" {
            idx += 1;
            let Some(value) = args.get(idx) else {
                return Err(GitError::Command(
                    "credential-cache --socket requires a value".into(),
                ));
            };
            socket_path = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--socket=") {
            socket_path = Some(PathBuf::from(value));
        } else if arg.starts_with('-') {
            return Err(GitError::Command(format!(
                "credential-cache: unsupported option {arg}"
            )));
        } else {
            positional.push(arg.clone());
        }
        idx += 1;
    }
    if positional.is_empty() {
        return Err(GitError::Command(
            "usage: git credential-cache [<options>] <action>".into(),
        ));
    }
    let op = &positional[0];
    let socket_path = socket_path.unwrap_or_else(default_socket_path);
    if socket_path.as_os_str().is_empty() {
        return Err(GitError::Command(
            "fatal: unable to find a suitable socket path; use --socket".into(),
        ));
    }
    match op.as_str() {
        "exit" => do_cache(&socket_path, op, timeout, 0)?,
        "get" | "erase" => do_cache(&socket_path, op, timeout, FLAG_RELAY)?,
        "store" => do_cache(&socket_path, op, timeout, FLAG_RELAY | FLAG_SPAWN)?,
        "capability" => announce_capabilities()?,
        _ => {}
    }
    Ok(())
}

#[cfg(unix)]
fn announce_capabilities() -> Result<()> {
    let mut credential = GitCredential::default();
    credential_set_all_capabilities(&mut credential, CredentialOpType::Initial);
    credential_announce_capabilities(&credential, &mut io::stdout().lock())?;
    Ok(())
}

#[cfg(unix)]
fn default_socket_path() -> PathBuf {
    if let Some(old) = home_dir().map(|home| home.join(".git-credential-cache"))
        && old.is_dir()
    {
        return old.join("socket");
    }
    xdg_cache_socket_path()
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(unix)]
fn xdg_cache_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path)
            .join("git")
            .join("credential")
            .join("socket");
    }
    home_dir()
        .map(|home| {
            home.join(".cache")
                .join("git")
                .join("credential")
                .join("socket")
        })
        .unwrap_or_else(|| PathBuf::from("/tmp/git-credential-cache"))
}

#[cfg(unix)]
fn do_cache(socket: &Path, action: &str, timeout: i32, flags: u8) -> Result<()> {
    let mut buf = format!("action={action}\ntimeout={timeout}\n");
    if flags & FLAG_RELAY != 0 {
        // This is local credential-helper input, not a remote response. The
        // credential protocol specifies no maximum for secret or extension
        // values, so a compatible ceiling needs a documented per-credential
        // value limit rather than borrowing an unrelated transport budget.
        let mut relay = Vec::new();
        io::stdin()
            .read_to_end(&mut relay)
            .map_err(|err| GitError::Io(err.to_string()))?;
        buf.push_str(&String::from_utf8_lossy(&relay));
    }
    if send_request(socket, buf.as_bytes()).is_err() {
        if connection_fatally_broken() {
            return Err(GitError::Io("unable to connect to cache daemon".into()));
        }
        if flags & FLAG_SPAWN != 0 {
            spawn_daemon(socket)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if send_request(socket, buf.as_bytes()).is_ok() {
                    break;
                }
                if connection_fatally_broken() {
                    return Err(GitError::Io("unable to connect to cache daemon".into()));
                }
                if Instant::now() >= deadline {
                    return Err(GitError::Io("unable to connect to cache daemon".into()));
                }
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn connection_fatally_broken() -> bool {
    let kind = io::Error::last_os_error().kind();
    kind != io::ErrorKind::NotFound && kind != io::ErrorKind::ConnectionRefused
}

#[cfg(unix)]
fn send_request(socket: &Path, out: &[u8]) -> Result<bool> {
    let mut stream = unix_stream_connect(socket).map_err(|err| {
        let _ = err;
        io::Error::last_os_error()
    })?;
    stream
        .write_all(out)
        .map_err(|err| GitError::Io(err.to_string()))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|err| GitError::Io(err.to_string()))?;
    let mut got_data = false;
    let mut buf = [0_u8; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                io::stdout()
                    .write_all(&buf[..n])
                    .map_err(|err| GitError::Io(err.to_string()))?;
                got_data = true;
            }
            Err(err) if connection_closed(&err) => break,
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    Ok(got_data)
}

#[cfg(unix)]
fn connection_closed(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
    )
}

#[cfg(unix)]
fn daemon_started(buf: &[u8]) -> bool {
    buf.windows(3).any(|window| window == b"ok\n")
}

#[cfg(unix)]
fn spawn_daemon(socket: &Path) -> Result<()> {
    let exe = std::env::current_exe().map_err(|err| GitError::Io(err.to_string()))?;
    let mut command = Command::new(exe);
    command
        .arg("credential-cache--daemon")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|err| GitError::Io(err.to_string()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Io("cache daemon stdout was not piped".into()))?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = Vec::new();
        let _ = io::BufReader::new(&mut stdout).read_until(b'\n', &mut line);
        let _ = tx.send(line);
    });
    let line = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(line) => line,
        Err(_) => {
            let _ = child.try_wait();
            return Err(GitError::Command("cache daemon did not start: ".into()));
        }
    };
    if daemon_started(&line) {
        Ok(())
    } else {
        Err(GitError::Command(format!(
            "cache daemon did not start: {}",
            String::from_utf8_lossy(&line)
        )))
    }
}
