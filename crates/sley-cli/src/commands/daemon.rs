//! `git daemon`: a minimal TCP server speaking the git daemon protocol.
//!
//! Mirrors `daemon.c`. On a connection the client sends a single pkt-line
//! `git-upload-pack <path>\0host=<host>\0\0version=2\0`; the daemon splits the
//! service + path off the first NUL, collects the post-second-NUL extra args
//! into `GIT_PROTOCOL`, resolves `<base-path>/<path>`, and runs the requested
//! service (`upload-pack` / `receive-pack`) against that repository by
//! re-executing this binary with the connection wired to the child's stdio.
//!
//! Scope: enough to serve the `git://` transport for the upstream protocol-v2
//! suite (`--base-path`, `--export-all`, `--enable=receive-pack`,
//! `--listen`/`--port`/`--reuseaddr`/`--pid-file`/`--verbose`). Interpolated
//! paths, user-relative paths (`~user`), syslog, and the inetd path are not
//! implemented.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sley_core::{GitError, Result};
use sley_protocol::{PktLineFrame, read_pkt_line_frame};

/// Parsed `git daemon` invocation.
struct DaemonOptions {
    listen: String,
    port: u16,
    base_path: Option<PathBuf>,
    export_all: bool,
    enable_receive_pack: bool,
    pid_file: Option<PathBuf>,
    verbose: bool,
    /// The trailing positional directory (the document root); requests resolve
    /// under it when no `--base-path` is given.
    root: Option<PathBuf>,
}

impl DaemonOptions {
    fn document_root(&self) -> Option<&Path> {
        self.base_path
            .as_deref()
            .or(self.root.as_deref())
    }
}

fn parse_options(args: &[String]) -> Result<DaemonOptions> {
    let mut opts = DaemonOptions {
        listen: "127.0.0.1".to_string(),
        port: 9418,
        base_path: None,
        export_all: false,
        enable_receive_pack: false,
        pid_file: None,
        verbose: false,
        root: None,
    };
    for arg in args {
        match arg.as_str() {
            "--export-all" => opts.export_all = true,
            "--verbose" => opts.verbose = true,
            "--reuseaddr" => {} // SO_REUSEADDR is always set below.
            "--informative-errors" | "--no-informative-errors" => {}
            "--strict-paths" => {}
            value if value.starts_with("--listen=") => {
                opts.listen = value["--listen=".len()..].to_string();
            }
            value if value.starts_with("--port=") => {
                opts.port = value["--port=".len()..]
                    .parse()
                    .map_err(|_| GitError::Command(format!("invalid daemon port: {value}")))?;
            }
            value if value.starts_with("--base-path=") => {
                opts.base_path = Some(PathBuf::from(&value["--base-path=".len()..]));
            }
            value if value.starts_with("--pid-file=") => {
                opts.pid_file = Some(PathBuf::from(&value["--pid-file=".len()..]));
            }
            value if value.starts_with("--enable=") => {
                if &value["--enable=".len()..] == "receive-pack" {
                    opts.enable_receive_pack = true;
                }
            }
            value if value.starts_with("--timeout=") || value.starts_with("--init-timeout=") => {}
            value if value.starts_with("--max-connections=") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "daemon: unsupported option {value}"
                )));
            }
            value => {
                if opts.root.is_some() {
                    return Err(GitError::Command(
                        "daemon: multiple document roots are not supported".into(),
                    ));
                }
                opts.root = Some(PathBuf::from(value));
            }
        }
    }
    Ok(opts)
}

/// The `[<pid>] <msg>` verbose log line `daemon.c::logreport` writes to stderr;
/// `lib-git-daemon.sh` greps the first such line for `Ready to rumble`.
fn loginfo(opts: &DaemonOptions, msg: &str) {
    if opts.verbose {
        eprintln!("[{}] {msg}", std::process::id());
    }
}

pub(crate) fn cmd_daemon(args: &[String]) -> Result<()> {
    let opts = parse_options(args)?;

    let listener = bind_listener(&opts.listen, opts.port)?;

    if let Some(pid_file) = &opts.pid_file {
        std::fs::write(pid_file, format!("{}\n", std::process::id()))?;
    }

    // The readiness banner MUST be the first verbose line: the test harness
    // reads it from the daemon's stderr fifo and only proceeds once it matches
    // `[<pid>] Ready to rumble`.
    loginfo(&opts, "Ready to rumble");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        // Serve connections serially: the upstream suite issues one request at a
        // time, and serial handling keeps the in-process refs/odb access free of
        // cross-connection races.
        if let Err(err) = handle_connection(&opts, stream) {
            loginfo(&opts, &format!("request failed: {err}"));
        }
    }
    Ok(())
}

fn bind_listener(listen: &str, port: u16) -> Result<TcpListener> {
    // TcpListener::bind sets SO_REUSEADDR-equivalent behavior on most platforms
    // via the standard library; bind directly to the requested host:port.
    let listener = TcpListener::bind((listen, port))
        .map_err(|err| GitError::Command(format!("daemon: cannot bind {listen}:{port}: {err}")))?;
    Ok(listener)
}

/// One request: read the service line, resolve the repo, run the service.
fn handle_connection(opts: &DaemonOptions, mut stream: TcpStream) -> Result<()> {
    let Some(PktLineFrame::Data(payload)) = read_pkt_line_frame(&mut stream)? else {
        return Ok(());
    };

    // `git-upload-pack <path>\0host=<h>\0\0version=2\0`
    // The service line ends at the first NUL (or the trailing LF); everything
    // after it is extra args, each NUL-terminated.
    let line_end = payload.iter().position(|&b| b == 0).unwrap_or(payload.len());
    let mut line = &payload[..line_end];
    if line.last() == Some(&b'\n') {
        line = &line[..line.len() - 1];
    }
    let line = std::str::from_utf8(line)
        .map_err(|_| GitError::InvalidFormat("daemon request is not valid UTF-8".into()))?;

    let git_protocol = parse_extra_args(&payload[line_end..]);

    // Match `git-<service> <path>`.
    let (service, path) = parse_service_line(line)?;
    let receive = match service {
        "upload-pack" => false,
        "receive-pack" => {
            if !opts.enable_receive_pack {
                write_error(&mut stream, "service not enabled")?;
                return Ok(());
            }
            true
        }
        other => {
            write_error(&mut stream, &format!("unknown service git-{other}"))?;
            return Ok(());
        }
    };

    let repo = resolve_repository(opts, path)?;
    let Some(repo) = repo else {
        write_error(&mut stream, "access denied or repository not exported")?;
        return Ok(());
    };

    loginfo(opts, &format!("Request {service} for '{}'", repo.display()));

    run_service(&repo, receive, git_protocol.as_deref(), stream)
}

/// Split `git-<service> <path>` into `(service, path)`.
fn parse_service_line(line: &str) -> Result<(&str, &str)> {
    let rest = line
        .strip_prefix("git-")
        .ok_or_else(|| GitError::InvalidFormat(format!("daemon: bad request line: {line}")))?;
    let (service, path) = rest
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat(format!("daemon: bad request line: {line}")))?;
    Ok((service, path))
}

/// Collect the post-first-NUL extra args (`host=`, `version=2`, ...) into the
/// colon-joined `GIT_PROTOCOL` value, dropping the `host=` arg (handled
/// separately by upstream; we have no vhost routing). Mirrors
/// `daemon.c::parse_extra_args`.
fn parse_extra_args(extra: &[u8]) -> Option<String> {
    let mut protocol_parts: Vec<String> = Vec::new();
    // `extra` begins with the NUL that terminated the service line; iterate the
    // NUL-separated tokens that follow.
    for token in extra.split(|&b| b == 0) {
        if token.is_empty() {
            continue;
        }
        let Ok(arg) = std::str::from_utf8(token) else {
            continue;
        };
        if let Some(rest) = arg.strip_prefix("host=") {
            let _ = rest; // No virtual-host routing; ignore.
            continue;
        }
        protocol_parts.push(arg.to_string());
    }
    if protocol_parts.is_empty() {
        None
    } else {
        Some(protocol_parts.join(":"))
    }
}

/// Resolve the request `path` against the document root, enforcing
/// `--export-all` / `git-daemon-export-ok`. Returns `None` when the repository
/// is not exported. Mirrors `daemon.c::run_service`'s `path_ok` + export check.
fn resolve_repository(opts: &DaemonOptions, path: &str) -> Result<Option<PathBuf>> {
    let root = opts.document_root().ok_or_else(|| {
        GitError::Command("daemon: no --base-path or document root configured".into())
    })?;
    // The wire path is absolute relative to the base-path: `/parent` => join.
    let rel = path.trim_start_matches('/');
    let mut repo = root.to_path_buf();
    repo.push(rel);

    // Resolve to a real repository: accept either a bare repo dir or a worktree
    // with `.git`. We canonicalize to guard against `..` traversal outside root.
    let repo = match std::fs::canonicalize(&repo) {
        Ok(repo) => repo,
        Err(_) => return Ok(None),
    };
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !repo.starts_with(&canon_root) {
        return Ok(None);
    }

    if !opts.export_all && !repo.join("git-daemon-export-ok").exists() {
        // For a worktree the export marker lives under `.git`.
        let dotgit = repo.join(".git");
        if !dotgit.join("git-daemon-export-ok").exists() {
            return Ok(None);
        }
    }
    Ok(Some(repo))
}

/// Run the resolved service against `repo` by re-executing this binary
/// (`sley upload-pack <repo>` / `sley receive-pack <repo>`) with the connection
/// as the child's stdio and `GIT_PROTOCOL` set. Re-exec keeps the daemon free
/// of the service's refs/odb state and reuses the already-tested command paths.
fn run_service(
    repo: &Path,
    receive: bool,
    git_protocol: Option<&str>,
    stream: TcpStream,
) -> Result<()> {
    let self_exe = std::env::current_exe()
        .map_err(|err| GitError::Command(format!("daemon: cannot locate self: {err}")))?;
    let service = if receive {
        "receive-pack"
    } else {
        "upload-pack"
    };

    let read_half = stream
        .try_clone()
        .map_err(|err| GitError::Command(format!("daemon: cannot clone connection: {err}")))?;

    // Wire the connection to the child's stdio. On Unix a `TcpStream` converts
    // to `Stdio` through its `OwnedFd`.
    use std::os::fd::OwnedFd;
    let child_stdin: Stdio = OwnedFd::from(read_half).into();
    let child_stdout: Stdio = OwnedFd::from(stream).into();

    let mut command = Command::new(self_exe);
    command
        .arg(service)
        .arg(repo)
        .stdin(child_stdin)
        .stdout(child_stdout)
        .stderr(Stdio::null())
        .env_remove("GIT_PROTOCOL");
    if let Some(protocol) = git_protocol {
        command.env("GIT_PROTOCOL", protocol);
    }

    let mut child = command
        .spawn()
        .map_err(|err| GitError::Command(format!("daemon: cannot spawn {service}: {err}")))?;
    let _ = child.wait();
    Ok(())
}

/// Write a `ERR <msg>` pkt-line to the client (the format git's clients expect
/// for a daemon-side refusal).
fn write_error(stream: &mut TcpStream, msg: &str) -> Result<()> {
    let body = format!("ERR {msg}");
    let frame = PktLineFrame::data(format!("{body}\n").into_bytes())?;
    let encoded = frame.encode();
    stream.write_all(&encoded)?;
    stream.flush()?;
    let _ = read_drain(stream);
    Ok(())
}

fn read_drain(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    Ok(())
}
