//! External credential-helper subprocess execution.
//!
//! Generic, embedder-safe pieces of a git credential-helper chain:
//!
//! * [`find_external_helper_executable`] — resolve a bare helper name such as
//!   `manager` to a `git-credential-manager` binary found on a search path,
//!   matching upstream git's lookup rules (including Windows `PATHEXT`).
//! * [`external_helper_command`] — build the direct `<executable> [args…] <op>`
//!   invocation used once a helper binary is known.
//! * [`HelperExecOptions`] — execution policy: kill-on-hang deadline, bounded
//!   output, fallthrough-on-error across the helper chain, and whether bare
//!   names prefer PATH-discovered helpers over embedded dispatch.
//! * [`credential_do_with_options`] / [`credential_fill_with_options`] — the
//!   option-carrying counterparts of [`super::credential_do`] /
//!   [`super::credential_fill`].
//!
//! The defaults on [`HelperExecOptions`] are the hardened embedder profile:
//! helpers hang-kill after [`DEFAULT_HELPER_TIMEOUT`], a dead or hanging helper
//! falls through to the next one instead of failing authentication, and bare
//! names resolve through PATH first (falling back to embedded dispatch).
//! Callers wanting byte-for-byte legacy behavior use the plain
//! [`super::credential_fill`] entry point, which pins the pre-options policy.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sley_core::{GitError, Result};

use super::{CredentialOpType, GitCredential, credential_helper_command, credential_write};

/// How long a helper may run before it is killed (`get`, `store`, or `erase`).
///
/// A credential helper that never exits must not stall an authenticated fetch
/// forever; git itself relies on the OS pipe buffer plus user patience, which
/// is not a contract an embedder can ship.
pub const DEFAULT_HELPER_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on bytes read from a `get` helper's stdout.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = super::MAX_GIT_CREDENTIAL_RESPONSE_BYTES;

/// Poll interval while waiting for a helper to exit under a deadline.
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Execution policy for external credential-helper subprocesses.
///
/// Construct with [`HelperExecOptions::new`] (hardened embedder defaults) and
/// adjust with the builder setters. Every field is opt-out rather than opt-in:
/// the defaults already kill hung helpers, fall through broken ones, and
/// resolve bare names against PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperExecOptions {
    timeout: Option<Duration>,
    max_output_bytes: usize,
    fallthrough_on_error: bool,
    prefer_path_helpers: bool,
    search_path: Option<OsString>,
}

impl HelperExecOptions {
    /// Hardened embedder defaults: 30-second kill-on-hang deadline, fall
    /// through failed helpers, PATH-first resolution of bare names.
    pub fn new() -> Self {
        Self {
            timeout: Some(DEFAULT_HELPER_TIMEOUT),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            fallthrough_on_error: true,
            prefer_path_helpers: true,
            search_path: None,
        }
    }

    /// Kill a helper that has not exited within `timeout`; `None` waits
    /// indefinitely (the legacy policy).
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Kill (and discard the output of) a `get` helper whose stdout exceeds
    /// this many bytes.
    pub fn max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// Continue down the configured helper chain when a helper fails, hangs,
    /// or emits garbage, instead of failing the whole fill/store/erase.
    pub fn fallthrough_on_error(mut self, fallthrough_on_error: bool) -> Self {
        self.fallthrough_on_error = fallthrough_on_error;
        self
    }

    /// Resolve bare helper names (`manager`) to `git-credential-<name>`
    /// binaries discovered on `PATH` before falling back to the embedding
    /// executable's built-in dispatch.
    pub fn prefer_path_helpers(mut self, prefer_path_helpers: bool) -> Self {
        self.prefer_path_helpers = prefer_path_helpers;
        self
    }

    /// Override the search path used for bare-name discovery. `None` (the
    /// default) reads the process `PATH`. Embedders that resolve a user's
    /// environment themselves pass it here instead of mutating the process.
    pub fn search_path(mut self, search_path: Option<OsString>) -> Self {
        self.search_path = search_path;
        self
    }

    pub fn get_timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn get_max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    pub fn falls_through_on_error(&self) -> bool {
        self.fallthrough_on_error
    }

    pub fn prefers_path_helpers(&self) -> bool {
        self.prefer_path_helpers
    }

    pub fn get_search_path(&self) -> Option<&OsStr> {
        self.search_path.as_deref()
    }
}

impl Default for HelperExecOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a bare credential-helper name to an executable on `search_path`.
///
/// `name` is the spec head as written in config (`manager`,
/// `git-credential-manager`); the `git-credential-` prefix is added when
/// missing, mirroring git's helper lookup. Returns `None` when no candidate
/// names an executable file. Pass `None` for `search_path` when the caller has
/// no override; see [`find_external_helper_executable_from_env`].
pub fn find_external_helper_executable(name: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let search_path = search_path?;
    let executable_name = if name.starts_with("git-credential-") {
        name.to_string()
    } else {
        format!("git-credential-{name}")
    };
    find_executable(&executable_name, search_path)
}

/// Convenience wrapper resolving against the process `PATH`.
pub fn find_external_helper_executable_from_env(name: &str) -> Option<PathBuf> {
    find_external_helper_executable(name, std::env::var_os("PATH").as_deref())
}

/// Build the direct invocation `<executable> [args…] <operation>` for a
/// discovered helper binary. Arguments precede the operation, matching how git
/// expands `helper = store --file=…`.
pub fn external_helper_command(executable: &Path, args: &[String], operation: &str) -> Command {
    let mut command = Command::new(executable);
    command.args(args);
    command.arg(operation);
    command
}

fn find_executable(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    let names = executable_names(name);
    std::env::split_paths(search_path).find_map(|directory| {
        names.iter().find_map(|name| {
            let candidate = directory.join(name);
            executable_file(&candidate).then_some(candidate)
        })
    })
}

fn executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        path.metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Candidate file names for a helper, including platform executability suffixes.
fn executable_names(name: &str) -> Vec<String> {
    let names = vec![name.to_string()];
    #[cfg(windows)]
    let names = {
        let mut names = names;
        if let Ok(extensions) = std::env::var("PATHEXT") {
            names.extend(
                extensions
                    .split(';')
                    .filter(|extension| !extension.is_empty())
                    .map(|extension| format!("{name}{extension}")),
            );
        }
        names
    };
    names
}

/// Split a helper spec into its command head and argument words.
fn split_spec(spec: &str) -> (&str, Vec<String>) {
    match spec.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.split_whitespace().map(str::to_string).collect()),
        None => (spec, Vec::new()),
    }
}

/// Resolve `helper` to the command to spawn, honoring the options policy.
///
/// Bare names try PATH discovery first when
/// [`HelperExecOptions::prefer_path_helpers`] is set, then fall back to the
/// legacy embedded dispatch (`<current_exe> credential-<name>`). Shell
/// snippets (`!…`) and absolute paths always take the legacy path.
pub(crate) fn resolve_helper_command(
    helper: &str,
    operation: &str,
    options: &HelperExecOptions,
) -> Option<Command> {
    let spec = helper.trim();
    if spec.is_empty() {
        return None;
    }
    if options.prefer_path_helpers && !spec.starts_with('!') && !spec.starts_with('/') {
        let (head, args) = split_spec(spec);
        if !head.contains('/') {
            let env_path = std::env::var_os("PATH");
            let search_path = match &options.search_path {
                Some(path) => Some(path.as_os_str()),
                None => env_path.as_deref(),
            };
            if let Some(executable) = find_external_helper_executable(head, search_path) {
                return Some(external_helper_command(&executable, &args, operation));
            }
        }
    }
    credential_helper_command(spec, operation)
}

/// Run one helper subprocess to completion under `options`.
///
/// Feeds `credential` on stdin, enforces the kill-on-hang deadline and output
/// bound, and returns the helper's stdout bytes (`get` operations only). A
/// non-zero exit, timeout, or oversized response is an error; with
/// `fallthrough_on_error` the fill loop decides what that means.
pub(crate) fn run_helper_process(
    mut command: Command,
    credential: &GitCredential,
    helper: &str,
    want_output: bool,
    options: &HelperExecOptions,
) -> Result<Vec<u8>> {
    // Resolvers return bare commands; the I/O wiring is this function's job so
    // every resolution path gets identical pipes.
    command.stdin(Stdio::piped());
    command.stdout(if want_output {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let mut child = command.spawn().map_err(|e| GitError::Io(e.to_string()))?;
    let op_type = if want_output {
        CredentialOpType::Helper
    } else {
        CredentialOpType::Response
    };
    if let Some(mut stdin) = child.stdin.take()
        && credential_write(credential, &mut stdin, op_type).is_err()
    {
        terminate_helper(&mut child);
        return Err(GitError::Io(format!(
            "failed to write credential protocol input to helper '{helper}'"
        )));
    }
    if !want_output {
        return match wait_for_exit(&mut child, deadline) {
            Some(status) => helper_exit_status(helper, status).map(|()| Vec::new()),
            None => Err(helper_timeout_error(helper, options)),
        };
    }
    let Some(stdout) = child.stdout.take() else {
        terminate_helper(&mut child);
        return Err(GitError::Io(
            "credential helper stdout was not piped".into(),
        ));
    };
    let output = read_bounded_output(&mut child, stdout, helper, deadline, options)?;
    match wait_for_exit(&mut child, deadline) {
        Some(status) => helper_exit_status(helper, status).map(|()| output),
        None => Err(helper_timeout_error(helper, options)),
    }
}

/// Drain a `get` helper's stdout on a side thread so a silent helper cannot
/// exceed the deadline, capping the read at the configured byte budget. On any
/// failure path the child is killed so the reader thread cannot outlive us.
fn read_bounded_output(
    child: &mut Child,
    stdout: std::process::ChildStdout,
    helper: &str,
    deadline: Option<Instant>,
    options: &HelperExecOptions,
) -> Result<Vec<u8>> {
    let max_output_bytes = options.max_output_bytes;
    let (send_output, receive_output) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take(max_output_bytes.saturating_add(1) as u64)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = send_output.send(result);
    });
    let received = match deadline {
        Some(deadline) => receive_output.recv_timeout(remaining_time(deadline)),
        None => receive_output
            .recv()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
    };
    match received {
        Ok(Ok(output)) if output.len() <= max_output_bytes => Ok(output),
        Ok(Ok(_)) => {
            terminate_helper(child);
            Err(GitError::InvalidFormat(format!(
                "credential helper '{helper}' output exceeds maximum size of {max_output_bytes} bytes"
            )))
        }
        Ok(Err(err)) => {
            terminate_helper(child);
            Err(GitError::Io(err.to_string()))
        }
        Err(_) => {
            terminate_helper(child);
            Err(helper_timeout_error(helper, options))
        }
    }
}

fn remaining_time(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Wait for `child` to exit, killing it when the deadline passes. `None` means
/// the helper had to be killed (or its exit status could not be read).
fn wait_for_exit(child: &mut Child, deadline: Option<Instant>) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => {
                terminate_helper(child);
                return None;
            }
        }
        let Some(deadline) = deadline else {
            match child.wait() {
                Ok(status) => return Some(status),
                Err(_) => {
                    terminate_helper(child);
                    return None;
                }
            }
        };
        let remaining = remaining_time(deadline);
        if remaining.is_zero() {
            terminate_helper(child);
            return None;
        }
        thread::sleep(remaining.min(HELPER_POLL_INTERVAL));
    }
}

fn terminate_helper(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn helper_exit_status(helper: &str, status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Command(format!(
            "credential helper '{helper}' failed"
        )))
    }
}

fn helper_timeout_error(helper: &str, options: &HelperExecOptions) -> GitError {
    match options.timeout {
        Some(timeout) => {
            let seconds = timeout.as_secs_f64();
            GitError::Command(format!(
                "credential helper '{helper}' timed out after {seconds}s"
            ))
        }
        // Only reachable when a blocking read fails outright with no deadline.
        None => GitError::Command(format!("credential helper '{helper}' did not complete")),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use sley_config::GitConfig;

    use super::super::{credential_do_with_options, credential_fill_with_options};
    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sley-credential-exec-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Write an executable helper script and return its absolute path, for
    /// use as an absolute-path credential-helper spec.
    fn write_helper(directory: &Path, name: &str, body: &str) -> PathBuf {
        let helper = directory.join(format!("git-credential-{name}"));
        fs::write(&helper, format!("#!/bin/sh\n{body}\n")).expect("write helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions).expect("make helper executable");
        helper
    }

    fn fill_config(helpers: &[String]) -> GitConfig {
        let lines: Vec<String> = helpers
            .iter()
            .map(|helper| format!("\thelper = {helper}\n"))
            .collect();
        let body = format!("[credential]\n\tinteractive = false\n{}", lines.join(""));
        GitConfig::parse(body.as_bytes()).expect("parse config")
    }

    fn https_request() -> GitCredential {
        GitCredential {
            protocol: Some("https".into()),
            host: Some("example.test".into()),
            ..GitCredential::default()
        }
    }

    #[test]
    fn discovery_resolves_prefixed_helper_on_custom_search_path() {
        let temp = TempDir::new();
        let helper = write_helper(&temp.path, "discovery", "exit 0");
        assert_eq!(
            find_external_helper_executable("discovery", Some(temp.path.as_os_str())),
            Some(helper.clone())
        );
        // The prefixed spelling resolves to the same binary.
        assert_eq!(
            find_external_helper_executable(
                "git-credential-discovery",
                Some(temp.path.as_os_str())
            ),
            Some(helper)
        );
        assert!(
            find_external_helper_executable("definitely-missing", Some(temp.path.as_os_str()))
                .is_none()
        );
    }

    #[test]
    fn bare_name_resolution_falls_back_to_embedded_dispatch_when_not_found() {
        let options = HelperExecOptions::new().search_path(Some("/nonexistent".into()));
        let command =
            resolve_helper_command("nowhere-helper", "get", &options).expect("fallback command");
        // Legacy dispatch re-execs the embedding executable with a
        // `credential-<name>` subcommand, via the shell.
        let argv: Vec<String> =
            std::iter::once(command.get_program().to_string_lossy().into_owned())
                .chain(
                    command
                        .get_args()
                        .map(|arg| arg.to_string_lossy().into_owned()),
                )
                .collect();
        assert!(
            argv.iter()
                .any(|arg| arg.contains("credential-nowhere-helper")),
            "expected embedded credential-<name> dispatch, got {argv:?}"
        );
    }

    #[test]
    fn prefer_path_helpers_runs_discovered_binary_over_embedded_dispatch() {
        let temp = TempDir::new();
        let marker = temp.path.join("marker.out");
        write_helper(
            &temp.path,
            "direct",
            &format!(
                "cat >/dev/null\nprintf 'RAN:%s\\n' \"$*\" > '{}'\nprintf 'username=alice\\npassword=secret\\n'",
                marker.display()
            ),
        );
        let config = fill_config(&["direct".to_string()]);
        let options = HelperExecOptions::new().search_path(Some(temp.path.clone().into()));
        let mut credential = https_request();
        credential_fill_with_options(Some(&config), None, &mut credential, true, &options)
            .expect("PATH-discovered helper fills the credential");
        assert_eq!(credential.username.as_deref(), Some("alice"));
        let recorded = fs::read_to_string(&marker).expect("discovered binary must have run");
        assert_eq!(recorded.trim(), "RAN:get");
    }

    #[test]
    fn hanging_get_helper_is_killed_at_deadline_and_falls_through() {
        let temp = TempDir::new();
        let hangs = write_helper(&temp.path, "hangs", "cat >/dev/null\nexec sleep 30");
        let recovers = write_helper(
            &temp.path,
            "after-hang",
            "cat >/dev/null\nprintf 'username=fallback\\npassword=secret\\n'",
        );
        let config = fill_config(&[hangs.display().to_string(), recovers.display().to_string()]);
        let options = HelperExecOptions::new().timeout(Some(Duration::from_secs(2)));
        let started = Instant::now();
        let mut credential = https_request();
        credential_fill_with_options(Some(&config), None, &mut credential, true, &options)
            .expect("timed-out helper falls through to the next one");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "kill-on-hang must not wait out the full sleep"
        );
        assert_eq!(credential.username.as_deref(), Some("fallback"));
    }

    #[test]
    fn strict_mode_propagates_the_first_failing_helper() {
        let temp = TempDir::new();
        let hangs = write_helper(&temp.path, "strict-hangs", "cat >/dev/null\nexec sleep 30");
        let config = fill_config(&[hangs.display().to_string()]);
        let options = HelperExecOptions::new()
            .timeout(Some(Duration::from_secs(2)))
            .fallthrough_on_error(false);
        let started = Instant::now();
        let mut credential = https_request();
        let error =
            credential_fill_with_options(Some(&config), None, &mut credential, true, &options)
                .expect_err("strict mode surfaces the failure");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            error.to_string().contains("timed out"),
            "expected a timeout error, got {error}"
        );
    }

    #[test]
    fn failing_helper_falls_through_only_when_configured() {
        let temp = TempDir::new();
        let fails = write_helper(&temp.path, "fails", "exit 23");
        let recovers = write_helper(
            &temp.path,
            "recovers",
            "cat >/dev/null\nprintf 'username=carol\\npassword=secret\\n'",
        );
        let config = fill_config(&[fails.display().to_string(), recovers.display().to_string()]);

        let mut lenient = https_request();
        credential_fill_with_options(
            Some(&config),
            None,
            &mut lenient,
            true,
            &HelperExecOptions::new(),
        )
        .expect("failing helper falls through by default");
        assert_eq!(lenient.username.as_deref(), Some("carol"));

        let mut strict = https_request();
        let error = credential_fill_with_options(
            Some(&config),
            None,
            &mut strict,
            true,
            &HelperExecOptions::new().fallthrough_on_error(false),
        )
        .expect_err("strict mode stops at the failing helper");
        assert!(
            error.to_string().contains("git-credential-fails' failed"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn store_operation_kills_hanging_helper_at_deadline() {
        let temp = TempDir::new();
        let hangs = write_helper(&temp.path, "store-hangs", "cat >/dev/null\nexec sleep 30");
        let mut credential = https_request();
        credential.username = Some("alice".into());
        credential.password = Some("secret".into());
        let options = HelperExecOptions::new().timeout(Some(Duration::from_secs(1)));
        let started = Instant::now();
        let error = credential_do_with_options(
            &mut credential,
            hangs.to_str().expect("utf-8 temp path"),
            "store",
            false,
            &options,
        )
        .expect_err("hanging store helper is killed and reported");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("timed out"), "got {error}");
    }

    #[test]
    fn malformed_response_leaves_the_request_untouched() {
        let temp = TempDir::new();
        let garbage = write_helper(
            &temp.path,
            "garbage",
            "cat >/dev/null\nprintf 'not-a-credential-line\\n'",
        );
        let mut credential = https_request();
        let before = credential.clone();
        let error = credential_do_with_options(
            &mut credential,
            garbage.to_str().expect("utf-8 temp path"),
            "get",
            true,
            &HelperExecOptions::new(),
        )
        .expect_err("malformed helper output is rejected");
        assert!(matches!(error, GitError::InvalidFormat(_)));
        assert_eq!(
            credential, before,
            "a rejected response must not half-apply"
        );
    }
}
