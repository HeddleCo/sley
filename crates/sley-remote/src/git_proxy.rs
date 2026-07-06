//! `core.gitproxy` / `GIT_PROXY_COMMAND` support for native `git://` transport.
//!
//! Mirrors upstream `connect.c`'s `git_use_proxy` / `git_proxy_connect`: when a
//! proxy command is configured, spawn it with `(host, port)` and speak the git
//! daemon protocol over the proxy's stdin/stdout instead of opening a TCP socket.

use std::env;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use sley_config::GitConfig;
use sley_core::{GitError, Result};

/// A `git://` transport stream: either a direct TCP connection or a proxy child.
pub(crate) struct GitConnection {
    inner: GitConnectionInner,
}

enum GitConnectionInner {
    Tcp(TcpStream),
    Proxy {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: ChildStdout,
    },
}

impl Read for GitConnection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            GitConnectionInner::Tcp(stream) => stream.read(buf),
            GitConnectionInner::Proxy { stdout, .. } => stdout.read(buf),
        }
    }
}

impl Write for GitConnection {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            GitConnectionInner::Tcp(stream) => stream.write(buf),
            GitConnectionInner::Proxy { stdin, .. } => stdin
                .as_mut()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "proxy stdin closed")
                })?
                .write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            GitConnectionInner::Tcp(stream) => stream.flush(),
            GitConnectionInner::Proxy { stdin, .. } => stdin
                .as_mut()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "proxy stdin closed")
                })?
                .flush(),
        }
    }
}

impl GitConnection {
    pub(crate) fn shutdown(&mut self, how: Shutdown) -> std::io::Result<()> {
        match &mut self.inner {
            GitConnectionInner::Tcp(stream) => stream.shutdown(how),
            GitConnectionInner::Proxy { stdin, .. } => {
                if matches!(how, Shutdown::Write | Shutdown::Both) {
                    drop(stdin.take());
                }
                Ok(())
            }
        }
    }
}

impl Drop for GitConnection {
    fn drop(&mut self) {
        if let GitConnectionInner::Proxy { child, .. } = &mut self.inner {
            let _ = child.wait();
        }
    }
}

/// Open a `git://` byte stream to `host`:`port`, using `core.gitproxy` when set.
pub(crate) fn connect_git_transport(
    config: Option<&GitConfig>,
    host: &str,
    port: u16,
) -> Result<GitConnection> {
    validate_git_daemon_host(host)?;
    let port_string = port.to_string();
    validate_git_daemon_port(&port_string)?;

    if let Some(command) = resolve_git_proxy_command(config, host) {
        return spawn_git_proxy(&command, host, &port_string);
    }

    let stream = TcpStream::connect((host, port))?;
    Ok(GitConnection {
        inner: GitConnectionInner::Tcp(stream),
    })
}

fn spawn_git_proxy(command: &str, host: &str, port: &str) -> Result<GitConnection> {
    let mut child = Command::new(command)
        .arg(host)
        .arg(port)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Upstream git inherits stderr; piping without draining deadlocks when the
        // proxy is chatty (same class of bug as SSH service stderr in ssh.rs).
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| GitError::Command(format!("cannot start proxy {command}: {err}")))?;
    let stdin = child.stdin.take().ok_or_else(|| {
        GitError::Command(format!("proxy {command} stdin was not piped"))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        GitError::Command(format!("proxy {command} stdout was not piped"))
    })?;
    Ok(GitConnection {
        inner: GitConnectionInner::Proxy {
            child,
            stdin: Some(stdin),
            stdout,
        },
    })
}

/// Resolve the proxy executable for `host`, mirroring `git_use_proxy`.
pub(crate) fn resolve_git_proxy_command(
    config: Option<&GitConfig>,
    host: &str,
) -> Option<String> {
    if let Ok(command) = env::var("GIT_PROXY_COMMAND")
        && !command.is_empty()
    {
        return Some(command);
    }
    let config = config?;
    // connect.c's git_proxy_command_options overwrites on every matching line while
    // repo_config walks the file in order — last matching entry wins.
    let mut resolved = None;
    for value in config.get_all("core", None, "gitproxy") {
        let Some(value) = value else {
            continue;
        };
        if let Some(command) = match_git_proxy_value(value, host) {
            resolved = Some(command);
        }
    }
    resolved
}

fn match_git_proxy_value(value: &str, host: &str) -> Option<String> {
    let command = if let Some(for_pos) = value.find(" for ") {
        let host_pattern = &value[for_pos + 5..];
        if !host_matches_git_proxy_pattern(host, host_pattern) {
            return None;
        }
        &value[..for_pos]
    } else {
        value
    };
    if command == "none" {
        return None;
    }
    Some(command.to_string())
}

fn host_matches_git_proxy_pattern(host: &str, pattern: &str) -> bool {
    let pattern_len = pattern.len();
    if host.len() < pattern_len {
        return false;
    }
    if &host[host.len() - pattern_len..] != pattern {
        return false;
    }
    host.len() == pattern_len
        || host.as_bytes().get(host.len() - pattern_len - 1) == Some(&b'.')
}

fn validate_git_daemon_host(host: &str) -> Result<()> {
    if looks_like_command_line_option(host) {
        return Err(GitError::Command(format!(
            "strange hostname '{host}' blocked"
        )));
    }
    Ok(())
}

fn validate_git_daemon_port(port: &str) -> Result<()> {
    if looks_like_command_line_option(port) {
        return Err(GitError::Command(format!("strange port '{port}' blocked")));
    }
    Ok(())
}

fn looks_like_command_line_option(value: &str) -> bool {
    value.starts_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_proxy_value_matches_suffix_and_rejects_none() {
        assert_eq!(
            match_git_proxy_value("proxy-cmd for example.com", "git.example.com").as_deref(),
            Some("proxy-cmd")
        );
        assert_eq!(
            match_git_proxy_value("proxy-cmd for example.com", "example.com").as_deref(),
            Some("proxy-cmd")
        );
        assert!(match_git_proxy_value("proxy-cmd for example.com", "notexample.com").is_none());
        assert!(match_git_proxy_value("none for example.com", "example.com").is_none());
        assert_eq!(
            match_git_proxy_value("default-proxy", "any.host").as_deref(),
            Some("default-proxy")
        );
        assert!(match_git_proxy_value("none", "any.host").is_none());
    }

    #[test]
    fn blocked_hostnames_are_rejected() {
        assert!(validate_git_daemon_host("-remote").is_err());
        assert!(validate_git_daemon_host("example.com").is_ok());
    }

    #[test]
    fn gitproxy_last_match_wins() {
        let config = GitConfig::parse(
            b"\
[core]\n\
\tgitproxy = first-proxy for example.com\n\
\tgitproxy = second-proxy for example.com\n\
\tgitproxy = fallback-proxy\n\
",
        )
        .expect("config");
        // Universal fallback is last and matches every host (connect.c semantics).
        assert_eq!(
            resolve_git_proxy_command(Some(&config), "git.example.com").as_deref(),
            Some("fallback-proxy")
        );
        let config = GitConfig::parse(
            b"\
[core]\n\
\tgitproxy = fallback-proxy\n\
\tgitproxy = host-proxy for example.com\n\
",
        )
        .expect("config");
        assert_eq!(
            resolve_git_proxy_command(Some(&config), "git.example.com").as_deref(),
            Some("host-proxy")
        );
    }
}
