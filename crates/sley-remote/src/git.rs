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
    ProgressInstaller, emit_remote_sideband_progress,
    install_protocol_v2_fetch_promisor_response_from_reader_with_cancel,
    install_protocol_v2_fetch_response_from_reader_with_cancel,
    install_upload_pack_raw_promisor_response_from_reader_with_cancel,
    install_upload_pack_raw_response_from_reader_with_cancel,
    shallow_info_from_protocol_v2_fetch_header,
};
use sley_core::{CancelFlag, Capability, GitError, ObjectFormat, ObjectId, Result};
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    GitService, ProtocolV2FetchShallowInfo, ProtocolVersion, ReceivePackCommand,
    ReceivePackFeatures, RefAdvertisement, TransportLimits, UploadPackFeatures,
    UploadPackNegotiationRequest, UploadPackRequest, parse_receive_pack_features, parse_refspec,
    plan_push_commands, read_ref_advertisement_set, read_to_end_bounded,
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
    pub deepen_since: Option<i64>,
    pub deepen_not: Vec<String>,
    pub deepen_relative: bool,
    pub filter: Option<&'a sley_odb::PackObjectFilter>,
    pub promisor: bool,
    /// Historical caller hint. The connection negotiates from the configured
    /// protocol version and the server greeting, so this flag does not force
    /// protocol v0.
    pub protocol_v2: bool,
    /// Maximum raw pack bytes to accept from the remote (`fetch.maxInputSize` /
    /// `transfer.maxSize`). `None` means unlimited.
    pub max_input_size: Option<u64>,
    /// Send v2 `no-progress` so a quiet or non-terminal fetch does not print
    /// sideband progress. Ignored on the v0 path, which installs a raw pack.
    pub no_progress: bool,
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
    let _ = protocol_v2;
    let discovered =
        git_upload_pack_advertisements_with_protocol(remote, ObjectFormat::Sha1, false, config)?;
    let features = discovered.features;
    let format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    let refs = discovered.refs;
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
    for advertisement in refs {
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
    let _ = protocol_v2;
    read_git_upload_pack_advertisements(remote, Some(format), config)
}

/// Discover upload-pack capabilities and object format before creating a clone.
pub fn discover_git_upload_pack_advertisements(
    remote: &RemoteUrl,
    protocol_v2: bool,
    config: Option<&GitConfig>,
) -> Result<GitUploadPackAdvertisements> {
    let _ = protocol_v2;
    read_git_upload_pack_advertisements(remote, None, config)
}

fn read_git_upload_pack_advertisements(
    remote: &RemoteUrl,
    format: Option<ObjectFormat>,
    config: Option<&GitConfig>,
) -> Result<GitUploadPackAdvertisements> {
    let wire = crate::v2_client::upload_pack_protocol_parameter(
        crate::v2_client::client_upload_pack_version(config),
    );
    let mut stream = connect_git_service(remote, GitService::UploadPack, wire, config)?;
    let discovered = match format {
        Some(format) => {
            match crate::v2_client::read_stateful_upload_pack_greeting(format, &mut stream)? {
                crate::v2_client::StatefulUploadPackGreeting::V2(handshake) => {
                    finish_git_v2_ls_refs(&mut stream, format, &handshake)?
                }
                crate::v2_client::StatefulUploadPackGreeting::Legacy(set) => {
                    let features = crate::v2_client::legacy_upload_pack_features(&set.refs)?;
                    GitUploadPackAdvertisements {
                        refs: set.refs,
                        features,
                        protocol_v2: false,
                        object_format: format,
                    }
                }
            }
        }
        None => match crate::v2_client::read_discovered_stateful_upload_pack_greeting(&mut stream)?
        {
            crate::v2_client::DiscoveredStatefulUploadPack::V2 {
                handshake,
                object_format,
            } => finish_git_v2_ls_refs(&mut stream, object_format, &handshake)?,
            crate::v2_client::DiscoveredStatefulUploadPack::Legacy { set, object_format } => {
                let features = crate::v2_client::legacy_upload_pack_features(&set.refs)?;
                GitUploadPackAdvertisements {
                    refs: set.refs,
                    features,
                    protocol_v2: false,
                    object_format,
                }
            }
        },
    };
    let _ = stream.shutdown(Shutdown::Both);
    Ok(discovered)
}

fn finish_git_v2_ls_refs(
    stream: &mut GitConnection,
    format: ObjectFormat,
    handshake: &sley_protocol::TransportHandshake,
) -> Result<GitUploadPackAdvertisements> {
    let remote_format = sley_protocol::protocol_v2_object_format(&handshake.capabilities)?;
    if remote_format != format {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository uses {}, local repository uses {}",
            remote_format.name(),
            format.name()
        )));
    }
    crate::v2_client::write_stateful_v2_ls_refs(stream, format, handshake)?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    let set = crate::v2_client::read_stateful_v2_ls_refs(format, stream)?;
    let features = crate::v2_client::upload_pack_features_from_v2(handshake, &set.refs)?;
    Ok(GitUploadPackAdvertisements {
        refs: set.refs,
        features,
        protocol_v2: true,
        object_format: format,
    })
}

pub fn install_fetch_pack_via_git_upload_pack(
    policy: &crate::RemotePolicy,
    request: GitFetchPackRequest<'_>,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if request.wants.is_empty() {
        return Ok(Vec::new());
    }
    let _ = request.protocol_v2;
    let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    let boundary_may_move = request.deepen.is_some()
        || request.deepen_since.is_some()
        || !request.deepen_not.is_empty()
        || request.deepen_relative;
    if !boundary_may_move
        && request.filter.is_none()
        && all_wants_present(&local_db, &request.wants)?
    {
        return Ok(Vec::new());
    }
    let haves = request.haves.clone().map(Ok).unwrap_or_else(|| {
        crate::local::local_have_oids(policy, request.git_dir, request.format)
    })?;
    let wire = crate::v2_client::upload_pack_protocol_parameter(
        crate::v2_client::client_upload_pack_version(request.config),
    );
    let mut stream =
        connect_git_service(request.remote, GitService::UploadPack, wire, request.config)?;
    match crate::v2_client::read_stateful_upload_pack_greeting(request.format, &mut stream)? {
        crate::v2_client::StatefulUploadPackGreeting::V2(handshake) => {
            let remote_format = sley_protocol::protocol_v2_object_format(&handshake.capabilities)?;
            if remote_format != request.format {
                return Err(GitError::InvalidObjectId(format!(
                    "remote repository uses {}, local repository uses {}",
                    remote_format.name(),
                    request.format.name()
                )));
            }
            let sideband_all = crate::v2_client::write_stateful_v2_fetch(
                &mut stream,
                crate::v2_client::StatefulV2FetchParams {
                    format: request.format,
                    handshake: &handshake,
                    wants: request.wants.clone(),
                    want_refs: Vec::new(),
                    haves,
                    shallow: request.shallow.clone(),
                    deepen: request.deepen,
                    deepen_since: request.deepen_since,
                    deepen_not: request.deepen_not.clone(),
                    deepen_relative: request.deepen_relative,
                    filter: request.filter,
                    packfile_uri_protocols: None,
                    no_progress: request.no_progress,
                },
            )?;
            stream.flush()?;
            let _ = stream.shutdown(Shutdown::Write);
            let (header, _) = if request.promisor {
                install_protocol_v2_fetch_promisor_response_from_reader_with_cancel(
                    request.format,
                    &mut stream,
                    sideband_all,
                    &local_db,
                    request.max_input_size,
                    cancel,
                    Some(&mut |chunk: &[u8]| emit_remote_sideband_progress(&mut *progress, chunk)),
                )?
            } else {
                let installer = ProgressInstaller::new(&local_db, progress);
                install_protocol_v2_fetch_response_from_reader_with_cancel(
                    request.format,
                    &mut stream,
                    sideband_all,
                    &installer,
                    request.max_input_size,
                    cancel,
                    Some(&mut installer.remote_sideband_forwarder()),
                )?
            };
            Ok(shallow_info_from_protocol_v2_fetch_header(&header))
        }
        crate::v2_client::StatefulUploadPackGreeting::Legacy(set) => {
            let advertised = crate::v2_client::legacy_upload_pack_features(&set.refs)?;
            let filter = request
                .filter
                .filter(|_| advertised.filter)
                .and_then(crate::local::upload_pack_filter_protocol_spec);
            let expects_shallow = request.deepen.is_some()
                || request.deepen_since.is_some()
                || !request.deepen_not.is_empty();
            let upload_request = UploadPackRequest {
                wants: request.wants.clone(),
                capabilities: shallow_request_capabilities(expects_shallow, filter.is_some()),
                shallow: request.shallow.clone(),
                deepen: request.deepen,
                deepen_since: crate::v2_client::deepen_since_u64(request.deepen_since),
                deepen_not: request.deepen_not.clone(),
                filter,
            };
            write_upload_pack_request(&mut stream, Some(&upload_request))?;
            stream.flush()?;
            // A shallow request is stateful: the server writes shallow/unshallow
            // and a flush before it will read haves.
            let shallow_info = if expects_shallow {
                sley_protocol::read_upload_pack_shallow_info_section(request.format, &mut stream)?
            } else {
                Vec::new()
            };
            write_upload_pack_negotiation_request(
                &mut stream,
                &UploadPackNegotiationRequest { haves, done: true },
            )?;
            stream.flush()?;
            let _ = stream.shutdown(Shutdown::Write);
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
            Ok(shallow_info)
        }
    }
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
        drain_unreported_receive_pack_response(
            &mut stream,
            crate::transport_limits_from_config(Some(request.config)),
        )?;
        None
    };
    Ok(PushOutcome {
        commands,
        report,
        remote_progress: Vec::new(),
    })
}

fn drain_unreported_receive_pack_response(
    stream: &mut dyn Read,
    limits: TransportLimits,
) -> Result<()> {
    read_to_end_bounded(stream, limits.receive_pack_response())?;
    Ok(())
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

fn git_host_parameter(remote: &RemoteUrl, host: &str, port: u16) -> String {
    match remote.port {
        Some(_) if host.contains(':') && !host.starts_with('[') => format!("[{host}]:{port}"),
        Some(_) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn shallow_request_capabilities(shallow: bool, filter: bool) -> Vec<Capability> {
    let mut capabilities = Vec::new();
    if shallow {
        capabilities.push(Capability {
            name: "shallow".into(),
            value: None,
        });
    }
    if filter {
        capabilities.push(Capability {
            name: "filter".into(),
            value: None,
        });
    }
    capabilities
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

    fn response_limits(bytes: u64) -> TransportLimits {
        TransportLimits {
            max_ref_advertisement_bytes: bytes,
            ..TransportLimits::default()
        }
    }

    #[test]
    fn oversized_git_receive_pack_stream_is_refused_cleanly() {
        let exact = vec![b'x'; 64 * 1024];
        drain_unreported_receive_pack_response(&mut exact.as_slice(), response_limits(64 * 1024))
            .expect("git:// receive-pack output exactly at the ceiling must be admitted");

        let oversized = vec![b'x'; exact.len() + 1];
        let error = drain_unreported_receive_pack_response(
            &mut oversized.as_slice(),
            response_limits(64 * 1024),
        )
        .expect_err("git:// receive-pack output past the ceiling must be refused");
        assert!(
            error.to_string().contains("exceeds the configured ceiling"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("stopped at 65537 bytes"),
            "the bounded read must stop one byte past the ceiling: {error}"
        );
    }

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
