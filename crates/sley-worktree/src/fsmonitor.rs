//! Native lifecycle and IPC session for `fsmonitor--daemon`.
//!
//! This module deliberately owns the daemon protocol below the CLI.  The
//! current session is a correctness-first implementation: it speaks Git's
//! simple-IPC pkt-line framing and conservatively invalidates the entire
//! worktree for every query.  A platform watcher can later replace that
//! response policy without changing process lifecycle or the public API.

use sley_core::{GitError, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Whether the repository's fsmonitor IPC endpoint accepts connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsmonitorDaemonState {
    Listening,
    NotListening,
}

/// A repository-scoped fsmonitor daemon session.
///
/// The endpoint is `$GIT_DIR/fsmonitor--daemon.ipc`, matching Git on local
/// Unix filesystems.  Liveness is always determined by connecting to that
/// endpoint; no pid or marker file is treated as authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsmonitorDaemonSession {
    git_dir: PathBuf,
    socket_path: PathBuf,
    token: String,
}

impl FsmonitorDaemonSession {
    pub fn new(git_dir: impl AsRef<Path>) -> Self {
        let git_dir = std::fs::canonicalize(git_dir.as_ref())
            .unwrap_or_else(|_| git_dir.as_ref().to_path_buf());
        let socket_path = git_dir.join("fsmonitor--daemon.ipc");
        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let token = format!("builtin:sley-{}-{epoch_nanos}:1", std::process::id());
        Self {
            git_dir,
            socket_path,
            token,
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Probe the real IPC endpoint.
    pub fn state(&self) -> Result<FsmonitorDaemonState> {
        platform::state(&self.socket_path)
    }

    /// Run the blocking IPC service until a client sends `quit`.
    ///
    /// Queries receive a Git protocol-v2 token followed by `/`, Git's
    /// correctness-preserving instruction to invalidate the whole worktree.
    pub fn serve(&self) -> Result<()> {
        platform::serve(&self.socket_path, &self.token)
    }

    /// Send Git's simple-IPC `quit` request and wait for the endpoint to stop.
    pub fn request_stop(&self, timeout: Duration) -> Result<()> {
        platform::request_stop(&self.socket_path)?;
        if self.wait_for_state(FsmonitorDaemonState::NotListening, timeout)? {
            Ok(())
        } else {
            Err(GitError::Io(format!(
                "fsmonitor daemon did not stop within {} seconds",
                timeout.as_secs()
            )))
        }
    }

    /// Poll until `wanted` is observed or `timeout` expires.
    pub fn wait_for_state(&self, wanted: FsmonitorDaemonState, timeout: Duration) -> Result<bool> {
        let started = Instant::now();
        loop {
            if self.state()? == wanted {
                return Ok(true);
            }
            if started.elapsed() >= timeout {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, symlink};
    use std::os::unix::net::{UnixListener, UnixStream};

    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    const CLIENT_READ_TIMEOUT: Duration = Duration::from_millis(100);

    pub(super) fn state(path: &Path) -> Result<FsmonitorDaemonState> {
        let address = endpoint_address(path)?;
        match UnixStream::connect(&address) {
            Ok(stream) => {
                // A state probe never sends a command. Close it explicitly so
                // the server can distinguish the probe from an idle client.
                drop(stream);
                Ok(FsmonitorDaemonState::Listening)
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) || fs::symlink_metadata(path)
                    .is_ok_and(|metadata| !metadata.file_type().is_socket()) =>
            {
                Ok(FsmonitorDaemonState::NotListening)
            }
            Err(err) => Err(GitError::Io(format!(
                "could not connect to fsmonitor IPC endpoint '{}': {err}",
                path.display()
            ))),
        }
    }

    pub(super) fn serve(path: &Path, token: &str) -> Result<()> {
        if state(path)? == FsmonitorDaemonState::Listening {
            return Err(GitError::Io(format!(
                "fsmonitor daemon is already listening on '{}'",
                path.display()
            )));
        }
        remove_stale_endpoint(path)?;
        let alias = prepare_endpoint_alias(path)?;
        let address = alias.join(
            path.file_name()
                .ok_or_else(|| GitError::Io("invalid fsmonitor IPC path".into()))?,
        );
        let listener = UnixListener::bind(&address).map_err(|err| {
            GitError::Io(format!(
                "could not create fsmonitor IPC endpoint '{}': {err}",
                path.display()
            ))
        })?;
        let _cleanup = SocketCleanup {
            endpoint: path.to_path_buf(),
            alias,
        };
        listener
            .set_nonblocking(true)
            .map_err(|err| GitError::Io(err.to_string()))?;

        loop {
            // A Scalar enlistment can be deleted without an explicit `stop`.
            // Do not leave an unreachable background process behind once its
            // repository or socket has disappeared.
            if path.parent().is_none_or(|git_dir| !git_dir.exists()) || !path.exists() {
                return Ok(());
            }
            let mut stream = match listener.accept() {
                Ok((stream, _address)) => stream,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(err) => {
                    return Err(GitError::Io(format!(
                        "could not accept fsmonitor IPC client: {err}"
                    )));
                }
            };
            stream
                .set_nonblocking(true)
                .map_err(|err| GitError::Io(err.to_string()))?;
            let request = read_packetized(&mut stream);
            stream
                .set_nonblocking(false)
                .map_err(|err| GitError::Io(err.to_string()))?;
            match request {
                Ok(Some(command)) if command == b"quit" => {
                    write_flush(&mut stream)?;
                    return Ok(());
                }
                Ok(Some(_command)) => {
                    // Git's trivial response: a fresh protocol-v2 token and
                    // `/`, which tells the client to invalidate every path.
                    let mut response = Vec::with_capacity(token.len() + 3);
                    response.extend_from_slice(token.as_bytes());
                    response.push(0);
                    response.extend_from_slice(b"/\0");
                    write_packet(&mut stream, &response)?;
                    write_flush(&mut stream)?;
                }
                Ok(None) => {
                    // `ipc_get_active_state()` probes by connecting and then
                    // closing without a command.  It is not a protocol error.
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => {
                    return Err(GitError::Io(format!(
                        "could not read fsmonitor IPC request: {err}"
                    )));
                }
            }
        }
    }

    pub(super) fn request_stop(path: &Path) -> Result<()> {
        let address = endpoint_address(path)?;
        let mut stream = UnixStream::connect(address)
            .map_err(|err| GitError::Io(format!("fsmonitor--daemon is not running: {err}")))?;
        write_packet(&mut stream, b"quit")?;
        write_flush(&mut stream)?;
        // The server replies with a flush packet and closes the connection.
        let _ = read_packetized(&mut stream);
        Ok(())
    }

    fn remove_stale_endpoint(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(GitError::Io(format!(
                "could not remove stale fsmonitor IPC endpoint '{}': {err}",
                path.display()
            ))),
        }
    }

    /// Return a short socket address that resolves to the endpoint in the Git
    /// directory. Git temporarily `chdir(2)`s for the same reason; a stable
    /// symlink avoids mutating process-global cwd in an embeddable library.
    fn endpoint_address(path: &Path) -> Result<PathBuf> {
        let alias = endpoint_alias(path)?;
        Ok(alias.join(
            path.file_name()
                .ok_or_else(|| GitError::Io("invalid fsmonitor IPC path".into()))?,
        ))
    }

    fn endpoint_alias(path: &Path) -> Result<PathBuf> {
        let git_dir = path
            .parent()
            .ok_or_else(|| GitError::Io("invalid fsmonitor IPC path".into()))?;
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in git_dir.as_os_str().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let alias = PathBuf::from(format!("/tmp/.sley-fsmonitor-{hash:016x}"));
        match fs::symlink_metadata(&alias) {
            Ok(metadata) if !metadata.file_type().is_symlink() => Err(GitError::Io(format!(
                "fsmonitor IPC alias '{}' is not a symbolic link",
                alias.display()
            ))),
            Ok(_) => {
                let target = match fs::read_link(&alias) {
                    Ok(target) => target,
                    // The daemon removes its endpoint alias during shutdown.
                    // Treat a removal between the metadata probe and readlink
                    // like Git's IPC_STATE__PATH_NOT_FOUND, not an I/O error.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(alias),
                    Err(err) => return Err(GitError::Io(err.to_string())),
                };
                if target == git_dir {
                    Ok(alias)
                } else {
                    Err(GitError::Io(format!(
                        "fsmonitor IPC alias '{}' points to an unexpected repository",
                        alias.display()
                    )))
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(alias),
            Err(err) => Err(GitError::Io(format!(
                "could not inspect fsmonitor IPC alias '{}': {err}",
                alias.display()
            ))),
        }
    }

    fn prepare_endpoint_alias(path: &Path) -> Result<PathBuf> {
        let alias = endpoint_alias(path)?;
        if fs::symlink_metadata(&alias).is_err() {
            let git_dir = path
                .parent()
                .ok_or_else(|| GitError::Io("invalid fsmonitor IPC path".into()))?;
            symlink(git_dir, &alias).map_err(|err| {
                GitError::Io(format!(
                    "could not create fsmonitor IPC alias '{}': {err}",
                    alias.display()
                ))
            })?;
        }
        Ok(alias)
    }

    struct SocketCleanup {
        endpoint: PathBuf,
        alias: PathBuf,
    }

    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.endpoint);
            let _ = fs::remove_file(&self.alias);
        }
    }

    fn read_packetized(stream: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
        let mut result = Vec::new();
        let deadline = Instant::now() + CLIENT_READ_TIMEOUT;
        loop {
            let mut header = [0_u8; 4];
            match read_exact_until(stream, &mut header, deadline) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof && result.is_empty() => {
                    return Ok(None);
                }
                Err(err) => return Err(err),
            }
            let header = std::str::from_utf8(&header)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pkt-line"))?;
            let length = usize::from_str_radix(header, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pkt-line"))?;
            if length == 0 {
                return Ok(Some(result));
            }
            if length < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid pkt-line length",
                ));
            }
            let payload_len = length - 4;
            if result.len().saturating_add(payload_len) > MAX_REQUEST_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fsmonitor IPC request is too large",
                ));
            }
            let offset = result.len();
            result.resize(offset + payload_len, 0);
            read_exact_until(stream, &mut result[offset..], deadline)?;
        }
    }

    fn read_exact_until(
        stream: &mut UnixStream,
        buffer: &mut [u8],
        deadline: Instant,
    ) -> io::Result<()> {
        let mut offset = 0;
        while offset < buffer.len() {
            match stream.read(&mut buffer[offset..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "fsmonitor IPC client closed the connection",
                    ));
                }
                Ok(count) => offset += count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "fsmonitor IPC client did not send a request",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn write_packet(stream: &mut UnixStream, payload: &[u8]) -> Result<()> {
        let length = payload.len().saturating_add(4);
        if length > 0xffff {
            return Err(GitError::Io("fsmonitor IPC packet is too large".into()));
        }
        stream
            .write_all(format!("{length:04x}").as_bytes())
            .and_then(|()| stream.write_all(payload))
            .map_err(|err| GitError::Io(format!("could not write fsmonitor IPC response: {err}")))
    }

    fn write_flush(stream: &mut UnixStream) -> Result<()> {
        stream
            .write_all(b"0000")
            .map_err(|err| GitError::Io(format!("could not flush fsmonitor IPC response: {err}")))
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;

    fn unsupported() -> GitError {
        GitError::Unsupported(
            "native fsmonitor daemon IPC is not implemented on this platform".into(),
        )
    }

    pub(super) fn state(_path: &Path) -> Result<FsmonitorDaemonState> {
        Err(unsupported())
    }

    pub(super) fn serve(_path: &Path, _token: &str) -> Result<()> {
        Err(unsupported())
    }

    pub(super) fn request_stop(_path: &Path) -> Result<()> {
        Err(unsupported())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::mpsc;

    // Generous deadlines: under full-workspace parallel load the daemon
    // thread can exceed tight bounds before binding its socket; genuine
    // startup failures still surface immediately via the done-channel.
    const START_DEADLINE: Duration = Duration::from_secs(30);
    const STOP_DEADLINE: Duration = Duration::from_secs(30);

    #[test]
    fn daemon_lifecycle_uses_a_live_endpoint() {
        let root = tempfile::Builder::new()
            .prefix("sley-fsm-")
            .tempdir_in("/tmp")
            .expect("temp directory");
        let git_dir = root.path().join(".git");
        fs::create_dir(&git_dir).expect("git directory");
        let session = FsmonitorDaemonSession::new(&git_dir);
        assert_eq!(
            session.state().expect("initial state"),
            FsmonitorDaemonState::NotListening
        );

        let server = session.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            done_tx.send(server.serve()).expect("report server result");
        });
        let listening = session
            .wait_for_state(FsmonitorDaemonState::Listening, START_DEADLINE)
            .expect("wait for start");
        assert!(
            listening,
            "server exited before listening: {:?}",
            done_rx.try_recv()
        );
        session.request_stop(STOP_DEADLINE).expect("stop daemon");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server stopped")
            .expect("server succeeded");
        assert!(!session.socket_path().exists());
    }

    #[test]
    fn stale_endpoint_is_replaced_before_serving() {
        let root = tempfile::Builder::new()
            .prefix("sley-fsm-")
            .tempdir_in("/tmp")
            .expect("temp directory");
        let git_dir = root.path().join(".git");
        fs::create_dir(&git_dir).expect("git directory");
        let session = FsmonitorDaemonSession::new(&git_dir);
        fs::write(session.socket_path(), b"stale").expect("stale endpoint");

        let server = session.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = server.serve();
            let _ = done_tx.send(result.clone());
            result
        });
        let listening = session
            .wait_for_state(FsmonitorDaemonState::Listening, START_DEADLINE)
            .expect("wait for start");
        assert!(
            listening,
            "server exited before listening: {:?}",
            done_rx.try_recv()
        );
        session.request_stop(STOP_DEADLINE).expect("stop daemon");
        handle.join().expect("join daemon").expect("daemon result");
    }

    #[test]
    fn long_git_directory_uses_a_short_address_without_changing_the_endpoint() {
        let root = tempfile::Builder::new()
            .prefix("sley-fsm-")
            .tempdir_in("/tmp")
            .expect("temp directory");
        let git_dir = root
            .path()
            .join("a-very-long-enlistment-directory-name")
            .join("another-long-component-for-the-worktree")
            .join("yet-another-component")
            .join(".git");
        fs::create_dir_all(&git_dir).expect("git directory");
        let session = FsmonitorDaemonSession::new(&git_dir);
        assert!(session.socket_path().as_os_str().as_bytes().len() > 104);

        let server = session.clone();
        let handle = std::thread::spawn(move || server.serve());
        assert!(
            session
                .wait_for_state(FsmonitorDaemonState::Listening, START_DEADLINE)
                .expect("wait for start")
        );
        assert!(session.socket_path().exists());
        session.request_stop(STOP_DEADLINE).expect("stop daemon");
        handle.join().expect("join daemon").expect("daemon result");
    }

    #[test]
    fn daemon_exits_when_its_repository_is_deleted() {
        let root = tempfile::Builder::new()
            .prefix("sley-fsm-")
            .tempdir_in("/tmp")
            .expect("temp directory");
        let git_dir = root.path().join(".git");
        fs::create_dir(&git_dir).expect("git directory");
        let session = FsmonitorDaemonSession::new(&git_dir);
        let server = session.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            done_tx.send(server.serve()).expect("report server result");
        });
        assert!(
            session
                .wait_for_state(FsmonitorDaemonState::Listening, START_DEADLINE)
                .expect("wait for start")
        );

        fs::remove_dir_all(&git_dir).expect("remove repository");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("daemon noticed repository deletion")
            .expect("daemon shutdown");
    }
}
