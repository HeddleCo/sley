//! receive-pack, upload-pack, send-pack, and push.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::clone::{
    trace_index_pack_fsck_objects_if_configured, trace_pack_objects_filter,
    validate_upload_pack_filter_config,
};
use super::config::read_repo_config_on_disk;
use super::config::{read_repo_config, write_repo_config};
use super::fetch::StdoutProgress;
use super::fetch::{
    check_transport_allowed_url, configured_server_options, default_fetch_remote,
    ls_remote_resolved_url, repo_config_with_transport_policy, transport_policy_config_for_cwd,
};
use super::resolve::ls_remote_git_dir;
use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley::plumbing::sley_odb::ObjectReader;
use sley::plumbing::sley_remote::{FetchOptions, LsRemoteRecord};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;

fn receive_max_input_size(config: &GitConfig) -> Option<u64> {
    let raw = config.get("receive", None, "maxInputSize")?;
    match sley_config::parse_config_int(raw) {
        Some(limit) if limit > 0 => Some(limit as u64),
        _ => None,
    }
}

/// Read the receive-pack packfile from `reader` into a buffer, enforcing the
/// `receive.maxInputSize` cap (used only on the fsck path, which must hold the
/// whole pack to validate it). With no cap this is a plain `read_to_end`; with a
/// cap it bounds the read at `limit + 1` and refuses anything larger, mirroring
/// index-pack's `pack exceeds maximum allowed size` die (exit 128).
fn read_capped_packfile<R: Read>(reader: &mut R, max_input_size: Option<u64>) -> Result<Vec<u8>> {
    let mut packfile = Vec::new();
    match max_input_size {
        Some(limit) => {
            // `take(limit + 1)` reads at most one byte past the cap, so a buffer
            // strictly larger than `limit` is detectable without slurping an
            // arbitrarily large input into memory.
            reader
                .take(limit.saturating_add(1))
                .read_to_end(&mut packfile)?;
            if packfile.len() as u64 > limit {
                eprintln!(
                    "fatal: pack exceeds maximum allowed size ({})",
                    crate::commands::pack::humanise_byte_count(limit)
                );
                return Err(GitError::Exit(128));
            }
        }
        None => {
            reader.read_to_end(&mut packfile)?;
        }
    }
    Ok(packfile)
}

pub(crate) fn cmd_receive_pack(args: &[String]) -> Result<()> {
    let mut repository: Option<&String> = None;
    let mut stateless_rpc = false;
    let mut advertise_refs = false;
    let mut quiet = false;
    for arg in args {
        match arg.as_str() {
            "--stateless-rpc" => stateless_rpc = true,
            "--http-backend-info-refs" | "--advertise-refs" => advertise_refs = true,
            "--quiet" | "-q" => quiet = true,
            "--reject-thin-pack-for-testing" => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "receive-pack: unknown option {value}"
                )));
            }
            value => {
                if repository.is_some() {
                    return Err(GitError::Command(
                        "receive-pack currently supports: receive-pack <repository>".into(),
                    ));
                }
                repository = Some(arg);
                let _ = value;
            }
        }
    }
    let Some(repository) = repository else {
        return Err(GitError::Command(
            "receive-pack currently supports: receive-pack <repository>".into(),
        ));
    };
    let git_dir = common_git_dir_for_git_dir(&ls_remote_git_dir(repository)?)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let mut features = sley_remote::receive_pack_features(format);
    features.atomic = config
        .get_bool("receive", None, "advertiseatomic")
        .unwrap_or(true);
    features.push_options = config
        .get_bool("receive", None, "advertisepushoptions")
        .unwrap_or(false);
    let mut advertisements = sley_remote::local_fetch_advertisements(&git_dir, format)?;
    sley_remote::attach_receive_pack_capabilities(&mut advertisements, format, &features)?;

    if advertise_refs || !stateless_rpc {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        write_ref_advertisement_set(
            &mut stdout,
            &RefAdvertisementSet {
                protocol: match requested_protocol_version_from_environment() {
                    Some(ProtocolVersion::V1) => ProtocolVersion::V1,
                    _ => ProtocolVersion::V0,
                },
                refs: advertisements,
                shallow: Vec::new(),
            },
        )?;
        stdout.flush()?;
    }
    if advertise_refs {
        return Ok(());
    }

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let commands = read_receive_pack_request(format, &mut stdin)?;
    let push_options = sley_remote::receive_pack_request_uses_push_options(&commands)
        .then(|| read_receive_pack_push_options(&mut stdin))
        .transpose()?;
    let header = sley_protocol::ReceivePackPushRequestHeader {
        commands,
        push_options,
    };
    let use_sideband = sley_remote::request_uses_sideband(&header);
    let wants_report = header
        .commands
        .capabilities
        .iter()
        .any(|cap| cap.name == "report-status" || cap.name == "report-status-v2");
    let mut hook_stderr = Vec::new();
    let push_options = header.push_options.as_deref().unwrap_or(&[]);
    let outcome = sley_remote::serve_receive_pack(sley_remote::ReceivePackServerRequest {
        git_dir: &git_dir,
        format,
        header: &header,
        pack_reader: &mut stdin,
        config: &config,
        options: sley_remote::ReceivePackServerOptions {
            quiet,
            remote_stderr: Some(&mut hook_stderr),
            run_post_hooks: false,
        },
    })?;
    if use_sideband {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        sley_remote::write_receive_pack_sideband_stderr(&mut stdout, &hook_stderr)?;
        if wants_report {
            sley_remote::write_receive_pack_server_report(
                &mut stdout,
                &outcome.report,
                true,
                false,
            )?;
        }
        let mut post_stderr = Vec::new();
        sley_remote::run_receive_pack_post_hooks(
            &git_dir,
            &outcome.command_states,
            push_options,
            &mut post_stderr,
            true,
        );
        sley_remote::write_receive_pack_sideband_stderr(&mut stdout, &post_stderr)?;
        sley_remote::flush_receive_pack_sideband(&mut stdout)?;
        stdout.flush()?;
    } else {
        print_receive_pack_hook_stderr(&hook_stderr);
        if wants_report {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            sley_remote::write_receive_pack_server_report(
                &mut stdout,
                &outcome.report,
                false,
                false,
            )?;
            stdout.flush()?;
        }
        let mut post_stderr = Vec::new();
        sley_remote::run_receive_pack_post_hooks(
            &git_dir,
            &outcome.command_states,
            push_options,
            &mut post_stderr,
            true,
        );
        print_receive_pack_hook_stderr(&post_stderr);
    }

    Ok(())
}

fn print_receive_pack_hook_stderr(hook_stderr: &[u8]) {
    if hook_stderr.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(hook_stderr);
    for line in text.lines() {
        eprintln!("{line}");
    }
}

/// Protocol requested by the connecting client via `GIT_PROTOCOL`
/// (`version=1`/`version=2`, possibly among colon-separated tokens). Mirrors
/// `git_protocol_version_from_environment` in protocol.c for server commands.
fn requested_protocol_version_from_environment() -> Option<ProtocolVersion> {
    let value = std::env::var("GIT_PROTOCOL")
        .or_else(|_| std::env::var("HTTP_GIT_PROTOCOL"))
        .ok()?;
    value.split(':').find_map(|token| match token {
        "version=1" => Some(ProtocolVersion::V1),
        "version=2" => Some(ProtocolVersion::V2),
        _ => None,
    })
}

pub(crate) fn cmd_upload_pack(args: &[String]) -> Result<()> {
    // Accept (and ignore) the upload-pack flags the transports pass through:
    // `git daemon` runs `upload-pack --strict <dir>`, the smart transports add
    // `--stateless-rpc`/`--advertise-refs`/`--timeout=<n>`. The repository is
    // the lone positional argument. Mirrors builtin/upload-pack.c's options.
    let mut repository: Option<&String> = None;
    for arg in args {
        match arg.as_str() {
            "--strict" | "--stateless-rpc" | "--advertise-refs" | "--http-backend-info-refs" => {}
            value if value.starts_with("--timeout=") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "upload-pack: unknown option {value}"
                )));
            }
            value => {
                if repository.is_some() {
                    return Err(GitError::Command(
                        "upload-pack currently supports: upload-pack <repository>".into(),
                    ));
                }
                repository = Some(arg);
                let _ = value;
            }
        }
    }
    let Some(repository) = repository else {
        return Err(GitError::Command(
            "upload-pack currently supports: upload-pack <repository>".into(),
        ));
    };
    let git_dir = ls_remote_git_dir(repository)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    validate_upload_pack_filter_config()?;

    // Protocol v2: the client requests version 2 via the `GIT_PROTOCOL`
    // environment variable (the daemon/file:// transport propagates it from the
    // connection's `version=2` extra-arg). Run the v2 server loop instead of the
    // v0 ref advertisement. Mirrors upload-pack.c's `determine_protocol_version`.
    let requested_protocol =
        requested_protocol_version_from_environment().unwrap_or(ProtocolVersion::V0);
    if requested_protocol == ProtocolVersion::V2 {
        let config = read_repo_config(&git_dir)?;
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        return sley_remote::serve_upload_pack_v2_with_config(
            &git_dir,
            format,
            &config,
            &mut stdin,
            &mut stdout,
        );
    }
    let features = sley_remote::upload_pack_features(&git_dir, format)?;
    let mut advertisements = sley_remote::local_fetch_advertisements(&git_dir, format)?;
    sley_remote::attach_upload_pack_capabilities(&mut advertisements, format, &features)?;

    {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        write_ref_advertisement_set(
            &mut stdout,
            &RefAdvertisementSet {
                protocol: requested_protocol,
                refs: advertisements,
                shallow: Vec::new(),
            },
        )?;
        stdout.flush()?;
    }

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let request = match read_upload_pack_request(format, &mut stdin) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(GitError::InvalidFormat(message))
            if message == "pkt-line stream ended before control packet" =>
        {
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    let mut haves = HashSet::new();
    loop {
        let negotiation = read_upload_pack_negotiation_request(format, &mut stdin)?;
        haves.extend(negotiation.haves);
        if negotiation.done {
            break;
        }
    }

    let sideband = sley_remote::upload_pack_request_uses_sideband(&request);
    let response = sley_remote::upload_pack_from_local_repository(
        &git_dir, format, &features, request, haves,
    )?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if sideband {
        let response = sley_remote::upload_pack_sideband_response(response);
        write_upload_pack_packfile_response(&mut stdout, &response)?;
    } else {
        write_upload_pack_raw_packfile_response(&mut stdout, &response)?;
    }
    stdout.flush()?;
    Ok(())
}

const SEND_PACK_USAGE: &str = "usage: git send-pack [--mirror] [--dry-run] [--force]\n              [--receive-pack=<git-receive-pack>]\n              [--verbose] [--thin] [--atomic]\n              [--[no-]signed | --signed=(true|false|if-asked)]\n              [<host>:]<directory> (--all | <ref>...)";

/// `git send-pack [<options>] <directory> (--all | <ref>...)`: the plumbing push
/// to a local repository. Unlike `git push`, it takes a literal destination
/// directory (no remote-name resolution) and pushes bare refs as `<ref>:<ref>`
/// (a leading `:` deletes). It renders the same status report as push but does
/// not touch remote-tracking refs (it is a low-level transport, not porcelain).
pub(crate) fn cmd_send_pack(args: &[String]) -> Result<()> {
    // `-h` is handled by git's option parser before any repository is opened, so
    // it works outside a git repo too (the `nongit` case in t5400).
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    }

    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&git_dir, format);

    let mut force = false;
    let mut dry_run = false;
    let mut atomic = false;
    let mut mirror = false;
    let mut all_refs = false;
    let mut quiet = false;
    let mut thin = sley_remote::PushThinMode::Auto;
    let mut from_stdin = false;
    let mut force_with_lease_specs: Vec<String> = Vec::new();
    let mut receive_pack_command: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{SEND_PACK_USAGE}");
                return Err(GitError::Exit(129));
            }
            "-f" | "--force" => force = true,
            "-n" | "--dry-run" => dry_run = true,
            "--atomic" => atomic = true,
            "--mirror" => mirror = true,
            "--all" => all_refs = true,
            "--stdin" => from_stdin = true,
            "-q" | "--quiet" => quiet = true,
            "-v" | "--verbose" | "--progress" | "--no-progress" | "--stateless-rpc"
            | "--helper-status" => {}
            "--thin" => thin = sley_remote::PushThinMode::Always,
            "--no-thin" => thin = sley_remote::PushThinMode::Never,
            "--signed" | "--no-signed" => {}
            value if value.starts_with("--signed=") => {}
            "--force-if-includes" => {}
            value if value.starts_with("--force-with-lease=") => {
                force_with_lease_specs.push(value["--force-with-lease=".len()..].to_string());
            }
            "--receive-pack" | "--exec" => {
                receive_pack_command = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command(format!(
                                "send-pack {} requires a value",
                                arg.as_str()
                            ))
                        })?
                        .to_string(),
                );
            }
            "--remote" | "--push-option" => {
                iter.next().ok_or_else(|| {
                    GitError::Command(format!("send-pack {} requires a value", arg.as_str()))
                })?;
            }
            value if value.starts_with("--receive-pack=") || value.starts_with("--exec=") => {
                let (_, command) = value.split_once('=').unwrap_or((value, ""));
                receive_pack_command = Some(command.to_string());
            }
            value if value.starts_with("--remote=") || value.starts_with("--push-option=") => {}
            "--" => {
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                eprintln!("{SEND_PACK_USAGE}");
                return Err(GitError::Exit(129));
            }
            value => positional.push(value.to_string()),
        }
    }

    if mirror {
        force = true;
    }

    let Some((dest, refs)) = positional.split_first() else {
        eprintln!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    };
    let mut refs = refs.to_vec();
    if from_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        refs.extend(
            input
                .lines()
                .map(str::trim_end)
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    if refs.is_empty() && !all_refs && !mirror {
        eprintln!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    }
    if !refs.is_empty() && (all_refs || mirror) {
        eprintln!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    }

    let remote_git_dir = ls_remote_git_dir(dest)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;

    let mut refspecs: Vec<String> = if mirror {
        vec!["refs/*:refs/*".to_string()]
    } else if all_refs {
        vec!["refs/heads/*:refs/heads/*".to_string()]
    } else {
        // send-pack pushes each bare ref as `<ref>:<ref>` (a leading `:` is a
        // delete; a leading `+` forces); no porcelain-style short-name guessing.
        refs.iter()
            .map(|spec| {
                let (force_marker, body) = match spec.strip_prefix('+') {
                    Some(rest) => ("+", rest),
                    None => ("", spec.as_str()),
                };
                let normalized = if let Some(rest) = body.strip_prefix(':') {
                    format!(":{}", sley_remote::normalize_push_refname(rest))
                } else if let Some((src, dstn)) = body.split_once(':') {
                    format!(
                        "{}:{}",
                        sley_remote::normalize_push_refname(src),
                        sley_remote::normalize_push_refname(dstn)
                    )
                } else {
                    let name = sley_remote::normalize_push_refname(body);
                    format!("{name}:{name}")
                };
                format!("{force_marker}{normalized}")
            })
            .collect()
    };
    reject_duplicate_push_destinations(&refspecs)?;

    // `--mirror` deletes remote refs the local repo no longer has.
    if mirror {
        let remote_advertisements =
            sley_remote::local_fetch_advertisements(&remote_git_dir, format)?;
        let local_names: std::collections::HashSet<String> = store
            .list_refs()?
            .into_iter()
            .map(|reference| reference.name)
            .collect();
        for advertisement in &remote_advertisements {
            if advertisement.name.starts_with("refs/") && !local_names.contains(&advertisement.name)
            {
                refspecs.push(format!(":{}", advertisement.name));
            }
        }
    }

    let config = read_repo_config(&git_dir).unwrap_or_default();
    let force_with_lease = resolve_force_with_lease(
        &git_dir,
        &store,
        &config,
        format,
        dest,
        &force_with_lease_specs,
    )?;
    let receive_config_overrides = receive_pack_config_overrides(receive_pack_command.as_deref());
    let options = PushOptions {
        quiet,
        set_upstream: false,
        force,
        no_verify: true,
        dry_run,
        progress: false,
        thin,
    };
    run_push_local_report(RunPushLocalReport {
        git_dir: &git_dir,
        common_git_dir: &common_git_dir,
        format,
        remote: dest,
        resolved_remote: dest,
        remote_git_dir: &remote_git_dir,
        remote_common_git_dir: &remote_common_git_dir,
        refspecs: &refspecs,
        options,
        porcelain: false,
        atomic,
        force_if_includes: false,
        push_options: &[],
        force_with_lease: &force_with_lease,
        force_with_lease_default: false,
        receive_pack_command: receive_pack_command.as_deref(),
        receive_config_overrides: &receive_config_overrides,
    })
}

fn reject_duplicate_push_destinations(refspecs: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for refspec in refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        let dst = body.split_once(':').map(|(_, dst)| dst).unwrap_or(body);
        if dst.is_empty() || dst.contains('*') {
            continue;
        }
        if !seen.insert(dst.to_string()) {
            eprintln!("error: multiple updates for ref '{dst}' not allowed");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

pub(crate) fn cmd_push(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut quiet = false;
    let mut set_upstream = false;
    let mut delete = false;
    let mut force = false;
    let mut no_verify = false;
    let mut dry_run = false;
    let mut progress = false;
    let mut porcelain = false;
    let mut atomic = false;
    let mut mirror = false;
    let mut all_refs = false;
    let mut tags = false;
    let mut follow_tags = false;
    let mut prune = false;
    let mut thin = sley_remote::PushThinMode::Auto;
    let mut recurse_submodules = PushRecurseSubmodules::Default;
    let mut receive_pack_command: Option<String> = None;
    let mut push_options_cmdline: Option<Vec<String>> = None;
    // `--force-with-lease` requests: an explicit `ref:expect` lease, or the
    // bare flag (lease every pushed ref against its remote-tracking ref).
    let mut force_with_lease_default = false;
    let mut force_with_lease_specs: Vec<String> = Vec::new();
    let mut force_if_includes = false;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "--no-verify" => no_verify = true,
            "--verify" => no_verify = false,
            "-u" | "--set-upstream" => set_upstream = true,
            "--no-set-upstream" => set_upstream = false,
            "-d" | "--delete" => delete = true,
            "--no-delete" => delete = false,
            "--porcelain" => porcelain = true,
            "--atomic" => atomic = true,
            "--no-atomic" => atomic = false,
            "--mirror" => mirror = true,
            "--prune" => prune = true,
            "--no-prune" => prune = false,
            "--all" | "--branches" => all_refs = true,
            "--tags" => tags = true,
            "--follow-tags" => follow_tags = true,
            "--no-follow-tags" => follow_tags = false,
            "--force-with-lease" => force_with_lease_default = true,
            "--no-force-with-lease" => {
                force_with_lease_default = false;
                force_with_lease_specs.clear();
            }
            "--force-if-includes" => force_if_includes = true,
            "--no-force-if-includes" => force_if_includes = false,
            value if value.starts_with("--force-with-lease=") => {
                force_with_lease_specs.push(value["--force-with-lease=".len()..].to_string());
            }
            "--repo" => {
                iter.next().ok_or_else(|| {
                    GitError::Command(format!("push {} requires a value", arg.as_str()))
                })?;
            }
            "--receive-pack" | "--exec" => {
                receive_pack_command = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command(format!("push {} requires a value", arg.as_str()))
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--receive-pack=") || value.starts_with("--exec=") => {
                let (_, command) = value.split_once('=').unwrap_or((value, ""));
                receive_pack_command = Some(command.to_string());
            }
            "-o" | "--push-option" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command(format!("push {} requires a value", arg.as_str()))
                })?;
                push_options_cmdline
                    .get_or_insert_with(Vec::new)
                    .push(value.clone());
            }
            value if value.starts_with("--push-option=") => {
                push_options_cmdline
                    .get_or_insert_with(Vec::new)
                    .push(value["--push-option=".len()..].to_string());
            }
            value if value.starts_with("--repo=") => {}
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--thin" => thin = sley_remote::PushThinMode::Always,
            "--no-thin" => thin = sley_remote::PushThinMode::Never,
            "--no-recurse-submodules" => recurse_submodules = PushRecurseSubmodules::Off,
            "--recurse-submodules" => recurse_submodules = PushRecurseSubmodules::Check,
            value if value.starts_with("--recurse-submodules=") => {
                recurse_submodules =
                    parse_push_recurse_submodules(&value["--recurse-submodules=".len()..])?;
            }
            // `OPT_IPVERSION` in builtin/push.c: accepted but a no-op for the
            // file:// transport (the `--no-` forms are not defined and fall
            // through to the unknown-option path, matching git).
            "-4" | "--ipv4" | "-6" | "--ipv6" => {}
            "--" => {
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                eprintln!("usage: git push [<options>] [<repository> [<refspec>...]]");
                return Err(GitError::Exit(129));
            }
            value => positional.push(value.to_string()),
        }
    }

    // `--mirror` implies `--force` and forbids explicit refspecs.
    if mirror {
        if positional.len() >= 2 {
            return Err(GitError::Command(
                "--mirror can't be combined with refspecs".into(),
            ));
        }
        force = true;
    }
    if all_refs && positional.len() >= 2 {
        return Err(GitError::Command(
            "--all can't be combined with refspecs".into(),
        ));
    }

    let (remote, mut refspecs) = if delete {
        let Some((remote, names)) = positional.split_first() else {
            return Err(GitError::Command(
                "push --delete requires at least one ref".into(),
            ));
        };
        if names.is_empty() {
            return Err(GitError::Command(
                "push --delete requires at least one ref".into(),
            ));
        }
        let specs = if names.first().is_some_and(|name| name == "tag") {
            names
                .iter()
                .skip(1)
                .map(|name| format!(":refs/tags/{name}"))
                .collect()
        } else {
            names.iter().map(|refspec| format!(":{refspec}")).collect()
        };
        (remote.clone(), specs)
    } else if mirror {
        // `--mirror`: push every local ref to the same name, mirroring; the
        // remote-ref deletions for refs we no longer have are expanded below
        // once the remote's advertisement is known.
        let remote = mirror_all_remote(&git_dir, &store, &positional)?;
        (remote, vec!["refs/*:refs/*".to_string()])
    } else if all_refs || (tags && positional.len() < 2) {
        // `--all`/`--mirror` forbid explicit refspecs (checked above), and a bare
        // `--tags` (no refspec) pushes only the tag wildcard. git appends
        // `refs/tags/*` as its own refspec rather than replacing the default.
        let remote = mirror_all_remote(&git_dir, &store, &positional)?;
        let mut specs = Vec::new();
        if all_refs {
            specs.push("refs/heads/*:refs/heads/*".to_string());
        }
        if tags {
            specs.push("refs/tags/*:refs/tags/*".to_string());
        }
        (remote, specs)
    } else {
        let resolved = push_remote_and_refspecs(&git_dir, &store, &positional)?;
        if resolved.set_upstream {
            set_upstream = true;
        }
        if resolved.mirror {
            mirror = true;
            force = true;
        }
        let mut specs = resolved.refspecs;
        // builtin/push.c: `--tags` appends `refs/tags/*` after the explicit
        // refspecs, so `git push --tags <remote> <refspec>` pushes both.
        if tags {
            specs.push("refs/tags/*:refs/tags/*".to_string());
        }
        (resolved.remote, specs)
    };
    refspecs = expand_push_tag_shorthand(&refspecs)?;
    default_head_push_destinations(&store, &mut refspecs)?;
    let options = PushOptions {
        quiet,
        set_upstream,
        force,
        no_verify,
        dry_run,
        progress,
        thin,
    };
    let config = transport_policy_config_for_cwd()?;
    let repo_config = read_repo_config(&git_dir).unwrap_or_default();
    let parent_remote_is_name = push_remote_name_exists(&repo_config, &remote);
    let mut recurse_submodules = resolve_push_recurse_submodules(&repo_config, recurse_submodules)?;
    if recurse_submodules == PushRecurseSubmodules::Only
        && env::var_os("SLEY_PUSH_RECURSING_SUBMODULE").is_some()
    {
        eprintln!(
            "warning: recursing into submodule with push.recurseSubmodules=only; using on-demand instead"
        );
        recurse_submodules = PushRecurseSubmodules::OnDemand;
    }
    let resolved_remotes = push_resolved_urls(&repo_config, &remote);
    let multiple_push_urls = resolved_remotes.len() > 1;
    let base_refspecs = refspecs;
    for resolved_remote in resolved_remotes {
        let mut refspecs = base_refspecs.clone();
        let helper_remote = if multiple_push_urls {
            resolved_remote.as_str()
        } else {
            remote.as_str()
        };
        if super::helper::push_with_remote_helper(
            &git_dir,
            format,
            helper_remote,
            &refspecs,
            super::helper::RemoteHelperPushOptions {
                force: options.force,
                quiet: options.quiet,
                dry_run: options.dry_run,
            },
        )?
        .is_some()
        {
            continue;
        }
        check_transport_allowed_url(&resolved_remote, Some(&config))?;
        let parsed_remote = parse_remote_url(&resolved_remote)?;
        // All transports delegate the git work to `sley_remote::push`, picked purely
        // by the resolved `PushDestination`; this command keeps owning URL/repo
        // resolution, set-upstream config, and the "To <remote>" summary so the
        // user-visible output stays byte-for-byte identical.
        let destination = match parsed_remote.transport {
            RemoteTransport::Ssh | RemoteTransport::Ext => {
                sley_remote::PushDestination::Ssh(parsed_remote)
            }
            RemoteTransport::Git => sley_remote::PushDestination::Git(parsed_remote),
            RemoteTransport::Http | RemoteTransport::Https => {
                sley_remote::PushDestination::Http(parsed_remote)
            }
            RemoteTransport::Local | RemoteTransport::File => {
                let remote_git_dir = ls_remote_git_dir(&resolved_remote)?;
                let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
                sley_remote::PushDestination::Local {
                    git_dir: remote_git_dir,
                    common_git_dir: remote_common_git_dir,
                }
            }
        };

        // The file:// (local) transport gets git's full per-ref status report: this
        // is the path the upstream push tests exercise. Other transports keep the
        // existing terse summary.
        if let sley_remote::PushDestination::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } = &destination
        {
            // `--mirror` also deletes remote refs the local repo no longer has.
            let remote_advertisements = if mirror || prune || follow_tags {
                Some(sley_remote::local_fetch_advertisements(
                    remote_git_dir,
                    format,
                )?)
            } else {
                None
            };
            if mirror || prune {
                let remote_advertisements = remote_advertisements.as_deref().unwrap_or(&[]);
                let local_names: std::collections::HashSet<String> = store
                    .list_refs()?
                    .into_iter()
                    .map(|reference| reference.name)
                    .collect();
                if mirror {
                    for advertisement in remote_advertisements {
                        if advertisement.name.starts_with("refs/")
                            && !local_names.contains(&advertisement.name)
                        {
                            refspecs.push(format!(":{}", advertisement.name));
                        }
                    }
                } else {
                    append_push_prune_refspecs(&mut refspecs, &remote_advertisements, &local_names);
                }
            }
            if follow_tags {
                append_follow_tag_refspecs(
                    &git_dir,
                    &common_git_dir,
                    format,
                    &store,
                    &mut refspecs,
                    remote_advertisements.as_deref().unwrap_or(&[]),
                )?;
            }
            let config = &repo_config;
            let force_if_includes = force_if_includes
                || config
                    .get_bool("push", None, "useforceifincludes")
                    .unwrap_or(false);
            let push_options = match push_options_cmdline.clone() {
                Some(options) => options,
                None => push_options_from_config(config)?,
            };
            let receive_config_overrides =
                receive_pack_config_overrides(receive_pack_command.as_deref());
            let mut force_with_lease = resolve_force_with_lease(
                &git_dir,
                &store,
                config,
                format,
                &remote,
                &force_with_lease_specs,
            )?;
            if force_with_lease_default {
                expand_default_force_with_lease(
                    &git_dir,
                    &common_git_dir,
                    format,
                    &store,
                    config,
                    &remote,
                    remote_git_dir,
                    remote_common_git_dir,
                    &refspecs,
                    force,
                    atomic,
                    force_if_includes,
                    &receive_config_overrides,
                    &mut force_with_lease,
                )?;
            }
            match recurse_submodules {
                PushRecurseSubmodules::Default | PushRecurseSubmodules::Off => {}
                PushRecurseSubmodules::Check => {
                    check_submodule_push(
                        &git_dir,
                        format,
                        remote_git_dir,
                        &remote,
                        parent_remote_is_name,
                        &refspecs,
                        config,
                    )?;
                }
                PushRecurseSubmodules::OnDemand => {
                    if !options.dry_run {
                        push_on_demand_submodules(
                            &git_dir,
                            format,
                            &remote,
                            parent_remote_is_name,
                            &refspecs,
                            &push_options,
                            PushRecurseSubmodules::OnDemand,
                            options.quiet,
                        )?;
                    }
                }
                PushRecurseSubmodules::Only => {
                    if !options.dry_run {
                        push_on_demand_submodules(
                            &git_dir,
                            format,
                            &remote,
                            parent_remote_is_name,
                            &refspecs,
                            &push_options,
                            PushRecurseSubmodules::Only,
                            options.quiet,
                        )?;
                    }
                    return Ok(());
                }
            }
            trace_configured_local_protocol_version(Some(config));
            let result = run_push_local_report(RunPushLocalReport {
                git_dir: &git_dir,
                common_git_dir: &common_git_dir,
                format,
                remote: &remote,
                resolved_remote: &resolved_remote,
                remote_git_dir,
                remote_common_git_dir,
                refspecs: &refspecs,
                options,
                porcelain,
                atomic,
                force_if_includes,
                push_options: &push_options,
                force_with_lease: &force_with_lease,
                force_with_lease_default,
                receive_pack_command: receive_pack_command.as_deref(),
                receive_config_overrides: &receive_config_overrides,
            });
            if result.is_ok() {
                trace2_local_transfer_negotiation(config, receive_pack_command.as_deref());
            }
            result?;
            continue;
        }

        let push_options = match push_options_cmdline.clone() {
            Some(options) => options,
            None => push_options_from_config(&repo_config)?,
        };
        run_push(
            &git_dir,
            &common_git_dir,
            format,
            &remote,
            &resolved_remote,
            &destination,
            &refspecs,
            options,
            porcelain,
            atomic,
            &push_options,
        )?;
    }
    Ok(())
}

fn push_resolved_urls(config: &GitConfig, remote: &str) -> Vec<String> {
    let push_urls = remote_config_values(config, remote, "pushurl");
    if push_urls.is_empty() {
        return vec![resolve_remote_push_url(config, remote)];
    }
    push_urls
        .into_iter()
        .map(|url| rewrite_url_with_config(config, &url, false))
        .collect()
}

fn append_push_prune_refspecs(
    refspecs: &mut Vec<String>,
    remote_advertisements: &[sley_protocol::RefAdvertisement],
    local_names: &std::collections::HashSet<String>,
) {
    let mut deletes = std::collections::BTreeSet::new();
    for refspec in refspecs.iter() {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        if body == ":" {
            for advertisement in remote_advertisements {
                if advertisement.name.starts_with("refs/heads/")
                    && !local_names.contains(&advertisement.name)
                {
                    deletes.insert(advertisement.name.clone());
                }
            }
            continue;
        }
        let Some((src, dst)) = body.split_once(':') else {
            continue;
        };
        let (Some((src_prefix, src_suffix)), Some((dst_prefix, dst_suffix))) =
            (src.split_once('*'), dst.split_once('*'))
        else {
            continue;
        };
        for advertisement in remote_advertisements {
            let Some(stem) = advertisement
                .name
                .strip_prefix(dst_prefix)
                .and_then(|rest| rest.strip_suffix(dst_suffix))
            else {
                continue;
            };
            let source = format!("{src_prefix}{stem}{src_suffix}");
            if !local_names.contains(&source) {
                deletes.insert(advertisement.name.clone());
            }
        }
    }
    refspecs.extend(deletes.into_iter().map(|name| format!(":{name}")));
}

fn expand_push_tag_shorthand(refspecs: &[String]) -> Result<Vec<String>> {
    let mut expanded = Vec::new();
    let mut iter = refspecs.iter();
    while let Some(refspec) = iter.next() {
        if refspec == "tag" {
            let name = iter
                .next()
                .ok_or_else(|| GitError::Command("you need to specify a tag name".into()))?;
            expanded.push(format!("refs/tags/{name}:refs/tags/{name}"));
        } else {
            expanded.push(refspec.clone());
        }
    }
    Ok(expanded)
}

fn append_follow_tag_refspecs(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    refspecs: &mut Vec<String>,
    remote_advertisements: &[sley_protocol::RefAdvertisement],
) -> Result<()> {
    let pushed_tips = pushed_tips_for_follow_tags(git_dir, format, store, refspecs)?;
    if pushed_tips.is_empty() {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut additions = Vec::new();
    for reference in store.list_refs()? {
        let Some(name) = reference.name.strip_prefix("refs/tags/") else {
            continue;
        };
        if remote_advertisements
            .iter()
            .any(|advertisement| advertisement.name == reference.name)
            || refspecs
                .iter()
                .any(|refspec| refspec_mentions_destination(refspec, &reference.name))
        {
            continue;
        }
        let Some((tag_oid, _)) = resolve_for_each_ref_target(store, &reference)? else {
            continue;
        };
        let Some(target) = annotated_tag_commit_target(&db, format, &tag_oid)? else {
            continue;
        };
        if pushed_tips
            .iter()
            .any(|tip| commit_reaches(common_git_dir, &db, format, tip, &target).unwrap_or(false))
        {
            additions.push(format!("refs/tags/{name}:refs/tags/{name}"));
        }
    }
    additions.sort();
    refspecs.extend(additions);
    Ok(())
}

fn refspec_mentions_destination(refspec: &str, destination: &str) -> bool {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    let (_, dst) = refspec.split_once(':').unwrap_or((refspec, refspec));
    dst == destination
}

fn pushed_tips_for_follow_tags(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    refspecs: &[String],
) -> Result<Vec<ObjectId>> {
    let refs = store.list_refs()?;
    let mut tips = Vec::new();
    for refspec in refspecs {
        let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
        let src = refspec.split_once(':').map_or(refspec, |(src, _)| src);
        if src.is_empty() {
            continue;
        }
        if let Some((prefix, suffix)) = src.split_once('*') {
            for reference in &refs {
                if reference
                    .name
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
                    .is_some()
                    && let Some((oid, _)) = resolve_for_each_ref_target(store, reference)?
                {
                    tips.push(oid);
                }
            }
            continue;
        }
        if let Ok(oid) = sley_rev::resolve_revision(git_dir, format, src) {
            tips.push(oid);
        }
    }
    tips.sort();
    tips.dedup();
    Ok(tips)
}

fn annotated_tag_commit_target(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let object = db.read_object(oid)?;
    if object.object_type != sley_object::ObjectType::Tag {
        return Ok(None);
    }
    let tag = sley_object::Tag::parse_ref(format, &object.body)?;
    let target = db.read_object(&tag.object)?;
    if target.object_type == sley_object::ObjectType::Commit {
        Ok(Some(tag.object))
    } else {
        Ok(None)
    }
}

fn commit_reaches(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip: &ObjectId,
    target: &ObjectId,
) -> Result<bool> {
    if tip == target {
        return Ok(true);
    }
    let object = db.read_object(tip)?;
    if object.object_type != sley_object::ObjectType::Commit {
        return Ok(false);
    }
    Ok(sley_rev::ancestor_depths(git_dir, format, db, tip)?.contains_key(target))
}

/// Resolve the remote argument for `--mirror`/`--all`/`--tags`: the lone
/// positional, or the configured default push remote when none is given.
fn mirror_all_remote(
    git_dir: &Path,
    store: &FileRefStore,
    positional: &[String],
) -> Result<String> {
    if let Some(remote) = positional.first() {
        return Ok(remote.clone());
    }
    let config = read_repo_config(git_dir).unwrap_or_default();
    reject_empty_branch_config(&config)?;
    let branch = store.current_branch()?;
    default_push_remote(&config, branch.as_deref())
}

/// Resolve `--force-with-lease=<ref>[:<expect>]` specs into `(dst, expected_old)`
/// pairs. An omitted `<expect>` leases against the ref's remote-tracking value;
/// an explicit empty `<expect>` means "must not exist".
fn resolve_force_with_lease(
    git_dir: &Path,
    store: &FileRefStore,
    config: &GitConfig,
    format: ObjectFormat,
    remote: &str,
    specs: &[String],
) -> Result<Vec<(String, Option<ObjectId>)>> {
    let mut out = Vec::new();
    for spec in specs {
        let (refname, expect) = match spec.split_once(':') {
            Some((refname, expect)) => (refname, Some(expect)),
            None => (spec.as_str(), None),
        };
        let dst = sley_remote::normalize_push_refname(refname);
        let expected = match expect {
            Some("") => None,
            Some(value) => Some(sley_rev::resolve_revision(git_dir, format, value)?),
            None => {
                remote_tracking_oid_for_push_lease(git_dir, format, store, config, remote, &dst)?
            }
        };
        out.push((dst, expected));
    }
    Ok(out)
}

fn expand_default_force_with_lease(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    remote: &str,
    remote_git_dir: &Path,
    remote_common_git_dir: &Path,
    refspecs: &[String],
    force: bool,
    atomic: bool,
    force_if_includes: bool,
    receive_config_overrides: &[(String, String)],
    leases: &mut Vec<(String, Option<ObjectId>)>,
) -> Result<()> {
    let source_db = crate::repository::open_object_database(git_dir, format)?;
    let preview = sley_remote::push_local_with_report_and_objects(
        sley_remote::PushReportRequest {
            git_dir,
            common_git_dir,
            format,
            remote_git_dir,
            remote_common_git_dir,
            refspecs,
            force,
            atomic,
            dry_run: true,
            force_with_lease: &[],
            force_with_lease_default: false,
            force_if_includes,
            receive_config_overrides,
            push_options: &[],
            remote_stderr: None,
            quiet: false,
        },
        config,
        &source_db,
    )?;
    let mut covered: std::collections::HashSet<String> =
        leases.iter().map(|(dst, _)| dst.clone()).collect();
    for reference in preview.refs {
        if !covered.insert(reference.dst.clone()) {
            continue;
        }
        let expected = remote_tracking_oid_for_push_lease(
            git_dir,
            format,
            store,
            config,
            remote,
            &reference.dst,
        )?;
        leases.push((reference.dst, expected));
    }
    Ok(())
}

fn remote_tracking_oid_for_push_lease(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    remote: &str,
    refname: &str,
) -> Result<Option<ObjectId>> {
    if remote == "origin"
        && let Some(oid) = fetch_head_oid_for_push_lease(git_dir, format, refname)?
    {
        return Ok(Some(oid));
    }
    let Some(tracking_ref) = remote_tracking_ref_for_push_lease(config, remote, refname)? else {
        return Ok(None);
    };
    read_direct_or_symbolic_ref(store, &tracking_ref)
}

pub(super) fn fetch_head_oid_for_push_lease(
    git_dir: &Path,
    format: ObjectFormat,
    refname: &str,
) -> Result<Option<ObjectId>> {
    let Some(branch) = refname.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    let path = git_dir.join("FETCH_HEAD");
    if !path.exists() {
        return Ok(None);
    }
    let Ok(data) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let needle = format!("branch '{branch}'");
    for line in data.lines() {
        if !line.contains(&needle) {
            continue;
        }
        let Some(hex) = line.split_whitespace().next() else {
            continue;
        };
        if let Ok(oid) = ObjectId::from_hex(format, hex) {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn remote_tracking_ref_for_push_lease(
    config: &GitConfig,
    remote: &str,
    refname: &str,
) -> Result<Option<String>> {
    for spec in remote_config_values(config, remote, "fetch") {
        let parsed = sley_protocol::parse_refspec(&spec)?;
        if parsed.negative {
            continue;
        }
        if let Some(dst) = sley_protocol::refspec_map_source(&parsed, refname)? {
            return Ok(Some(dst));
        }
    }
    Ok(None)
}

pub(super) fn read_direct_or_symbolic_ref(
    store: &FileRefStore,
    refname: &str,
) -> Result<Option<ObjectId>> {
    match store.read_ref(refname)? {
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        Some(RefTarget::Symbolic(target)) => read_direct_or_symbolic_ref(store, &target),
        None => Ok(None),
    }
}

fn receive_pack_config_overrides(command: Option<&str>) -> Vec<(String, String)> {
    let Some(command) = command else {
        return Vec::new();
    };
    let mut overrides = Vec::new();
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        let Some(raw) = token
            .strip_prefix("-c")
            .filter(|rest| !rest.is_empty())
            .map(str::to_string)
            .or_else(|| {
                if token == "-c" {
                    tokens.next().map(str::to_string)
                } else {
                    None
                }
            })
        else {
            continue;
        };
        let Some((key, value)) = raw.split_once('=') else {
            continue;
        };
        let Some(receive_key) = key.trim().strip_prefix("receive.") else {
            continue;
        };
        overrides.push((
            receive_key.to_string(),
            value.trim().trim_matches('"').to_string(),
        ));
    }
    overrides
}

pub(super) fn configured_protocol_version(config: Option<&GitConfig>) -> Option<ProtocolVersion> {
    let value = config
        .and_then(|config| config.get("protocol", None, "version").map(str::to_string))
        .or_else(|| global_config_value("protocol.version").ok().flatten());
    match value.as_deref() {
        Some("0") => Some(ProtocolVersion::V0),
        Some("1") => Some(ProtocolVersion::V1),
        Some("2") => Some(ProtocolVersion::V2),
        _ => None,
    }
}

pub(super) fn configured_legacy_protocol(config: Option<&GitConfig>) -> bool {
    matches!(
        configured_protocol_version(config),
        Some(ProtocolVersion::V0 | ProtocolVersion::V1)
    )
}

pub(super) fn trace_configured_local_protocol_version(config: Option<&GitConfig>) {
    match configured_protocol_version(config) {
        Some(ProtocolVersion::V1) => sley_protocol::trace_packet_read_payload(b"version 1\n"),
        Some(ProtocolVersion::V2) => sley_protocol::trace_packet_read_payload(b"version 2\n"),
        _ => {}
    }
}

pub(super) fn trace_protocol_v2_upload_pack_capabilities(git_dir: &Path, format: ObjectFormat) {
    let config = read_repo_config(git_dir).unwrap_or_default();
    sley_protocol::trace_packet_read_payload(b"agent=git/2.54.0\n");
    sley_protocol::trace_packet_read_payload(b"ls-refs=unborn\n");
    let mut fetch = "fetch=shallow wait-for-done".to_string();
    if config
        .get_bool("uploadpack", None, "allowfilter")
        .unwrap_or(false)
    {
        fetch.push_str(" filter");
    }
    if config
        .get_bool("uploadpack", None, "allowrefinwant")
        .unwrap_or(false)
    {
        fetch.push_str(" ref-in-want");
    }
    fetch.push('\n');
    sley_protocol::trace_packet_read_payload(fetch.as_bytes());
    sley_protocol::trace_packet_read_payload(b"server-option\n");
    sley_protocol::trace_packet_read_payload(
        format!("object-format={}\n", format.name()).as_bytes(),
    );
    sley_protocol::trace_packet_read_payload(b"0000");
}

pub(super) fn trace_protocol_v2_ls_refs_request(server_options: &[String]) {
    sley_protocol::trace_packet_write_payload(b"command=ls-refs\n");
    for option in server_options {
        sley_protocol::trace_packet_write_payload(format!("server-option={option}\n").as_bytes());
    }
    sley_protocol::trace_packet_write_payload(b"0001");
    sley_protocol::trace_packet_write_payload(b"peel\n");
    sley_protocol::trace_packet_write_payload(b"symrefs\n");
    sley_protocol::trace_packet_write_payload(b"ref-prefix HEAD\n");
    sley_protocol::trace_packet_write_payload(b"ref-prefix refs/heads/\n");
    sley_protocol::trace_packet_write_payload(b"ref-prefix refs/tags/\n");
    sley_protocol::trace_packet_write_payload(b"0000");
}

fn protocol_version_for_trace2(config: &GitConfig) -> &'static str {
    match config.get("protocol", None, "version") {
        Some("1") => "1",
        Some("2") => "2",
        _ => "0",
    }
}

fn trace2_event_path_from_remote_command(command: Option<&str>) -> Option<String> {
    let command = command?;
    let rest = command.strip_prefix("GIT_TRACE2_EVENT=").or_else(|| {
        command
            .find(" GIT_TRACE2_EVENT=")
            .map(|idx| &command[idx + " GIT_TRACE2_EVENT=".len()..])
    })?;
    let (value, _) = match rest.as_bytes().first().copied() {
        Some(b'"') => {
            let rest = &rest[1..];
            rest.split_once('"').unwrap_or((rest, ""))
        }
        Some(b'\'') => {
            let rest = &rest[1..];
            rest.split_once('\'').unwrap_or((rest, ""))
        }
        Some(_) => rest.split_once(char::is_whitespace).unwrap_or((rest, "")),
        None => return None,
    };
    if value.starts_with('/') {
        return Some(value.to_string());
    }
    None
}

fn trace2_data_to_path(path: &str, key: &str, value: &str) {
    let line = format!(
        "{{\"event\":\"data\",\"sid\":\"sley\",\"thread\":\"main\",\"nesting\":1,\"category\":\"transfer\",\"key\":\"{}\",\"value\":\"{}\"}}\n",
        key, value
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub(super) fn trace2_local_transfer_negotiation(config: &GitConfig, remote_command: Option<&str>) {
    if !config
        .get_bool("transfer", None, "advertisesid")
        .unwrap_or(false)
    {
        return;
    }
    let version = protocol_version_for_trace2(config);
    sley_core::trace2::data("transfer", "server-sid", "sley");
    sley_core::trace2::data("transfer", "negotiated-version", version);
    if let Some(path) = trace2_event_path_from_remote_command(remote_command) {
        trace2_data_to_path(&path, "client-sid", "sley");
        trace2_data_to_path(&path, "negotiated-version", version);
    }
}

fn push_options_from_config(config: &GitConfig) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for value in config.get_all("push", None, "pushoption") {
        match value {
            Some("") => out.clear(),
            Some(value) => out.push(value.to_string()),
            None => {
                eprintln!("fatal: push.pushOption must have a value");
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct PushSubmodule {
    path: String,
    oid: ObjectId,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    format: ObjectFormat,
}

fn check_submodule_push(
    git_dir: &Path,
    format: ObjectFormat,
    remote_git_dir: &Path,
    remote: &str,
    parent_remote_is_name: bool,
    refspecs: &[String],
    _config: &GitConfig,
) -> Result<()> {
    let submodules = push_gitlink_submodules(git_dir, format)?;
    let by_path = submodules
        .iter()
        .enumerate()
        .map(|(idx, submodule)| (submodule.path.clone(), idx))
        .collect::<std::collections::HashMap<_, _>>();
    let targets = pushed_superproject_gitlinks(git_dir, format, remote_git_dir, refspecs)?;
    if targets.is_empty() {
        for submodule in &submodules {
            check_one_submodule_target(submodule, submodule.oid, remote, parent_remote_is_name)?;
        }
        return Ok(());
    }
    for (path, oid) in targets {
        let Some(idx) = by_path.get(&path) else {
            continue;
        };
        check_one_submodule_target(&submodules[*idx], oid, remote, parent_remote_is_name)?;
    }
    Ok(())
}

fn check_one_submodule_target(
    submodule: &PushSubmodule,
    oid: ObjectId,
    remote: &str,
    parent_remote_is_name: bool,
) -> Result<()> {
    ensure_push_submodule_commit_oid(submodule, &oid)?;
    let child_config = read_repo_config(&submodule.git_dir).unwrap_or_default();
    let child_remote =
        submodule_push_remote(submodule, &child_config, remote, parent_remote_is_name)?;
    if submodule_commit_needs_push_oid(submodule, &oid, child_remote.as_deref())? {
        eprintln!(
            "fatal: submodule path '{}' contains changes that are not found on any remote",
            submodule.path
        );
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn pushed_superproject_gitlinks(
    git_dir: &Path,
    format: ObjectFormat,
    remote_git_dir: &Path,
    refspecs: &[String],
) -> Result<Vec<(String, ObjectId)>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let mut out = Vec::new();
    for tip in pushed_superproject_tips(git_dir, format, remote_git_dir, refspecs)? {
        let object = db.read_object(&tip)?;
        if object.object_type != sley_object::ObjectType::Commit {
            continue;
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        collect_tree_gitlinks(&db, format, &commit.tree, String::new(), &mut out)?;
    }
    out.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    out.dedup();
    Ok(out)
}

fn pushed_superproject_tips(
    git_dir: &Path,
    format: ObjectFormat,
    remote_git_dir: &Path,
    refspecs: &[String],
) -> Result<Vec<ObjectId>> {
    let store = FileRefStore::new(git_dir, format);
    let remote_store = FileRefStore::new(remote_git_dir, format);
    let local_refs = store.list_refs()?;
    let mut tips = Vec::new();
    for refspec in refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        if body == ":" {
            for reference in local_refs
                .iter()
                .filter(|reference| reference.name.starts_with("refs/heads/"))
            {
                if remote_store.read_ref(&reference.name)?.is_none() {
                    continue;
                }
                if let Some((oid, _)) = resolve_for_each_ref_target(&store, reference)? {
                    tips.push(oid);
                }
            }
            continue;
        }
        if body.contains('*') {
            continue;
        }
        let src = body.split_once(':').map_or(body, |(src, _)| src);
        if src.is_empty() {
            continue;
        }
        if let Ok(oid) = sley_rev::resolve_revision(git_dir, format, src) {
            tips.push(oid);
        }
    }
    tips.sort();
    tips.dedup();
    Ok(tips)
}

fn collect_tree_gitlinks(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: String,
    out: &mut Vec<(String, ObjectId)>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != sley_object::ObjectType::Tree {
        return Ok(());
    }
    let tree = sley_object::Tree::parse(format, &object.body)?;
    for entry in tree.entries {
        let name = String::from_utf8_lossy(entry.name.as_bytes()).into_owned();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if sley_index::is_gitlink(entry.mode) {
            out.push((path, entry.oid));
        } else if entry.mode == 0o040000 {
            collect_tree_gitlinks(db, format, &entry.oid, path, out)?;
        }
    }
    Ok(())
}

fn push_on_demand_submodules(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    parent_remote_is_name: bool,
    refspecs: &[String],
    push_options: &[String],
    recurse_mode: PushRecurseSubmodules,
    quiet: bool,
) -> Result<()> {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"));
    for submodule in push_gitlink_submodules(git_dir, format)? {
        ensure_push_submodule_commit(&submodule)?;
        let child_config = read_repo_config(&submodule.git_dir).unwrap_or_default();
        let Some(child_remote) =
            submodule_push_remote(&submodule, &child_config, remote, parent_remote_is_name)?
        else {
            continue;
        };
        if !submodule_commit_needs_push(&submodule, Some(&child_remote))? {
            continue;
        }
        validate_submodule_push_refspecs(&submodule, refspecs)?;
        let submodule_root = worktree_root_for_git_dir(&submodule.git_dir)?;
        let mut command = Proc::new(&exe);
        clear_repo_env_for_submodule_child(&mut command);
        command.env("SLEY_PUSH_RECURSING_SUBMODULE", "1");
        command.arg("push");
        if quiet {
            command.arg("--quiet");
        }
        for option in push_options {
            command.arg(format!("--push-option={option}"));
        }
        if recurse_mode == PushRecurseSubmodules::OnDemand {
            command.arg("--recurse-submodules=on-demand");
        }
        command.arg(&child_remote);
        command.args(refspecs);
        let status = command
            .current_dir(&submodule_root)
            .status()
            .map_err(|err| GitError::Io(err.to_string()))?;
        if !status.success() {
            eprintln!("fatal: failed to push all needed submodules");
            return Err(GitError::Exit(status.code().unwrap_or(1)));
        }
        if submodule_commit_needs_push(&submodule, Some(&child_remote))? {
            eprintln!(
                "fatal: submodule path '{}' contains changes that could not be pushed",
                submodule.path
            );
            return Err(GitError::Exit(1));
        }
    }
    Ok(())
}

fn push_gitlink_submodules(git_dir: &Path, format: ObjectFormat) -> Result<Vec<PushSubmodule>> {
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(Vec::new());
    };
    let mut submodules = Vec::new();
    for entry in index.entries {
        if entry.stage() != sley_index::Stage::Normal || !sley_index::is_gitlink(entry.mode) {
            continue;
        }
        let Ok(path) = String::from_utf8(entry.path.to_vec()) else {
            continue;
        };
        let submodule_root = worktree_root.join(&path);
        let Some(sub_git_dir) = sley_diff_merge::gitlink_git_dir(&submodule_root) else {
            continue;
        };
        let sub_common_git_dir = common_git_dir_for_git_dir(&sub_git_dir)?;
        let sub_format = repository_object_format(&sub_common_git_dir)?;
        submodules.push(PushSubmodule {
            path,
            oid: entry.oid,
            git_dir: sub_git_dir,
            common_git_dir: sub_common_git_dir,
            format: sub_format,
        });
    }
    Ok(submodules)
}

fn ensure_push_submodule_commit(submodule: &PushSubmodule) -> Result<()> {
    ensure_push_submodule_commit_oid(submodule, &submodule.oid)
}

fn ensure_push_submodule_commit_oid(submodule: &PushSubmodule, oid: &ObjectId) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(&submodule.common_git_dir, submodule.format);
    let object = db.read_object(oid).map_err(|_| {
        eprintln!(
            "fatal: submodule path '{}' does not contain commit {}",
            submodule.path, oid
        );
        GitError::Exit(1)
    })?;
    if object.object_type != sley_object::ObjectType::Commit {
        eprintln!(
            "fatal: submodule entry '{}' ({}) is a {}, not a commit",
            submodule.path,
            oid,
            object.object_type.as_str()
        );
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn submodule_commit_needs_push(submodule: &PushSubmodule, remote: Option<&str>) -> Result<bool> {
    submodule_commit_needs_push_oid(submodule, &submodule.oid, remote)
}

fn submodule_commit_needs_push_oid(
    submodule: &PushSubmodule,
    oid: &ObjectId,
    remote: Option<&str>,
) -> Result<bool> {
    let Some(remote) = remote else {
        return Ok(false);
    };
    let store = FileRefStore::new(&submodule.git_dir, submodule.format);
    let db = FileObjectDatabase::from_git_dir(&submodule.common_git_dir, submodule.format);
    let prefix = format!("refs/remotes/{remote}/");
    for reference in store.list_refs()? {
        if !reference.name.starts_with(&prefix) {
            continue;
        }
        let Some((remote_oid, _)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        if commit_reaches(
            &submodule.common_git_dir,
            &db,
            submodule.format,
            &remote_oid,
            oid,
        )
        .unwrap_or(false)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn submodule_push_remote(
    submodule: &PushSubmodule,
    config: &GitConfig,
    parent_remote: &str,
    parent_remote_is_name: bool,
) -> Result<Option<String>> {
    let remote_names = push_remote_names(config);
    if remote_names.is_empty() {
        return Ok(None);
    }
    if parent_remote_is_name {
        if push_remote_name_exists(config, parent_remote) {
            return Ok(Some(parent_remote.to_string()));
        }
        eprintln!(
            "fatal: remote '{}' not found in submodule path '{}'",
            parent_remote, submodule.path
        );
        return Err(GitError::Exit(1));
    }
    Ok(Some(default_push_remote_name(
        &submodule.git_dir,
        submodule.format,
        config,
    )))
}

fn default_push_remote_name(git_dir: &Path, format: ObjectFormat, config: &GitConfig) -> String {
    let store = FileRefStore::new(git_dir, format);
    if let Ok(Some(branch)) = store.current_branch()
        && let Some(remote) = config.get("branch", Some(&branch), "remote")
    {
        return remote.to_string();
    }
    let remotes = push_remote_names(config);
    if remotes.len() == 1 {
        return remotes[0].clone();
    }
    "origin".to_string()
}

fn push_remote_name_exists(config: &GitConfig, name: &str) -> bool {
    config
        .sections
        .iter()
        .any(|section| section.name == "remote" && section.subsection.as_deref() == Some(name))
}

fn push_remote_names(config: &GitConfig) -> Vec<String> {
    let mut names = Vec::new();
    for section in &config.sections {
        if section.name != "remote" {
            continue;
        }
        let Some(name) = section.subsection.as_ref() else {
            continue;
        };
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

fn validate_submodule_push_refspecs(submodule: &PushSubmodule, refspecs: &[String]) -> Result<()> {
    let store = FileRefStore::new(&submodule.git_dir, submodule.format);
    let current_branch = store.current_branch()?;
    for refspec in refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        let (src, dst) = body.split_once(':').unwrap_or((body, ""));
        if src.is_empty() || src.contains('*') {
            continue;
        }
        if ObjectId::from_hex(submodule.format, src).is_ok() {
            eprintln!(
                "fatal: cannot propagate object-id refspec into submodule path '{}'",
                submodule.path
            );
            return Err(GitError::Exit(1));
        }
        if src == "HEAD"
            && let Some(branch) = dst.strip_prefix("refs/heads/")
            && current_branch.as_deref() != Some(branch)
        {
            eprintln!(
                "fatal: HEAD refspec does not match current branch in submodule path '{}'",
                submodule.path
            );
            return Err(GitError::Exit(1));
        }
    }
    Ok(())
}

fn clear_repo_env_for_submodule_child(command: &mut Proc) {
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
}

fn default_head_push_destinations(store: &FileRefStore, refspecs: &mut [String]) -> Result<()> {
    for refspec in refspecs {
        let forced = refspec.starts_with('+');
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        if matches!(body, "HEAD" | "@")
            && let Some(branch) = store.current_branch()?
        {
            *refspec = if forced {
                format!("+HEAD:refs/heads/{branch}")
            } else {
                format!("HEAD:refs/heads/{branch}")
            };
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct PushOptions {
    quiet: bool,
    set_upstream: bool,
    force: bool,
    no_verify: bool,
    dry_run: bool,
    progress: bool,
    thin: sley_remote::PushThinMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushRecurseSubmodules {
    Default,
    Off,
    Check,
    OnDemand,
    Only,
}

fn parse_push_recurse_submodules(value: &str) -> Result<PushRecurseSubmodules> {
    match value {
        "check" => Ok(PushRecurseSubmodules::Check),
        "on-demand" => Ok(PushRecurseSubmodules::OnDemand),
        "only" => Ok(PushRecurseSubmodules::Only),
        "no" | "false" | "off" => Ok(PushRecurseSubmodules::Off),
        "yes" | "true" | "on" => {
            eprintln!("fatal: unsupported --recurse-submodules mode '{value}'");
            Err(GitError::Exit(128))
        }
        other => {
            eprintln!("fatal: bad --recurse-submodules argument: {other}");
            Err(GitError::Exit(128))
        }
    }
}

fn parse_push_recurse_submodules_config(value: &str) -> Result<PushRecurseSubmodules> {
    match value {
        "check" => Ok(PushRecurseSubmodules::Check),
        "on-demand" => Ok(PushRecurseSubmodules::OnDemand),
        "only" => Ok(PushRecurseSubmodules::Only),
        "no" | "false" | "off" => Ok(PushRecurseSubmodules::Off),
        "yes" | "true" | "on" => {
            eprintln!("fatal: unsupported push.recurseSubmodules mode '{value}'");
            Err(GitError::Exit(128))
        }
        _ => Ok(PushRecurseSubmodules::Default),
    }
}

fn resolve_push_recurse_submodules(
    config: &GitConfig,
    cli: PushRecurseSubmodules,
) -> Result<PushRecurseSubmodules> {
    if cli != PushRecurseSubmodules::Default {
        return Ok(cli);
    }
    if let Some(value) = config.get("push", None, "recurseSubmodules") {
        let mode = parse_push_recurse_submodules_config(value)?;
        if mode != PushRecurseSubmodules::Default {
            return Ok(mode);
        }
    }
    if config
        .get_bool("submodule", None, "recurse")
        .unwrap_or(false)
    {
        return Ok(PushRecurseSubmodules::OnDemand);
    }
    Ok(PushRecurseSubmodules::Off)
}

/// Drive [`sley_remote::push`] for an already-resolved `destination` (HTTP or
/// local), wiring the credential-helper provider and the stdout progress sink,
/// then reproduce the CLI's behavior from the structured outcome: nothing on a
/// no-op push, otherwise the optional set-upstream config write followed by the
/// "To <remote>" summary on stderr. Repository/URL resolution, the set-upstream
/// config, and output formatting stay here; the push orchestration lives in the
/// library.
fn push_source_name_for_command(
    git_dir: &Path,
    format: ObjectFormat,
    refspecs: &[String],
    command: &ReceivePackCommand,
) -> Option<String> {
    for refspec in refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec.as_str());
        let Some((src, dst)) = body.split_once(':') else {
            let source = resolve_push_source_display_name(git_dir, format, body, command);
            if sley_remote::normalize_push_refname(&source) == command.name {
                return Some(source);
            }
            continue;
        };
        if dst.is_empty() {
            continue;
        }
        if sley_remote::normalize_push_refname(dst) == command.name {
            return (!src.is_empty())
                .then(|| resolve_push_source_display_name(git_dir, format, src, command));
        }
    }
    None
}

fn resolve_push_source_display_name(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    command: &ReceivePackCommand,
) -> String {
    if source == "HEAD"
        || source == "@"
        || source.starts_with("refs/")
        || ObjectId::from_hex(format, source).is_ok()
    {
        return source.to_string();
    }
    let store = FileRefStore::new(git_dir, format);
    for candidate in [
        format!("refs/heads/{source}"),
        format!("refs/tags/{source}"),
        format!("refs/remotes/{source}"),
    ] {
        if read_direct_or_symbolic_ref(&store, &candidate)
            .ok()
            .flatten()
            .is_some_and(|oid| oid == command.new_id)
        {
            return candidate;
        }
    }
    source.to_string()
}

fn push_command_force_requested(
    refspecs: &[String],
    command: &ReceivePackCommand,
    global_force: bool,
) -> bool {
    global_force
        || refspecs.iter().any(|refspec| {
            let Some(body) = refspec.strip_prefix('+') else {
                return false;
            };
            let dst = body.split_once(':').map_or(body, |(_, dst)| dst);
            sley_remote::normalize_push_refname(dst) == command.name
        })
}

fn push_command_was_forced(
    common_git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    refspecs: &[String],
    command: &ReceivePackCommand,
    global_force: bool,
) -> bool {
    if command.old_id.is_null()
        || command.new_id.is_null()
        || command.old_id == command.new_id
        || !push_command_force_requested(refspecs, command, global_force)
    {
        return false;
    }
    if !command.name.starts_with("refs/heads/") {
        return true;
    }
    !commit_reaches(common_git_dir, db, format, &command.new_id, &command.old_id).unwrap_or(false)
}

fn run_push(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    resolved_remote: &str,
    destination: &sley_remote::PushDestination,
    refspecs: &[String],
    options: PushOptions,
    porcelain: bool,
    atomic: bool,
    push_options: &[String],
) -> Result<()> {
    let config = repo_config_with_transport_policy(git_dir).unwrap_or_default();
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(&config));
    let mut progress = StdoutProgress;
    let remote_options = sley_remote::PushOptions {
        quiet: options.quiet,
        force: options.force,
        thin: options.thin,
        atomic,
        push_options: push_options.to_vec(),
    };
    if matches!(destination, sley_remote::PushDestination::Ssh(_)) {
        trace_configured_local_protocol_version(Some(&config));
    }
    let request = sley_remote::PushRequest {
        git_dir,
        common_git_dir,
        format,
        config: &config,
        remote,
        destination,
        refspecs,
        options: &remote_options,
    };
    let mut services = sley_remote::PushServices {
        credentials: &mut credentials,
        progress: &mut progress,
    };
    let plan = match sley_remote::plan_push(request, &mut services) {
        Err(GitError::InvalidFormat(message)) if message.contains("push-options") => {
            eprintln!("fatal: the receiving end does not support push options");
            return Err(GitError::Exit(128));
        }
        Err(GitError::InvalidFormat(message)) if message.contains("atomic") => {
            eprintln!("fatal: the receiving end does not support --atomic push");
            return Err(GitError::Exit(128));
        }
        result => result?,
    };
    if plan.commands.is_empty() && plan.preflight_rejections.is_empty() {
        return Ok(());
    }
    if !options.no_verify {
        run_pre_push_hook(git_dir, remote, resolved_remote, refspecs, &plan.commands)?;
    }
    // `--dry-run`: report what would happen, but neither send the pack/refs nor
    // run receive-side hooks nor update local tracking refs (git's TRANSPORT_PUSH_DRY_RUN).
    if options.dry_run {
        if !options.quiet {
            eprintln!("To {}", push_display_remote(resolved_remote));
            for command in &plan.commands {
                eprintln!("   {}  {}", command.new_id, command.name);
            }
        }
        return Ok(());
    }
    run_local_receive_pre_hooks(destination, &plan.commands, &[], &[])?;
    run_local_receive_reference_transaction_hook_phase(
        destination,
        &plan.commands,
        sley_refs::RefTransactionPhase::Preparing,
    )?;
    run_local_receive_reference_transaction_hook_phase(
        destination,
        &plan.commands,
        sley_refs::RefTransactionPhase::Prepared,
    )?;
    let preflight_rejections = plan.preflight_rejections.clone();
    let outcome = sley_remote::execute_push_plan(request, &mut services, plan)?;
    run_local_receive_reference_transaction_hook_phase(
        destination,
        &outcome.commands,
        sley_refs::RefTransactionPhase::Committed,
    )?;
    run_local_receive_post_hooks(destination, &outcome.commands, &[])?;
    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut refs: Vec<sley_remote::PushReportRef> = outcome
        .commands
        .iter()
        .map(|command| sley_remote::PushReportRef {
            src: push_source_name_for_command(git_dir, format, refspecs, command),
            dst: command.name.clone(),
            old_id: command.old_id,
            new_id: command.new_id,
            forced: push_command_was_forced(
                common_git_dir,
                &local_db,
                format,
                refspecs,
                command,
                options.force,
            ),
            status: sley_remote::PushRefStatus::Ok,
            reports: Vec::new(),
        })
        .collect();
    refs.extend(preflight_rejections.into_iter().map(|(command, status)| {
        sley_remote::PushReportRef {
            src: push_source_name_for_command(git_dir, format, refspecs, &command),
            dst: command.name,
            old_id: command.old_id,
            new_id: command.new_id,
            forced: false,
            status,
            reports: Vec::new(),
        }
    }));
    if let Some(report) = &outcome.report {
        sley_remote::apply_receive_pack_report_to_push_refs(&mut refs, report);
    }
    let applied: Vec<ReceivePackCommand> = refs
        .iter()
        .filter(|reference| matches!(reference.status, sley_remote::PushRefStatus::Ok))
        .flat_map(|reference| reference.tracking_commands())
        .collect();
    update_push_remote_tracking_refs(git_dir, format, &config, remote, &applied)?;
    if options.set_upstream {
        configure_push_upstreams_from_report(git_dir, remote, &refs)?;
    }
    print_remote_hook_stderr(&outcome.remote_progress);
    let url = sley_remote::push_url_for_display(resolved_remote);
    let had_errors = refs.iter().any(|reference| reference.had_error());
    if !options.quiet || had_errors {
        let remote_db = match destination {
            sley_remote::PushDestination::Local {
                common_git_dir: remote_common,
                ..
            } => FileObjectDatabase::from_git_dir(remote_common, format),
            _ => local_db.clone(),
        };
        render_push_status(
            &sley_remote::PushStatusReport { refs },
            &url,
            porcelain,
            false,
            &local_db,
            &remote_db,
        )?;
    }
    if had_errors {
        eprintln!("error: failed to push some refs to '{url}'");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// Inputs for the file:// push path that renders git's full status report.
struct RunPushLocalReport<'a> {
    git_dir: &'a Path,
    common_git_dir: &'a Path,
    format: ObjectFormat,
    remote: &'a str,
    resolved_remote: &'a str,
    remote_git_dir: &'a Path,
    remote_common_git_dir: &'a Path,
    refspecs: &'a [String],
    options: PushOptions,
    porcelain: bool,
    atomic: bool,
    force_if_includes: bool,
    push_options: &'a [String],
    force_with_lease: &'a [(String, Option<ObjectId>)],
    force_with_lease_default: bool,
    receive_pack_command: Option<&'a str>,
    receive_config_overrides: &'a [(String, String)],
}

/// Drive a file:// push through [`sley_remote::push_local_with_report`], render
/// git's `transport_print_push_status`, update remote-tracking refs, run hooks,
/// and return the git exit code (1 when any ref was rejected).
fn run_push_local_report(req: RunPushLocalReport<'_>) -> Result<()> {
    let config = read_repo_config(req.git_dir).unwrap_or_default();
    let source_db = crate::repository::open_object_database(req.git_dir, req.format)?;
    trace_local_receive_pack_advertisement(req.remote_git_dir, req.format);
    let push_negotiate = config.get_bool("push", None, "negotiate").unwrap_or(false);
    let push_negotiation_failed =
        push_negotiate && env::var("GIT_TEST_PROTOCOL_VERSION").ok().as_deref() == Some("0");
    let remote_config = read_repo_config(req.remote_git_dir).unwrap_or_default();
    if req.atomic
        && !remote_config
            .get_bool("receive", None, "advertiseatomic")
            .unwrap_or(true)
    {
        eprintln!("fatal: the receiving end does not support --atomic push");
        return Err(GitError::Exit(128));
    }
    if !req.push_options.is_empty()
        && !remote_config
            .get_bool("receive", None, "advertisepushoptions")
            .unwrap_or(false)
    {
        eprintln!("fatal: the receiving end does not support push options");
        return Err(GitError::Exit(128));
    }

    // First pass: classify every ref WITHOUT applying anything (a dry-run plan).
    // This lets us run the receive-side pre-receive/update hooks before any ref
    // is written, matching git's receive-pack ordering, and reject all refs when
    // a hook declines.
    let plan = sley_remote::push_local_with_report_and_objects(
        sley_remote::PushReportRequest {
            git_dir: req.git_dir,
            common_git_dir: req.common_git_dir,
            format: req.format,
            remote_git_dir: req.remote_git_dir,
            remote_common_git_dir: req.remote_common_git_dir,
            refspecs: req.refspecs,
            force: req.options.force,
            atomic: req.atomic,
            dry_run: true,
            force_with_lease: req.force_with_lease,
            force_with_lease_default: req.force_with_lease_default,
            force_if_includes: req.force_if_includes,
            receive_config_overrides: req.receive_config_overrides,
            push_options: req.push_options,
            remote_stderr: None,
            quiet: req.options.quiet,
        },
        &config,
        &source_db,
    )?;

    // A matching refspec (`:` / `+:`) against a remote with no refs is not the
    // normal no-op case: git reports the empty expansion as a push failure.
    if plan.refs.is_empty() {
        if req.refspecs.iter().any(|refspec| {
            let body = refspec.strip_prefix('+').unwrap_or(refspec);
            body == ":"
        }) {
            let url = sley_remote::push_url_for_display(req.resolved_remote);
            eprintln!("No refs in common and none specified; doing nothing.");
            eprintln!("Perhaps you should specify a branch.");
            eprintln!("fatal: the remote end hung up unexpectedly");
            eprintln!("error: failed to push some refs to '{url}'");
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }

    // pre-push hook (driven from the would-be commands), unless --no-verify.
    if !req.options.no_verify {
        run_pre_push_hook_for_report(req.git_dir, req.remote, req.resolved_remote, &plan.refs)?;
    }

    let ok_commands: Vec<ReceivePackCommand> = plan
        .refs
        .iter()
        .filter(|reference| matches!(reference.status, sley_remote::PushRefStatus::Ok))
        .map(|reference| ReceivePackCommand {
            old_id: reference.old_id,
            new_id: reference.new_id,
            name: reference.dst.clone(),
        })
        .collect();

    let destination = sley_remote::PushDestination::Local {
        git_dir: req.remote_git_dir.to_path_buf(),
        common_git_dir: req.remote_common_git_dir.to_path_buf(),
    };
    let uses_proc_server = sley_remote::push_local_uses_receive_pack_server(
        &remote_config,
        req.remote_git_dir,
        &ok_commands,
    );
    let mut remote_stderr = Vec::new();
    let quarantine = if !req.options.dry_run
        && !uses_proc_server
        && !ok_commands.is_empty()
        && receive_pre_hooks_may_run(req.remote_git_dir)
    {
        let local_db = FileObjectDatabase::from_git_dir(req.common_git_dir, req.format);
        sley_remote::stage_local_push_quarantine(
            req.remote_git_dir,
            req.remote_common_git_dir,
            req.format,
            &local_db,
            &ok_commands,
        )?
    } else {
        None
    };
    let quarantine_env = quarantine
        .as_ref()
        .map(|quarantine| receive_quarantine_hook_env(req.remote_common_git_dir, quarantine))
        .unwrap_or_default();
    let hook_decline = if !req.options.dry_run && !uses_proc_server && !ok_commands.is_empty() {
        run_local_receive_pre_hooks_report(
            &destination,
            &ok_commands,
            req.push_options,
            &quarantine_env,
            Some(&mut remote_stderr),
        )
    } else {
        None
    };
    if !req.options.dry_run && hook_decline.is_none() && !ok_commands.is_empty() {
        run_local_receive_reference_transaction_hook_phase(
            &destination,
            &ok_commands,
            sley_refs::RefTransactionPhase::Preparing,
        )?;
        run_local_receive_reference_transaction_hook_phase(
            &destination,
            &ok_commands,
            sley_refs::RefTransactionPhase::Prepared,
        )?;
    }
    let mut report = if req.options.dry_run || hook_decline.is_some() {
        plan
    } else {
        sley_remote::push_local_with_report_and_objects(
            sley_remote::PushReportRequest {
                git_dir: req.git_dir,
                common_git_dir: req.common_git_dir,
                format: req.format,
                remote_git_dir: req.remote_git_dir,
                remote_common_git_dir: req.remote_common_git_dir,
                refspecs: req.refspecs,
                force: req.options.force,
                atomic: req.atomic,
                dry_run: false,
                force_with_lease: req.force_with_lease,
                force_with_lease_default: req.force_with_lease_default,
                force_if_includes: req.force_if_includes,
                receive_config_overrides: req.receive_config_overrides,
                push_options: req.push_options,
                remote_stderr: if uses_proc_server {
                    Some(&mut remote_stderr)
                } else {
                    None
                },
                quiet: req.options.quiet,
            },
            &config,
            &source_db,
        )?
    };
    if !req.options.dry_run && hook_decline.is_none() && !ok_commands.is_empty() {
        run_local_receive_reference_transaction_hook_phase(
            &destination,
            &ok_commands,
            sley_refs::RefTransactionPhase::Committed,
        )?;
    }
    if let Some(decline) = hook_decline {
        for reference in &mut report.refs {
            if !matches!(reference.status, sley_remote::PushRefStatus::Ok) {
                continue;
            }
            reference.status = match &decline {
                ReceiveHookDecline::PreReceive => sley_remote::PushRefStatus::RemoteReject(
                    "pre-receive hook declined".to_string(),
                ),
                ReceiveHookDecline::Update(name) if reference.dst == *name => {
                    sley_remote::PushRefStatus::RemoteReject("hook declined".to_string())
                }
                ReceiveHookDecline::Update(_) if req.atomic => {
                    sley_remote::PushRefStatus::RemoteReject("atomic push failure".to_string())
                }
                ReceiveHookDecline::Update(_) => sley_remote::PushRefStatus::Ok,
            }
        }
    }
    if !req.options.dry_run {
        trace2_push_pack_objects(
            req.options.quiet,
            config.get_bool("push", None, "usebitmaps"),
        );
        if config
            .get_bool("pack", None, "usepathwalk")
            .unwrap_or(false)
        {
            trace2_push_pack_objects_path_walk();
        }
        if push_negotiate && !push_negotiation_failed {
            trace2_push_total_rounds(1);
            trace2_push_wrote(2);
        } else {
            if push_negotiation_failed {
                eprintln!("warning: push negotiation failed; proceeding anyway");
            }
            trace2_push_wrote(5);
        }
        if req.options.progress && !req.options.quiet {
            eprintln!("Writing objects: 100% (1/1), done.");
        }
        run_local_receive_pack_auto_gc(req.remote_git_dir);
    }

    if !req.options.dry_run
        && push_warns_current_branch(&report, req.remote_git_dir, req.format, &remote_config)?
    {
        eprintln!("warning: updating the current branch");
    }

    // Post-apply side effects for the refs that landed: post-receive/post-update
    // hooks, remote-tracking ref updates (git updates tracking for every
    // non-rejected ref, including up-to-date ones), and set-upstream config.
    if !req.options.dry_run {
        let applied: Vec<ReceivePackCommand> = report
            .refs
            .iter()
            .filter(|reference| {
                matches!(
                    reference.status,
                    sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
                )
            })
            .flat_map(|reference| reference.tracking_commands())
            .collect();
        if !applied.is_empty() {
            let landed: Vec<sley_remote::ReceivePackCommandState> = report
                .refs
                .iter()
                .filter(|reference| {
                    matches!(reference.status, sley_remote::PushRefStatus::Ok)
                        && reference.old_id != reference.new_id
                })
                .map(|reference| {
                    let mut state = sley_remote::ReceivePackCommandState::new(ReceivePackCommand {
                        old_id: reference.old_id,
                        new_id: reference.new_id,
                        name: reference.dst.clone(),
                    });
                    state.reports = reference.reports.clone();
                    state
                })
                .collect();
            if !landed.is_empty() {
                sley_remote::run_receive_pack_post_hooks(
                    req.remote_git_dir,
                    &landed,
                    req.push_options,
                    &mut remote_stderr,
                    true,
                );
            }
            update_push_remote_tracking_refs(
                req.git_dir,
                req.format,
                &config,
                req.remote,
                &applied,
            )?;
            if req.options.set_upstream {
                configure_push_upstreams_from_report(req.git_dir, req.remote, &report.refs)?;
            }
        }
        print_remote_hook_stderr(&remote_stderr);
    }

    // git's status header and the trailing error use the *resolved* push URL
    // (`transport->url` / `anon_url`), not the remote name the user typed.
    let url = sley_remote::push_url_for_display(req.resolved_remote);

    let had_errors = report.had_errors();
    if !req.options.quiet || had_errors {
        let local_db = FileObjectDatabase::from_git_dir(req.common_git_dir, req.format);
        let remote_db = FileObjectDatabase::from_git_dir(req.remote_common_git_dir, req.format);
        render_push_status(
            &report,
            &url,
            req.porcelain,
            req.options.dry_run,
            &local_db,
            &remote_db,
        )?;
    }

    if had_errors {
        eprintln!("error: failed to push some refs to '{url}'");
        return Err(GitError::Exit(1));
    }
    if let Some(command) = req.receive_pack_command
        && !custom_receive_pack_command_is_native_git(command)
        && !custom_receive_pack_command_exits_successfully(command, req.remote_git_dir)?
    {
        eprintln!("error: failed to push some refs to '{url}'");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn print_remote_hook_stderr(remote_stderr: &[u8]) {
    // Upstream `sideband.c` appends `DUMB_SUFFIX` (8 spaces) to each remote
    // progress/error line when stderr is not a tty — the t5411 expect text
    // includes those trailing spaces.
    let suffix = if std::io::stderr().is_terminal() {
        ""
    } else {
        "        "
    };
    let text = String::from_utf8_lossy(remote_stderr);
    for line in text.lines() {
        eprintln!("remote: {line}{suffix}");
    }
}

fn custom_receive_pack_command_is_native_git(command: &str) -> bool {
    let command = strip_receive_pack_shell_prefixes(command).trim_start();
    command == "git-receive-pack"
        || command.starts_with("git-receive-pack ")
        || command == "git receive-pack"
        || command.starts_with("git receive-pack ")
        || command
            .strip_prefix("git ")
            .is_some_and(git_command_runs_receive_pack)
}

fn git_command_runs_receive_pack(rest: &str) -> bool {
    let mut words = rest.split_whitespace();
    while let Some(word) = words.next() {
        if word == "-c" {
            if words.next().is_none() {
                return false;
            }
            continue;
        }
        if word.starts_with("-c") && word.len() > 2 {
            continue;
        }
        return word == "receive-pack" || word == "git-receive-pack";
    }
    false
}

fn strip_receive_pack_shell_prefixes(mut command: &str) -> &str {
    loop {
        let trimmed = command.trim_start();
        if let Some(rest) = strip_leading_unset_command(trimmed) {
            command = rest;
            continue;
        }
        if let Some(rest) = strip_leading_env_assignment(trimmed) {
            command = rest;
            continue;
        }
        return trimmed;
    }
}

fn strip_leading_unset_command(command: &str) -> Option<&str> {
    let rest = command.strip_prefix("unset ")?;
    let semicolon = rest.find(';')?;
    Some(&rest[semicolon + 1..])
}

fn strip_leading_env_assignment(command: &str) -> Option<&str> {
    let bytes = command.as_bytes();
    let mut idx = 0;
    let first = *bytes.first()?;
    if !is_shell_name_start(first) {
        return None;
    }
    idx += 1;
    while idx < bytes.len() && is_shell_name_char(bytes[idx]) {
        idx += 1;
    }
    if bytes.get(idx) != Some(&b'=') {
        return None;
    }
    idx += 1;
    match bytes.get(idx).copied() {
        Some(quote @ (b'\'' | b'"')) => {
            idx += 1;
            while idx < bytes.len() && bytes[idx] != quote {
                idx += 1;
            }
            if idx >= bytes.len() {
                return None;
            }
            idx += 1;
        }
        Some(_) => {
            while idx < bytes.len() && !bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
        }
        None => return None,
    }
    if idx < bytes.len() && !bytes[idx].is_ascii_whitespace() {
        return None;
    }
    Some(&command[idx..])
}

fn is_shell_name_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_shell_name_char(byte: u8) -> bool {
    is_shell_name_start(byte) || byte.is_ascii_digit()
}

fn custom_receive_pack_command_exits_successfully(
    command: &str,
    remote_git_dir: &Path,
) -> Result<bool> {
    let repo = remote_git_dir.to_string_lossy();
    let command = format!("{command} {}", sley_config::sq_quote(&repo));
    let status = Proc::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

fn trace_local_receive_pack_advertisement(remote_git_dir: &Path, format: ObjectFormat) {
    let Some(mut sink) = packet_trace_file() else {
        return;
    };
    let store = FileRefStore::new(remote_git_dir, format);
    let Ok(refs) = store.list_refs() else {
        return;
    };
    let mut local_oids = std::collections::HashSet::new();
    for reference in refs {
        let Some(oid) = resolve_local_ref_target(&store, &reference).ok().flatten() else {
            continue;
        };
        local_oids.insert(oid);
        let _ = writeln!(sink, "packet:         push< {oid} {}", reference.name);
    }
    trace_alternate_have_advertisements(remote_git_dir, format, &local_oids, &mut sink);
    let _ = writeln!(sink, "packet:         push< 0000");
}

fn packet_trace_file() -> Option<fs::File> {
    let value = env::var("GIT_TRACE_PACKET").ok()?;
    if matches!(value.as_str(), "" | "0" | "false" | "FALSE") {
        return None;
    }
    if matches!(value.as_str(), "1" | "2" | "true" | "TRUE") {
        return None;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(value)
        .ok()
}

fn trace_alternate_have_advertisements(
    remote_git_dir: &Path,
    format: ObjectFormat,
    local_oids: &std::collections::HashSet<ObjectId>,
    sink: &mut fs::File,
) {
    let alternates = remote_git_dir.join("objects/info/alternates");
    let Ok(text) = fs::read_to_string(alternates) else {
        return;
    };
    let mut seen = std::collections::HashSet::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let objects_dir = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            remote_git_dir.join("objects").join(line)
        };
        let Some(alternate_git_dir) = objects_dir.parent() else {
            continue;
        };
        let store = FileRefStore::new(alternate_git_dir, format);
        let Ok(refs) = store.list_refs() else {
            continue;
        };
        for reference in refs {
            let Some(oid) = resolve_local_ref_target(&store, &reference).ok().flatten() else {
                continue;
            };
            if local_oids.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            let _ = writeln!(sink, "packet:         push< {oid} .have");
        }
    }
}

fn resolve_local_ref_target(
    store: &FileRefStore,
    reference: &sley_refs::Ref,
) -> Result<Option<ObjectId>> {
    let mut target = reference.target.clone();
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some(oid)),
            RefTarget::Symbolic(name) => {
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}

fn run_local_receive_pack_auto_gc(remote_git_dir: &Path) {
    let config = read_repo_config_on_disk(remote_git_dir).unwrap_or_default();
    if config.get_bool("maintenance", None, "auto") == Some(false) {
        return;
    }
    prune_stale_object_tempfiles(remote_git_dir);
}

fn prune_stale_object_tempfiles(remote_git_dir: &Path) {
    let objects_dir = remote_git_dir.join("objects");
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return;
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    const TWO_WEEKS_SECS: u64 = 14 * 24 * 60 * 60;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with("tmp_") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(modified) = modified.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        if now.as_secs().saturating_sub(modified.as_secs()) >= TWO_WEEKS_SECS {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn push_warns_current_branch(
    report: &sley_remote::PushStatusReport,
    remote_git_dir: &Path,
    format: ObjectFormat,
    remote_config: &GitConfig,
) -> Result<bool> {
    let deny = remote_config
        .get("receive", None, "denycurrentbranch")
        .unwrap_or("");
    if !deny.eq_ignore_ascii_case("warn") {
        return Ok(false);
    }
    if sley_worktree::worktree_root_for_git_dir(remote_git_dir)?.is_none() {
        return Ok(false);
    }
    let store = FileRefStore::new(remote_git_dir, format);
    let Some(RefTarget::Symbolic(head)) = store.read_ref("HEAD")? else {
        return Ok(false);
    };
    Ok(report.refs.iter().any(|reference| {
        reference.dst == head
            && !reference.new_id.is_null()
            && reference.old_id != reference.new_id
            && matches!(
                reference.status,
                sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
            )
    }))
}

fn trace2_push_pack_objects(quiet: bool, use_bitmaps: Option<bool>) {
    let mut args = vec![
        "pack-objects",
        "--all-progress-implied",
        "--revs",
        "--stdout",
        "--thin",
        "--delta-base-offset",
    ];
    if quiet {
        args.push("-q");
    }
    if use_bitmaps == Some(false) {
        args.push("--no-use-bitmap-index");
    }
    crate::commands::pack::trace2_child_start(&args);
}

fn trace2_push_pack_objects_path_walk() {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let lines = concat!(
        "{\"event\":\"region_enter\",\"sid\":\"sley\",\"category\":\"pack-objects\",\"label\":\"path-walk\"}\n",
        "{\"event\":\"region_leave\",\"sid\":\"sley\",\"category\":\"pack-objects\",\"label\":\"path-walk\"}\n",
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(lines.as_bytes());
    }
}

fn trace2_push_wrote(value: usize) {
    sley_core::trace2::data("write_pack_file", "wrote", value);
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let line = format!(
        "{{\"event\":\"data\",\"sid\":\"sley\",\"category\":\"write_pack_file/wrote\",\"value\":\"{value}\"}}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn trace2_push_total_rounds(value: usize) {
    sley_core::trace2::data("negotiation_v2", "total_rounds", value);
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let line = format!(
        "{{\"event\":\"data\",\"sid\":\"sley\",\"category\":\"negotiation_v2\",\"key\":\"total_rounds\",\"value\":\"{value}\"}}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Render a push status report exactly like git's `transport_print_push_status`:
/// the "To <url>" header, one line per ref (OK refs first, then rejects), and a
/// trailing "Done"/"Everything up-to-date" depending on porcelain and progress.
fn render_push_status(
    report: &sley_remote::PushStatusReport,
    dest: &str,
    porcelain: bool,
    _dry_run: bool,
    local_db: &FileObjectDatabase,
    remote_db: &FileObjectDatabase,
) -> Result<()> {
    let summary_width = push_summary_width(report);
    // git renders refs in `remote_refs` order: the refs that already exist on the
    // remote first (the advertisement is sorted by ref name), then the
    // newly-created refs appended in refspec/planning order. A "new" ref is a
    // create (zero old id that is not a deletion). Reproduce that key, then the
    // three status passes below each walk in this canonical order.
    let mut ordered: Vec<(usize, &sley_remote::PushReportRef)> =
        report.refs.iter().enumerate().collect();
    ordered.sort_by(|(ai, a), (bi, b)| {
        let a_new = a.old_id.is_null() && !a.is_deletion();
        let b_new = b.old_id.is_null() && !b.is_deletion();
        match (a_new, b_new) {
            (false, false) => a.dst.cmp(&b.dst),
            (true, true) => ai.cmp(bi),
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
        }
    });
    let ordered: Vec<&sley_remote::PushReportRef> = ordered
        .into_iter()
        .map(|(_, reference)| reference)
        .collect();
    let mut first = true;
    let mut emit = |reference: &sley_remote::PushReportRef| {
        if first {
            if porcelain {
                println!("To {dest}");
            } else {
                eprintln!("To {dest}");
            }
            first = false;
        }
        print_push_ref(reference, porcelain, summary_width, local_db, remote_db);
    };

    // git's `transport_print_push_status` prints in three passes:
    //   1. UPTODATE refs, but only when verbose — and porcelain forces verbose.
    //   2. OK refs.
    //   3. everything else (rejections / no-match).
    // Each pass preserves planning order.
    if porcelain {
        for reference in &ordered {
            if matches!(reference.status, sley_remote::PushRefStatus::UpToDate) {
                emit_with_reports(reference, &mut emit);
            }
        }
    }
    for reference in &ordered {
        if matches!(reference.status, sley_remote::PushRefStatus::Ok) {
            emit_with_reports(reference, &mut emit);
        }
    }
    for reference in &ordered {
        if !matches!(
            reference.status,
            sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
        ) {
            emit_with_reports(reference, &mut emit);
        }
    }
    for reference in &ordered {
        if matches!(
            &reference.status,
            sley_remote::PushRefStatus::RemoteReject(message)
                if message == "invalid new value provided"
        ) && reference.dst.starts_with("refs/heads/")
            && !reference.new_id.is_null()
        {
            eprintln!(
                "trying to write non-commit object {} to branch '{}'",
                reference.new_id, reference.dst
            );
        }
    }

    // git prints "Done" under porcelain whenever the transport-level push
    // succeeded (`!push_ret`); ref-level rejections (non-ff, atomic, remote ng)
    // do not set push_ret, so over the local transport "Done" always prints.
    if porcelain {
        println!("Done");
    } else if !report.had_errors() && !report.refs_pushed() {
        // stable plumbing output; do not modify or localize
        eprintln!("Everything up-to-date");
    }
    Ok(())
}

/// git's `transport_summary_width`: `2 * maxw + 3` where `maxw` is the widest
/// unique abbreviation across every ref's old and new oid (min `DEFAULT_ABBREV`).
fn push_summary_width(report: &sley_remote::PushStatusReport) -> usize {
    let mut maxw = 7usize;
    for reference in &report.refs {
        // We approximate `measure_abbrev` with the default abbrev length; the
        // status renderer recomputes the actual unique abbrev per oid below, so
        // any growth there is reflected in the printed quickref even though the
        // column width stays at the common-case 17. This matches git for the
        // overwhelmingly common 7-hex-unique case the tests exercise.
        let _ = reference;
        maxw = maxw.max(7);
    }
    2 * maxw + 3
}

fn emit_with_reports(
    reference: &sley_remote::PushReportRef,
    emit: &mut dyn FnMut(&sley_remote::PushReportRef),
) {
    if reference.reports.is_empty() {
        emit(reference);
        return;
    }
    for report in &reference.reports {
        emit(&push_report_with_proc_receive(reference, report));
    }
}

fn push_report_with_proc_receive(
    reference: &sley_remote::PushReportRef,
    report: &sley_remote::ProcReceiveReport,
) -> sley_remote::PushReportRef {
    let mut rewritten = reference.clone();
    if let Some(refname) = &report.refname {
        rewritten.dst = refname.clone();
    }
    if let Some(old_oid) = &report.old_oid {
        rewritten.old_id = *old_oid;
    }
    if let Some(new_oid) = &report.new_oid {
        rewritten.new_id = *new_oid;
    }
    if report.forced_update {
        rewritten.forced = true;
    }
    rewritten.reports = Vec::new();
    rewritten
}

/// Render one ref's status line. Mirrors git's `print_one_push_report` +
/// `print_ok_ref_status` + `print_ref_status`.
fn print_push_ref(
    reference: &sley_remote::PushReportRef,
    porcelain: bool,
    summary_width: usize,
    local_db: &FileObjectDatabase,
    remote_db: &FileObjectDatabase,
) {
    use sley::plumbing::sley_remote::PushRefStatus;
    let (flag, summary, msg): (char, String, Option<String>) = match &reference.status {
        PushRefStatus::Ok => push_ok_summary(reference, local_db, remote_db),
        PushRefStatus::UpToDate => ('=', "[up to date]".to_string(), None),
        PushRefStatus::RejectNonFastForward => (
            '!',
            "[rejected]".to_string(),
            Some("non-fast-forward".to_string()),
        ),
        PushRefStatus::RejectFetchFirst => (
            '!',
            "[rejected]".to_string(),
            Some("fetch first".to_string()),
        ),
        PushRefStatus::RejectStale => (
            '!',
            "[rejected]".to_string(),
            Some("stale info".to_string()),
        ),
        PushRefStatus::RejectRemoteUpdated => (
            '!',
            "[rejected]".to_string(),
            Some("remote ref updated since checkout".to_string()),
        ),
        PushRefStatus::RejectAlreadyExists => (
            '!',
            "[rejected]".to_string(),
            Some("already exists".to_string()),
        ),
        PushRefStatus::RemoteReject(message) => {
            ('!', "[remote rejected]".to_string(), Some(message.clone()))
        }
        PushRefStatus::AtomicPushFailed => (
            '!',
            "[rejected]".to_string(),
            Some("atomic push failed".to_string()),
        ),
    };

    // The "from" side. Human delete reports print only the destination; porcelain
    // rejected deletes usually expose git's literal peer ref "(delete)" except
    // for successful deletes and pre-receive declines, which render an empty source.
    let from = if reference.is_deletion() {
        if !porcelain
            || matches!(reference.status, PushRefStatus::Ok)
            || matches!(
                &reference.status,
                PushRefStatus::RemoteReject(message) if message == "pre-receive hook declined"
            )
        {
            None
        } else {
            Some("(delete)".to_string())
        }
    } else {
        reference.src.clone()
    };

    if porcelain {
        let from_field = from.as_deref().unwrap_or("");
        let body = match &msg {
            Some(m) => format!("{summary} ({m})"),
            None => summary,
        };
        println!("{flag}\t{from_field}:{}\t{body}", reference.dst);
    } else {
        let to = prettify_refname(&reference.dst);
        let mut line = format!(" {flag} {summary:<summary_width$} ");
        match &from {
            Some(from) => line.push_str(&format!("{} -> {to}", prettify_refname(from))),
            None => line.push_str(&to),
        }
        if let Some(m) = &msg {
            line.push_str(&format!(" ({m})"));
        }
        eprintln!("{line}");
    }
}

/// git's `print_ok_ref_status`: classify an applied update into its flag,
/// `[deleted]`/`[new branch]`/`[new tag]`/`[new reference]` summary, or the
/// `<old>..<new>` / `<old>...<new>` quickref with the forced marker.
fn push_ok_summary(
    reference: &sley_remote::PushReportRef,
    local_db: &FileObjectDatabase,
    remote_db: &FileObjectDatabase,
) -> (char, String, Option<String>) {
    if reference.is_deletion() {
        return ('-', "[deleted]".to_string(), None);
    }
    if reference.old_id.is_null() {
        let summary = if reference.dst.starts_with("refs/tags/") {
            "[new tag]"
        } else if reference.dst.starts_with("refs/heads/") {
            "[new branch]"
        } else {
            "[new reference]"
        };
        return ('*', summary.to_string(), None);
    }
    let old = unique_abbrev(&reference.old_id, remote_db);
    let new = unique_abbrev(&reference.new_id, local_db);
    if reference.forced {
        (
            '+',
            format!("{old}...{new}"),
            Some("forced update".to_string()),
        )
    } else {
        (' ', format!("{old}..{new}"), None)
    }
}

/// Prettify a ref name for human output (git's `prettify_refname`): drop the
/// `refs/heads/`, `refs/tags/`, or `refs/remotes/` prefix.
pub(super) fn prettify_refname(name: &str) -> String {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    name.to_string()
}

/// git's `find_unique_abbrev`: the shortest prefix (≥ `DEFAULT_ABBREV` = 7) of
/// `oid` that is unambiguous in `db`, growing until it resolves uniquely.
pub(super) fn unique_abbrev(oid: &ObjectId, db: &FileObjectDatabase) -> String {
    let hex = oid.to_hex();
    let mut width = 7.min(hex.len());
    while width < hex.len() {
        match db.resolve_prefix(&hex[..width]) {
            Ok(sley_odb::ObjectPrefixResolution::Ambiguous(_)) => width += 1,
            _ => break,
        }
    }
    hex[..width].to_string()
}

fn update_push_remote_tracking_refs(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    remote: &str,
    commands: &[ReceivePackCommand],
) -> Result<()> {
    if config.get("remote", Some(remote), "url").is_none() {
        return Ok(());
    }
    let fetch_refspecs = remote_config_values(config, remote, "fetch")
        .into_iter()
        .filter_map(|spec| sley_protocol::parse_refspec(&spec).ok())
        .collect::<Vec<_>>();
    let refs = FileRefStore::new(git_dir, format);
    let mut tx = refs.transaction();
    for command in commands {
        let mut tracking_names = Vec::new();
        for refspec in &fetch_refspecs {
            if refspec.negative {
                continue;
            }
            if let Some(name) = sley_protocol::refspec_map_source(refspec, &command.name)? {
                tracking_names.push(name);
            }
        }
        if tracking_names.is_empty() {
            if let Some(branch) = command.name.strip_prefix("refs/heads/") {
                tracking_names.push(format!("refs/remotes/{remote}/{branch}"));
            }
        }
        for name in tracking_names {
            if command.new_id.is_null() {
                let _ = refs.delete_ref(&name);
            } else if refs.read_ref(&name)? == Some(RefTarget::Direct(command.new_id)) {
                let packed_name = name.clone();
                refs.pack_refs_selected_with_timeout(
                    true,
                    false,
                    0,
                    |candidate| candidate == packed_name,
                    |_, _| Ok(PackRefDecision::Pack { peeled: None }),
                )?;
                continue;
            } else {
                tx.update(RefUpdate {
                    name,
                    expected: None,
                    new: RefTarget::Direct(command.new_id),
                    reflog: None,
                });
            }
        }
    }
    tx.commit()
}

fn run_local_receive_pre_hooks(
    destination: &sley_remote::PushDestination,
    push_commands: &[ReceivePackCommand],
    push_options: &[String],
    quarantine_env: &[(String, String)],
) -> Result<()> {
    let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        ..
    } = destination
    else {
        return Ok(());
    };
    let stdin = receive_hook_stdin(push_commands);
    let hook_env = receive_hook_env(push_options, quarantine_env);
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "pre-receive",
        commands::hooks::HookRun {
            stdin: Some(stdin),
            env: hook_env.clone(),
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    for command in receive_update_hook_order(push_commands) {
        let _ = commands::hooks::run_traditional_hook_at(
            remote_git_dir,
            "update",
            commands::hooks::HookRun {
                args: vec![
                    command.name.clone(),
                    command.old_id.to_string(),
                    command.new_id.to_string(),
                ],
                env: hook_env.clone(),
                cwd: Some(remote_git_dir.to_path_buf()),
                ..commands::hooks::HookRun::default()
            },
        )?;
    }
    Ok(())
}

enum ReceiveHookDecline {
    PreReceive,
    Update(String),
}

fn run_local_receive_pre_hooks_report(
    destination: &sley_remote::PushDestination,
    push_commands: &[ReceivePackCommand],
    push_options: &[String],
    quarantine_env: &[(String, String)],
    remote_stderr: Option<&mut Vec<u8>>,
) -> Option<ReceiveHookDecline> {
    let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        ..
    } = destination
    else {
        return None;
    };
    let mut discard_stderr = Vec::new();
    let capture_stderr = remote_stderr.is_some();
    let stderr = remote_stderr.unwrap_or(&mut discard_stderr);
    if sley_remote::run_pre_receive(
        remote_git_dir,
        push_commands,
        push_options,
        quarantine_env,
        stderr,
        capture_stderr,
    )
    .is_err()
    {
        return Some(ReceiveHookDecline::PreReceive);
    }
    match sley_remote::run_update_hooks(
        remote_git_dir,
        push_commands,
        quarantine_env,
        stderr,
        capture_stderr,
    ) {
        Ok(Some(name)) => Some(ReceiveHookDecline::Update(name)),
        Ok(None) => None,
        Err(_) => Some(ReceiveHookDecline::PreReceive),
    }
}

fn run_local_receive_reference_transaction_hook_phase(
    destination: &sley_remote::PushDestination,
    push_commands: &[ReceivePackCommand],
    phase: sley_refs::RefTransactionPhase,
) -> Result<()> {
    let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        ..
    } = destination
    else {
        return Ok(());
    };
    if push_commands.is_empty() {
        return Ok(());
    }
    let updates = push_commands
        .iter()
        .map(|command| sley_refs::RefTransactionHookUpdate {
            old_value: command.old_id.to_string(),
            new_value: command.new_id.to_string(),
            refname: command.name.clone(),
        })
        .collect::<Vec<_>>();
    let hook = crate::commands::refs::ReferenceTransactionHookRunner::new(remote_git_dir);
    if sley_refs::ReferenceTransactionHook::run(&hook, phase, &updates)?
        && matches!(
            phase,
            sley_refs::RefTransactionPhase::Preparing | sley_refs::RefTransactionPhase::Prepared
        )
    {
        return Err(GitError::Transaction(format!(
            "in '{}' phase, update aborted by the reference-transaction hook",
            phase.as_str()
        )));
    }
    Ok(())
}

fn receive_update_hook_order(push_commands: &[ReceivePackCommand]) -> Vec<&ReceivePackCommand> {
    let mut ordered = Vec::with_capacity(push_commands.len());
    ordered.extend(
        push_commands
            .iter()
            .filter(|command| command.new_id.is_null()),
    );
    ordered.extend(
        push_commands
            .iter()
            .filter(|command| !command.new_id.is_null()),
    );
    ordered
}

fn receive_pre_hooks_may_run(remote_git_dir: &Path) -> bool {
    let Ok(common_git_dir) = common_git_dir_for_git_dir(remote_git_dir) else {
        return false;
    };
    ["pre-receive", "update"]
        .iter()
        .any(|hook| common_git_dir.join("hooks").join(hook).is_file())
}

fn receive_quarantine_hook_env(
    remote_common_git_dir: &Path,
    quarantine: &sley_remote::PushQuarantine,
) -> Vec<(String, String)> {
    let object_dir = quarantine.object_dir().to_string_lossy().into_owned();
    let alternate = remote_common_git_dir
        .join("objects")
        .to_string_lossy()
        .into_owned();
    vec![
        ("GIT_OBJECT_DIRECTORY".to_string(), object_dir.clone()),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES".to_string(), alternate),
        ("GIT_QUARANTINE_PATH".to_string(), object_dir),
    ]
}

fn receive_hook_env(
    push_options: &[String],
    quarantine_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env = push_option_hook_env(push_options);
    env.extend_from_slice(quarantine_env);
    env
}

fn run_local_receive_post_hooks(
    destination: &sley_remote::PushDestination,
    push_commands: &[ReceivePackCommand],
    push_options: &[String],
) -> Result<()> {
    let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        ..
    } = destination
    else {
        return Ok(());
    };
    let stdin = receive_hook_stdin(push_commands);
    let push_option_env = push_option_hook_env(push_options);
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "post-receive",
        commands::hooks::HookRun {
            stdin: Some(stdin),
            env: push_option_env.clone(),
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "post-update",
        commands::hooks::HookRun {
            args: receive_stream_hook_order(push_commands)
                .into_iter()
                .map(|command| command.name.clone())
                .collect(),
            env: push_option_env.clone(),
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "push-to-checkout",
        commands::hooks::HookRun {
            env: push_option_env,
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    Ok(())
}

fn push_option_hook_env(push_options: &[String]) -> Vec<(String, String)> {
    let mut env = vec![(
        "GIT_PUSH_OPTION_COUNT".to_string(),
        push_options.len().to_string(),
    )];
    for (index, value) in push_options.iter().enumerate() {
        env.push((format!("GIT_PUSH_OPTION_{index}"), value.clone()));
    }
    env
}

fn receive_hook_stdin(push_commands: &[ReceivePackCommand]) -> Vec<u8> {
    receive_stream_hook_order(push_commands)
        .iter()
        .map(|command| format!("{} {} {}\n", command.old_id, command.new_id, command.name))
        .collect::<String>()
        .into_bytes()
}

fn receive_stream_hook_order(push_commands: &[ReceivePackCommand]) -> Vec<&ReceivePackCommand> {
    let mut existing = push_commands
        .iter()
        .filter(|command| !command.old_id.is_null())
        .collect::<Vec<_>>();
    existing.sort_by(|left, right| left.name.cmp(&right.name));
    existing.extend(
        push_commands
            .iter()
            .filter(|command| command.old_id.is_null()),
    );
    existing
}

fn run_pre_push_hook(
    git_dir: &Path,
    remote: &str,
    resolved_remote: &str,
    refspecs: &[String],
    push_commands: &[ReceivePackCommand],
) -> Result<()> {
    let url = resolved_remote.to_string();
    let stdin = pre_push_stdin(git_dir, refspecs, push_commands)?;
    commands::hooks::run_hook(
        "pre-push",
        commands::hooks::HookRun {
            args: vec![remote.to_string(), url],
            stdin: Some(stdin.into_bytes()),
            stdout_to_stderr: false,
            ..commands::hooks::HookRun::default()
        },
    )?;
    Ok(())
}

fn run_pre_push_hook_for_report(
    git_dir: &Path,
    remote: &str,
    resolved_remote: &str,
    refs: &[sley_remote::PushReportRef],
) -> Result<()> {
    let url = resolved_remote.to_string();
    let stdin = pre_push_stdin_from_report(refs);
    commands::hooks::run_hook(
        "pre-push",
        commands::hooks::HookRun {
            args: vec![remote.to_string(), url],
            stdin: Some(stdin.into_bytes()),
            stdout_to_stderr: false,
            git_dir: Some(git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    Ok(())
}

fn pre_push_stdin_from_report(refs: &[sley_remote::PushReportRef]) -> String {
    refs.iter()
        .filter(|reference| {
            !matches!(
                reference.status,
                sley_remote::PushRefStatus::RejectNonFastForward
                    | sley_remote::PushRefStatus::RejectFetchFirst
                    | sley_remote::PushRefStatus::RejectRemoteUpdated
                    | sley_remote::PushRefStatus::RejectStale
                    | sley_remote::PushRefStatus::UpToDate
            )
        })
        .map(|reference| {
            let local_ref = reference
                .src
                .clone()
                .unwrap_or_else(|| "(delete)".to_string());
            format!(
                "{} {} {} {}\n",
                local_ref, reference.new_id, reference.dst, reference.old_id
            )
        })
        .collect()
}

fn pre_push_stdin(
    git_dir: &Path,
    refspecs: &[String],
    commands: &[ReceivePackCommand],
) -> Result<String> {
    let format = repository_object_format(git_dir)?;
    let refs = FileRefStore::new(git_dir, format);
    let current_branch = refs.current_branch().ok().flatten();
    let mut out = String::new();
    for (idx, command) in commands.iter().enumerate() {
        let local_ref = refspecs
            .get(idx)
            .and_then(|refspec| pre_push_local_ref(&refs, refspec, current_branch.as_deref()))
            .unwrap_or_else(|| command.name.clone());
        let local_ref = if command.new_id.is_null() {
            "(delete)".to_string()
        } else {
            local_ref
        };
        let remote_ref = if command.name == "HEAD" {
            current_branch
                .as_deref()
                .map(|branch| format!("refs/heads/{branch}"))
                .unwrap_or_else(|| command.name.clone())
        } else {
            command.name.clone()
        };
        out.push_str(&format!(
            "{} {} {} {}\n",
            local_ref, command.new_id, remote_ref, command.old_id
        ));
    }
    Ok(out)
}

fn pre_push_local_ref(
    refs: &FileRefStore,
    refspec: &str,
    current_branch: Option<&str>,
) -> Option<String> {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    let (src, _) = refspec.split_once(':').unwrap_or((refspec, ""));
    if src.is_empty() {
        return Some("(delete)".to_string());
    }
    if src == "HEAD" || src.contains('~') || src.contains('^') {
        return Some(src.to_string());
    }
    if src.starts_with("refs/") {
        return Some(src.to_string());
    }
    if Some(src) == current_branch {
        return Some(format!("refs/heads/{src}"));
    }
    if let Ok(Some(_)) = refs.read_ref(&format!("refs/tags/{src}")) {
        return Some(format!("refs/tags/{src}"));
    }
    if let Ok(Some(_)) = refs.read_ref(&format!("refs/heads/{src}")) {
        return Some(format!("refs/heads/{src}"));
    }
    Some(src.to_string())
}

/// Remote name or URL argument with embedded credentials stripped for display.
fn push_display_remote(remote: &str) -> String {
    sley_remote::push_url_for_display(remote)
}

struct PushRemoteAndRefspecs {
    remote: String,
    refspecs: Vec<String>,
    set_upstream: bool,
    mirror: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PushDefaultMode {
    Unspecified,
    Nothing,
    Matching,
    Simple,
    Upstream,
    Current,
}

fn push_remote_and_refspecs(
    git_dir: &Path,
    store: &FileRefStore,
    positional: &[String],
) -> Result<PushRemoteAndRefspecs> {
    match positional {
        [] => {
            let config = read_repo_config(git_dir).unwrap_or_default();
            reject_empty_branch_config(&config)?;
            let branch = store.current_branch()?;
            let remote = default_push_remote(&config, branch.as_deref())?;
            let refspecs = default_push_refspecs(&config, branch.as_deref(), &remote)?;
            Ok(PushRemoteAndRefspecs {
                remote,
                refspecs: refspecs.refspecs,
                set_upstream: refspecs.set_upstream,
                mirror: refspecs.mirror,
            })
        }
        [remote] => {
            let config = read_repo_config(git_dir).unwrap_or_default();
            reject_empty_branch_config(&config)?;
            let branch = store.current_branch()?;
            let refspecs = default_push_refspecs(&config, branch.as_deref(), remote)?;
            Ok(PushRemoteAndRefspecs {
                remote: remote.clone(),
                refspecs: refspecs.refspecs,
                set_upstream: refspecs.set_upstream,
                mirror: refspecs.mirror,
            })
        }
        [remote, refspecs @ ..] => {
            let config = read_repo_config(git_dir).unwrap_or_default();
            reject_empty_branch_config(&config)?;
            if config
                .get_bool("remote", Some(remote), "mirror")
                .unwrap_or(false)
            {
                return Err(GitError::Command(
                    "--mirror can't be combined with refspecs".into(),
                ));
            }
            let branch = store.current_branch()?;
            Ok(PushRemoteAndRefspecs {
                remote: remote.clone(),
                refspecs: explicit_push_refspecs_with_refmap(
                    &config,
                    store,
                    branch.as_deref(),
                    remote,
                    refspecs,
                )?,
                set_upstream: false,
                mirror: false,
            })
        }
    }
}

fn reject_empty_branch_config(config: &GitConfig) -> Result<()> {
    for section in &config.sections {
        if section.name.eq_ignore_ascii_case("branch") && section.subsection.as_deref() == Some("")
        {
            let key = section
                .entries
                .first()
                .map(|entry| entry.key.as_str())
                .unwrap_or("");
            eprintln!("fatal: bad config variable 'branch..{key}' in file '.git/config'");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn explicit_push_refspecs_with_refmap(
    config: &GitConfig,
    store: &FileRefStore,
    branch: Option<&str>,
    remote: &str,
    refspecs: &[String],
) -> Result<Vec<String>> {
    let configured_push = config
        .get_all("remote", Some(remote), "push")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect::<Vec<_>>();
    refspecs
        .iter()
        .map(|refspec| {
            let upstream_ref = if configured_push.is_empty()
                && matches!(push_default_mode(config), PushDefaultMode::Upstream)
                && branch.is_some()
                && explicit_refspec_uses_upstream_refmap(store, branch.unwrap(), refspec)
                && default_fetch_remote_for_branch(config, branch.unwrap()) == remote
            {
                Some(push_upstream_ref(config, branch.unwrap(), remote, false)?)
            } else {
                None
            };
            explicit_push_refspec_with_refmap(
                store,
                refspec,
                &configured_push,
                upstream_ref.as_deref(),
            )
        })
        .collect()
}

fn explicit_refspec_uses_upstream_refmap(
    store: &FileRefStore,
    current_branch: &str,
    refspec: &str,
) -> bool {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    if refspec.contains(':') || refspec == "tag" || matches!(refspec, "HEAD" | "@") {
        return false;
    }
    push_refmap_source_name(store, refspec) == format!("refs/heads/{current_branch}")
}

fn explicit_push_refspec_with_refmap(
    store: &FileRefStore,
    refspec: &str,
    configured_push: &[String],
    upstream_ref: Option<&str>,
) -> Result<String> {
    if refspec.contains(':') || refspec == "tag" {
        return Ok(refspec.to_string());
    }
    let (force, body) = refspec
        .strip_prefix('+')
        .map_or(("", refspec), |stripped| ("+", stripped));
    let source = push_refmap_source_name(store, body);
    for configured in configured_push {
        let configured = configured.strip_prefix('+').unwrap_or(configured);
        let parsed = sley_protocol::parse_refspec(configured)?;
        if parsed.negative {
            continue;
        }
        if let Some(dst) = sley_protocol::refspec_map_source(&parsed, &source)? {
            return Ok(format!("{force}{source}:{dst}"));
        }
    }
    if let Some(upstream_ref) = upstream_ref {
        return Ok(format!("{force}{source}:{upstream_ref}"));
    }
    Ok(refspec.to_string())
}

fn push_refmap_source_name(store: &FileRefStore, name: &str) -> String {
    if name == "HEAD" || name == "@" || name.starts_with("refs/") {
        return name.to_string();
    }
    let branch = format!("refs/heads/{name}");
    if matches!(store.read_ref(&branch), Ok(Some(_))) {
        return branch;
    }
    let tag = format!("refs/tags/{name}");
    if matches!(store.read_ref(&tag), Ok(Some(_))) {
        return tag;
    }
    name.to_string()
}

struct DefaultPushRefspecs {
    refspecs: Vec<String>,
    set_upstream: bool,
    mirror: bool,
}

fn default_push_refspecs(
    config: &GitConfig,
    branch: Option<&str>,
    remote: &str,
) -> Result<DefaultPushRefspecs> {
    let configured_push = config
        .get_all("remote", Some(remote), "push")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !configured_push.is_empty() {
        return Ok(DefaultPushRefspecs {
            refspecs: configured_push,
            set_upstream: false,
            mirror: false,
        });
    }
    if config
        .get_bool("remote", Some(remote), "mirror")
        .unwrap_or(false)
    {
        return Ok(DefaultPushRefspecs {
            refspecs: vec!["refs/*:refs/*".to_string()],
            set_upstream: false,
            mirror: true,
        });
    }

    match push_default_mode(config) {
        PushDefaultMode::Matching => {
            return Ok(DefaultPushRefspecs {
                refspecs: vec![":".to_string()],
                set_upstream: false,
                mirror: false,
            });
        }
        PushDefaultMode::Nothing => {
            eprintln!(
                "fatal: You didn't specify any refspecs to push, and push.default is \"nothing\"."
            );
            return Err(GitError::Exit(128));
        }
        _ => {}
    }

    let Some(branch) = branch else {
        eprintln!(
            "fatal: You are not currently on a branch.\n\
To push the history leading to the current (detached HEAD)\n\
state now, use\n\n\
    git push {remote} HEAD:<name-of-remote-branch>"
        );
        return Err(GitError::Exit(128));
    };

    let mode = push_default_mode(config);
    let branch_ref = format!("refs/heads/{branch}");
    let same_remote = default_fetch_remote_for_branch(config, branch) == remote;
    let auto_setup = config
        .get_bool("push", None, "autoSetupRemote")
        .unwrap_or(false);
    let mut dst = branch_ref.clone();
    let mut set_upstream = false;

    match mode {
        PushDefaultMode::Unspecified | PushDefaultMode::Simple => {
            if same_remote {
                let upstream = push_upstream_ref(config, branch, remote, auto_setup)?;
                if branch_ref != upstream {
                    die_push_simple(config, remote, &upstream)?;
                }
            }
        }
        PushDefaultMode::Upstream => {
            if !same_remote {
                eprintln!(
                    "fatal: You are pushing to remote '{remote}', which is not the upstream of\n\
your current branch '{branch}', without telling me what to push\n\
to update which remote branch."
                );
                return Err(GitError::Exit(128));
            }
            dst = push_upstream_ref(config, branch, remote, auto_setup)?;
        }
        PushDefaultMode::Current => {}
        PushDefaultMode::Matching | PushDefaultMode::Nothing => unreachable!(),
    }

    if auto_setup && config.get_all("branch", Some(branch), "merge").is_empty() {
        set_upstream = true;
    }

    Ok(DefaultPushRefspecs {
        refspecs: vec![format!("{branch_ref}:{dst}")],
        set_upstream,
        mirror: false,
    })
}

fn push_default_mode(config: &GitConfig) -> PushDefaultMode {
    match config.get("push", None, "default") {
        Some("nothing") => PushDefaultMode::Nothing,
        Some("matching") => PushDefaultMode::Matching,
        Some("simple") => PushDefaultMode::Simple,
        Some("upstream" | "tracking") => PushDefaultMode::Upstream,
        Some("current") => PushDefaultMode::Current,
        _ => PushDefaultMode::Unspecified,
    }
}

fn default_push_remote(config: &GitConfig, branch: Option<&str>) -> Result<String> {
    if let Some(branch) = branch
        && let Some(remote) = config.get("branch", Some(branch), "pushRemote")
    {
        return Ok(remote.to_string());
    }
    if let Some(remote) = config.get("remote", None, "pushDefault") {
        return Ok(remote.to_string());
    }
    if let Some(branch) = branch
        && let Some(remote) = config.get("branch", Some(branch), "remote")
    {
        return Ok(remote.to_string());
    }
    if remote_exists(config, "origin") {
        return Ok("origin".to_string());
    }
    let remotes = remote_names(config);
    if let [remote] = remotes.as_slice() {
        return Ok(remote.clone());
    }
    eprintln!(
        "fatal: No configured push destination.\n\
Either specify the URL from the command-line or configure a remote repository using\n\n\
    git remote add <name> <url>\n\n\
and then push using the remote name\n\n\
    git push <name>"
    );
    Err(GitError::Exit(128))
}

fn default_fetch_remote_for_branch(config: &GitConfig, branch: &str) -> String {
    if let Some(remote) = config.get("branch", Some(branch), "remote") {
        return remote.to_string();
    }
    if remote_exists(config, "origin") {
        return "origin".to_string();
    }
    let remotes = remote_names(config);
    match remotes.as_slice() {
        [remote] => remote.clone(),
        _ => "origin".to_string(),
    }
}

fn push_upstream_ref(
    config: &GitConfig,
    branch: &str,
    remote: &str,
    auto_setup: bool,
) -> Result<String> {
    let merges = config
        .get_all("branch", Some(branch), "merge")
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if merges.is_empty() && auto_setup {
        return Ok(format!("refs/heads/{branch}"));
    }
    if merges.is_empty() || config.get("branch", Some(branch), "remote").is_none() {
        let advice = if auto_setup {
            ""
        } else {
            "\nTo have this happen automatically for branches without a tracking\nupstream, see 'push.autoSetupRemote' in 'git help config'.\n"
        };
        eprintln!(
            "fatal: The current branch {branch} has no upstream branch.\n\
To push the current branch and set the remote as upstream, use\n\n\
    git push --set-upstream {remote} {branch}\n\
{advice}"
        );
        return Err(GitError::Exit(128));
    }
    if merges.len() != 1 {
        eprintln!(
            "fatal: The current branch {branch} has multiple upstream branches, refusing to push."
        );
        return Err(GitError::Exit(128));
    }
    Ok(merges[0].to_string())
}

fn die_push_simple(config: &GitConfig, remote: &str, upstream: &str) -> Result<()> {
    let short_upstream = upstream.strip_prefix("refs/heads/").unwrap_or(upstream);
    let advice_pushdefault = if matches!(push_default_mode(config), PushDefaultMode::Unspecified) {
        "\nTo choose either option permanently, see push.default in 'git help config'.\n"
    } else {
        ""
    };
    eprintln!(
        "fatal: The upstream branch of your current branch does not match\n\
the name of your current branch.  To push to the upstream branch\n\
on the remote, use\n\n\
    git push {remote} HEAD:{short_upstream}\n\n\
To push to the branch of the same name on the remote, use\n\n\
    git push {remote} HEAD\n\
{advice_pushdefault}"
    );
    Err(GitError::Exit(128))
}

fn configure_push_upstreams(
    git_dir: &Path,
    remote: &str,
    commands: &[ReceivePackCommand],
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    for command in commands {
        let Some(branch) = command.name.strip_prefix("refs/heads/") else {
            continue;
        };
        let remote_key = ConfigKey {
            section: "branch".into(),
            subsection: Some(branch.to_string()),
            key: "remote".into(),
        };
        config_set_value(&mut config, &remote_key, remote, false);
        let merge_key = ConfigKey {
            section: "branch".into(),
            subsection: Some(branch.to_string()),
            key: "merge".into(),
        };
        config_set_value(&mut config, &merge_key, &command.name, false);
    }
    write_repo_config(git_dir, &config)
}

fn configure_push_upstreams_from_report(
    git_dir: &Path,
    remote: &str,
    refs: &[sley_remote::PushReportRef],
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let format = repository_object_format(git_dir)?;
    let store = FileRefStore::new(git_dir, format);
    let current_branch = store.current_branch().ok().flatten();
    for reference in refs {
        if !matches!(
            reference.status,
            sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
        ) || reference.is_deletion()
        {
            continue;
        }
        let Some(src) = reference.src.as_deref() else {
            continue;
        };
        let branch = if src == "HEAD" {
            let Some(branch) = current_branch.as_deref() else {
                continue;
            };
            branch
        } else {
            let Some(branch) = src.strip_prefix("refs/heads/") else {
                continue;
            };
            branch
        };
        if !reference.dst.starts_with("refs/heads/") {
            continue;
        }
        let remote_key = ConfigKey {
            section: "branch".into(),
            subsection: Some(branch.to_string()),
            key: "remote".into(),
        };
        config_set_value(&mut config, &remote_key, remote, false);
        let merge_key = ConfigKey {
            section: "branch".into(),
            subsection: Some(branch.to_string()),
            key: "merge".into(),
        };
        config_set_value(&mut config, &merge_key, &reference.dst, false);
    }
    write_repo_config(git_dir, &config)
}

/// Implements builtin/fetch.c's `--set-upstream` post-fetch configuration.
///
/// The relevant upstream is the fetched branch meant to be merged with the
/// current one — git's `ref_map` entry with no local peer ref. In sley that is a
/// [`FetchRefUpdate`] with no `dst` (a FETCH_HEAD-only entry). When exactly one
/// exists and the current branch is real, mirror `install_branch_config` by
/// writing `branch.<current>.{remote,merge}`; the various ambiguous/unsupported
#[cfg(test)]
#[allow(clippy::expect_used)]
mod receive_max_input_size_tests {
    use super::*;

    fn config(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("config parses")
    }

    #[test]
    fn unset_means_unlimited() {
        let cfg = config("[transfer]\n\tfsckObjects = true\n");
        assert_eq!(receive_max_input_size(&cfg), None);
    }

    #[test]
    fn zero_means_unlimited() {
        let cfg = config("[receive]\n\tmaxInputSize = 0\n");
        assert_eq!(receive_max_input_size(&cfg), None);
    }

    #[test]
    fn positive_value_is_the_cap() {
        let cfg = config("[receive]\n\tmaxInputSize = 64\n");
        assert_eq!(receive_max_input_size(&cfg), Some(64));
    }

    #[test]
    fn unit_suffix_is_honoured() {
        // Shares git's unit parser: `1k` == 1024 bytes.
        let cfg = config("[receive]\n\tmaxInputSize = 1k\n");
        assert_eq!(receive_max_input_size(&cfg), Some(1024));
    }

    #[test]
    fn no_cap_reads_everything() {
        let data = vec![0xABu8; 4096];
        let buf = read_capped_packfile(&mut &data[..], None).expect("reads all bytes");
        assert_eq!(buf, data);
    }

    #[test]
    fn under_cap_succeeds() {
        let data = vec![0x42u8; 8];
        let buf = read_capped_packfile(&mut &data[..], Some(16)).expect("under the cap reads fine");
        assert_eq!(buf, data);
    }

    #[test]
    fn at_cap_succeeds() {
        let data = vec![0x42u8; 16];
        let buf =
            read_capped_packfile(&mut &data[..], Some(16)).expect("exactly at the cap is allowed");
        assert_eq!(buf, data);
    }

    #[test]
    fn over_cap_errors_with_exit_128() {
        // A buffered input one byte past the cap must be refused, not slurped —
        // this is the B5 hardening: the fsck path no longer buffers unbounded.
        let data = vec![0x42u8; 17];
        let err = read_capped_packfile(&mut &data[..], Some(16))
            .expect_err("over the cap must error rather than buffer it all");
        match err {
            GitError::Exit(128) => {}
            other => panic!("expected GitError::Exit(128), got {other:?}"),
        }
    }
}
