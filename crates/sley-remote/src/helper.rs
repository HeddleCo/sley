//! Git remote-helper discovery and line-protocol support.
//!
//! A remote helper is user-supplied (`git-remote-<name>` on `PATH`). Built-in
//! transports are deliberately excluded here: Sley must never fall through to
//! an installed Git's core `git-remote-http` (or similar) executable.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use sley_config::GitConfig;
use sley_config::remotes::{remote_config_values, remote_exists, rewrite_url_with_config};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_protocol::{RefAdvertisement, parse_refspec, refspec_map_source};
use sley_refs::{FileRefStore, RefTarget, RefUpdate};

/// A resolved user-owned remote-helper invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHelperSpec {
    /// The suffix in `git-remote-<name>`.
    pub name: String,
    /// The helper's first argument (a configured remote name or literal URL).
    pub alias: String,
    /// The optional helper URL argument. `remote.<name>.vcs` is valid without a
    /// URL, so this is intentionally optional.
    pub url: Option<String>,
}

/// Resolve a custom remote helper from a remote name/URL and effective config.
///
/// Returns `None` for Sley's native transports. In particular, names used by
/// Git's core remote helpers are never returned, preventing an installed Git's
/// executables from becoming an accidental implementation dependency.
pub fn resolve_remote_helper(config: &GitConfig, remote: &str) -> Option<RemoteHelperSpec> {
    let named = remote_exists(config, remote);
    if named && let Some(vcs) = config.get("remote", Some(remote), "vcs") {
        if native_helper_name(vcs) {
            return None;
        }
        let url = remote_config_values(config, remote, "url")
            .into_iter()
            .next()
            .map(|url| rewrite_url_with_config(config, &url, false));
        return Some(RemoteHelperSpec {
            name: vcs.to_string(),
            alias: remote.to_string(),
            url,
        });
    }

    let resolved = if named {
        remote_config_values(config, remote, "url")
            .into_iter()
            .next()
            .map(|url| rewrite_url_with_config(config, &url, false))?
    } else {
        rewrite_url_with_config(config, remote, false)
    };
    if let Some((name, url)) = split_double_colon_helper(&resolved) {
        if native_helper_name(name) {
            return None;
        }
        return Some(RemoteHelperSpec {
            name: name.to_string(),
            alias: if named {
                remote.to_string()
            } else {
                resolved.clone()
            },
            url: Some(url.to_string()),
        });
    }
    let name = unknown_url_scheme(&resolved)?;
    if native_helper_name(name) {
        return None;
    }
    Some(RemoteHelperSpec {
        name: name.to_string(),
        alias: if named {
            remote.to_string()
        } else {
            resolved.clone()
        },
        url: Some(resolved),
    })
}

fn native_helper_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "file" | "local" | "ssh" | "git" | "ext" | "fd" | "http" | "https" | "ftp" | "ftps"
    )
}

fn split_double_colon_helper(value: &str) -> Option<(&str, &str)> {
    let (name, url) = value.split_once("::")?;
    helper_scheme_name_is_valid(name).then_some((name, url))
}

fn unknown_url_scheme(value: &str) -> Option<&str> {
    let (name, _) = value.split_once("://")?;
    helper_scheme_name_is_valid(name).then_some(name)
}

fn helper_scheme_name_is_valid(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
}

/// Capabilities advertised by a remote helper.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteHelperCapabilities {
    pub import: bool,
    pub export: bool,
    pub option: bool,
    pub object_format: bool,
    pub signed_tags: bool,
    pub no_private_update: bool,
    pub refspecs: Vec<String>,
    pub import_marks: Option<String>,
    pub export_marks: Option<String>,
}

impl RemoteHelperCapabilities {
    fn parse(lines: &[String]) -> Result<Self> {
        let mut out = Self::default();
        for line in lines {
            let mandatory = line.starts_with('*');
            let line = line.strip_prefix('*').unwrap_or(line);
            let mut recognized = true;
            match line {
                "import" => out.import = true,
                "export" => out.export = true,
                "option" => out.option = true,
                "object-format" => out.object_format = true,
                "signed-tags" => out.signed_tags = true,
                "no-private-update" => out.no_private_update = true,
                _ => {
                    if let Some(value) = line.strip_prefix("refspec ") {
                        out.refspecs.push(value.to_string());
                    } else if let Some(value) = line.strip_prefix("import-marks ") {
                        out.import_marks = Some(value.to_string());
                    } else if let Some(value) = line.strip_prefix("export-marks ") {
                        out.export_marks = Some(value.to_string());
                    } else {
                        recognized = false;
                    }
                }
            }
            if mandatory && !recognized {
                return Err(GitError::Unsupported(format!(
                    "unknown mandatory remote-helper capability '{line}'"
                )));
            }
        }
        Ok(out)
    }
}

/// One entry from a helper's `list` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteHelperRefValue {
    Object(ObjectId),
    Unknown,
    Symbolic(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHelperRef {
    pub name: String,
    pub value: RemoteHelperRefValue,
}

/// A live remote-helper process. The caller may inspect capabilities/listing,
/// then consume the session into either an import or export operation.
pub struct RemoteHelperSession {
    spec: RemoteHelperSpec,
    format: ObjectFormat,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    capabilities: RemoteHelperCapabilities,
}

impl RemoteHelperSession {
    pub fn start(spec: RemoteHelperSpec, git_dir: &Path, format: ObjectFormat) -> Result<Self> {
        let executable = format!("git-remote-{}", spec.name);
        Self::start_with_executable(spec, git_dir, format, executable)
    }

    fn start_with_executable(
        spec: RemoteHelperSpec,
        git_dir: &Path,
        format: ObjectFormat,
        executable: impl AsRef<std::ffi::OsStr>,
    ) -> Result<Self> {
        let mut command = Command::new(executable.as_ref());
        command.arg(&spec.alias);
        if let Some(url) = spec.url.as_deref() {
            command.arg(url);
        }
        let mut child = command
            .env("GIT_DIR", git_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| {
                GitError::Command(format!(
                    "unable to find remote helper for '{}': {err}",
                    spec.name
                ))
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            GitError::Command(format!("remote helper '{}' has no stdin", spec.name))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            GitError::Command(format!("remote helper '{}' has no stdout", spec.name))
        })?;
        let mut session = Self {
            spec,
            format,
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            capabilities: RemoteHelperCapabilities::default(),
        };
        session.write_line("capabilities")?;
        let capability_lines = session.read_block()?;
        if capability_lines.is_empty() {
            return Err(session.aborted_error());
        }
        session.capabilities = RemoteHelperCapabilities::parse(&capability_lines)?;
        if session.capabilities.option && session.capabilities.object_format {
            session.write_line("option object-format true")?;
            let response = session.read_line()?;
            if response != "ok" && response != "unsupported" {
                return Err(GitError::Command(format!(
                    "remote helper '{}' rejected object format {}: {response}",
                    session.spec.name,
                    format.name()
                )));
            }
        }
        Ok(session)
    }

    pub fn capabilities(&self) -> &RemoteHelperCapabilities {
        &self.capabilities
    }

    pub fn list(&mut self) -> Result<Vec<RemoteHelperRef>> {
        self.write_line("list")?;
        let lines = self.read_block()?;
        if lines.is_empty() {
            return Err(self.aborted_error());
        }
        let mut refs = Vec::new();
        for line in lines {
            if let Some(value) = line.strip_prefix(":object-format ") {
                if value != self.format.name() {
                    return Err(GitError::InvalidObjectId(format!(
                        "remote helper uses {value}, local repository uses {}",
                        self.format.name()
                    )));
                }
                continue;
            }
            refs.push(parse_list_line(&line, self.capabilities.object_format)?);
        }
        Ok(refs)
    }

    /// Negotiate one standard remote-helper option. Returns `false` when the
    /// helper reports `unsupported`.
    pub fn set_option(&mut self, name: &str, value: &str) -> Result<bool> {
        if !self.capabilities.option {
            return Ok(false);
        }
        self.write_line(&format!("option {name} {value}"))?;
        match self.read_line()?.as_str() {
            "ok" => Ok(true),
            "unsupported" => Ok(false),
            response => Err(GitError::Command(format!(
                "remote helper '{}' rejected option {name}: {response}",
                self.spec.name
            ))),
        }
    }

    /// Request an import and return the complete fast-import byte stream.
    /// The session is consumed: closing helper stdin after the request lets a
    /// one-operation helper exit without leaving a protocol process behind.
    pub fn import(mut self, refs: &[String]) -> Result<Vec<u8>> {
        if !self.capabilities.import {
            return Err(GitError::Unsupported(format!(
                "remote helper '{}' does not support import",
                self.spec.name
            )));
        }
        for reference in refs {
            self.write_line(&format!("import {reference}"))?;
        }
        self.write_raw(b"\n")?;
        drop(self.stdin.take());
        let mut stream = Vec::new();
        self.stdout.read_to_end(&mut stream)?;
        let status = self.child.wait()?;
        if !status.success() {
            return Err(GitError::Command(format!(
                "error while running remote helper '{}' import",
                self.spec.name
            )));
        }
        Ok(stream)
    }

    /// Send a fast-export stream and return the helper's status response.
    pub fn export(mut self, stream: &[u8]) -> Result<Vec<String>> {
        if !self.capabilities.export {
            return Err(GitError::Unsupported(format!(
                "remote helper '{}' does not support export",
                self.spec.name
            )));
        }
        self.write_line("export")?;
        self.write_raw(stream)?;
        drop(self.stdin.take());
        let mut response = String::new();
        self.stdout.read_to_string(&mut response)?;
        let status = self.child.wait()?;
        if !status.success() {
            return Err(GitError::Command(format!(
                "error while running remote helper '{}' export",
                self.spec.name
            )));
        }
        Ok(response
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        self.write_raw(format!("{line}\n").as_bytes())
    }

    fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(GitError::Command(format!(
                "remote helper '{}' aborted session",
                self.spec.name
            )));
        };
        stdin.write_all(bytes)?;
        stdin.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let count = self.stdout.read_line(&mut line)?;
        if count == 0 {
            return Err(self.aborted_error());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    fn read_block(&mut self) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        loop {
            let line = self.read_line()?;
            if line.is_empty() {
                return Ok(lines);
            }
            lines.push(line);
        }
    }

    fn aborted_error(&mut self) -> GitError {
        let _ = self.child.try_wait();
        GitError::Command(format!(
            "remote helper '{}' aborted session",
            self.spec.name
        ))
    }
}

impl Drop for RemoteHelperSession {
    fn drop(&mut self) {
        // Closing stdin first lets well-behaved helpers terminate naturally.
        // Drop cannot wait unboundedly for a helper that ignores EOF, so reap an
        // already-exited child and otherwise kill then wait (preventing zombies).
        drop(self.stdin.take());
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

fn parse_list_line(line: &str, object_format_capability: bool) -> Result<RemoteHelperRef> {
    if line.starts_with(':') {
        return Err(GitError::InvalidFormat(format!(
            "unexpected remote-helper attribute: {line}"
        )));
    }
    let (value, name) = line
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat(format!("malformed remote-helper ref: {line}")))?;
    let value = if value == "?" {
        RemoteHelperRefValue::Unknown
    } else if let Some(target) = value.strip_prefix('@') {
        RemoteHelperRefValue::Symbolic(target.to_string())
    } else {
        let format = if object_format_capability && value.len() == ObjectFormat::Sha256.hex_len() {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        };
        RemoteHelperRefValue::Object(ObjectId::from_hex(format, value)?)
    };
    Ok(RemoteHelperRef {
        name: name.to_string(),
        value,
    })
}

/// Convert a helper listing into ordinary advertisements after its import
/// stream has been installed. Unknown object IDs are resolved from the private
/// namespaces declared by `refspec` capabilities.
pub fn imported_remote_helper_advertisements(
    git_dir: &Path,
    format: ObjectFormat,
    capabilities: &RemoteHelperCapabilities,
    refs: &[RemoteHelperRef],
) -> Result<(Vec<RefAdvertisement>, Option<String>)> {
    let store = FileRefStore::new(git_dir, format);
    let mappings = capabilities
        .refspecs
        .iter()
        .map(|spec| parse_refspec(spec))
        .collect::<Result<Vec<_>>>()?;
    let mut advertisements = Vec::new();
    let mut head_symref = None;
    for reference in refs {
        if let RemoteHelperRefValue::Symbolic(target) = &reference.value {
            if reference.name == "HEAD" {
                head_symref = Some(target.clone());
            }
            continue;
        }
        let oid = match reference.value {
            RemoteHelperRefValue::Object(oid) => oid,
            RemoteHelperRefValue::Unknown => {
                let mut mapped = None;
                for mapping in mappings.iter().filter(|mapping| !mapping.negative) {
                    if let Some(destination) = refspec_map_source(mapping, &reference.name)? {
                        mapped = Some(destination);
                        break;
                    }
                }
                let local_name = mapped.as_deref().unwrap_or(&reference.name);
                if let Some(oid) = helper_ref_oid(&store, local_name)? {
                    oid
                } else if mapped.is_some()
                    && let Some(oid) = helper_ref_oid(&store, &reference.name)?
                {
                    // Some importers (including older Sley fast-export) leave a
                    // pattern refspec's source spelling in the stream. Preserve
                    // the helper contract by materializing its declared private
                    // namespace before normal fetch ref planning continues.
                    let mut transaction = store.transaction();
                    transaction.update(RefUpdate {
                        name: local_name.to_string(),
                        expected: None,
                        new: RefTarget::Direct(oid),
                        reflog: None,
                    });
                    transaction.commit()?;
                    oid
                } else {
                    return Err(GitError::not_found(format!(
                        "remote-helper imported ref {local_name}"
                    )));
                }
            }
            RemoteHelperRefValue::Symbolic(_) => unreachable!(),
        };
        advertisements.push(RefAdvertisement {
            oid,
            name: reference.name.clone(),
            capabilities: Vec::new(),
        });
    }
    if let Some(target) = head_symref.as_deref()
        && let Some(target_ref) = advertisements
            .iter()
            .find(|reference| reference.name == target)
    {
        advertisements.push(RefAdvertisement {
            oid: target_ref.oid,
            name: "HEAD".to_string(),
            capabilities: Vec::new(),
        });
    }
    Ok((advertisements, head_symref))
}

/// Rewrite branch/reset destinations in a helper-provided fast-import stream
/// through its declared import refspecs. This is byte-aware: counted `data N`
/// payloads are copied verbatim, so blob or message contents that resemble
/// fast-import commands are never interpreted as protocol lines.
pub fn rewrite_remote_helper_import_stream(stream: &[u8], refspecs: &[String]) -> Result<Vec<u8>> {
    if refspecs.is_empty() {
        return Ok(stream.to_vec());
    }
    let mappings = refspecs
        .iter()
        .map(|spec| parse_refspec(spec))
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(stream.len());
    let mut offset = 0;
    while offset < stream.len() {
        let relative_end = stream[offset..].iter().position(|byte| *byte == b'\n');
        let line_end = relative_end.map_or(stream.len(), |end| offset + end);
        let line = &stream[offset..line_end];
        let has_newline = line_end < stream.len();
        if let Some(name) = line
            .strip_prefix(b"commit ")
            .or_else(|| line.strip_prefix(b"reset "))
        {
            let name = std::str::from_utf8(name)
                .map_err(|_| GitError::InvalidFormat("non-utf8 remote-helper ref".into()))?;
            let mut mapped = None;
            for mapping in mappings.iter().filter(|mapping| !mapping.negative) {
                if let Some(destination) = refspec_map_source(mapping, name)? {
                    mapped = Some(destination);
                    break;
                }
            }
            let prefix = if line.starts_with(b"commit ") {
                b"commit ".as_slice()
            } else {
                b"reset ".as_slice()
            };
            out.extend_from_slice(prefix);
            out.extend_from_slice(mapped.as_deref().unwrap_or(name).as_bytes());
        } else {
            out.extend_from_slice(line);
        }
        if has_newline {
            out.push(b'\n');
        }
        offset = line_end + usize::from(has_newline);
        if let Some(count) = line
            .strip_prefix(b"data ")
            .and_then(|count| std::str::from_utf8(count).ok())
            .and_then(|count| count.parse::<usize>().ok())
        {
            let data_end = offset.checked_add(count).ok_or_else(|| {
                GitError::InvalidFormat("remote-helper data length overflow".into())
            })?;
            if data_end > stream.len() {
                return Err(GitError::InvalidFormat(
                    "remote-helper data payload is truncated".into(),
                ));
            }
            out.extend_from_slice(&stream[offset..data_end]);
            offset = data_end;
        } else if let Some(delimiter) = line.strip_prefix(b"data <<") {
            if delimiter.is_empty() {
                return Err(GitError::InvalidFormat(
                    "remote-helper data delimiter is empty".into(),
                ));
            }
            loop {
                if offset >= stream.len() {
                    return Err(GitError::InvalidFormat(
                        "remote-helper delimited data is truncated".into(),
                    ));
                }
                let relative_end = stream[offset..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .ok_or_else(|| {
                        GitError::InvalidFormat(
                            "remote-helper delimited data has no terminator".into(),
                        )
                    })?;
                let payload_end = offset + relative_end;
                let payload_line = &stream[offset..payload_end];
                out.extend_from_slice(&stream[offset..=payload_end]);
                offset = payload_end + 1;
                if payload_line == delimiter {
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn helper_ref_oid(store: &FileRefStore, name: &str) -> Result<Option<ObjectId>> {
    Ok(match store.read_ref(name)? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        Some(RefTarget::Symbolic(target)) => sley_refs::resolve_ref_peeled(store, &target)?,
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_config::{ConfigEntry, ConfigSection};

    #[test]
    fn resolves_double_colon_and_vcs_helpers_but_not_core_helpers() {
        let empty = GitConfig::default();
        assert_eq!(
            resolve_remote_helper(&empty, "testgit::/tmp/repo"),
            Some(RemoteHelperSpec {
                name: "testgit".into(),
                alias: "testgit::/tmp/repo".into(),
                url: Some("/tmp/repo".into()),
            })
        );
        assert!(resolve_remote_helper(&empty, "https://example.com/repo").is_none());
        assert!(resolve_remote_helper(&empty, "fd::3").is_none());

        let config = GitConfig {
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin".into()),
                vec![
                    ConfigEntry::new("vcs", Some("testgit".into())),
                    ConfigEntry::new("url", Some("/tmp/repo".into())),
                ],
            )],
            ..GitConfig::default()
        };
        assert_eq!(
            resolve_remote_helper(&config, "origin"),
            Some(RemoteHelperSpec {
                name: "testgit".into(),
                alias: "origin".into(),
                url: Some("/tmp/repo".into()),
            })
        );
        let core_config = GitConfig {
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin".into()),
                vec![ConfigEntry::new("vcs", Some("fd".into()))],
            )],
            ..GitConfig::default()
        };
        assert!(resolve_remote_helper(&core_config, "origin").is_none());
    }

    #[test]
    fn parses_capabilities_and_unknown_refs() {
        let capabilities = RemoteHelperCapabilities::parse(&[
            "import".into(),
            "export".into(),
            "refspec refs/heads/*:refs/private/*".into(),
            "*import-marks /tmp/marks".into(),
        ])
        .expect("capabilities");
        assert!(capabilities.import && capabilities.export);
        assert_eq!(capabilities.refspecs, ["refs/heads/*:refs/private/*"]);
        assert_eq!(capabilities.import_marks.as_deref(), Some("/tmp/marks"));
        assert_eq!(
            parse_list_line("? refs/heads/main", false).expect("ref"),
            RemoteHelperRef {
                name: "refs/heads/main".into(),
                value: RemoteHelperRefValue::Unknown,
            }
        );
        assert!(RemoteHelperCapabilities::parse(&["*future-protocol".into()]).is_err());
        assert_eq!(
            RemoteHelperCapabilities::parse(&["future-protocol".into()])
                .expect("optional unknown capability"),
            RemoteHelperCapabilities::default()
        );
    }

    #[test]
    fn rewrites_import_refs_without_touching_counted_data() {
        let stream =
            b"commit refs/heads/main\ndata 20\nreset refs/heads/x\n\nreset refs/heads/topic\n";
        let rewritten = rewrite_remote_helper_import_stream(
            stream,
            &["refs/heads/*:refs/private/heads/*".into()],
        )
        .expect("rewrite");
        assert_eq!(
            rewritten,
            b"commit refs/private/heads/main\ndata 20\nreset refs/heads/x\n\nreset refs/private/heads/topic\n"
        );
    }

    #[test]
    fn rewrites_import_refs_without_touching_delimited_data() {
        let stream = b"commit refs/heads/main\ndata <<END\ncommit refs/heads/payload\nreset refs/heads/payload\nEND\nreset refs/heads/topic\n";
        let rewritten = rewrite_remote_helper_import_stream(
            stream,
            &["refs/heads/*:refs/private/heads/*".into()],
        )
        .expect("rewrite");
        assert_eq!(
            rewritten,
            b"commit refs/private/heads/main\ndata <<END\ncommit refs/heads/payload\nreset refs/heads/payload\nEND\nreset refs/private/heads/topic\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejected_mandatory_capability_reaps_a_waiting_helper() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sley-remote-helper-drop-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("temp dir");
        let helper = root.join("git-remote-waiting");
        std::fs::write(
            &helper,
            b"#!/bin/sh\nread command\nprintf '*future-protocol\\n\\n'\nsleep 30\n",
        )
        .expect("helper script");
        let mut permissions = std::fs::metadata(&helper).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("permissions");

        let started = Instant::now();
        let result = RemoteHelperSession::start_with_executable(
            RemoteHelperSpec {
                name: "waiting".into(),
                alias: "origin".into(),
                url: Some("unused".into()),
            },
            &root,
            ObjectFormat::Sha1,
            &helper,
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(root);
    }
}
