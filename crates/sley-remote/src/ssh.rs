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
use std::io::Read;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};

use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use sley_fetch::{install_upload_pack_raw_promisor_response, install_upload_pack_raw_response};
use sley_odb::{build_reachable_pack, collect_reachable_object_ids, FileObjectDatabase};
use sley_protocol::{
    build_receive_pack_push_request, parse_receive_pack_features, parse_refspec,
    parse_upload_pack_features, plan_push_commands, read_receive_pack_report_status,
    read_ref_advertisement_set, read_upload_pack_raw_packfile_response,
    read_upload_pack_shallow_info_and_raw_packfile_response, write_receive_pack_push_request,
    write_upload_pack_negotiation_request, write_upload_pack_request, GitService,
    ProtocolV2FetchShallowInfo, ReceivePackPushRequestOptions, RefAdvertisement,
    UploadPackFeatures, UploadPackNegotiationRequest, UploadPackRawPackfileResponse,
    UploadPackRequest,
};
use sley_refs::FileRefStore;
use sley_transport::{ssh_process_command, RemoteTransport, RemoteUrl, SshCommandVariant};

use crate::{CredentialProvider, PushOutcome};

/// The `ssh` program to spawn for SSH transport: the `GIT_SSH` environment
/// variable when set, otherwise `ssh`. This mirrors git's basic `GIT_SSH`
/// selection (the richer `GIT_SSH_COMMAND`/`core.sshCommand` forms are not
/// handled, matching the CLI behavior being lifted).
pub fn ssh_program() -> String {
    env::var("GIT_SSH").unwrap_or_else(|_| "ssh".into())
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_ssh(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    remote: &RemoteUrl,
    refspecs: &[String],
    quiet: bool,
    force: bool,
    _credentials: &mut dyn CredentialProvider,
) -> Result<PushOutcome> {
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH receive-pack requires an SSH remote".into(),
        ));
    }
    let ssh = ssh_process_command(
        remote,
        GitService::ReceivePack,
        ssh_program(),
        SshCommandVariant::OpenSsh,
    )?;
    let mut child = ProcessCommand::new(&ssh.program)
        .args(&ssh.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Command("ssh receive-pack stdout was not piped".into()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command("ssh receive-pack stdin was not piped".into()))?;

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
    let local_refs = crate::push::local_push_source_refs(&local_store, format)?;
    let parsed_refspecs = refspecs
        .iter()
        .map(|refspec| parse_refspec(&crate::push::normalize_push_refspec(refspec)))
        .collect::<Result<Vec<_>>>()?;
    let mut command_forces = Vec::new();
    for refspec in &parsed_refspecs {
        for command in plan_push_commands(
            format,
            &local_refs,
            &advertisement_set.refs,
            std::slice::from_ref(refspec),
        )? {
            command_forces.push((command, force || refspec.force));
        }
    }
    let commands = command_forces
        .iter()
        .map(|(command, _)| command.clone())
        .collect::<Vec<_>>();
    if commands.is_empty() {
        drop(stdin);
        let _ = child.wait_with_output()?;
        return Ok(PushOutcome::default());
    }

    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    crate::push::reject_non_fast_forward_pushes(&local_db, format, &command_forces)?;
    let remote_excluded_tips =
        crate::push::remote_advertisement_tips_known_to_local(&local_db, &advertisement_set.refs)?;
    let remote_excluded = collect_reachable_object_ids(&local_db, format, remote_excluded_tips)?;
    let starts = commands
        .iter()
        .filter(|command| !is_zero_object_id(&command.new_id))
        .map(|command| command.new_id.clone());
    let packfile = build_reachable_pack(&local_db, format, starts, &remote_excluded)?
        .map(|pack| pack.pack)
        .unwrap_or_default();
    let request = build_receive_pack_push_request(
        &features,
        commands.clone(),
        packfile,
        ReceivePackPushRequestOptions {
            report_status: features.report_status,
            ofs_delta: features.ofs_delta,
            quiet: quiet && features.quiet,
            object_format: features
                .object_format
                .filter(|_| format != ObjectFormat::Sha1),
            ..ReceivePackPushRequestOptions::default()
        },
    )?;
    write_receive_pack_push_request(&mut stdin, &request)?;
    drop(stdin);

    let report = if features.report_status {
        let report = read_receive_pack_report_status(&mut stdout)?;
        crate::push::validate_receive_pack_report(&report)?;
        Some(report)
    } else {
        let mut sink = Vec::new();
        stdout.read_to_end(&mut sink)?;
        None
    };
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "ssh receive-pack failed for {}: {}",
            ssh_remote_display(remote),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(PushOutcome { commands, report })
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
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH upload-pack requires an SSH remote".into(),
        ));
    }
    let ssh = ssh_process_command(
        remote,
        GitService::UploadPack,
        ssh_program(),
        SshCommandVariant::OpenSsh,
    )?;
    let output = ProcessCommand::new(&ssh.program)
        .args(&ssh.args)
        .stdin(Stdio::null())
        .output()?;
    let mut stdout = output.stdout.as_slice();
    let set = match read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout) {
        Ok(set) => set,
        Err(_) if !output.status.success() => {
            return Err(GitError::Command(format!(
                "ssh upload-pack failed for {}: {}",
                ssh_remote_display(remote),
                String::from_utf8_lossy(&output.stderr).trim()
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
        if is_zero_object_id(&advertisement.oid) {
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
#[allow(clippy::too_many_arguments)]
pub fn install_fetch_pack_via_ssh_upload_pack(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &RemoteUrl,
    features: &UploadPackFeatures,
    wants: Vec<ObjectId>,
    shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    promisor: bool,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    // A deepen request must always reach the server (the shallow boundary may move
    // even when every wanted object is already present), so only the plain fetch
    // takes the "everything is local already" shortcut.
    if deepen.is_none()
        && wants
            .iter()
            .map(|want| local_db.contains(want))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .all(|contains| contains)
    {
        return Ok(Vec::new());
    }
    let request = UploadPackRequest {
        wants,
        capabilities: ssh_shallow_request_capabilities(deepen),
        shallow,
        deepen,
        ..UploadPackRequest::default()
    };
    let haves = crate::local::local_have_oids(git_dir, format)?;
    // Only a deepen request gets a leading shallow-info section in the response;
    // a plain fetch must use the non-shallow reader (the response starts straight
    // at the NAK/ACK), preserving the existing SSH wire handling exactly.
    let (shallow_info, response) = if deepen.is_some() {
        ssh_upload_pack_shallow_fetch_response(remote, format, features, request, haves)?
    } else {
        let response = ssh_upload_pack_fetch_response(remote, format, features, request, haves)?;
        (Vec::new(), response)
    };
    if promisor {
        install_upload_pack_raw_promisor_response(&response, &local_db)?;
    } else {
        install_upload_pack_raw_response(&response, &local_db)?;
    }
    Ok(shallow_info)
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
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH upload-pack requires an SSH remote".into(),
        ));
    }
    let ssh = ssh_process_command(
        remote,
        GitService::UploadPack,
        ssh_program(),
        SshCommandVariant::OpenSsh,
    )?;
    let output = ProcessCommand::new(&ssh.program)
        .args(&ssh.args)
        .stdin(Stdio::null())
        .output()?;
    let mut stdout = output.stdout.as_slice();
    let set = match read_ref_advertisement_set(format, &mut stdout) {
        Ok(set) => set,
        Err(_) if !output.status.success() => {
            return Err(GitError::Command(format!(
                "ssh upload-pack failed for {}: {}",
                ssh_remote_display(remote),
                String::from_utf8_lossy(&output.stderr).trim()
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

/// Post an upload-pack `request` + `haves` over SSH and read back the raw packfile
/// response. The leading re-advertised ref set in the RPC stream is read and
/// discarded before the request is written. For a plain (non-deepen) request; see
/// [`ssh_upload_pack_shallow_fetch_response`] for the deepen case.
pub fn ssh_upload_pack_fetch_response(
    remote: &RemoteUrl,
    format: ObjectFormat,
    _features: &UploadPackFeatures,
    request: UploadPackRequest,
    haves: Vec<ObjectId>,
) -> Result<UploadPackRawPackfileResponse> {
    let (_shallow, response) =
        ssh_upload_pack_fetch_response_inner(remote, format, request, haves, false)?;
    Ok(response)
}

/// Post a deepen upload-pack `request` + `haves` over SSH and read back the
/// shallow-info section plus the raw packfile response. Use this when `request`
/// carries a `shallow`/`deepen`/`deepen-since`/`deepen-not` argument: the response
/// is then prefixed with a shallow-info section (possibly empty). The returned
/// [`ProtocolV2FetchShallowInfo`] entries are the server's shallow-info updates.
pub fn ssh_upload_pack_shallow_fetch_response(
    remote: &RemoteUrl,
    format: ObjectFormat,
    _features: &UploadPackFeatures,
    request: UploadPackRequest,
    haves: Vec<ObjectId>,
) -> Result<(
    Vec<ProtocolV2FetchShallowInfo>,
    UploadPackRawPackfileResponse,
)> {
    ssh_upload_pack_fetch_response_inner(remote, format, request, haves, true)
}

/// Drive the `ssh` upload-pack subprocess for `request` + `haves`, reading back the
/// raw packfile response. When `expect_shallow_info` is set (the request is a
/// deepen request) the response's leading shallow-info section is parsed and
/// returned; otherwise no shallow-info is expected and the returned vec is empty.
fn ssh_upload_pack_fetch_response_inner(
    remote: &RemoteUrl,
    format: ObjectFormat,
    request: UploadPackRequest,
    haves: Vec<ObjectId>,
    expect_shallow_info: bool,
) -> Result<(
    Vec<ProtocolV2FetchShallowInfo>,
    UploadPackRawPackfileResponse,
)> {
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH upload-pack requires an SSH remote".into(),
        ));
    }
    let ssh = ssh_process_command(
        remote,
        GitService::UploadPack,
        ssh_program(),
        SshCommandVariant::OpenSsh,
    )?;
    let mut child = ProcessCommand::new(&ssh.program)
        .args(&ssh.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Command("ssh upload-pack stdout was not piped".into()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command("ssh upload-pack stdin was not piped".into()))?;

    read_ref_advertisement_set(format, &mut stdout)?;
    write_upload_pack_request(&mut stdin, Some(&request))?;
    write_upload_pack_negotiation_request(
        &mut stdin,
        &UploadPackNegotiationRequest { haves, done: true },
    )?;
    drop(stdin);

    let result = if expect_shallow_info {
        read_upload_pack_shallow_info_and_raw_packfile_response(format, &mut stdout)?
    } else {
        (
            Vec::new(),
            read_upload_pack_raw_packfile_response(format, &mut stdout)?,
        )
    };
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "ssh upload-pack failed for {}: {}",
            ssh_remote_display(remote),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(result)
}

/// A human-readable rendering of an SSH `remote` for error messages. The CLI built
/// these messages from the resolved URL string; the library only has the parsed
/// [`RemoteUrl`], so reconstruct the `user@host[:port]/path` (or `host:path` SCP)
/// form for the diagnostic text.
fn ssh_remote_display(remote: &RemoteUrl) -> String {
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

/// Whether `oid` is the all-zero object id (a ref creation/deletion sentinel).
fn is_zero_object_id(oid: &ObjectId) -> bool {
    oid.as_bytes().iter().all(|byte| *byte == 0)
}
