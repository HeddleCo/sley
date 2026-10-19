//! Spawn a caller-selected `upload-pack` for a `file://` or local path.
//!
//! Stock git's connect layer runs `--upload-pack` / `-u` through a shell with
//! the repository directory (not the `file://` URL) and `GIT_PROTOCOL` when
//! the negotiated version is greater than 0. Unset `protocol.version` defaults
//! to v2, matching `get_protocol_version_config`.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use sley_config::GitConfig;
use sley_core::{CancelFlag, Capability, GitError, ObjectFormat, ObjectId, Result};
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    ProtocolV2CommandOptions, ProtocolV2FetchRequest, ProtocolV2FetchShallowInfo,
    ProtocolV2LsRefsRequest, UploadPackNegotiationRequest, UploadPackRequest,
    encode_protocol_v2_command_options, parse_protocol_v2_fetch_features,
    protocol_v2_ls_refs_records_to_ref_advertisement_set, protocol_v2_object_format,
    read_protocol_v2_advertisement, read_protocol_v2_ls_refs_response, read_ref_advertisement_set,
    read_upload_pack_shallow_info_section, write_protocol_v2_command_request,
    write_protocol_v2_fetch_request, write_upload_pack_negotiation_request,
    write_upload_pack_request,
};

use crate::ProgressSink;
use crate::install::{
    ProgressInstaller, emit_remote_sideband_progress,
    install_protocol_v2_fetch_promisor_response_from_reader_with_cancel,
    install_protocol_v2_fetch_response_from_reader_with_cancel,
    install_upload_pack_raw_promisor_response_from_reader_with_cancel,
    install_upload_pack_raw_response_from_reader_with_cancel,
    shallow_info_from_protocol_v2_fetch_header,
};

/// Whether a custom local upload-pack should speak protocol v2.
///
/// Explicit `protocol.version` wins. Otherwise `GIT_TEST_PROTOCOL_VERSION`
/// can force v0/v1. Unset defaults to v2, which is what stock git sends a
/// `file://` `--upload-pack` child.
pub(crate) fn custom_local_upload_pack_v2(config: &GitConfig) -> bool {
    match config.get("protocol", None, "version") {
        Some("0") | Some("1") => false,
        Some("2") => true,
        _ => !matches!(
            std::env::var("GIT_TEST_PROTOCOL_VERSION")
                .ok()
                .as_deref()
                .filter(|value| !value.is_empty()),
            Some("0") | Some("1")
        ),
    }
}

/// Read upload-pack advertisements by executing `command` against `git_dir`.
pub(crate) fn advertise(
    command: &str,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Vec<sley_protocol::RefAdvertisement>> {
    let protocol_v2 = custom_local_upload_pack_v2(config);
    let (mut child, mut stdin, mut stdout, stderr) =
        spawn_custom_upload_pack(command, git_dir, protocol_v2)?;
    let refs = if protocol_v2 {
        let handshake = read_protocol_v2_advertisement(&mut stdout)?;
        let advertised = protocol_v2_object_format(&handshake.capabilities)?;
        if advertised != format {
            drop(stdin);
            let _ = finish_upload_pack_child(&mut child, stderr);
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                advertised.name(),
                format.name()
            )));
        }
        let mut request = ProtocolV2LsRefsRequest {
            peel: true,
            symrefs: true,
            unborn: true,
            ref_prefixes: vec!["HEAD".into(), "refs/heads/".into(), "refs/tags/".into()],
        }
        .to_command_request()?;
        request.capabilities = encode_protocol_v2_command_options(&ProtocolV2CommandOptions {
            object_format: Some(format),
            ..ProtocolV2CommandOptions::default()
        })?;
        write_protocol_v2_command_request(&mut stdin, &request)?;
        stdin.flush()?;
        drop(stdin);
        let records = read_protocol_v2_ls_refs_response(format, &mut stdout)?;
        protocol_v2_ls_refs_records_to_ref_advertisement_set(&records)?.refs
    } else {
        let set = read_ref_advertisement_set(format, &mut stdout)?;
        drop(stdin);
        set.refs
    };
    drop(stdout);
    finish_upload_pack_child(&mut child, stderr)?;
    Ok(refs)
}

pub(crate) struct CustomUploadPackFetch<'a> {
    pub command: &'a str,
    pub remote_git_dir: &'a Path,
    pub destination_git_dir: &'a Path,
    pub policy: &'a crate::RemotePolicy,
    pub format: ObjectFormat,
    pub config: &'a GitConfig,
    pub wants: Vec<ObjectId>,
    pub haves: Option<Vec<ObjectId>>,
    pub shallow: Vec<ObjectId>,
    pub deepen: Option<u32>,
    pub filter: Option<&'a sley_odb::PackObjectFilter>,
    pub promisor: bool,
    pub max_input_size: Option<u64>,
}

/// Fetch `wants` by executing the custom upload-pack command.
pub(crate) fn fetch(
    request: CustomUploadPackFetch<'_>,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if request.wants.is_empty() && request.deepen.is_none() {
        return Ok(Vec::new());
    }
    let protocol_v2 = custom_local_upload_pack_v2(request.config);
    let (mut child, mut stdin, mut stdout, stderr) =
        spawn_custom_upload_pack(request.command, request.remote_git_dir, protocol_v2)?;
    let haves = request.haves.clone().map(Ok).unwrap_or_else(|| {
        crate::local::local_have_oids(request.policy, request.destination_git_dir, request.format)
    })?;
    let local_db = FileObjectDatabase::from_git_dir(request.destination_git_dir, request.format);
    let shallow_info = if protocol_v2 {
        fetch_v2(
            &request,
            &haves,
            &local_db,
            &mut stdin,
            &mut stdout,
            progress,
            cancel,
        )?
    } else {
        fetch_v0(
            &request,
            &haves,
            &local_db,
            &mut stdin,
            &mut stdout,
            progress,
            cancel,
        )?
    };
    drop(stdin);
    drop(stdout);
    finish_upload_pack_child(&mut child, stderr)?;
    Ok(shallow_info)
}

fn fetch_v2(
    request: &CustomUploadPackFetch<'_>,
    haves: &[ObjectId],
    local_db: &FileObjectDatabase,
    stdin: &mut impl Write,
    stdout: &mut impl Read,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    let handshake = read_protocol_v2_advertisement(stdout)?;
    let v2 = parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
    let filter = request
        .filter
        .filter(|_| v2.filter)
        .and_then(crate::local::upload_pack_filter_protocol_spec);
    let fetch_request = ProtocolV2FetchRequest {
        wants: request.wants.clone(),
        haves: haves.to_vec(),
        shallow: request.shallow.clone(),
        deepen: request.deepen,
        filter,
        ofs_delta: true,
        done: true,
        sideband_all: v2.sideband_all,
        ..ProtocolV2FetchRequest::default()
    };
    write_protocol_v2_fetch_request(stdin, &fetch_request)?;
    stdin.flush()?;
    let (header, _) = if request.promisor {
        install_protocol_v2_fetch_promisor_response_from_reader_with_cancel(
            request.format,
            stdout,
            v2.sideband_all,
            local_db,
            request.max_input_size,
            cancel,
            Some(&mut |chunk: &[u8]| emit_remote_sideband_progress(progress, chunk)),
        )?
    } else {
        let installer = ProgressInstaller::new(local_db, progress);
        install_protocol_v2_fetch_response_from_reader_with_cancel(
            request.format,
            stdout,
            v2.sideband_all,
            &installer,
            request.max_input_size,
            cancel,
            Some(&mut installer.remote_sideband_forwarder()),
        )?
    };
    Ok(shallow_info_from_protocol_v2_fetch_header(&header))
}

fn fetch_v0(
    request: &CustomUploadPackFetch<'_>,
    haves: &[ObjectId],
    local_db: &FileObjectDatabase,
    stdin: &mut impl Write,
    stdout: &mut impl Read,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    let set = read_ref_advertisement_set(request.format, stdout)?;
    let features = set
        .refs
        .first()
        .map(|advertisement| sley_protocol::parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    let filter = request
        .filter
        .filter(|_| features.filter)
        .and_then(crate::local::upload_pack_filter_protocol_spec);
    let mut capabilities = Vec::new();
    if request.deepen.is_some() {
        capabilities.push(Capability {
            name: "shallow".into(),
            value: None,
        });
    }
    if filter.is_some() {
        capabilities.push(Capability {
            name: "filter".into(),
            value: None,
        });
    }
    let upload_request = UploadPackRequest {
        wants: request.wants.clone(),
        capabilities,
        shallow: request.shallow.clone(),
        deepen: request.deepen,
        filter,
        ..UploadPackRequest::default()
    };
    write_upload_pack_request(stdin, Some(&upload_request))?;
    stdin.flush()?;
    let prefetched_shallow = if request.deepen.is_some() {
        Some(read_upload_pack_shallow_info_section(
            request.format,
            stdout,
        )?)
    } else {
        None
    };
    write_upload_pack_negotiation_request(
        stdin,
        &UploadPackNegotiationRequest {
            haves: haves.to_vec(),
            done: true,
        },
    )?;
    stdin.flush()?;
    if request.promisor {
        install_upload_pack_raw_promisor_response_from_reader_with_cancel(
            request.format,
            stdout,
            local_db,
            request.max_input_size,
            cancel,
        )?;
    } else {
        install_upload_pack_raw_response_from_reader_with_cancel(
            request.format,
            stdout,
            &ProgressInstaller::new(local_db, progress),
            request.max_input_size,
            cancel,
        )?;
    }
    Ok(prefetched_shallow.unwrap_or_default())
}

fn spawn_custom_upload_pack(
    command: &str,
    git_dir: &Path,
    protocol_v2: bool,
) -> Result<(
    Child,
    std::process::ChildStdin,
    std::process::ChildStdout,
    std::process::ChildStderr,
)> {
    let path = git_dir.to_string_lossy();
    let shell = format!("{command} {}", sley_config::sq_quote(&path));
    let mut proc = Command::new("/bin/sh");
    proc.arg("-c").arg(shell);
    if protocol_v2 {
        proc.env("GIT_PROTOCOL", "version=2");
    } else {
        proc.env_remove("GIT_PROTOCOL");
    }
    proc.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = proc.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command("custom upload-pack stdin was not piped".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Command("custom upload-pack stdout was not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| GitError::Command("custom upload-pack stderr was not piped".into()))?;
    Ok((child, stdin, stdout, stderr))
}

fn finish_upload_pack_child(
    child: &mut Child,
    mut stderr: std::process::ChildStderr,
) -> Result<()> {
    let status = child.wait()?;
    let mut err = Vec::new();
    let _ = stderr.read_to_end(&mut err);
    if !status.success() {
        return Err(GitError::Command(format!(
            "upload-pack command failed: {}",
            String::from_utf8_lossy(&err).trim()
        )));
    }
    Ok(())
}
