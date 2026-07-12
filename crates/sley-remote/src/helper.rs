//! Git remote-helper discovery and line-protocol support.
//!
//! A remote helper is user-supplied (`git-remote-<name>` on `PATH`). Built-in
//! transports are deliberately excluded here: Sley must never fall through to
//! an installed Git's core `git-remote-http` (or similar) executable.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use sley_config::GitConfig;
use sley_config::remotes::{remote_config_values, remote_exists, rewrite_url_with_config};
use sley_core::{CliExit, GitError, ObjectFormat, ObjectId, Result};
use sley_protocol::{RefAdvertisement, parse_refspec, refspec_map_source};
use sley_refs::{FileRefStore, RefTarget, RefUpdate};

use crate::{CredentialProvider, FetchOptions, FetchServices, ProgressSink};

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

/// A runtime request for the fast-export half of a remote-helper push.
///
/// The helper engine owns protocol sequencing and supplies the resolved
/// capability values. The caller owns how native fast-export is executed.
pub struct RemoteHelperExportRequest<'a> {
    /// Local repository receiving the helper operation.
    pub git_dir: &'a Path,
    /// Object format negotiated with the helper.
    pub format: ObjectFormat,
    /// Preserve signed tag objects verbatim during export.
    pub signed_tags: bool,
    /// Existing marks file the helper asked fast-export to consume.
    pub import_marks: Option<&'a str>,
    /// Marks file the helper asked fast-export to update.
    pub export_marks: Option<&'a str>,
    /// Expanded source-to-destination refspecs to export.
    pub refspecs: &'a [String],
}

/// Injected native plumbing used by import/export remote helpers.
///
/// The CLI implementation re-enters the currently running Sley executable;
/// embedders can provide in-process implementations without depending on an
/// installed Git executable.
pub trait RemoteHelperPlumbing {
    /// Install one helper-produced fast-import stream into `git_dir`.
    fn fast_import(&mut self, git_dir: &Path, stream: &[u8]) -> Result<()>;
    /// Produce the fast-export stream requested by the helper engine.
    fn fast_export(&mut self, request: RemoteHelperExportRequest<'_>) -> Result<Vec<u8>>;
}

/// Structured presentation events emitted while executing a helper operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteHelperEvent {
    /// Import helper omitted the recommended private `refspec` capability.
    MissingRefspecCapability,
    /// The helper process failed while producing its fast-import stream.
    FastImportFailed,
    /// Helper rejected a destination and supplied this protocol detail.
    PushRejected(String),
}

/// Presentation seam for helper warnings and per-ref rejection diagnostics.
pub trait RemoteHelperEventSink {
    /// Receive one structured event in protocol order.
    fn event(&mut self, event: RemoteHelperEvent);
}

/// Fully resolved inputs for a remote-helper fetch.
pub struct RemoteHelperFetchOperation<'a> {
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Effective repository configuration.
    pub config: &'a GitConfig,
    /// Remote name used for fetch config and FETCH_HEAD descriptions.
    pub remote_name: &'a str,
    /// Already-resolved user-owned helper invocation.
    pub spec: &'a RemoteHelperSpec,
    /// Caller-requested fetch refspecs.
    pub refspecs: &'a [String],
    /// Ordinary fetch behavior applied after helper import.
    pub options: &'a FetchOptions,
}

/// Fetch inputs used after a live helper has already negotiated capabilities,
/// refs, and object format during clone bootstrap.
pub struct DiscoveredRemoteHelperFetchOperation<'a> {
    /// Final repository `$GIT_DIR`; must equal the discovery directory.
    pub git_dir: &'a Path,
    /// Object format adopted from helper discovery.
    pub format: ObjectFormat,
    /// Effective configuration after clone bootstrap finalization.
    pub config: &'a GitConfig,
    /// Configured remote name used for fetch finalization.
    pub remote_name: &'a str,
    /// Caller-requested fetch refspecs.
    pub refspecs: &'a [String],
    /// Ordinary fetch behavior applied after helper import.
    pub options: &'a FetchOptions,
}

/// Runtime seams used by a remote-helper fetch.
pub struct RemoteHelperFetchServices<'a> {
    /// Native fast-import/export runtime.
    pub plumbing: &'a mut dyn RemoteHelperPlumbing,
    /// Warning and rejection presentation sink.
    pub events: &'a mut dyn RemoteHelperEventSink,
    /// Credential provider passed to ordinary fetch finalization.
    pub credentials: &'a mut dyn CredentialProvider,
    /// Fetch progress and prune-event sink.
    pub progress: &'a mut dyn ProgressSink,
    /// Optional reference-transaction hook runner.
    pub ref_hook: Option<&'a dyn sley_refs::ReferenceTransactionHook>,
}

/// Controls for the protocol-affecting portion of a remote-helper push.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteHelperPushOptions {
    /// Ask an option-capable helper to accept non-fast-forward updates.
    pub force: bool,
    /// Validate and discover the helper without exporting objects or refs.
    pub dry_run: bool,
}

/// Classified failure from [`push_via_remote_helper`].
#[derive(Debug)]
pub enum RemoteHelperPushError {
    /// The helper omitted the mandatory private `refspec` mapping needed by the
    /// import/export push protocol.
    RefspecRequired,
    /// Git v2.55 rejects export helpers that advertise neither import nor
    /// export marks; callers preserve that oracle-visible classification.
    MarksRequired,
    /// Any other protocol, repository, runtime, or ref-update failure.
    Engine(GitError),
}

impl std::fmt::Display for RemoteHelperPushError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RefspecRequired => {
                formatter.write_str("remote-helper doesn't support push; refspec needed")
            }
            Self::MarksRequired => formatter.write_str("remote-helper export requires marks"),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RemoteHelperPushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::RefspecRequired | Self::MarksRequired => None,
        }
    }
}

impl From<GitError> for RemoteHelperPushError {
    fn from(error: GitError) -> Self {
        Self::Engine(error)
    }
}

/// Fully resolved inputs for a remote-helper push.
pub struct RemoteHelperPushOperation<'a> {
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Effective repository configuration.
    pub config: &'a GitConfig,
    /// Remote name used to update configured tracking refs.
    pub remote_name: &'a str,
    /// Already-resolved user-owned helper invocation.
    pub spec: &'a RemoteHelperSpec,
    /// Caller-requested push refspecs before wildcard expansion.
    pub refspecs: &'a [String],
    /// Protocol-affecting push controls.
    pub options: RemoteHelperPushOptions,
}

/// Runtime seams used by a remote-helper push.
pub struct RemoteHelperPushServices<'a> {
    /// Native fast-import/export runtime.
    pub plumbing: &'a mut dyn RemoteHelperPlumbing,
    /// Warning and rejection presentation sink.
    pub events: &'a mut dyn RemoteHelperEventSink,
}

/// Structured result of a successful remote-helper push.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteHelperPushOutcome {
    /// Concrete refspecs sent to fast-export after wildcard expansion.
    pub expanded_refspecs: Vec<String>,
    /// Destination ref names acknowledged by the helper.
    pub successful_refs: Vec<String>,
    /// Whether execution stopped after helper discovery because of `dry_run`.
    pub dry_run: bool,
}

/// A live remote-helper process. The caller may inspect capabilities/listing,
/// then consume the session into either an import or export operation.
pub struct RemoteHelperSession {
    spec: RemoteHelperSpec,
    format: ObjectFormat,
    adopt_advertised_format: bool,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    capabilities: RemoteHelperCapabilities,
}

impl RemoteHelperSession {
    pub fn start(spec: RemoteHelperSpec, git_dir: &Path, format: ObjectFormat) -> Result<Self> {
        let executable = format!("git-remote-{}", spec.name);
        Self::start_with_executable_mode(spec, git_dir, format, false, executable)
    }

    /// Start a helper against a valid provisional repository, allowing its
    /// `list` response to select the final object format before import begins.
    pub fn start_for_discovery(
        spec: RemoteHelperSpec,
        git_dir: &Path,
        provisional_format: ObjectFormat,
    ) -> Result<Self> {
        let executable = format!("git-remote-{}", spec.name);
        Self::start_with_executable_mode(spec, git_dir, provisional_format, true, executable)
    }

    #[cfg(test)]
    fn start_with_executable(
        spec: RemoteHelperSpec,
        git_dir: &Path,
        format: ObjectFormat,
        executable: impl AsRef<std::ffi::OsStr>,
    ) -> Result<Self> {
        Self::start_with_executable_mode(spec, git_dir, format, false, executable)
    }

    fn start_with_executable_mode(
        spec: RemoteHelperSpec,
        git_dir: &Path,
        format: ObjectFormat,
        adopt_advertised_format: bool,
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
            adopt_advertised_format,
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

    pub fn object_format(&self) -> ObjectFormat {
        self.format
    }

    pub fn list(&mut self) -> Result<Vec<RemoteHelperRef>> {
        self.write_line("list")?;
        let lines = self.read_block()?;
        if lines.is_empty() {
            return Err(self.aborted_error());
        }
        for line in &lines {
            let Some(value) = line.strip_prefix(":object-format ") else {
                continue;
            };
            let advertised = parse_helper_object_format(value)?;
            if self.adopt_advertised_format {
                self.format = advertised;
                self.adopt_advertised_format = false;
            } else if advertised != self.format {
                return Err(GitError::InvalidObjectId(format!(
                    "remote helper uses {value}, local repository uses {}",
                    self.format.name()
                )));
            }
        }
        let mut refs = Vec::new();
        for line in lines {
            if line.starts_with(":object-format ") {
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
        GitError::cli_exit(
            CliExit::UserError,
            format!("remote helper '{}' aborted session", self.spec.name),
        )
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

fn parse_helper_object_format(value: &str) -> Result<ObjectFormat> {
    match value {
        "sha1" => Ok(ObjectFormat::Sha1),
        "sha256" => Ok(ObjectFormat::Sha256),
        _ => Err(GitError::InvalidFormat(format!(
            "remote helper uses unknown object format {value}"
        ))),
    }
}

/// A live helper after capability and ref discovery, before any import or
/// local object/ref mutation. Clone can finalize its provisional repository's
/// object format and then continue this same protocol process.
pub struct RemoteHelperFetchDiscovery {
    session: RemoteHelperSession,
    capabilities: RemoteHelperCapabilities,
    refs: Vec<RemoteHelperRef>,
    git_dir: PathBuf,
}

impl RemoteHelperFetchDiscovery {
    pub fn object_format(&self) -> ObjectFormat {
        self.session.object_format()
    }
}

pub fn discover_remote_helper_fetch(
    spec: RemoteHelperSpec,
    git_dir: &Path,
    provisional_format: ObjectFormat,
) -> Result<RemoteHelperFetchDiscovery> {
    let mut session = RemoteHelperSession::start_for_discovery(spec, git_dir, provisional_format)?;
    let capabilities = session.capabilities().clone();
    let refs = session.list()?;
    Ok(RemoteHelperFetchDiscovery {
        session,
        capabilities,
        refs,
        git_dir: git_dir.to_path_buf(),
    })
}

/// Execute a user-owned remote helper's import flow and finalize it through the
/// ordinary fetch engine. Repository semantics remain identical to native
/// transports: refspec planning, FETCH_HEAD, pruning, and ref transactions all
/// pass through [`crate::finalize_remote_helper_fetch`].
pub fn fetch_via_remote_helper(
    request: RemoteHelperFetchOperation<'_>,
    services: RemoteHelperFetchServices<'_>,
) -> Result<crate::FetchOutcome> {
    let mut session =
        RemoteHelperSession::start(request.spec.clone(), request.git_dir, request.format)?;
    let capabilities = session.capabilities().clone();
    let refs = session.list()?;
    fetch_via_discovered_remote_helper(
        RemoteHelperFetchDiscovery {
            session,
            capabilities,
            refs,
            git_dir: request.git_dir.to_path_buf(),
        },
        DiscoveredRemoteHelperFetchOperation {
            git_dir: request.git_dir,
            format: request.format,
            config: request.config,
            remote_name: request.remote_name,
            refspecs: request.refspecs,
            options: request.options,
        },
        services,
    )
}

/// Continue a previously discovered helper session after clone has finalized
/// the provisional repository's object format. No second helper process is
/// started, so helper-private marks and protocol state remain intact.
pub fn fetch_via_discovered_remote_helper(
    discovery: RemoteHelperFetchDiscovery,
    request: DiscoveredRemoteHelperFetchOperation<'_>,
    services: RemoteHelperFetchServices<'_>,
) -> Result<crate::FetchOutcome> {
    if discovery.git_dir != request.git_dir {
        return Err(GitError::InvalidPath(
            "remote helper discovery and import must use the same git directory".into(),
        ));
    }
    if discovery.object_format() != request.format {
        return Err(GitError::InvalidFormat(format!(
            "remote helper uses {}, local repository uses {}",
            discovery.object_format().name(),
            request.format.name()
        )));
    }
    let RemoteHelperFetchDiscovery {
        session,
        capabilities,
        refs,
        git_dir: _,
    } = discovery;
    if capabilities.refspecs.is_empty() {
        services
            .events
            .event(RemoteHelperEvent::MissingRefspecCapability);
    }
    let import_refs = refs
        .iter()
        .filter(|reference| !matches!(reference.value, RemoteHelperRefValue::Symbolic(_)))
        .map(|reference| reference.name.clone())
        .collect::<Vec<_>>();
    let source_refs_before_import = if capabilities.refspecs.is_empty() {
        Vec::new()
    } else {
        snapshot_helper_source_refs(request.git_dir, request.format, &import_refs)?
    };
    let stream = match session.import(&import_refs) {
        Ok(stream) => stream,
        Err(error) => {
            services.events.event(RemoteHelperEvent::FastImportFailed);
            return Err(error);
        }
    };
    let stream = rewrite_remote_helper_import_stream(&stream, &capabilities.refspecs)?;
    services.plumbing.fast_import(request.git_dir, &stream)?;
    let (advertisements, head_symref) = imported_remote_helper_advertisements(
        request.git_dir,
        request.format,
        &capabilities,
        &refs,
    )?;
    restore_helper_import_source_refs(request.git_dir, request.format, &source_refs_before_import)?;
    crate::finalize_remote_helper_fetch(
        crate::RemoteHelperFetchRequest {
            git_dir: request.git_dir,
            format: request.format,
            config: request.config,
            remote_name: request.remote_name,
            advertisements: &advertisements,
            head_symref,
            refspecs: request.refspecs,
            options: request.options,
        },
        FetchServices {
            credentials: services.credentials,
            progress: services.progress,
            ref_hook: services.ref_hook,
        },
    )
}

/// Execute a user-owned remote helper's export flow, including wildcard
/// expansion, marks rollback on helper failure, and tracking/private-ref
/// updates for successful destinations.
pub fn push_via_remote_helper(
    request: RemoteHelperPushOperation<'_>,
    services: RemoteHelperPushServices<'_>,
) -> std::result::Result<RemoteHelperPushOutcome, RemoteHelperPushError> {
    let mut session =
        RemoteHelperSession::start(request.spec.clone(), request.git_dir, request.format)?;
    let capabilities = session.capabilities().clone();
    let _remote_refs = session.list()?;
    if capabilities.refspecs.is_empty() {
        return Err(RemoteHelperPushError::RefspecRequired);
    }
    // Git v2.55's import/export helper path still treats a marks-free export as
    // unsupported. Keep that oracle behavior until the enrolled TODO changes.
    if capabilities.import_marks.is_none() && capabilities.export_marks.is_none() {
        return Err(RemoteHelperPushError::MarksRequired);
    }
    if request.options.dry_run {
        return Ok(RemoteHelperPushOutcome {
            dry_run: true,
            ..RemoteHelperPushOutcome::default()
        });
    }
    if request.options.force {
        let _ = session.set_option("force", "true")?;
    }
    let refspecs = expand_helper_push_refspecs(request.git_dir, request.format, request.refspecs)?;
    let marks_snapshot = snapshot_marks(capabilities.export_marks.as_deref())?;
    let stream = services.plumbing.fast_export(RemoteHelperExportRequest {
        git_dir: request.git_dir,
        format: request.format,
        signed_tags: capabilities.signed_tags,
        import_marks: capabilities.import_marks.as_deref(),
        export_marks: capabilities.export_marks.as_deref(),
        refspecs: &refspecs,
    })?;
    let response = match session.export(&stream) {
        Ok(response) => response,
        Err(error) => {
            restore_marks(marks_snapshot)?;
            return Err(error.into());
        }
    };
    let mut failed = false;
    let mut successful = Vec::new();
    for line in response {
        if let Some(reference) = line.strip_prefix("ok ") {
            successful.push(reference.to_string());
        } else if let Some(rest) = line.strip_prefix("error ") {
            failed = true;
            services
                .events
                .event(RemoteHelperEvent::PushRejected(rest.to_string()));
        }
    }
    if failed {
        return Err(GitError::Exit(1).into());
    }
    update_helper_push_tracking_refs(
        request.git_dir,
        request.format,
        request.config,
        request.remote_name,
        &capabilities,
        &refspecs,
        &successful,
    )?;
    Ok(RemoteHelperPushOutcome {
        expanded_refspecs: refspecs,
        successful_refs: successful,
        dry_run: false,
    })
}

fn expand_helper_push_refspecs(
    git_dir: &Path,
    format: ObjectFormat,
    refspecs: &[String],
) -> Result<Vec<String>> {
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    let mut expanded = Vec::new();
    for raw in refspecs {
        let force = raw.starts_with('+');
        let normalized = crate::normalize_push_refspec(raw);
        let body = normalized.strip_prefix('+').unwrap_or(&normalized);
        let (src, dst) = body.split_once(':').unwrap_or((body, body));
        let (Some((src_prefix, src_suffix)), Some((dst_prefix, dst_suffix))) =
            (src.split_once('*'), dst.split_once('*'))
        else {
            expanded.push(normalized);
            continue;
        };
        for reference in &refs {
            let Some(stem) = reference
                .name
                .strip_prefix(src_prefix)
                .and_then(|rest| rest.strip_suffix(src_suffix))
            else {
                continue;
            };
            expanded.push(format!(
                "{}{}:{}{}{}",
                if force { "+" } else { "" },
                reference.name,
                dst_prefix,
                stem,
                dst_suffix
            ));
        }
    }
    Ok(expanded)
}

struct MarksSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

fn snapshot_marks(path: Option<&str>) -> Result<Option<MarksSnapshot>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    let contents = match fs::read(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(Some(MarksSnapshot { path, contents }))
}

fn restore_marks(snapshot: Option<MarksSnapshot>) -> Result<()> {
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    match snapshot.contents {
        Some(contents) => fs::write(snapshot.path, contents)?,
        None => match fs::remove_file(snapshot.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

fn update_helper_push_tracking_refs(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    remote: &str,
    capabilities: &RemoteHelperCapabilities,
    refspecs: &[String],
    successful: &[String],
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let private_mappings = capabilities
        .refspecs
        .iter()
        .map(|spec| parse_refspec(spec))
        .collect::<Result<Vec<_>>>()?;
    let tracking_mappings = config
        .get_all("remote", Some(remote), "fetch")
        .into_iter()
        .flatten()
        .map(parse_refspec)
        .collect::<Result<Vec<_>>>()?;
    for refspec in refspecs {
        let normalized = crate::normalize_push_refspec(refspec);
        let body = normalized.strip_prefix('+').unwrap_or(&normalized);
        let (source, destination) = body.split_once(':').unwrap_or((body, body));
        if !successful.iter().any(|name| name == destination) {
            continue;
        }
        let oid = if source.is_empty() {
            None
        } else {
            sley_refs::resolve_ref_peeled(&store, source)?
        };
        let mut destinations = Vec::new();
        if !capabilities.no_private_update {
            for mapping in &private_mappings {
                if let Some(name) = refspec_map_source(mapping, destination)? {
                    destinations.push(name);
                }
            }
        }
        for mapping in &tracking_mappings {
            if let Some(name) = refspec_map_source(mapping, destination)? {
                destinations.push(name);
            }
        }
        destinations.sort();
        destinations.dedup();
        for name in destinations {
            match oid {
                Some(oid) => {
                    let mut transaction = store.transaction();
                    transaction.update(RefUpdate {
                        name,
                        expected: None,
                        new: RefTarget::Direct(oid),
                        reflog: None,
                    });
                    transaction.commit()?;
                }
                None if store.read_ref(&name)?.is_some() => {
                    store.delete_ref(&name)?;
                }
                None => {}
            }
        }
    }
    Ok(())
}

fn snapshot_helper_source_refs(
    git_dir: &Path,
    format: ObjectFormat,
    refs: &[String],
) -> Result<Vec<(String, Option<RefTarget>)>> {
    let store = FileRefStore::new(git_dir, format);
    refs.iter()
        .map(|name| store.read_ref(name).map(|target| (name.clone(), target)))
        .collect()
}

fn restore_helper_import_source_refs(
    git_dir: &Path,
    format: ObjectFormat,
    refs: &[(String, Option<RefTarget>)],
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    for (name, previous) in refs {
        match previous {
            Some(target) => {
                let mut transaction = store.transaction();
                transaction.update(RefUpdate {
                    name: name.clone(),
                    expected: None,
                    new: target.clone(),
                    reflog: None,
                });
                transaction.commit()?;
            }
            None if store.read_ref(name)?.is_some() => {
                store.delete_ref(name)?;
            }
            None => {}
        }
    }
    Ok(())
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn helper_test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sley-remote-helper-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

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

    #[test]
    fn expands_helper_push_wildcards_and_updates_tracking_refs() {
        let git_dir = helper_test_dir("push-refs");
        fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("oid");
        fs::write(
            git_dir.join("refs/heads/main"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("main ref");
        fs::write(
            git_dir.join("refs/heads/topic"),
            format!("{}\n", oid.to_hex()),
        )
        .expect("topic ref");

        let expanded = expand_helper_push_refspecs(
            &git_dir,
            ObjectFormat::Sha1,
            &["+refs/heads/*:refs/heads/*".into()],
        )
        .expect("expand");
        assert_eq!(
            expanded,
            [
                "+refs/heads/main:refs/heads/main",
                "+refs/heads/topic:refs/heads/topic"
            ]
        );

        let config = GitConfig {
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin".into()),
                vec![ConfigEntry::new(
                    "fetch",
                    Some("+refs/heads/*:refs/remotes/origin/*".into()),
                )],
            )],
            ..GitConfig::default()
        };
        let capabilities = RemoteHelperCapabilities {
            refspecs: vec!["refs/heads/*:refs/private/*".into()],
            ..RemoteHelperCapabilities::default()
        };
        update_helper_push_tracking_refs(
            &git_dir,
            ObjectFormat::Sha1,
            &config,
            "origin",
            &capabilities,
            &["refs/heads/main:refs/heads/main".into()],
            &["refs/heads/main".into()],
        )
        .expect("tracking update");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert_eq!(
            store.read_ref("refs/private/main").expect("private ref"),
            Some(RefTarget::Direct(oid))
        );
        assert_eq!(
            store
                .read_ref("refs/remotes/origin/main")
                .expect("tracking ref"),
            Some(RefTarget::Direct(oid))
        );
        let _ = fs::remove_dir_all(git_dir);
    }

    #[test]
    fn restores_marks_and_import_source_refs() {
        let git_dir = helper_test_dir("rollback");
        fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
        let marks = git_dir.join("marks");
        fs::write(&marks, b"old marks\n").expect("marks");
        let marks_path = marks.to_string_lossy().into_owned();
        let marks_snapshot = snapshot_marks(Some(&marks_path)).expect("marks snapshot");
        fs::write(&marks, b"new marks\n").expect("mutated marks");
        restore_marks(marks_snapshot).expect("restore marks");
        assert_eq!(fs::read(&marks).expect("marks read"), b"old marks\n");

        let original = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("original");
        let imported = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("imported");
        fs::write(
            git_dir.join("refs/heads/main"),
            format!("{}\n", original.to_hex()),
        )
        .expect("original ref");
        let names = vec!["refs/heads/main".into(), "refs/heads/new".into()];
        let refs = snapshot_helper_source_refs(&git_dir, ObjectFormat::Sha1, &names)
            .expect("ref snapshot");
        fs::write(
            git_dir.join("refs/heads/main"),
            format!("{}\n", imported.to_hex()),
        )
        .expect("mutated ref");
        fs::write(
            git_dir.join("refs/heads/new"),
            format!("{}\n", imported.to_hex()),
        )
        .expect("new ref");
        restore_helper_import_source_refs(&git_dir, ObjectFormat::Sha1, &refs)
            .expect("restore refs");
        let store = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        assert_eq!(
            store.read_ref("refs/heads/main").expect("main"),
            Some(RefTarget::Direct(original))
        );
        assert_eq!(store.read_ref("refs/heads/new").expect("new"), None);
        let _ = fs::remove_dir_all(git_dir);
    }

    #[test]
    fn classifies_push_setup_errors_without_message_matching() {
        assert_eq!(
            RemoteHelperPushError::RefspecRequired.to_string(),
            "remote-helper doesn't support push; refspec needed"
        );
        assert_eq!(
            RemoteHelperPushError::MarksRequired.to_string(),
            "remote-helper export requires marks"
        );
        assert!(matches!(
            RemoteHelperPushError::from(GitError::Exit(7)),
            RemoteHelperPushError::Engine(GitError::Exit(7))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejected_mandatory_capability_reaps_a_waiting_helper() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

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

    #[cfg(unix)]
    #[test]
    fn helper_abort_during_capabilities_is_a_typed_fatal_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = helper_test_dir("abort-capabilities");
        fs::create_dir_all(&root).expect("temp dir");
        let helper = root.join("git-remote-broken");
        fs::write(&helper, b"#!/bin/sh\nread command\nexit 1\n").expect("helper script");
        let mut permissions = fs::metadata(&helper).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions).expect("permissions");

        let error = RemoteHelperSession::start_with_executable(
            RemoteHelperSpec {
                name: "broken".into(),
                alias: "broken://example.com/repo".into(),
                url: Some("broken://example.com/repo".into()),
            },
            &root,
            ObjectFormat::Sha1,
            &helper,
        )
        .err()
        .expect("helper should abort");
        assert_eq!(
            error,
            GitError::cli_exit(CliExit::UserError, "remote helper 'broken' aborted session")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_adopts_sha256_without_restarting_or_mutating_repository() {
        use std::os::unix::fs::PermissionsExt;

        let git_dir = helper_test_dir("discover-sha256");
        fs::create_dir_all(git_dir.join("objects/pack")).expect("objects");
        fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("HEAD");
        let helper = git_dir.join("git-remote-discovery");
        fs::write(
            &helper,
            b"#!/bin/sh\nread command\ntest \"$command\" = capabilities || exit 2\nprintf 'import\\noption\\nobject-format\\n\\n'\nread command\ntest \"$command\" = 'option object-format true' || exit 3\nprintf 'ok\\n'\nread command\ntest \"$command\" = list || exit 4\nprintf ':object-format sha256\\n? refs/heads/main\\n@refs/heads/main HEAD\\n\\n'\nread command\ntest \"$command\" = 'import refs/heads/main' || exit 5\nread command\ntest -z \"$command\" || exit 6\nprintf 'done\\n'\n",
        )
        .expect("helper script");
        let mut permissions = fs::metadata(&helper).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions).expect("permissions");

        let mut session = RemoteHelperSession::start_with_executable_mode(
            RemoteHelperSpec {
                name: "discovery".into(),
                alias: "origin".into(),
                url: Some("unused".into()),
            },
            &git_dir,
            ObjectFormat::Sha1,
            true,
            &helper,
        )
        .expect("start discovery");
        let refs = session.list().expect("list");
        assert_eq!(session.object_format(), ObjectFormat::Sha256);
        assert_eq!(refs.len(), 2);
        assert!(
            FileRefStore::new(&git_dir, ObjectFormat::Sha1)
                .list_refs()
                .expect("refs remain empty")
                .is_empty()
        );
        assert!(
            fs::read_dir(git_dir.join("objects/pack"))
                .expect("pack dir")
                .next()
                .is_none()
        );
        assert_eq!(
            session
                .import(&["refs/heads/main".into()])
                .expect("continue same session"),
            b"done\n"
        );
        let _ = fs::remove_dir_all(git_dir);
    }
}
