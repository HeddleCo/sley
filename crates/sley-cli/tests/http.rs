//! Integration tests for sley's smart-HTTP transport.
//!
//! These tests stand up a tiny in-process HTTP/1.1 server (std only) that runs
//! the SYSTEM git's `git http-backend` CGI for each request, then exercise
//! `sley clone`/`fetch`/`ls-remote`/`push` over `http://` and compare the
//! results against upstream `git`.
//!
//! Each test SKIPS cleanly (returns early) when the system `git` or its
//! `git http-backend` CGI helper is unavailable, so the suite stays green on
//! machines without a usable upstream git.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_rs() -> &'static str {
    env!("CARGO_BIN_EXE_sley")
}

fn trimmed_utf8(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .expect("git output is utf8")
        .trim()
        .to_string()
}

/// Returns the directory holding git's helper binaries (the output of
/// `git --exec-path`), or `None` when system `git` is not usable.
fn git_exec_path() -> Option<PathBuf> {
    let output = Command::new("git").arg("--exec-path").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Locates the `git-http-backend` CGI executable. Returns `None` when system
/// `git` or the http-backend helper is unavailable, in which case the calling
/// test should skip (return early).
fn git_http_backend() -> Option<PathBuf> {
    let exec_path = git_exec_path()?;
    for name in ["git-http-backend", "git-http-backend.exe"] {
        let candidate = exec_path.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A running in-process HTTP server that proxies each request to
/// `git http-backend`. Dropping it signals the accept loop to stop and joins
/// the background thread.
struct HttpBackendServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HttpBackendServer {
    /// Spawns a server bound to an ephemeral `127.0.0.1` port. `project_root`
    /// becomes `GIT_PROJECT_ROOT`; bare repos directly beneath it are served at
    /// `/<name>` (e.g. `/repo.git`). `http_backend` is the `git-http-backend`
    /// executable located via [`git_http_backend`].
    fn start(project_root: &Path, http_backend: &Path) -> HttpBackendServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral http port");
        listener
            .set_nonblocking(true)
            .expect("set listener non-blocking");
        let port = listener.local_addr().expect("listener local addr").port();
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_shutdown = Arc::clone(&shutdown);
        let project_root = project_root.to_path_buf();
        let http_backend = http_backend.to_path_buf();
        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        // One request per accepted connection (Connection: close).
                        if let Err(err) = handle_connection(stream, &project_root, &http_backend) {
                            // Surface unexpected I/O problems but keep serving.
                            eprintln!("http-backend test server error: {err}");
                        }
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::yield_now();
                    }
                    Err(err) => {
                        eprintln!("http-backend test server accept error: {err}");
                        break;
                    }
                }
            }
        });

        HttpBackendServer {
            port,
            shutdown,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
}

impl Drop for HttpBackendServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Nudge the accept loop out of its WouldBlock yield by connecting once.
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", self.port)) {
            drop(stream);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Reads and serves a single HTTP request from `stream` by invoking
/// `git http-backend` as a CGI program.
fn handle_connection(
    stream: TcpStream,
    project_root: &Path,
    http_backend: &Path,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // Request line, e.g. "GET /repo.git/info/refs?service=git-upload-pack HTTP/1.1".
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        // Connection closed before sending anything (e.g. the shutdown nudge).
        return Ok(());
    }
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    if method.is_empty() || target.is_empty() {
        return Ok(());
    }

    // Request headers until a blank line.
    let mut content_length = 0usize;
    let mut content_type: Option<String> = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            match name.as_str() {
                "content-length" => content_length = value.parse().unwrap_or(0),
                "content-type" => content_type = Some(value),
                _ => {}
            }
        }
    }

    // Request body (POST). git smart-HTTP always sends Content-Length.
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    // Split the request target into PATH_INFO and QUERY_STRING.
    let (path_info, query_string) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };

    // Run git-http-backend as a CGI process.
    let mut command = Command::new(http_backend);
    command
        .env("GIT_PROJECT_ROOT", project_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", &path_info)
        .env("QUERY_STRING", &query_string)
        .env("CONTENT_LENGTH", content_length.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(content_type) = &content_type {
        command.env("CONTENT_TYPE", content_type);
    }

    let mut child = command.spawn()?;
    {
        let mut child_stdin = child.stdin.take().expect("child stdin");
        child_stdin.write_all(&body)?;
        // Drop closes stdin so the CGI sees EOF.
    }
    let cgi = child.wait_with_output()?;

    // Parse the CGI response: headers (terminated by a blank line) then body.
    let (status_line, headers, cgi_body) = parse_cgi_output(&cgi.stdout);

    // Forward as an HTTP/1.1 response with Connection: close.
    let mut response = Vec::new();
    response.extend_from_slice(format!("HTTP/1.1 {status_line}\r\n").as_bytes());
    for (name, value) in &headers {
        response.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    response.extend_from_slice(format!("Content-Length: {}\r\n", cgi_body.len()).as_bytes());
    response.extend_from_slice(b"Connection: close\r\n");
    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(cgi_body);
    writer.write_all(&response)?;
    writer.flush()?;
    Ok(())
}

/// Splits CGI program output into `(status_line, headers, body)`.
///
/// The CGI header block is terminated by a blank line; headers use either CRLF
/// or LF endings (git http-backend emits CRLF). A `Status:` header is
/// translated into the HTTP status line and removed from the forwarded headers;
/// when absent the status defaults to `200 OK`.
fn parse_cgi_output(output: &[u8]) -> (String, Vec<(String, String)>, &[u8]) {
    let header_end = find_header_terminator(output);
    let (header_bytes, body) = match header_end {
        Some((end, body_start)) => (&output[..end], &output[body_start..]),
        None => (output, &output[output.len()..]),
    };

    let header_text = String::from_utf8_lossy(header_bytes);
    let mut status_line = String::from("200 OK");
    let mut headers = Vec::new();
    for line in header_text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("Status") {
                status_line = value.to_string();
            } else {
                headers.push((name.to_string(), value.to_string()));
            }
        }
    }

    (status_line, headers, body)
}

/// Finds the end of the CGI header block, returning
/// `(header_end_index, body_start_index)`. Handles both `\r\n\r\n` and `\n\n`
/// separators.
fn find_header_terminator(output: &[u8]) -> Option<(usize, usize)> {
    if let Some(pos) = find_subslice(output, b"\r\n\r\n") {
        return Some((pos, pos + 4));
    }
    if let Some(pos) = find_subslice(output, b"\n\n") {
        return Some((pos, pos + 2));
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Creates a bare upstream repo named `repo.git` under `project_root` containing
/// a couple of commits on `main`, a `feature/topic` branch, and a `v1.0` tag.
/// Returns the path to the bare repo.
fn create_upstream_repo(project_root: &Path) -> PathBuf {
    let work = project_root.join("seed-work");
    let bare = project_root.join("repo.git");
    std::fs::create_dir_all(&work).expect("create seed work dir");
    std::fs::create_dir_all(&bare).expect("create bare repo dir");

    run_success("git", &bare, &["init", "-q", "--bare"]);

    run_success("git", &work, &["init", "-q"]);
    // Ensure the default branch is "main" regardless of the host git config.
    run_success("git", &work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::write(work.join("payload.txt"), b"http payload\n").expect("write payload");
    run_success("git", &work, &["add", "payload.txt"]);
    run_success(
        "git",
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "initial",
            "-q",
        ],
    );
    std::fs::write(work.join("payload.txt"), b"http payload v2\n").expect("write payload v2");
    run_success("git", &work, &["add", "payload.txt"]);
    run_success(
        "git",
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "second",
            "-q",
        ],
    );
    run_success("git", &work, &["branch", "feature/topic"]);
    run_success("git", &work, &["tag", "v1.0"]);

    // Publish everything (branches + tags) into the bare repo.
    let bare_arg = bare.to_string_lossy().to_string();
    run_success(
        "git",
        &work,
        &["push", "-q", &bare_arg, "refs/heads/*:refs/heads/*"],
    );
    run_success(
        "git",
        &work,
        &["push", "-q", &bare_arg, "refs/tags/*:refs/tags/*"],
    );
    // Point the bare repo's HEAD at main so HTTP clients resolve a default branch.
    run_success("git", &bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    bare
}

/// Adds a new commit to `main` in the bare upstream repo and returns the new
/// `refs/heads/main` OID. Uses a temporary work tree because the upstream is
/// bare.
fn add_upstream_commit(project_root: &Path, bare: &Path) -> String {
    let work = project_root.join("update-work");
    std::fs::create_dir_all(&work).expect("create update work dir");
    let bare_arg = bare.to_string_lossy().to_string();
    run_success("git", &work, &["init", "-q"]);
    run_success("git", &work, &["fetch", "-q", &bare_arg, "main"]);
    run_success(
        "git",
        &work,
        &["checkout", "-q", "-B", "main", "FETCH_HEAD"],
    );
    std::fs::write(work.join("payload.txt"), b"http payload v3\n").expect("write payload v3");
    run_success("git", &work, &["add", "payload.txt"]);
    run_success(
        "git",
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "third",
            "-q",
        ],
    );
    run_success("git", &work, &["push", "-q", &bare_arg, "main"]);
    trimmed_utf8(run_success("git", &bare, &["rev-parse", "refs/heads/main"]))
}

/// Normalizes `git ls-remote`-style output into sorted `"<sha>\t<ref>"` lines so
/// two implementations can be compared regardless of advertisement ordering.
fn sorted_ref_lines(output: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(output);
    let mut lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next().unwrap_or("");
            let name = fields.next().unwrap_or("");
            format!("{sha}\t{name}")
        })
        .collect();
    lines.sort();
    lines
}

#[test]
fn ls_remote_http_matches_upstream() {
    let Some(http_backend) = git_http_backend() else {
        // System git or git-http-backend unavailable: skip cleanly.
        return;
    };
    let root = unique_temp_dir("http-ls-remote");
    let project_root = root.join("srv");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let result = (|| {
        create_upstream_repo(&project_root);
        let server = HttpBackendServer::start(&project_root, &http_backend);
        let url = server.url("/repo.git");

        let expected = run("git", &root, &["ls-remote", &url]);
        assert!(
            expected.status.success(),
            "upstream git ls-remote over http failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&expected.stdout),
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = run(git_rs(), &root, &["ls-remote", &url]);
        assert!(
            actual.status.success(),
            "sley ls-remote over http failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            sorted_ref_lines(&actual.stdout),
            sorted_ref_lines(&expected.stdout),
            "sley ls-remote refs differed from upstream over http"
        );
    })();
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn clone_http_smoke() {
    let Some(http_backend) = git_http_backend() else {
        return;
    };
    let root = unique_temp_dir("http-clone");
    let project_root = root.join("srv");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let result = (|| {
        let bare = create_upstream_repo(&project_root);
        let server = HttpBackendServer::start(&project_root, &http_backend);
        let url = server.url("/repo.git");
        let dst = root.join("clone");
        let dst_arg = dst.to_string_lossy().to_string();

        let output = run(git_rs(), &root, &["clone", "-q", &url, &dst_arg]);
        assert!(
            output.status.success(),
            "sley clone over http failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        // HEAD/branch OIDs in the clone must match the upstream bare repo.
        let upstream_main =
            trimmed_utf8(run_success("git", &bare, &["rev-parse", "refs/heads/main"]));
        let upstream_feature = trimmed_utf8(run_success(
            "git",
            &bare,
            &["rev-parse", "refs/heads/feature/topic"],
        ));
        let clone_head = trimmed_utf8(run_success("git", &dst, &["rev-parse", "HEAD"]));
        assert_eq!(clone_head, upstream_main, "cloned HEAD OID mismatch");
        let clone_main = trimmed_utf8(run_success(
            "git",
            &dst,
            &["rev-parse", "refs/remotes/origin/main"],
        ));
        assert_eq!(clone_main, upstream_main, "cloned origin/main OID mismatch");
        let clone_feature = trimmed_utf8(run_success(
            "git",
            &dst,
            &["rev-parse", "refs/remotes/origin/feature/topic"],
        ));
        assert_eq!(
            clone_feature, upstream_feature,
            "cloned origin/feature/topic OID mismatch"
        );

        // The cloned object store must be consistent.
        let fsck = run("git", &dst, &["fsck", "--no-progress"]);
        assert!(
            fsck.status.success(),
            "git fsck on cloned repo failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&fsck.stdout),
            String::from_utf8_lossy(&fsck.stderr)
        );
    })();
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_http_incremental() {
    let Some(http_backend) = git_http_backend() else {
        return;
    };
    let root = unique_temp_dir("http-fetch");
    let project_root = root.join("srv");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let result = (|| {
        let bare = create_upstream_repo(&project_root);
        let server = HttpBackendServer::start(&project_root, &http_backend);
        let url = server.url("/repo.git");
        let dst = root.join("clone");
        let dst_arg = dst.to_string_lossy().to_string();

        let clone = run(git_rs(), &root, &["clone", "-q", &url, &dst_arg]);
        assert!(
            clone.status.success(),
            "sley clone over http failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&clone.stdout),
            String::from_utf8_lossy(&clone.stderr)
        );

        // Advance the upstream main branch with a brand-new commit.
        let new_head = add_upstream_commit(&project_root, &bare);

        // The clone should not have the new commit yet.
        let before = run(
            "git",
            &dst,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/remotes/origin/main",
            ],
        );
        let before_head = trimmed_utf8(before.stdout);
        assert_ne!(
            before_head, new_head,
            "clone unexpectedly already had the new upstream commit"
        );

        // Incremental fetch over HTTP.
        let fetch = run(git_rs(), &dst, &["fetch", "-q", "origin"]);
        assert!(
            fetch.status.success(),
            "sley fetch over http failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&fetch.stdout),
            String::from_utf8_lossy(&fetch.stderr)
        );

        // The new commit must now be present on the remote-tracking ref...
        let after_head = trimmed_utf8(run_success(
            "git",
            &dst,
            &["rev-parse", "refs/remotes/origin/main"],
        ));
        assert_eq!(
            after_head, new_head,
            "sley fetch did not update origin/main to the new upstream commit"
        );
        // ...and the object itself must exist in the clone's object store.
        let cat = run("git", &dst, &["cat-file", "-e", &new_head]);
        assert!(
            cat.status.success(),
            "fetched commit object {new_head} missing from clone\nstderr:\n{}",
            String::from_utf8_lossy(&cat.stderr)
        );
    })();
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn push_http_creates_ref() {
    let Some(http_backend) = git_http_backend() else {
        return;
    };
    let root = unique_temp_dir("http-push");
    let project_root = root.join("srv");
    std::fs::create_dir_all(&project_root).expect("create project root");
    let result = (|| {
        let bare = create_upstream_repo(&project_root);
        // Enable pushing over smart HTTP on the upstream bare repo.
        run_success("git", &bare, &["config", "http.receivepack", "true"]);

        let server = HttpBackendServer::start(&project_root, &http_backend);
        let url = server.url("/repo.git");
        let dst = root.join("clone");
        let dst_arg = dst.to_string_lossy().to_string();

        let clone = run(git_rs(), &root, &["clone", "-q", &url, &dst_arg]);
        assert!(
            clone.status.success(),
            "sley clone over http failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&clone.stdout),
            String::from_utf8_lossy(&clone.stderr)
        );

        // Make a local commit on top of the cloned HEAD.
        std::fs::write(dst.join("payload.txt"), b"local push payload\n").expect("write local");
        run_success("git", &dst, &["add", "payload.txt"]);
        run_success(
            "git",
            &dst,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "local change",
                "-q",
            ],
        );
        let local_head = trimmed_utf8(run_success("git", &dst, &["rev-parse", "HEAD"]));

        // Push the local commit to a brand-new branch on the upstream over HTTP.
        let push = run(
            git_rs(),
            &dst,
            &["push", "-q", "origin", "HEAD:refs/heads/pushed"],
        );
        assert!(
            push.status.success(),
            "sley push over http failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&push.stdout),
            String::from_utf8_lossy(&push.stderr)
        );

        // The bare upstream must now hold the new ref at the expected OID.
        let remote_head = trimmed_utf8(run_success(
            "git",
            &bare,
            &["rev-parse", "refs/heads/pushed"],
        ));
        assert_eq!(
            remote_head, local_head,
            "pushed ref OID on upstream did not match local HEAD"
        );
        // The pushed object must be readable on the upstream.
        let cat = run("git", &bare, &["cat-file", "-e", &local_head]);
        assert!(
            cat.status.success(),
            "pushed commit object {local_head} missing from upstream\nstderr:\n{}",
            String::from_utf8_lossy(&cat.stderr)
        );
    })();
    let _ = std::fs::remove_dir_all(&root);
    result
}
