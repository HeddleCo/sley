//! Credential acquisition for authenticated remotes.
//!
//! Thin wrapper over [`sley_transport::credential`].

use sley_config::GitConfig;
use sley_core::Result;
use sley_transport::{
    GitCredential, RemoteTransport, RemoteUrl,
    credential::{credential_approve, credential_fill_simple, credential_reject},
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

/// Fill `request` using configured credential helpers.
pub fn credential_fill(
    config: Option<&GitConfig>,
    request: GitCredential,
) -> Result<Option<GitCredential>> {
    credential_fill_simple(config, request)
}

/// Tell configured helpers to store (`approve = true`) or erase a credential.
pub fn credential_store(config: Option<&GitConfig>, credential: &GitCredential, approve: bool) {
    let mut working = credential.clone();
    if approve {
        let _ = credential_approve(config, None, &mut working);
    } else {
        let _ = credential_reject(config, None, &mut working);
    }
}

/// The default [`CredentialProvider`]: fills and stores credentials via
/// `credential.helper` programs.
pub struct CredentialHelperProvider<'a> {
    config: Option<&'a GitConfig>,
}

impl<'a> CredentialHelperProvider<'a> {
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use sley_config::GitConfig;
    use sley_transport::GitCredential;
    use sley_transport::credential::{credential_fill_simple, credential_helper_command};

    use super::credential_fill;

    fn config_with_helper(helper: &str) -> GitConfig {
        let escaped = helper.replace('\\', "\\\\").replace('"', "\\\"");
        let body = format!("[credential]\n\thelper = \"{escaped}\"\n");
        GitConfig::parse(body.as_bytes()).expect("config parses")
    }

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
        assert_eq!(recorded.trim(), "ARGS:[--flag get]");
    }

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
        assert_eq!(recorded.trim(), "GOT:[get]");
    }

    #[test]
    fn relative_slash_name_is_bare_not_path() {
        let cmd = credential_helper_command("sub/relhelper", "get").expect("command built");
        let argv = command_argv(&cmd);
        assert_ne!(
            argv[0], "sub/relhelper",
            "relative slash name must not be exec'd directly"
        );
        assert!(
            argv.iter().any(|arg| arg == "credential-sub/relhelper")
                || argv[0] == "sh"
                || argv[0] == "/bin/sh",
            "expected credential-<name> dispatch, got argv {argv:?}"
        );
    }

    #[test]
    fn plain_bare_name_maps_to_credential_binary() {
        let cmd = credential_helper_command("myhelper --opt val", "get").expect("command built");
        let argv = command_argv(&cmd);
        assert!(
            argv.iter().any(|arg| arg == "credential-myhelper")
                || argv[0] == "sh"
                || argv[0] == "/bin/sh",
            "expected credential-<name> dispatch, got argv {argv:?}"
        );
        let rendered = argv.join(" ");
        assert!(
            rendered.contains("credential-myhelper")
                && rendered.contains("--opt")
                && rendered.contains("val")
                && rendered.contains("get"),
            "expected `credential-myhelper --opt val get` dispatch, got {rendered:?}"
        );
    }

    fn command_program(cmd: &std::process::Command) -> String {
        cmd.get_program().to_string_lossy().into_owned()
    }

    fn command_argv(cmd: &std::process::Command) -> Vec<String> {
        let mut out = vec![cmd.get_program().to_string_lossy().into_owned()];
        out.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
        out
    }

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