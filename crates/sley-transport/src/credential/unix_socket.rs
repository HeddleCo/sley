//! Unix domain socket helpers mirroring upstream `unix-socket.c` long-path handling.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 108;

struct UnixSockaddrContext {
    orig_dir: Option<PathBuf>,
}

impl UnixSockaddrContext {
    fn cleanup(self) -> io::Result<()> {
        if let Some(orig) = self.orig_dir {
            std::env::set_current_dir(orig)?;
        }
        Ok(())
    }
}

fn unix_sockaddr_setup(path: &Path, disallow_chdir: bool) -> io::Result<(PathBuf, UnixSockaddrContext)> {
    let path_str = path.as_os_str().as_encoded_bytes();
    if path_str.len() + 1 <= SUN_PATH_MAX {
        return Ok((path.to_path_buf(), UnixSockaddrContext { orig_dir: None }));
    }
    if disallow_chdir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be shorter than SUN_LEN",
        ));
    }
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be shorter than SUN_LEN",
        ));
    };
    let Some(file_name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be shorter than SUN_LEN",
        ));
    };
    if file_name.as_encoded_bytes().len() + 1 > SUN_PATH_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be shorter than SUN_LEN",
        ));
    }
    let orig_dir = std::env::current_dir()?;
    std::env::set_current_dir(parent)?;
    Ok((
        PathBuf::from(file_name),
        UnixSockaddrContext {
            orig_dir: Some(orig_dir),
        },
    ))
}

pub fn unix_stream_connect(path: &Path) -> io::Result<UnixStream> {
    let (connect_path, ctx) = unix_sockaddr_setup(path, false)?;
    let result = UnixStream::connect(&connect_path);
    ctx.cleanup()?;
    result
}

pub fn unix_stream_listen(path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let (bind_path, ctx) = unix_sockaddr_setup(path, false)?;
    let result = UnixListener::bind(&bind_path);
    ctx.cleanup()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::thread;
    use std::time::Duration;

    fn long_socket_path(base: &Path) -> PathBuf {
        let mut path = base.to_path_buf();
        for _ in 0..8 {
            path.push("credential-cache-long-path-component");
        }
        path.push("socket");
        path
    }

    #[test]
    fn connect_and_listen_with_long_path() {
        let base = std::env::temp_dir().join(format!(
            "sley-cred-socket-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp dir");
        let socket = long_socket_path(&base);
        assert!(
            socket.as_os_str().as_encoded_bytes().len() + 1 > SUN_PATH_MAX,
            "test socket path should exceed SUN_PATH_MAX"
        );
        std::fs::create_dir_all(socket.parent().expect("parent")).expect("create socket dir");

        let listener = unix_stream_listen(&socket).expect("bind long socket path");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 4];
            let n = stream.read(&mut buf).expect("read");
            assert_eq!(&buf[..n], b"ping");
            stream.write_all(b"pong").expect("write");
        });

        let mut client = unix_stream_connect(&socket).expect("connect long socket path");
        client.write_all(b"ping").expect("write");
        client.shutdown(std::net::Shutdown::Write).expect("shutdown");
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read response");
        assert_eq!(response, "pong");
        handle.join().expect("listener thread");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn listen_unlinks_stale_socket() {
        let base = std::env::temp_dir().join(format!(
            "sley-cred-stale-socket-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp dir");
        let socket = base.join("socket");
        std::fs::write(&socket, b"stale").expect("write stale socket file");

        let listener = unix_stream_listen(&socket).expect("bind after unlinking stale socket");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        thread::sleep(Duration::from_millis(20));
        assert!(unix_stream_connect(&socket).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }
}