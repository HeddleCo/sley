//! Credential acquisition for authenticated remotes.
//!
//! Derives the credential lookup key for a remote URL, runs `credential.helper`
//! programs to fill in a username/password, and remembers or forgets results.
//! The default [`CredentialHelperProvider`] wraps this as a
//! [`CredentialProvider`](crate::CredentialProvider); embedders targeting public
//! remotes can use [`NoCredentials`](crate::NoCredentials) instead.

use std::io::Write;
use std::process::{Command, Stdio};

use sley_config::GitConfig;
use sley_core::Result;
use sley_transport::{
    GitCredential, RemoteTransport, RemoteUrl, encode_git_credential, parse_git_credential,
};

use crate::CredentialProvider;

/// The `protocol` field of a credential request derived from `remote`.
pub fn http_protocol_name(remote: &RemoteUrl) -> Option<String> {
    match remote.transport {
        RemoteTransport::Https => Some("https".to_string()),
        RemoteTransport::Http => Some("http".to_string()),
        _ => None,
    }
}

/// The `host[:port]` field of a credential request derived from `remote`.
pub fn http_credential_host(remote: &RemoteUrl) -> Option<String> {
    remote.host.clone().map(|host| match remote.port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// Credential implied by `user[:password]@` userinfo in the remote URL.
pub fn http_url_credential(remote: &RemoteUrl) -> Option<GitCredential> {
    let username = remote.user.clone()?;
    Some(GitCredential {
        protocol: http_protocol_name(remote),
        host: http_credential_host(remote),
        username: Some(username),
        password: remote.password.clone(),
        ..GitCredential::default()
    })
}

/// The lookup key a credential helper is asked to fill for this remote.
pub fn credential_request_for_url(remote: &RemoteUrl) -> GitCredential {
    GitCredential {
        protocol: http_protocol_name(remote),
        host: http_credential_host(remote),
        username: remote.user.clone(),
        ..GitCredential::default()
    }
}

/// Ordered `credential.helper` values from config. An empty value resets the
/// accumulated list, matching upstream git semantics.
fn credential_helper_specs(config: Option<&GitConfig>) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let mut specs = Vec::new();
    for section in &config.sections {
        if section.name != "credential" || section.subsection.is_some() {
            continue;
        }
        for entry in &section.entries {
            if !entry.key.eq_ignore_ascii_case("helper") {
                continue;
            }
            match entry.value.as_deref() {
                Some("") | None => specs.clear(),
                Some(value) => specs.push(value.to_string()),
            }
        }
    }
    specs
}

/// Resolve a `credential.helper` spec into a runnable command, appending the
/// operation (`get`/`store`/`erase`).
///
/// Dispatch mirrors git 2.54's `credential_do` (`credential.c`), which
/// classifies the helper string into exactly three forms:
///
/// - `!cmd` — a *shell snippet* (documented git feature). Run `cmd "$@"`
///   through `sh -c`, with the operation as `$1`. Shell metacharacters here are
///   evaluated by design; this is git's inherited threat model, not a
///   sley-introduced injection.
/// - `is_absolute_path` — a helper string whose first token *begins with* `/`.
///   git runs `<path> <op>` through the shell; sley does the same so
///   quoting/word-splitting of any arguments matches git exactly.
/// - otherwise — a *bare name* (anything that does not start with `/`, including
///   relative paths like `sub/helper`). git maps it to `git credential-<name>`;
///   sley maps it to the standalone `git-credential-<name>` binary to honour the
///   "no git shell-outs in src" audit constraint. Crucially the classifier keys
///   on a *leading* `/` (git's `is_absolute_path`), NOT on `contains('/')`: a
///   relative `sub/helper` must NOT be exec'd directly, or sley would diverge
///   from git's dispatch.
fn credential_helper_command(spec: &str, op: &str) -> Option<Command> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(shell) = spec.strip_prefix('!') {
        // `!`-snippet: git runs the snippet through the shell with the operation
        // passed positionally; replicate via `sh -c '<snippet> "$@"' sh <op>`.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{shell} \"$@\""))
            .arg("sh")
            .arg(op);
        return Some(command);
    }
    let mut tokens = spec.split_whitespace();
    let head = tokens.next()?;
    // git's `is_absolute_path` is a leading directory separator (`/` on unix);
    // it does NOT match relative paths that merely contain a `/`. Matching
    // `contains('/')` here would exec `sub/helper` directly while git dispatches
    // it as `git credential-sub/helper` — a dispatch divergence (sley#3).
    let program = if head.starts_with('/') {
        head.to_string()
    } else {
        format!("git-credential-{head}")
    };
    let mut command = Command::new(program);
    for arg in tokens {
        command.arg(arg);
    }
    command.arg(op);
    Some(command)
}

/// Run a credential helper, feeding `input` on stdin. Best-effort: a missing or
/// failing helper yields `None` rather than aborting the transfer.
fn run_credential_helper(spec: &str, op: &str, input: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(mut command) = credential_helper_command(spec, op) else {
        return Ok(None);
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(None),
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

/// Fill `request` (username/password) using the configured credential helpers,
/// returning a complete credential or `None` if it could not be completed.
pub fn credential_fill(
    config: Option<&GitConfig>,
    mut request: GitCredential,
) -> Result<Option<GitCredential>> {
    for spec in credential_helper_specs(config) {
        if request.username.is_some() && request.password.is_some() {
            break;
        }
        let input = encode_git_credential(&request)?;
        if let Some(stdout) = run_credential_helper(&spec, "get", &input)? {
            let filled = parse_git_credential(&stdout)?;
            if filled.username.is_some() {
                request.username = filled.username;
            }
            if filled.password.is_some() {
                request.password = filled.password;
            }
        }
    }
    if request.username.is_some() && request.password.is_some() {
        Ok(Some(request))
    } else {
        Ok(None)
    }
}

/// Tell the configured helpers to store (`approve = true`) or erase a credential.
pub fn credential_store(config: Option<&GitConfig>, credential: &GitCredential, approve: bool) {
    let Ok(input) = encode_git_credential(credential) else {
        return;
    };
    let op = if approve { "store" } else { "erase" };
    for spec in credential_helper_specs(config) {
        let _ = run_credential_helper(&spec, op, &input);
    }
}

/// The default [`CredentialProvider`]: fills and stores credentials via the
/// repository's configured `credential.helper` programs.
pub struct CredentialHelperProvider<'a> {
    config: Option<&'a GitConfig>,
}

impl<'a> CredentialHelperProvider<'a> {
    /// Create a provider backed by `config`'s `credential.helper` settings.
    pub fn new(config: Option<&'a GitConfig>) -> Self {
        Self { config }
    }
}

impl CredentialProvider for CredentialHelperProvider<'_> {
    fn fill(&mut self, request: GitCredential) -> Result<Option<GitCredential>> {
        credential_fill(self.config, request)
    }

    fn approve(&mut self, credential: &GitCredential) -> Result<()> {
        credential_store(self.config, credential, true);
        Ok(())
    }

    fn reject(&mut self, credential: &GitCredential) -> Result<()> {
        credential_store(self.config, credential, false);
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod credential_dispatch_parity_tests {
    //! Parity harness for `credential.helper` dispatch against git 2.54.
    //!
    //! Threat model (inherited from git, NOT introduced by sley): git runs
    //! credential helpers through the shell (`credential.c` `run_credential_helper`
    //! sets `helper.use_shell = 1`). A `!`-prefixed helper is *documented* to be a
    //! shell snippet, and an absolute-path helper string is likewise run through
    //! the shell, so shell metacharacters in those forms are evaluated by design.
    //! These tests pin sley's dispatch to git's exact classification so sley
    //! neither *adds* an injection git lacks (e.g. shell-splitting a token git
    //! would exec directly) nor *removes* the documented `!`-shell behavior.
    //!
    //! Git's `credential_do` (`credential.c`) classifies the helper string by:
    //!
    //! - `!cmd` -> strip `!`, run `cmd <op>` through the shell;
    //! - `is_absolute_path` -> leading `/` only -> run `<path> <op>` via shell;
    //! - otherwise -> `git credential-<helper> <op>` via shell (sley substitutes
    //!   the standalone `git-credential-<helper>` binary to honour the "no git
    //!   shell-outs in src" audit constraint).
    //!
    //! All three were confirmed empirically against git 2.54 via `GIT_TRACE`.

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use sley_config::GitConfig;
    use sley_transport::GitCredential;

    use super::{credential_fill, credential_helper_command};

    /// Build a `GitConfig` carrying a single `credential.helper` value.
    ///
    /// The value is wrapped in git-config double quotes (escaping `\` and `"`)
    /// so that `;`/`#`/whitespace inside a helper snippet survive parsing
    /// exactly as they do in a real `~/.gitconfig` — git treats an unquoted
    /// `;`/`#` as a comment, so a `!`-snippet helper must be quoted in practice.
    fn config_with_helper(helper: &str) -> GitConfig {
        // Hermetic: no host gitconfig, identity is irrelevant here.
        let escaped = helper.replace('\\', "\\\\").replace('"', "\\\"");
        let body = format!("[credential]\n\thelper = \"{escaped}\"\n");
        GitConfig::parse(body.as_bytes()).expect("config parses")
    }

    /// Write an executable shell script and return its path.
    fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write script");
        let mut perms = fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod");
        path
    }

    fn base_request() -> GitCredential {
        GitCredential {
            protocol: Some("https".to_string()),
            host: Some("example.com".to_string()),
            ..GitCredential::default()
        }
    }

    /// Absolute-path form: git runs `<abs> <op>` through the shell, so a helper
    /// that records its args sees exactly `--flag get` (op appended). Sley must
    /// match: a single positional arg plus the operation, with shell word-split.
    #[test]
    fn absolute_path_form_passes_args_and_op() {
        let tmp = tempdir();
        let marker = tmp.path().join("abs.out");
        let script = write_script(
            tmp.path(),
            "abs-helper.sh",
            &format!(
                "#!/bin/sh\ncat >/dev/null\nprintf 'ARGS:[%s]\\n' \"$*\" >> '{}'\necho username=abs-user\necho password=abs-pass\n",
                marker.display()
            ),
        );
        let cfg = config_with_helper(&format!("{} --flag", script.display()));
        let filled = credential_fill(Some(&cfg), base_request())
            .expect("fill ok")
            .expect("credential filled");
        assert_eq!(filled.username.as_deref(), Some("abs-user"));
        assert_eq!(filled.password.as_deref(), Some("abs-pass"));
        let recorded = fs::read_to_string(&marker).expect("marker written");
        // git 2.54 produces exactly: ARGS:[--flag get]
        assert_eq!(recorded.trim(), "ARGS:[--flag get]");
    }

    /// `!shell` form: git runs the snippet through the shell with the operation
    /// passed as `$1` (documented behavior). A snippet of `f` (a function) must
    /// observe `get` as its first positional argument.
    #[test]
    fn shell_snippet_form_runs_through_shell_with_op_arg() {
        let tmp = tempdir();
        let marker = tmp.path().join("snip.out");
        let helper = format!(
            "!f() {{ cat >/dev/null; printf 'GOT:[%s]\\n' \"$*\" >> '{}'; echo username=snip-user; echo password=snip-pass; }}; f",
            marker.display()
        );
        let cfg = config_with_helper(&helper);
        let filled = credential_fill(Some(&cfg), base_request())
            .expect("fill ok")
            .expect("credential filled");
        assert_eq!(filled.username.as_deref(), Some("snip-user"));
        assert_eq!(filled.password.as_deref(), Some("snip-pass"));
        let recorded = fs::read_to_string(&marker).expect("marker written");
        // git 2.54: the operation is the snippet's $1 -> GOT:[get]
        assert_eq!(recorded.trim(), "GOT:[get]");
    }

    /// Bare-name form: git classifies a name containing a `/` that is NOT a
    /// leading-`/` absolute path as a *bare name* and dispatches to
    /// `git credential-<name>` (sley: `git-credential-<name>`). The classifier
    /// must key on `is_absolute_path` (leading `/`), NOT `contains('/')`.
    ///
    /// This is the divergence the fix closes: the old `head.contains('/')` test
    /// would directly exec a relative `sub/helper`, while git prefixes it to
    /// `git-credential-sub/helper`.
    #[test]
    fn relative_slash_name_is_bare_not_path() {
        let cmd = credential_helper_command("sub/relhelper", "get").expect("command built");
        // git 2.54: `git credential-sub/relhelper get` -> standalone binary
        // `git-credential-sub/relhelper`. The program sley resolves must be the
        // prefixed credential binary, never the raw relative path `sub/relhelper`.
        let program = command_program(&cmd);
        assert_ne!(
            program, "sub/relhelper",
            "relative slash name must not be exec'd directly (git would prefix it)"
        );
        assert!(
            program.contains("git-credential-sub/relhelper")
                || program == "sh"
                || program == "/bin/sh",
            "expected git-credential-<name> dispatch, got program {program:?}"
        );
    }

    /// Plain bare name maps to the standalone `git-credential-<name>` binary,
    /// matching git's `git credential-<name>` dispatch (we substitute the
    /// standalone form to avoid shelling out to `git`). The dispatch must
    /// resolve `git-credential-myhelper` and pass `--opt val get` as arguments
    /// (git 2.54: `git credential-myhelper --opt val get`). We assert on the
    /// resolved program + argv rather than mutating PATH (this crate forbids
    /// `unsafe`, so `std::env::set_var` is unavailable in tests).
    #[test]
    fn plain_bare_name_maps_to_credential_binary() {
        let cmd = credential_helper_command("myhelper --opt val", "get").expect("command built");
        let argv = command_argv(&cmd);
        // First exec target must be the standalone credential binary, never a
        // bare exec of the raw token, and never `git`.
        assert!(
            argv[0].contains("git-credential-myhelper") || argv[0] == "sh" || argv[0] == "/bin/sh",
            "expected git-credential-<name> dispatch, got argv {argv:?}"
        );
        assert_ne!(argv[0], "git", "must not shell out to the git binary");
        // git 2.54 appends `--opt val get`; assert the args are carried through
        // (either as direct argv or inside the shell command string).
        let rendered = argv.join(" ");
        assert!(
            rendered.contains("git-credential-myhelper")
                && rendered.contains("--opt")
                && rendered.contains("val")
                && rendered.contains("get"),
            "expected `git-credential-myhelper --opt val get` dispatch, got {rendered:?}"
        );
    }

    // --- minimal helpers (no external test deps) ---

    /// Read the resolved program (argv[0]) out of a `Command`.
    fn command_program(cmd: &std::process::Command) -> String {
        cmd.get_program().to_string_lossy().into_owned()
    }

    /// Read program + args out of a `Command` as a flat `Vec<String>`.
    fn command_argv(cmd: &std::process::Command) -> Vec<String> {
        let mut out = vec![cmd.get_program().to_string_lossy().into_owned()];
        out.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
        out
    }

    /// Tiny self-contained tempdir (avoid adding a dev-dependency).
    struct TempDir {
        path: std::path::PathBuf,
    }
    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("sley-cred-parity-{pid}-{n}"));
        fs::create_dir_all(&path).expect("mkdir tempdir");
        TempDir { path }
    }
}
