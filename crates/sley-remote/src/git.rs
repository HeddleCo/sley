//! Native `git://` transport over a raw TCP stream.
//!
//! This is the anonymous git-daemon protocol: send one service request pkt-line,
//! then speak upload-pack / receive-pack over the selected wire protocol.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::Path;

use sley_config::GitConfig;

use crate::install::{
    ProgressInstaller, install_protocol_v2_fetch_promisor_response_from_reader_with_cancel,
    install_protocol_v2_fetch_response_from_reader_with_cancel,
    install_upload_pack_raw_promisor_response_from_reader_with_cancel,
    install_upload_pack_raw_response_from_reader_with_cancel,
    install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel,
    install_upload_pack_shallow_raw_response_from_reader_with_cancel,
    shallow_info_from_protocol_v2_fetch_header,
};
use sley_core::{CancelFlag, Capability, GitError, ObjectFormat, ObjectId, Result};
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    GitService, ProtocolV2CommandOptions, ProtocolV2FetchRequest, ProtocolV2FetchShallowInfo,
    ProtocolV2LsRefsRequest, ProtocolVersion, ReceivePackCommand, ReceivePackFeatures,
    RefAdvertisement, TransportHandshake, UploadPackFeatures, UploadPackNegotiationRequest,
    UploadPackRequest, parse_protocol_v2_fetch_features, parse_receive_pack_features,
    parse_refspec, parse_upload_pack_features, plan_push_commands,
    protocol_v2_ls_refs_records_to_ref_advertisement_set, protocol_v2_object_format,
    read_protocol_v2_advertisement, read_protocol_v2_ls_refs_response, read_ref_advertisement_set,
    write_protocol_v2_command_request, write_protocol_v2_fetch_request,
    write_upload_pack_negotiation_request, write_upload_pack_request,
};
use sley_refs::FileRefStore;
use sley_transport::{RemoteTransport, RemoteUrl, ServiceRequest, write_service_request};

use crate::git_proxy::GitConnection;
use crate::{ProgressSink, PushOutcome, PushRequest};

const GIT_DAEMON_PORT: u16 = 9418;

pub(crate) struct GitPushRequest<'a> {
    pub git_dir: &'a Path,
    pub common_git_dir: &'a Path,
    pub format: ObjectFormat,
    pub remote: &'a RemoteUrl,
    pub config: Option<&'a GitConfig>,
    pub refspecs: &'a [String],
    pub force: bool,
}

pub(crate) struct GitPushCommandsRequest<'a> {
    pub common_git_dir: &'a Path,
    pub format: ObjectFormat,
    pub remote: &'a RemoteUrl,
    pub config: Option<&'a GitConfig>,
    pub command_forces: Vec<(ReceivePackCommand, bool)>,
    pub pack_objects: Vec<ObjectId>,
}

pub(crate) struct GitPushPlan {
    pub(crate) commands: Vec<ReceivePackCommand>,
    pub(crate) pack_objects: Vec<ObjectId>,
    stream: Option<GitConnection>,
    features: ReceivePackFeatures,
    advertisements: Vec<RefAdvertisement>,
}

pub struct GitFetchPackRequest<'a> {
    pub git_dir: &'a Path,
    pub format: ObjectFormat,
    pub remote: &'a RemoteUrl,
    pub config: Option<&'a GitConfig>,
    pub features: &'a UploadPackFeatures,
    pub wants: Vec<ObjectId>,
    pub haves: Option<Vec<ObjectId>>,
    pub shallow: Vec<ObjectId>,
    pub deepen: Option<u32>,
    pub promisor: bool,
    pub protocol_v2: bool,
    /// Maximum raw pack bytes to accept from the remote (`fetch.maxInputSize` /
    /// `transfer.maxSize`). `None` means unlimited.
    pub max_input_size: Option<u64>,
}

pub struct GitUploadPackAdvertisements {
    pub refs: Vec<RefAdvertisement>,
    pub features: UploadPackFeatures,
    pub protocol_v2: bool,
    pub object_format: ObjectFormat,
}

pub(crate) fn ls_remote_git(
    remote: &RemoteUrl,
    filter: &crate::ls_remote::LsRemoteFilter,
    matches: &dyn Fn(&str) -> bool,
    protocol_v2: bool,
    config: Option<&GitConfig>,
) -> Result<(Vec<crate::ls_remote::LsRemoteRecord>, ObjectFormat)> {
    let set = if protocol_v2 {
        git_protocol_v2_ls_refs(remote, ObjectFormat::Sha1, config)?
    } else {
        let mut stream = connect_git_service(remote, GitService::UploadPack, None, config)?;
        read_ref_advertisement_set(ObjectFormat::Sha1, &mut stream)?
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
            "git:// ls-remote currently supports SHA-1 advertisements, got {}",
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

pub fn git_upload_pack_advertisements(
    remote: &RemoteUrl,
    format: ObjectFormat,
) -> Result<GitUploadPackAdvertisements> {
    git_upload_pack_advertisements_with_protocol(remote, format, false, None)
}

pub fn git_upload_pack_advertisements_with_protocol(
    remote: &RemoteUrl,
    format: ObjectFormat,
    protocol_v2: bool,
    config: Option<&GitConfig>,
) -> Result<GitUploadPackAdvertisements> {
    if protocol_v2 {
        let mut stream = connect_git_service(
            remote,
            GitService::UploadPack,
            Some(ProtocolVersion::V2),
            config,
        )?;
        let handshake = read_protocol_v2_advertisement(&mut stream)?;
        let object_format = protocol_v2_object_format(&handshake.capabilities)?;
        if object_format != format {
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                object_format.name(),
                format.name()
            )));
        }
        let set = git_protocol_v2_ls_refs_on_stream(format, &mut stream)?;
        let features = upload_pack_features_from_v2(&handshake, &set.refs)?;
        return Ok(GitUploadPackAdvertisements {
            refs: set.refs,
            features,
            protocol_v2: true,
            object_format: format,
        });
    }

    let mut stream = connect_git_service(remote, GitService::UploadPack, None, config)?;
    let set = read_ref_advertisement_set(format, &mut stream)?;
    let features = set
        .refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    Ok(GitUploadPackAdvertisements {
        refs: set.refs,
        features,
        protocol_v2: false,
        object_format: format,
    })
}

/// Discover upload-pack capabilities and object format before creating a clone.
pub fn discover_git_upload_pack_advertisements(
    remote: &RemoteUrl,
    protocol_v2: bool,
    config: Option<&GitConfig>,
) -> Result<GitUploadPackAdvertisements> {
    if protocol_v2 {
        let mut stream = connect_git_service(
            remote,
            GitService::UploadPack,
            Some(ProtocolVersion::V2),
            config,
        )?;
        let handshake = read_protocol_v2_advertisement(&mut stream)?;
        let object_format = protocol_v2_object_format(&handshake.capabilities)?;
        let set = git_protocol_v2_ls_refs_on_stream(object_format, &mut stream)?;
        let features = upload_pack_features_from_v2(&handshake, &set.refs)?;
        return Ok(GitUploadPackAdvertisements {
            refs: set.refs,
            features,
            protocol_v2: true,
            object_format,
        });
    }

    let mut stream = connect_git_service(remote, GitService::UploadPack, None, config)?;
    let (set, object_format) = crate::read_discovered_upload_pack_advertisements(&mut stream)?;
    let features = set
        .refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    Ok(GitUploadPackAdvertisements {
        refs: set.refs,
        features,
        protocol_v2: false,
        object_format,
    })
}

pub fn install_fetch_pack_via_git_upload_pack(
    request: GitFetchPackRequest<'_>,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if request.wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    if request.deepen.is_none() && all_wants_present(&local_db, &request.wants)? {
        return Ok(Vec::new());
    }
    let haves = request
        .haves
        .clone()
        .map(Ok)
        .unwrap_or_else(|| crate::local::local_have_oids(request.git_dir, request.format))?;
    if request.protocol_v2 {
        return git_protocol_v2_fetch_into_repository(&request, haves, &local_db, progress, cancel);
    }

    let upload_request = UploadPackRequest {
        wants: request.wants,
        capabilities: shallow_request_capabilities(request.deepen),
        shallow: request.shallow,
        deepen: request.deepen,
        ..UploadPackRequest::default()
    };
    let mut stream =
        connect_git_service(request.remote, GitService::UploadPack, None, request.config)?;
    read_ref_advertisement_set(request.format, &mut stream)?;
    write_upload_pack_request(&mut stream, Some(&upload_request))?;
    write_upload_pack_negotiation_request(
        &mut stream,
        &UploadPackNegotiationRequest { haves, done: true },
    )?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    if request.deepen.is_some() {
        let shallow_info = if request.promisor {
            let (shallow_info, _) =
                install_upload_pack_shallow_raw_promisor_response_from_reader_with_cancel(
                    request.format,
                    &mut stream,
                    &local_db,
                    request.max_input_size,
                    cancel,
                )?;
            shallow_info
        } else {
            let (shallow_info, _) =
                install_upload_pack_shallow_raw_response_from_reader_with_cancel(
                    request.format,
                    &mut stream,
                    &ProgressInstaller::new(&local_db, progress),
                    request.max_input_size,
                    cancel,
                )?;
            shallow_info
        };
        return Ok(shallow_info);
    }
    if request.promisor {
        install_upload_pack_raw_promisor_response_from_reader_with_cancel(
            request.format,
            &mut stream,
            &local_db,
            request.max_input_size,
            cancel,
        )?;
    } else {
        install_upload_pack_raw_response_from_reader_with_cancel(
            request.format,
            &mut stream,
            &ProgressInstaller::new(&local_db, progress),
            request.max_input_size,
            cancel,
        )?;
    }
    Ok(Vec::new())
}

pub(crate) fn plan_push_git(request: GitPushRequest<'_>) -> Result<GitPushPlan> {
    let GitPushRequest {
        git_dir,
        common_git_dir,
        format,
        remote,
        config,
        refspecs,
        force,
    } = request;
    let (stream, advertisements, features) = receive_pack_advertisements(remote, format, config)?;

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
            &advertisements,
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
        let mut stream = stream;
        let _ = stream.shutdown(Shutdown::Both);
        return Ok(GitPushPlan {
            commands,
            pack_objects: Vec::new(),
            stream: None,
            features,
            advertisements,
        });
    }

    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    crate::push::reject_non_fast_forward_pushes(
        common_git_dir,
        &local_db,
        format,
        &command_forces,
    )?;
    Ok(GitPushPlan {
        commands,
        pack_objects: Vec::new(),
        stream: Some(stream),
        features,
        advertisements,
    })
}

pub(crate) fn plan_push_git_commands(request: GitPushCommandsRequest<'_>) -> Result<GitPushPlan> {
    let GitPushCommandsRequest {
        common_git_dir,
        format,
        remote,
        config,
        command_forces,
        pack_objects,
    } = request;
    let (stream, advertisements, features) = receive_pack_advertisements(remote, format, config)?;
    let commands = command_forces
        .iter()
        .map(|(command, _)| command.clone())
        .collect::<Vec<_>>();
    if commands.is_empty() {
        let mut stream = stream;
        let _ = stream.shutdown(Shutdown::Both);
        return Ok(GitPushPlan {
            commands,
            pack_objects,
            stream: None,
            features,
            advertisements,
        });
    }

    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    crate::push::reject_non_fast_forward_pushes(
        common_git_dir,
        &local_db,
        format,
        &command_forces,
    )?;
    Ok(GitPushPlan {
        commands,
        pack_objects,
        stream: Some(stream),
        features,
        advertisements,
    })
}

pub(crate) fn execute_push_git_plan(
    request: PushRequest<'_>,
    mut plan: GitPushPlan,
    cancel: CancelFlag<'_>,
) -> Result<PushOutcome> {
    if plan.commands.is_empty() {
        return Ok(PushOutcome::default());
    }
    let mut stream = plan
        .stream
        .take()
        .ok_or_else(|| GitError::Command("git:// receive-pack stream was not available".into()))?;
    let commands = plan.commands.clone();
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
    crate::pack::write_receive_pack_body_with_cancel(
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
        &mut stream,
        cancel,
    )?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);

    let report = if plan.features.report_status || plan.features.report_status_v2 {
        let report = crate::push::read_receive_pack_push_report(
            request.format,
            &mut stream,
            plan.features.report_status_v2,
            plan.features.side_band_64k,
        )?;
        crate::push::validate_receive_pack_unpack(&report)?;
        Some(report)
    } else {
        let mut sink = Vec::new();
        stream.read_to_end(&mut sink)?;
        None
    };
    Ok(PushOutcome {
        commands,
        report,
        remote_progress: Vec::new(),
    })
}

fn receive_pack_advertisements(
    remote: &RemoteUrl,
    format: ObjectFormat,
    config: Option<&GitConfig>,
) -> Result<(GitConnection, Vec<RefAdvertisement>, ReceivePackFeatures)> {
    let mut stream = connect_git_service(remote, GitService::ReceivePack, None, config)?;
    let advertisement_set = read_ref_advertisement_set(format, &mut stream)?;
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
    Ok((stream, advertisement_set.refs, features))
}

fn connect_git_service(
    remote: &RemoteUrl,
    service: GitService,
    protocol: Option<ProtocolVersion>,
    config: Option<&GitConfig>,
) -> Result<GitConnection> {
    if remote.transport != RemoteTransport::Git {
        return Err(GitError::InvalidFormat(
            "git:// service requires a git remote".into(),
        ));
    }
    let host = remote
        .host
        .as_deref()
        .ok_or_else(|| GitError::InvalidFormat("git:// remote is missing a host".into()))?;
    let port = remote.port.unwrap_or(GIT_DAEMON_PORT);
    let mut stream = crate::git_proxy::connect_git_transport(config, host, port)?;
    let protocol = protocol.or_else(|| configured_git_protocol_v1(config));
    let request = ServiceRequest {
        service,
        path: remote.path.clone(),
        host: Some(git_host_parameter(remote, host, port)),
        parameters: Vec::new(),
        protocol,
        extra_parameters: Vec::new(),
    };
    write_service_request(&mut stream, &request)?;
    stream.flush()?;
    Ok(stream)
}

fn configured_git_protocol_v1(config: Option<&GitConfig>) -> Option<ProtocolVersion> {
    (config.and_then(|config| config.get("protocol", None, "version")) == Some("1"))
        .then_some(ProtocolVersion::V1)
}

fn git_protocol_v2_command_options(format: ObjectFormat) -> Vec<Capability> {
    sley_protocol::encode_protocol_v2_command_options(&ProtocolV2CommandOptions {
        object_format: Some(format),
        ..ProtocolV2CommandOptions::default()
    })
    .unwrap_or_default()
}

fn git_protocol_v2_ls_refs(
    remote: &RemoteUrl,
    format: ObjectFormat,
    config: Option<&GitConfig>,
) -> Result<sley_protocol::RefAdvertisementSet> {
    let mut stream = connect_git_service(
        remote,
        GitService::UploadPack,
        Some(ProtocolVersion::V2),
        config,
    )?;
    let handshake = read_protocol_v2_advertisement(&mut stream)?;
    let object_format = protocol_v2_object_format(&handshake.capabilities)?;
    if object_format != format {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository uses {}, local repository uses {}",
            object_format.name(),
            format.name()
        )));
    }
    git_protocol_v2_ls_refs_on_stream(format, &mut stream)
}

fn git_protocol_v2_ls_refs_on_stream(
    format: ObjectFormat,
    stream: &mut GitConnection,
) -> Result<sley_protocol::RefAdvertisementSet> {
    let mut request = ProtocolV2LsRefsRequest {
        peel: true,
        symrefs: true,
        unborn: true,
        ref_prefixes: vec!["HEAD".into(), "refs/heads/".into(), "refs/tags/".into()],
    }
    .to_command_request()?;
    request.capabilities = git_protocol_v2_command_options(format);
    write_protocol_v2_command_request(stream, &request)?;
    stream.flush()?;
    let records = read_protocol_v2_ls_refs_response(format, stream)?;
    protocol_v2_ls_refs_records_to_ref_advertisement_set(&records)
}

fn upload_pack_features_from_v2(
    handshake: &TransportHandshake,
    refs: &[RefAdvertisement],
) -> Result<UploadPackFeatures> {
    let v2 = parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
    let mut features = UploadPackFeatures {
        object_format: Some(protocol_v2_object_format(&handshake.capabilities)?),
        shallow: v2.shallow,
        deepen_since: v2.shallow,
        deepen_not: v2.shallow,
        filter: v2.filter,
        ..UploadPackFeatures::default()
    };
    if let Some(first) = refs.first() {
        let bridged = parse_upload_pack_features(&first.capabilities)?;
        features.symrefs = bridged.symrefs;
    }
    Ok(features)
}

fn git_protocol_v2_fetch_into_repository(
    request: &GitFetchPackRequest<'_>,
    haves: Vec<ObjectId>,
    local_db: &FileObjectDatabase,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    let mut stream = connect_git_service(
        request.remote,
        GitService::UploadPack,
        Some(ProtocolVersion::V2),
        request.config,
    )?;
    let handshake = read_protocol_v2_advertisement(&mut stream)?;
    let v2_features =
        parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
    let fetch = ProtocolV2FetchRequest {
        wants: request.wants.clone(),
        haves,
        shallow: request.shallow.clone(),
        deepen: request.deepen,
        thin_pack: true,
        include_tag: true,
        ofs_delta: true,
        done: true,
        wait_for_done: v2_features.wait_for_done,
        sideband_all: v2_features.sideband_all,
        ..ProtocolV2FetchRequest::default()
    };
    write_protocol_v2_fetch_request(&mut stream, &fetch)?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    let (header, _install) = if request.promisor {
        install_protocol_v2_fetch_promisor_response_from_reader_with_cancel(
            request.format,
            &mut stream,
            v2_features.sideband_all,
            local_db,
            request.max_input_size,
            cancel,
        )?
    } else {
        install_protocol_v2_fetch_response_from_reader_with_cancel(
            request.format,
            &mut stream,
            v2_features.sideband_all,
            &ProgressInstaller::new(local_db, progress),
            request.max_input_size,
            cancel,
        )?
    };
    Ok(shallow_info_from_protocol_v2_fetch_header(&header))
}

fn git_host_parameter(remote: &RemoteUrl, host: &str, port: u16) -> String {
    match remote.port {
        Some(_) if host.contains(':') && !host.starts_with('[') => format!("[{host}]:{port}"),
        Some(_) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn shallow_request_capabilities(deepen: Option<u32>) -> Vec<Capability> {
    if deepen.is_some() {
        vec![Capability {
            name: "shallow".into(),
            value: None,
        }]
    } else {
        Vec::new()
    }
}

fn all_wants_present(db: &FileObjectDatabase, wants: &[ObjectId]) -> Result<bool> {
    for want in wants {
        if !db.contains(want)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    use sley_protocol::{ProtocolVersion, RefAdvertisement, RefAdvertisementSet};
    use sley_transport::read_service_request;

    #[test]
    fn configured_protocol_v1_is_added_to_git_daemon_requests() {
        let config = GitConfig::parse(b"[protocol]\n\tversion = 1\n").expect("config");
        assert_eq!(
            configured_git_protocol_v1(Some(&config)),
            Some(ProtocolVersion::V1)
        );
        let config = GitConfig::parse(b"[protocol]\n\tversion = 2\n").expect("config");
        assert_eq!(configured_git_protocol_v1(Some(&config)), None);
        assert_eq!(configured_git_protocol_v1(None), None);
    }

    #[test]
    fn ls_remote_git_sends_daemon_request_and_reads_advertisements() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind daemon");
        let port = listener.local_addr().expect("local addr").port();
        let tip = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("oid");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept daemon client");
            let request = read_service_request(&mut stream).expect("service request");
            assert_eq!(request.service, GitService::UploadPack);
            assert_eq!(request.path, "/repo.git");
            let expected_host = format!("127.0.0.1:{port}");
            assert_eq!(request.host.as_deref(), Some(expected_host.as_str()));
            sley_protocol::write_ref_advertisement_set(
                &mut stream,
                &RefAdvertisementSet {
                    protocol: ProtocolVersion::V0,
                    refs: vec![
                        RefAdvertisement {
                            oid: tip,
                            name: "HEAD".into(),
                            capabilities: vec![Capability {
                                name: "symref".into(),
                                value: Some("HEAD:refs/heads/main".into()),
                            }],
                        },
                        RefAdvertisement {
                            oid: tip,
                            name: "refs/heads/main".into(),
                            capabilities: Vec::new(),
                        },
                    ],
                    shallow: Vec::new(),
                },
            )
            .expect("write advertisements");
        });
        let remote = RemoteUrl {
            transport: RemoteTransport::Git,
            user: None,
            password: None,
            host: Some("127.0.0.1".into()),
            port: Some(port),
            path: "/repo.git".into(),
        };
        let (records, format) = ls_remote_git(
            &remote,
            &crate::ls_remote::LsRemoteFilter::default(),
            &|_| true,
            false,
            None,
        )
        .expect("ls-remote");

        server.join().expect("server thread");
        assert_eq!(format, ObjectFormat::Sha1);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "HEAD");
        assert_eq!(records[0].symref.as_deref(), Some("refs/heads/main"));
        assert_eq!(records[1].name, "refs/heads/main");
        assert_eq!(records[1].oid, tip);
    }
}
