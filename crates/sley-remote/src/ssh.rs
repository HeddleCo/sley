//! Callable SSH transport plumbing and fetch/push/ls-remote orchestration.
//!
//! These drive the transport-agnostic protocol codecs ([`sley_protocol`]) over an
//! `ssh` subprocess spawned via [`sley_transport::ssh_process_command`]. Like the
//! HTTP and local paths, everything is taken as explicit parameters — the
//! already-resolved [`RemoteUrl`], the [`ObjectFormat`], `git_dir`, the repository
//! [`GitConfig`], and the seam objects ([`CredentialProvider`], [`ProgressSink`]) —
//! so they never read process-global state, parse arguments, or print. SSH does not
//! authenticate at this layer (it delegates to the `ssh` program), so the
//! credential seam is accepted for uniformity but unused.
//!
//! The `ssh` program is chosen by [`ssh_program`], which reads the `GIT_SSH`
//! environment variable (git's standard mechanism) and falls back to `ssh`; this is
//! the one piece of ambient state the SSH transport inherently depends on.
//!
//! SSH mirrors the HTTP path ([`crate::http`]): two ref advertisements are read
//! from the spawned process's stdout (the second, re-advertised set in the RPC
//! stream is skipped), then the upload-pack/receive-pack request is written to its
//! stdin and the packfile/report read back from stdout. The ref-map / `FETCH_HEAD`
//! / prune helpers and the push-planning helpers are shared with the other
//! transports via [`crate::fetch`] and [`crate::push`].

use std::collections::HashMap;
use std::env;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::install::{
    ProgressInstaller, install_upload_pack_raw_promisor_response_from_reader_with_cancel,
    install_upload_pack_raw_response_from_reader_with_cancel,
    install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel,
    install_upload_pack_shallow_raw_response_from_reader_with_cancel,
};
use sley_config::GitConfig;
use sley_core::{
    CancelFlag, Capability, GitError, ObjectFormat, ObjectId, Result, is_cancelled_error,
    kill_child_if_cancelled,
};
use sley_odb::FileObjectDatabase;
use sley_protocol::write_pkt_line_payload;
use sley_protocol::{
    GitService, ProtocolV2FetchShallowInfo, ProtocolVersion, ReceivePackCommand,
    ReceivePackFeatures, RefAdvertisement, UploadPackFeatures, UploadPackNegotiationRequest,
    UploadPackRequest, parse_receive_pack_features, parse_upload_pack_features,
    read_ref_advertisement_set, write_upload_pack_negotiation_request, write_upload_pack_request,
};
use sley_refs::FileRefStore;
use sley_transport::{
    RemoteTransport, RemoteUrl, SshCommandVariant, SshIpVersion,
    ssh_process_args_with_ip_and_command,
};

use crate::{ProgressSink, PushOutcome, PushRequest};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SshTransportOptions {
    pub variant: Option<SshCommandVariant>,
    pub ip_version: Option<SshIpVersion>,
    /// Wire protocol requested through OpenSSH's `SendEnv=GIT_PROTOCOL`.
    pub protocol: Option<ProtocolVersion>,
}

/// Upload-pack discovery for a repository whose object format is not known yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshUploadPackDiscovery {
    pub refs: Vec<RefAdvertisement>,
    pub features: UploadPackFeatures,
    pub object_format: ObjectFormat,
}

pub fn ssh_transport_options_from_config(config: &GitConfig) -> SshTransportOptions {
    SshTransportOptions {
        variant: config
            .get("ssh", None, "variant")
            .and_then(ssh_variant_from_config_value),
        ip_version: None,
        protocol: (config.get("protocol", None, "version") == Some("1"))
            .then_some(ProtocolVersion::V1),
    }
}

fn ssh_variant_from_config_value(value: &str) -> Option<SshCommandVariant> {
    match value.to_ascii_lowercase().as_str() {
        "ssh" => Some(SshCommandVariant::OpenSsh),
        "plink" | "putty" => Some(SshCommandVariant::Plink),
        "tortoiseplink" => Some(SshCommandVariant::TortoisePlink),
        "simple" => Some(SshCommandVariant::Simple),
        "auto" => None,
        _ => None,
    }
}

/// The `ssh` program to spawn for SSH transport: `GIT_SSH_COMMAND`'s first shell
/// word, then `GIT_SSH`, then `ssh`.
pub fn ssh_program() -> String {
    match ssh_program_and_prefix_args() {
        Ok((program, _)) => program,
        Err(_) => env::var("GIT_SSH").unwrap_or_else(|_| "ssh".into()),
    }
}

fn ssh_process_command_for_remote(
    remote: &RemoteUrl,
    service: GitService,
    options: SshTransportOptions,
    service_command: Option<&str>,
) -> Result<ServiceProcessCommand> {
    if remote.transport == RemoteTransport::Ext {
        return ext_process_command_for_remote(remote, service);
    }
    let (program, mut args) = ssh_program_and_prefix_args()?;
    let variant = ssh_command_variant(&program, options.variant);
    let (protocol_args, protocol_env) = ssh_protocol_command_metadata(variant, options.protocol);
    args.extend(protocol_args);
    args.extend(ssh_process_args_with_ip_and_command(
        remote,
        service,
        variant,
        options.ip_version,
        service_command,
    )?);
    Ok(ServiceProcessCommand {
        program,
        args,
        env: protocol_env,
        git_request: None,
    })
}

fn ssh_protocol_command_metadata(
    variant: SshCommandVariant,
    protocol: Option<ProtocolVersion>,
) -> (Vec<String>, Vec<(String, String)>) {
    if variant == SshCommandVariant::OpenSsh && protocol == Some(ProtocolVersion::V1) {
        return (
            vec!["-o".into(), "SendEnv=GIT_PROTOCOL".into()],
            vec![("GIT_PROTOCOL".into(), "version=1".into())],
        );
    }
    (Vec::new(), Vec::new())
}

struct ServiceProcessCommand {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    git_request: Option<ExtGitRequest>,
}

struct StderrDrain {
    handle: JoinHandle<Vec<u8>>,
}

impl StderrDrain {
    fn start(stderr: ChildStderr) -> Self {
        Self {
            handle: std::thread::spawn(move || {
                let mut sink = Vec::new();
                let mut stderr = stderr;
                let _ = stderr.read_to_end(&mut sink);
                sink
            }),
        }
    }

    fn finish(self) -> Vec<u8> {
        self.handle.join().unwrap_or_default()
    }
}

/// Run `body` while a scoped watcher kills `child` if `cancel` trips.
///
/// Cooperative [`CancelFlag`] / pack-install [`CancellableRead`] alone cannot
/// interrupt a kernel `read` blocked on the SSH stdout pipe. The watcher polls
/// cancel and kills the child so the pipe closes and the blocked read wakes
/// (EOF / error), allowing the install path to surface [`GitError::Cancelled`].
fn with_ssh_child_cancel_watch<T>(
    child: &Arc<Mutex<Child>>,
    cancel: CancelFlag<'_>,
    body: impl FnOnce() -> T,
) -> T {
    let stop = AtomicBool::new(false);
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    std::thread::scope(|scope| {
        let watch_child = Arc::clone(child);
        let watch_cancel = cancel;
        let watch_stop = &stop;
        scope.spawn(move || {
            while !watch_stop.load(Ordering::Relaxed) {
                if watch_cancel.is_cancelled() {
                    if let Ok(mut guard) = watch_child.lock() {
                        let _ = guard.kill();
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        // Ensure the watcher always observes stop, even if `body` panics.
        let _stop_guard = StopOnDrop(&stop);
        body()
        // Scope joins the watcher before returning.
    })
}

struct ExtGitRequest {
    repo: String,
    vhost: Option<String>,
}

fn ext_process_command_for_remote(
    remote: &RemoteUrl,
    service: GitService,
) -> Result<ServiceProcessCommand> {
    let parsed = parse_remote_ext_command(&remote.path, service)?;
    let Some((program, args)) = parsed.argv.split_first() else {
        return Err(GitError::InvalidFormat(
            "ext remote command is empty".into(),
        ));
    };
    Ok(ServiceProcessCommand {
        program: program.clone(),
        args: args.to_vec(),
        env: vec![
            ("GIT_EXT_SERVICE".into(), service.as_str().into()),
            (
                "GIT_EXT_SERVICE_NOPREFIX".into(),
                service
                    .as_str()
                    .strip_prefix("git-")
                    .unwrap_or(service.as_str())
                    .into(),
            ),
        ],
        git_request: parsed.git_request.map(|repo| ExtGitRequest {
            repo,
            vhost: parsed.git_request_vhost,
        }),
    })
}

struct ParsedRemoteExtCommand {
    argv: Vec<String>,
    git_request: Option<String>,
    git_request_vhost: Option<String>,
}

fn parse_remote_ext_command(command: &str, service: GitService) -> Result<ParsedRemoteExtCommand> {
    let service_name = service.as_str();
    let service_noprefix = service_name.strip_prefix("git-").unwrap_or(service_name);
    let mut rest = command;
    let mut argv = Vec::new();
    let mut git_request = None;
    let mut git_request_vhost = None;

    while !rest.is_empty() {
        let (arg, next) = parse_remote_ext_arg(rest, service_name, service_noprefix)?;
        rest = next;
        match arg {
            RemoteExtArg::Arg(value) => argv.push(value),
            RemoteExtArg::GitRequest(value) => git_request = Some(value),
            RemoteExtArg::GitRequestVhost(value) => git_request_vhost = Some(value),
        }
    }

    Ok(ParsedRemoteExtCommand {
        argv,
        git_request,
        git_request_vhost,
    })
}

enum RemoteExtArg {
    Arg(String),
    GitRequest(String),
    GitRequestVhost(String),
}

fn parse_remote_ext_arg<'a>(
    input: &'a str,
    service: &str,
    service_noprefix: &str,
) -> Result<(RemoteExtArg, &'a str)> {
    let bytes = input.as_bytes();
    let mut end = 0;
    let mut escaped = false;
    let mut special = None::<u8>;
    while end < bytes.len() && (escaped || bytes[end] != b' ') {
        if escaped {
            match bytes[end] {
                b' ' | b'%' | b's' | b'S' => {}
                b'G' | b'V' if end == 1 => special = Some(bytes[end]),
                other => {
                    return Err(GitError::InvalidFormat(format!(
                        "Bad remote-ext placeholder '%{}'",
                        other as char
                    )));
                }
            }
            escaped = false;
        } else {
            escaped = bytes[end] == b'%';
        }
        end += 1;
    }
    if escaped && end == bytes.len() {
        return Err(GitError::InvalidFormat(
            "remote-ext command has incomplete placeholder".into(),
        ));
    }
    let mut next = &input[end..];
    if next.starts_with(' ') {
        next = &next[1..];
    }

    let body = if special.is_some() {
        &input[2..end]
    } else {
        &input[..end]
    };
    let expanded = expand_remote_ext_arg(body, service, service_noprefix)?;
    let arg = match special {
        Some(b'G') => RemoteExtArg::GitRequest(expanded),
        Some(b'V') => RemoteExtArg::GitRequestVhost(expanded),
        Some(_) => unreachable!("validated remote-ext special"),
        None => RemoteExtArg::Arg(expanded),
    };
    Ok((arg, next))
}

fn expand_remote_ext_arg(input: &str, service: &str, service_noprefix: &str) -> Result<String> {
    let mut out = String::new();
    let bytes = input.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] != b'%' {
            out.push(bytes[pos] as char);
            pos += 1;
            continue;
        }
        pos += 1;
        if pos == bytes.len() {
            return Err(GitError::InvalidFormat(
                "remote-ext command has incomplete placeholder".into(),
            ));
        }
        match bytes[pos] {
            b' ' => out.push(' '),
            b'%' => out.push('%'),
            b's' => out.push_str(service_noprefix),
            b'S' => out.push_str(service),
            other => {
                return Err(GitError::InvalidFormat(format!(
                    "Bad remote-ext placeholder '%{}'",
                    other as char
                )));
            }
        }
        pos += 1;
    }
    Ok(out)
}

fn ssh_command_variant(
    program: &str,
    config_variant: Option<SshCommandVariant>,
) -> SshCommandVariant {
    if let Ok(variant) = env::var("GIT_SSH_VARIANT") {
        return match variant.as_str() {
            "auto" => detect_ssh_command_variant(program),
            "ssh" => SshCommandVariant::OpenSsh,
            "simple" => SshCommandVariant::Simple,
            "plink" | "putty" => SshCommandVariant::Plink,
            "tortoiseplink" => SshCommandVariant::TortoisePlink,
            _ => detect_ssh_command_variant(program),
        };
    }
    config_variant.unwrap_or_else(|| detect_ssh_command_variant(program))
}

fn detect_ssh_command_variant(program: &str) -> SshCommandVariant {
    let basename = Path::new(&program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    match basename.as_str() {
        "plink" | "plink.exe" => SshCommandVariant::Plink,
        "tortoiseplink" | "tortoiseplink.exe" => SshCommandVariant::TortoisePlink,
        "simple" => SshCommandVariant::Simple,
        "uplink" => {
            if ssh_supports_openssh_config_probe(program) {
                SshCommandVariant::OpenSsh
            } else {
                SshCommandVariant::Simple
            }
        }
        _ => SshCommandVariant::OpenSsh,
    }
}

fn ssh_supports_openssh_config_probe(program: &str) -> bool {
    ProcessCommand::new(program)
        .arg("-G")
        .arg("example.com")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ssh_program_and_prefix_args() -> Result<(String, Vec<String>)> {
    if let Ok(command) = env::var("GIT_SSH_COMMAND") {
        let words = split_shell_words(&command)?;
        let Some((program, args)) = words.split_first() else {
            return Err(GitError::Command("GIT_SSH_COMMAND is empty".into()));
        };
        return Ok((program.clone(), args.to_vec()));
    }
    Ok((
        env::var("GIT_SSH").unwrap_or_else(|_| "ssh".into()),
        Vec::new(),
    ))
}

fn split_shell_words(command: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote = None::<char>;
    while let Some(ch) = chars.next() {
        if let Some(quote_ch) = quote {
            if ch == quote_ch {
                quote = None;
            } else if ch == '\\' && quote_ch == '"' {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() {
        return Err(GitError::Command(
            "unclosed quote in GIT_SSH_COMMAND".into(),
        ));
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

fn spawn_service_process(
    remote: &RemoteUrl,
    service: GitService,
    keep_stdin: bool,
    options: SshTransportOptions,
    service_command: Option<&str>,
) -> Result<(Child, Option<ChildStdin>, ChildStdout, StderrDrain)> {
    let command = ssh_process_command_for_remote(remote, service, options, service_command)?;
    let mut process = ProcessCommand::new(&command.program);
    process
        .args(&command.args)
        .envs(command.env)
        .env_remove("GIT_EXEC_PATH")
        .stdin(if keep_stdin || command.git_request.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn()?;
    let stderr_drain = child
        .stderr
        .take()
        .map(StderrDrain::start)
        .ok_or_else(|| GitError::Command("service stderr was not piped".into()))?;
    let mut stdin = child.stdin.take();
    if let Some(request) = &command.git_request {
        let Some(input) = stdin.as_mut() else {
            return Err(GitError::Command("remote-ext stdin was not piped".into()));
        };
        write_remote_ext_git_request(input, service, request)?;
    }
    if !keep_stdin {
        drop(stdin.take());
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Command("service stdout was not piped".into()))?;
    Ok((child, stdin, stdout, stderr_drain))
}

fn ssh_command_failure_message(
    program: &str,
    stderr: &[u8],
    status: std::process::ExitStatus,
) -> String {
    let trimmed = String::from_utf8_lossy(stderr).trim().to_string();
    if !trimmed.is_empty() {
        return trimmed;
    }
    if let Some(code) = status.code() {
        format!("{program} exited with code {code}")
    } else {
        format!("{program} terminated abnormally")
    }
}

fn write_remote_ext_git_request(
    writer: &mut impl Write,
    service: GitService,
    request: &ExtGitRequest,
) -> Result<()> {
    let mut payload = Vec::new();
    payload.extend_from_slice(service.as_str().as_bytes());
    payload.push(b' ');
    payload.extend_from_slice(request.repo.as_bytes());
    payload.push(0);
    if let Some(vhost) = &request.vhost {
        payload.extend_from_slice(b"host=");
        payload.extend_from_slice(vhost.as_bytes());
        payload.push(0);
    }
    write_pkt_line_payload(writer, &payload)
}

/// Push to a resolved SSH `remote` from the repository at `git_dir`.
///
/// Performs the work the CLI's `push_ssh_repository` did, sharing the
/// push-planning helpers with the HTTP and local transports: advertises the
/// remote's refs over `ssh`, plans the receive-pack commands for `refspecs`,
/// rejects non-fast-forward updates (unless forced), builds the pack of objects
/// the remote lacks, sends the receive-pack request, and validates the
/// report-status. `credentials` is accepted for seam uniformity but unused. The
/// "To <remote>" summary and set-upstream config stay with the caller, driven from
/// [`PushOutcome::commands`].
pub(crate) struct SshPushRequest<'a> {
    pub git_dir: &'a Path,
    pub common_git_dir: &'a Path,
    pub format: ObjectFormat,
    pub remote: &'a RemoteUrl,
    pub refspecs: &'a [String],
    pub force: bool,
    pub command_options: SshTransportOptions,
}

pub(crate) struct SshPushCommandsRequest<'a> {
    pub common_git_dir: &'a Path,
    pub format: ObjectFormat,
    pub remote: &'a RemoteUrl,
    pub command_forces: Vec<(ReceivePackCommand, bool)>,
    pub pack_objects: Vec<ObjectId>,
}

pub(crate) struct SshPushPlan {
    pub(crate) commands: Vec<ReceivePackCommand>,
    pub(crate) pack_objects: Vec<ObjectId>,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr_drain: StderrDrain,
    features: ReceivePackFeatures,
    advertisements: Vec<RefAdvertisement>,
    remote: RemoteUrl,
}

pub(crate) fn plan_push_ssh(request: SshPushRequest<'_>) -> Result<SshPushPlan> {
    let SshPushRequest {
        git_dir,
        common_git_dir,
        format,
        remote,
        refspecs,
        force,
        command_options,
    } = request;
    if !matches!(
        remote.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ) {
        return Err(GitError::InvalidFormat(
            "SSH receive-pack requires an SSH remote".into(),
        ));
    }
    let (child, stdin, mut stdout, stderr_drain) =
        spawn_service_process(remote, GitService::ReceivePack, true, command_options, None)?;
    let stdin =
        stdin.ok_or_else(|| GitError::Command("ssh receive-pack stdin was not piped".into()))?;

    let advertisement_set = read_ref_advertisement_set(format, &mut stdout)?;
    let features = advertisement_set
        .refs
        .first()
        .map(|advertisement| parse_receive_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    if let Some(remote_format) = features.object_format {
        if remote_format != format {
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                remote_format.name(),
                format.name()
            )));
        }
    } else if format != ObjectFormat::Sha1 {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository did not advertise object-format for {} push",
            format.name()
        )));
    }

    let local_store = FileRefStore::new(git_dir, format);
    let mut local_refs = crate::push::local_push_source_refs(&local_store, format)?;
    crate::push::add_revision_push_sources(git_dir, format, refspecs, &mut local_refs);
    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let command_forces = crate::push::plan_push_command_forces(
        format,
        &local_refs,
        &advertisement_set.refs,
        refspecs,
        force,
        &local_db,
        crate::push::advice_push_unqualified_ref_name(None),
    )?;
    let commands = command_forces
        .iter()
        .map(|(command, _)| command.clone())
        .collect::<Vec<_>>();
    if commands.is_empty() {
        drop(stdin);
        return Ok(SshPushPlan {
            commands,
            pack_objects: Vec::new(),
            child,
            stdin: None,
            stdout,
            stderr_drain,
            features,
            advertisements: advertisement_set.refs,
            remote: remote.clone(),
        });
    }

    crate::push::reject_non_fast_forward_pushes(
        common_git_dir,
        &local_db,
        format,
        &command_forces,
    )?;
    Ok(SshPushPlan {
        commands,
        pack_objects: Vec::new(),
        child,
        stdin: Some(stdin),
        stdout,
        stderr_drain,
        features,
        advertisements: advertisement_set.refs,
        remote: remote.clone(),
    })
}

pub(crate) fn plan_push_ssh_commands(request: SshPushCommandsRequest<'_>) -> Result<SshPushPlan> {
    let SshPushCommandsRequest {
        common_git_dir,
        format,
        remote,
        command_forces,
        pack_objects,
    } = request;
    if !matches!(
        remote.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ) {
        return Err(GitError::InvalidFormat(
            "SSH receive-pack requires an SSH remote".into(),
        ));
    }
    let (child, stdin, mut stdout, stderr_drain) = spawn_service_process(
        remote,
        GitService::ReceivePack,
        true,
        SshTransportOptions::default(),
        None,
    )?;
    let stdin =
        stdin.ok_or_else(|| GitError::Command("ssh receive-pack stdin was not piped".into()))?;

    let advertisement_set = read_ref_advertisement_set(format, &mut stdout)?;
    let features = advertisement_set
        .refs
        .first()
        .map(|advertisement| parse_receive_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    if let Some(remote_format) = features.object_format {
        if remote_format != format {
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                remote_format.name(),
                format.name()
            )));
        }
    } else if format != ObjectFormat::Sha1 {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository did not advertise object-format for {} push",
            format.name()
        )));
    }

    let commands = command_forces
        .iter()
        .map(|(command, _)| command.clone())
        .collect::<Vec<_>>();

    if commands.is_empty() {
        drop(stdin);
        return Ok(SshPushPlan {
            commands,
            pack_objects,
            child,
            stdin: None,
            stdout,
            stderr_drain,
            features,
            advertisements: advertisement_set.refs,
            remote: remote.clone(),
        });
    }

    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    crate::push::reject_non_fast_forward_pushes(
        common_git_dir,
        &local_db,
        format,
        &command_forces,
    )?;
    Ok(SshPushPlan {
        commands,
        pack_objects,
        child,
        stdin: Some(stdin),
        stdout,
        stderr_drain,
        features,
        advertisements: advertisement_set.refs,
        remote: remote.clone(),
    })
}

pub(crate) fn execute_push_ssh_plan(
    request: PushRequest<'_>,
    mut plan: SshPushPlan,
    cancel: CancelFlag<'_>,
) -> Result<PushOutcome> {
    if plan.commands.is_empty() {
        return Ok(PushOutcome::default());
    }
    let mut stdin = plan
        .stdin
        .take()
        .ok_or_else(|| GitError::Command("ssh receive-pack stdin was not available".into()))?;
    let commands = plan.commands.clone();
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
    let write_result = crate::pack::write_receive_pack_body_with_cancel(
        &crate::pack::PushPackRequest {
            local_db: &local_db,
            format: request.format,
            commands: &commands,
            pack_objects: &plan.pack_objects,
            remote_advertisements: &plan.advertisements,
            features: &plan.features,
            options: crate::push::receive_pack_push_options(
                &plan.features,
                request.format,
                request.options.quiet,
                false,
                &[],
            ),
            thin: request.options.thin.wants_thin(),
        },
        &mut stdin,
        cancel,
    );
    // Tear down the ssh child on cancel so a blocked remote does not linger.
    kill_child_if_cancelled(&mut plan.child, cancel);
    if cancel.is_cancelled() || write_result.as_ref().err().is_some_and(is_cancelled_error) {
        drop(stdin);
        let _ = plan.child.wait();
        let _ = plan.stderr_drain.finish();
        return match write_result {
            Err(err) if is_cancelled_error(&err) => Err(err),
            _ => Err(GitError::Cancelled),
        };
    }
    write_result?;
    drop(stdin);

    let report = if plan.features.report_status || plan.features.report_status_v2 {
        let report = crate::push::read_receive_pack_push_report(
            request.format,
            &mut plan.stdout,
            plan.features.report_status_v2,
            plan.features.side_band_64k,
        )?;
        crate::push::validate_receive_pack_unpack(&report)?;
        Some(report)
    } else {
        let mut sink = Vec::new();
        plan.stdout.read_to_end(&mut sink)?;
        None
    };
    let status = plan.child.wait()?; // Child::wait takes &mut self
    let stderr = plan.stderr_drain.finish();
    if !status.success() {
        return Err(GitError::Command(format!(
            "ssh receive-pack failed for {}: {}",
            ssh_remote_display(&plan.remote),
            ssh_command_failure_message("ssh receive-pack", &stderr, status)
        )));
    }

    Ok(PushOutcome {
        commands,
        report,
        remote_progress: Vec::new(),
    })
}

/// List the advertised refs for a resolved SSH `remote`, mirroring the records the
/// HTTP/local paths return ([`crate::ls_remote::LsRemoteRecord`]): advertise over
/// `ssh`, then apply the `--heads`/`--tags`/`--refs` class filters and the
/// caller-supplied `matches` predicate. Returns the records and the object format
/// in effect (currently SHA-1 only).
pub(crate) fn ls_remote_ssh(
    remote: &RemoteUrl,
    filter: &crate::ls_remote::LsRemoteFilter,
    matches: &dyn Fn(&str) -> bool,
) -> Result<(Vec<crate::ls_remote::LsRemoteRecord>, ObjectFormat)> {
    if !matches!(
        remote.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ) {
        return Err(GitError::InvalidFormat(
            "SSH upload-pack requires an SSH remote".into(),
        ));
    }
    let (mut child, _stdin, mut stdout, stderr_drain) = spawn_service_process(
        remote,
        GitService::UploadPack,
        false,
        SshTransportOptions::default(),
        None,
    )?;
    let set_result = read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout);
    let status = child.wait()?;
    let stderr = stderr_drain.finish();
    let set = match set_result {
        Ok(set) => set,
        Err(err) if is_ssh_protocol_v2_advertisement_error(&err) => {
            return Err(ssh_protocol_v2_unsupported_error());
        }
        Err(_) if !status.success() => {
            return Err(GitError::Command(format!(
                "ssh upload-pack failed for {}: {}",
                ssh_remote_display(remote),
                ssh_command_failure_message("ssh upload-pack", &stderr, status)
            )));
        }
        Err(err) => return Err(err),
    };
    let features = set
        .refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    let format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    if format != ObjectFormat::Sha1 {
        return Err(GitError::Unsupported(format!(
            "ssh ls-remote currently supports SHA-1 advertisements, got {}",
            format.name()
        )));
    }
    let symrefs = features
        .symrefs
        .iter()
        .filter_map(|symref| symref.split_once(':'))
        .map(|(name, target)| (name.to_string(), target.to_string()))
        .collect::<HashMap<_, _>>();
    let mut records = Vec::new();
    for advertisement in set.refs {
        if advertisement.oid.is_null() {
            continue;
        }
        if filter.refs_only && (advertisement.name == "HEAD" || advertisement.name.ends_with("^{}"))
        {
            continue;
        }
        let is_head = advertisement.name.starts_with("refs/heads/");
        let is_tag = advertisement.name.starts_with("refs/tags/");
        if (filter.heads || filter.tags) && !((filter.heads && is_head) || (filter.tags && is_tag))
        {
            continue;
        }
        if !matches(&advertisement.name) {
            continue;
        }
        records.push(crate::ls_remote::LsRemoteRecord {
            oid: advertisement.oid,
            symref: symrefs.get(&advertisement.name).cloned(),
            name: advertisement.name,
        });
    }
    Ok((records, format))
}

/// Fetch `wants` from an SSH upload-pack remote into the repository at `git_dir`,
/// installing the resulting pack. Objects already present locally are skipped (for
/// non-shallow fetches); `promisor` selects promisor-pack installation.
///
/// When `deepen` is set the fetch is shallow: the request replays `shallow` (the
/// client's current boundary from `$GIT_DIR/shallow`) and asks the server to
/// truncate history to `deepen` commits. The returned [`ProtocolV2FetchShallowInfo`]
/// entries are the server's shallow-info updates the caller must fold into
/// `$GIT_DIR/shallow` (see [`crate::apply_shallow_info`]); they are empty for a
/// non-deepen fetch.
pub struct SshFetchPackRequest<'a> {
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Resolved SSH remote.
    pub remote: &'a RemoteUrl,
    /// Upload-pack features advertised by the remote.
    pub features: &'a UploadPackFeatures,
    /// Wanted object ids.
    pub wants: Vec<ObjectId>,
    /// Caller-selected negotiation haves. `None` means advertise the default
    /// local haves.
    pub haves: Option<Vec<ObjectId>>,
    /// Existing shallow boundary to replay.
    pub shallow: Vec<ObjectId>,
    /// Requested deepen depth, if this is a shallow fetch.
    pub deepen: Option<u32>,
    /// Whether to install the response as a promisor pack.
    pub promisor: bool,
    /// SSH command-line shape (variant and `-4`/`-6`), usually derived by the
    /// caller from effective config and command-line flags.
    pub command_options: SshTransportOptions,
    /// Explicit remote upload-pack command selected by the caller.
    pub upload_pack_command: Option<&'a str>,
    /// Maximum raw pack bytes to accept from the remote (`fetch.maxInputSize` /
    /// `transfer.maxSize`). `None` means unlimited.
    pub max_input_size: Option<u64>,
}

pub fn install_fetch_pack_via_ssh_upload_pack(
    request: SshFetchPackRequest<'_>,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if request.wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    // A deepen request must always reach the server (the shallow boundary may move
    // even when every wanted object is already present), so only the plain fetch
    // takes the "everything is local already" shortcut.
    if request.deepen.is_none() && all_wants_present(&local_db, &request.wants)? {
        return Ok(Vec::new());
    }
    let upload_request = UploadPackRequest {
        wants: request.wants,
        capabilities: ssh_shallow_request_capabilities(request.deepen),
        shallow: request.shallow,
        deepen: request.deepen,
        ..UploadPackRequest::default()
    };
    let haves = request
        .haves
        .clone()
        .map(Ok)
        .unwrap_or_else(|| crate::local::local_have_oids(request.git_dir, request.format))?;
    let (child, stdin, mut stdout, stderr_drain) = spawn_service_process(
        request.remote,
        GitService::UploadPack,
        true,
        request.command_options,
        request.upload_pack_command,
    )?;
    let mut stdin =
        stdin.ok_or_else(|| GitError::Command("ssh upload-pack stdin was not piped".into()))?;

    read_ref_advertisement_set(request.format, &mut stdout)?;
    write_upload_pack_request(&mut stdin, Some(&upload_request))?;
    write_upload_pack_negotiation_request(
        &mut stdin,
        &UploadPackNegotiationRequest { haves, done: true },
    )?;
    drop(stdin);

    // Share the child with a cancel watcher so a mid-transfer cancel can kill
    // the ssh process and wake a blocked stdout read (CancellableRead alone only
    // polls between successful user-space reads).
    let child = Arc::new(Mutex::new(child));
    let install_result = with_ssh_child_cancel_watch(&child, cancel, || {
        if request.deepen.is_some() {
            if request.promisor {
                install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel(
                    request.format,
                    &mut stdout,
                    &local_db,
                    request.max_input_size,
                    cancel,
                )
                .map(|(shallow_info, _)| shallow_info)
            } else {
                install_upload_pack_shallow_raw_response_from_reader_with_cancel(
                    request.format,
                    &mut stdout,
                    &ProgressInstaller::new(&local_db, progress),
                    request.max_input_size,
                    cancel,
                )
                .map(|(shallow_info, _)| shallow_info)
            }
        } else if request.promisor {
            install_upload_pack_raw_promisor_response_from_reader_with_cancel(
                request.format,
                &mut stdout,
                &local_db,
                request.max_input_size,
                cancel,
            )
            .map(|_| Vec::new())
        } else {
            install_upload_pack_raw_response_from_reader_with_cancel(
                request.format,
                &mut stdout,
                &ProgressInstaller::new(&local_db, progress),
                request.max_input_size,
                cancel,
            )
            .map(|_| Vec::new())
        }
    });

    let mut child = child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Always attempt kill when cancel is set so a stuck child does not linger
    // after cooperative install returned Cancelled (or the watcher already
    // tore the process down).
    kill_child_if_cancelled(&mut child, cancel);
    if cancel.is_cancelled()
        || install_result
            .as_ref()
            .err()
            .is_some_and(is_cancelled_error)
    {
        let _ = child.wait();
        drop(child);
        let _ = stderr_drain.finish();
        return match install_result {
            Err(err) if is_cancelled_error(&err) => Err(err),
            _ => Err(GitError::Cancelled),
        };
    }

    let status = child.wait()?;
    drop(child);
    let stderr = stderr_drain.finish();
    if !status.success() {
        return Err(GitError::Command(format!(
            "ssh upload-pack failed for {}: {}",
            ssh_remote_display(request.remote),
            ssh_command_failure_message("ssh upload-pack", &stderr, status)
        )));
    }
    install_result
}

fn all_wants_present(db: &FileObjectDatabase, wants: &[ObjectId]) -> Result<bool> {
    for want in wants {
        if !db.contains(want)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The want-line capabilities for an SSH fetch: the `shallow` capability when a
/// deepen is requested, otherwise none (preserving the existing plain-fetch wire
/// form exactly).
fn ssh_shallow_request_capabilities(deepen: Option<u32>) -> Vec<Capability> {
    if deepen.is_some() {
        vec![Capability {
            name: "shallow".into(),
            value: None,
        }]
    } else {
        Vec::new()
    }
}

/// The upload-pack ref advertisements and parsed features for SSH `remote`.
pub fn ssh_upload_pack_advertisements(
    remote: &RemoteUrl,
    format: ObjectFormat,
) -> Result<(Vec<RefAdvertisement>, UploadPackFeatures)> {
    ssh_upload_pack_advertisements_with_options(remote, format, SshTransportOptions::default())
}

pub fn ssh_upload_pack_advertisements_with_options(
    remote: &RemoteUrl,
    format: ObjectFormat,
    options: SshTransportOptions,
) -> Result<(Vec<RefAdvertisement>, UploadPackFeatures)> {
    ssh_upload_pack_advertisements_with_command(remote, format, options, None)
}

/// Read SSH upload-pack advertisements using an optional caller-selected
/// service program while retaining the normal SSH command-shape options.
pub fn ssh_upload_pack_advertisements_with_command(
    remote: &RemoteUrl,
    format: ObjectFormat,
    options: SshTransportOptions,
    upload_pack_command: Option<&str>,
) -> Result<(Vec<RefAdvertisement>, UploadPackFeatures)> {
    if !matches!(
        remote.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ) {
        return Err(GitError::InvalidFormat(
            "SSH upload-pack requires an SSH remote".into(),
        ));
    }
    let (mut child, _stdin, mut stdout, stderr_drain) = spawn_service_process(
        remote,
        GitService::UploadPack,
        false,
        options,
        upload_pack_command,
    )?;
    let set_result = read_ref_advertisement_set(format, &mut stdout);
    let status = child.wait()?;
    let stderr = stderr_drain.finish();
    let set = match set_result {
        Ok(set) => set,
        Err(err) if is_ssh_protocol_v2_advertisement_error(&err) => {
            return Err(ssh_protocol_v2_unsupported_error());
        }
        Err(_) if !status.success() => {
            return Err(GitError::Command(format!(
                "ssh upload-pack failed for {}: {}",
                ssh_remote_display(remote),
                ssh_command_failure_message("ssh upload-pack", &stderr, status)
            )));
        }
        Err(err) => return Err(err),
    };
    let features = set
        .refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    Ok((set.refs, features))
}

/// Discover upload-pack capabilities and object format before creating a clone.
pub fn discover_ssh_upload_pack_advertisements_with_command(
    remote: &RemoteUrl,
    options: SshTransportOptions,
    upload_pack_command: Option<&str>,
) -> Result<SshUploadPackDiscovery> {
    if !matches!(
        remote.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ) {
        return Err(GitError::InvalidFormat(
            "SSH upload-pack requires an SSH remote".into(),
        ));
    }
    let (mut child, _stdin, mut stdout, stderr_drain) = spawn_service_process(
        remote,
        GitService::UploadPack,
        false,
        options,
        upload_pack_command,
    )?;
    let set_result = crate::read_discovered_upload_pack_advertisements(&mut stdout);
    let status = child.wait()?;
    let stderr = stderr_drain.finish();
    let (set, object_format) = match set_result {
        Ok(discovered) => discovered,
        Err(err) if is_ssh_protocol_v2_advertisement_error(&err) => {
            return Err(ssh_protocol_v2_unsupported_error());
        }
        Err(_) if !status.success() => {
            return Err(GitError::Command(format!(
                "ssh upload-pack failed for {}: {}",
                ssh_remote_display(remote),
                ssh_command_failure_message("ssh upload-pack", &stderr, status)
            )));
        }
        Err(err) => return Err(err),
    };
    let features = set
        .refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    Ok(SshUploadPackDiscovery {
        refs: set.refs,
        features,
        object_format,
    })
}

pub fn validate_ssh_fetch_options(
    filter: Option<&sley_odb::PackObjectFilter>,
    deepen_since: Option<i64>,
    deepen_not: &[String],
) -> Result<()> {
    if filter.is_some() {
        return Err(GitError::Unsupported(
            "partial clone --filter over SSH is not supported; use HTTPS".into(),
        ));
    }
    if deepen_since.is_some() {
        return Err(GitError::Unsupported(
            "shallow fetch --shallow-since over SSH is not supported; use HTTPS".into(),
        ));
    }
    if !deepen_not.is_empty() {
        return Err(GitError::Unsupported(
            "shallow fetch --shallow-exclude over SSH is not supported; use HTTPS".into(),
        ));
    }
    Ok(())
}

fn is_ssh_protocol_v2_advertisement_error(err: &GitError) -> bool {
    matches!(err, GitError::InvalidFormat(message)
        if message.contains("unsupported advertised ref protocol version"))
}

fn ssh_protocol_v2_unsupported_error() -> GitError {
    GitError::Unsupported(
        "protocol v2 over SSH is not supported; use HTTPS or configure the remote for upload-pack v0/v1"
            .into(),
    )
}

/// A human-readable rendering of an SSH `remote` for error messages. The CLI built
/// these messages from the resolved URL string; the library only has the parsed
/// [`RemoteUrl`], so reconstruct the `user@host[:port]/path` (or `host:path` SCP)
/// form for the diagnostic text.
fn ssh_remote_display(remote: &RemoteUrl) -> String {
    if remote.transport == RemoteTransport::Ext {
        return format!("ext::{}", remote.path);
    }
    let host = remote.host.as_deref().unwrap_or("");
    let mut out = String::new();
    if let Some(user) = &remote.user {
        out.push_str(user);
        out.push('@');
    }
    out.push_str(host);
    if let Some(port) = remote.port {
        out.push(':');
        out.push_str(&port.to_string());
    }
    if !remote.path.is_empty() {
        if !out.is_empty() {
            out.push(':');
        }
        out.push_str(&remote.path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_v1_uses_openssh_send_env_metadata() {
        let (args, env) =
            ssh_protocol_command_metadata(SshCommandVariant::OpenSsh, Some(ProtocolVersion::V1));
        assert_eq!(args, ["-o", "SendEnv=GIT_PROTOCOL"]);
        assert_eq!(env, [("GIT_PROTOCOL".into(), "version=1".into())]);

        let (args, env) =
            ssh_protocol_command_metadata(SshCommandVariant::Plink, Some(ProtocolVersion::V1));
        assert!(args.is_empty());
        assert!(env.is_empty());

        let (args, env) =
            ssh_protocol_command_metadata(SshCommandVariant::OpenSsh, Some(ProtocolVersion::V2));
        assert!(args.is_empty());
        assert!(env.is_empty());
    }

    #[test]
    fn remote_ext_parser_keeps_percent_escaped_spaces_inside_arguments() {
        let parsed = parse_remote_ext_command("sh -c %S% ..", GitService::UploadPack)
            .expect("remote-ext command parses");

        assert_eq!(parsed.argv, vec!["sh", "-c", "git-upload-pack .."],);
        assert_eq!(parsed.git_request, None);
        assert_eq!(parsed.git_request_vhost, None);
    }

    #[test]
    fn remote_ext_parser_expands_service_without_git_prefix() {
        let parsed = parse_remote_ext_command("helper %s %%", GitService::ReceivePack)
            .expect("remote-ext command parses");

        assert_eq!(parsed.argv, vec!["helper", "receive-pack", "%"]);
    }

    #[test]
    fn remote_ext_parser_extracts_git_daemon_request_arguments() {
        let parsed =
            parse_remote_ext_command("fake-daemon %G/two.git %Vhost", GitService::UploadPack)
                .expect("remote-ext command parses");

        assert_eq!(parsed.argv, vec!["fake-daemon"]);
        assert_eq!(parsed.git_request.as_deref(), Some("/two.git"));
        assert_eq!(parsed.git_request_vhost.as_deref(), Some("host"));
    }
}
