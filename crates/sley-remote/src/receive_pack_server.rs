//! Full receive-pack server: hooks, proc-receive, ref updates, and status reports.

use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, Result};
use sley_odb::{FileObjectDatabase, repository_common_dir};
use sley_protocol::{
    ReceivePackCommand, ReceivePackCommandStatus, ReceivePackCommandStatusV2,
    ReceivePackCommandStatusV2Options, ReceivePackPushRequest, ReceivePackPushRequestHeader,
    ReceivePackReportStatus, ReceivePackReportStatusV2, ReceivePackUnpackStatus, SideBandChannel,
    SideBandPacket, validate_receive_pack_push_request_features, write_receive_pack_report_status,
    write_receive_pack_report_status_v2, write_sideband_packet,
};
use sley_refs::FileRefStore;

use crate::local::{apply_receive_pack_ref_transaction, receive_pack_features};
use crate::push::stage_local_push_quarantine;
use crate::proc_receive::{
    ProcReceiveHookInput, ReceivePackCommandState, apply_proc_receive_hook_failure,
    mark_proc_receive_commands, parse_proc_receive_refs, run_proc_receive_hook,
};
use crate::receive_hooks::{
    hook_exists, run_post_receive, run_post_update, run_pre_receive, run_update_hooks,
};


pub struct ReceivePackServerOptions<'a> {
    pub quiet: bool,
    /// When set, traditional receive-pack hook stderr is captured for the push
    /// client to print as `remote:` lines (in-process local transport).
    pub remote_stderr: Option<&'a mut Vec<u8>>,
    /// Run post-receive/post-update after ref updates. The local push client
    /// runs these itself so hook stdin matches send-pack ordering.
    pub run_post_hooks: bool,
}

pub struct ReceivePackServerRequest<'a> {
    pub git_dir: &'a Path,
    pub format: ObjectFormat,
    pub header: &'a ReceivePackPushRequestHeader,
    pub pack_reader: &'a mut dyn Read,
    pub config: &'a GitConfig,
    pub options: ReceivePackServerOptions<'a>,
}

pub enum ReceivePackServerReport {
    V1(ReceivePackReportStatus),
    V2(ReceivePackReportStatusV2),
}

pub struct ReceivePackServerOutcome {
    pub report: ReceivePackServerReport,
    pub command_states: Vec<ReceivePackCommandState>,
}

pub fn serve_receive_pack(request: ReceivePackServerRequest<'_>) -> Result<ReceivePackServerOutcome> {
    let mut discard_stderr = Vec::new();
    let capture_stderr = request.options.remote_stderr.is_some();
    let remote_stderr = request
        .options
        .remote_stderr
        .unwrap_or(&mut discard_stderr);

    validate_receive_pack_push_request_features(
        &receive_pack_features(request.format),
        &ReceivePackPushRequest {
            commands: request.header.commands.clone(),
            push_options: request.header.push_options.clone(),
            packfile: Vec::new(),
        },
    )?;

    let use_atomic = request_uses_atomic(request.header);
    let use_report_v2 = request_uses_report_status_v2(request.header);
    let push_options = request.header.push_options.as_deref().unwrap_or(&[]);

    let mut command_states: Vec<ReceivePackCommandState> = request
        .header
        .commands
        .commands
        .iter()
        .cloned()
        .map(ReceivePackCommandState::new)
        .collect();

    let proc_patterns = parse_proc_receive_refs(request.config);
    let run_proc_receive = mark_proc_receive_commands(&proc_patterns, &mut command_states);

    let mut unpack_error = None;
    if needs_pack_data(&command_states) {
        match install_pack_from_reader(request.git_dir, request.format, request.pack_reader) {
            Ok(()) => {}
            Err(err) => {
                let message = err.to_string();
                unpack_error = Some(message.clone());
                for state in &mut command_states {
                    if state.error_string.is_none() {
                        state.error_string = Some(message.clone());
                    }
                }
            }
        }
    }

    let ok_commands: Vec<ReceivePackCommand> = command_states
        .iter()
        .filter(|state| state.error_string.is_none())
        .map(|state| state.command.clone())
        .collect();

    let quarantine = if unpack_error.is_none()
        && !ok_commands.is_empty()
        && receive_pre_hooks_may_run(request.git_dir)
    {
        let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
        let common_git_dir = repository_common_dir(request.git_dir);
        stage_local_push_quarantine(
            request.git_dir,
            &common_git_dir,
            request.format,
            &local_db,
            &ok_commands,
        )?
    } else {
        None
    };
    let quarantine_env = quarantine
        .as_ref()
        .map(|q| quarantine_hook_env(request.git_dir, q.object_dir()))
        .unwrap_or_default();

    if unpack_error.is_none() && !ok_commands.is_empty() {
        if run_pre_receive(
            request.git_dir,
            &ok_commands,
            push_options,
            &quarantine_env,
            remote_stderr,
            capture_stderr,
        )
        .is_err()
        {
            for state in &mut command_states {
                if state.error_string.is_none() {
                    state.error_string = Some("pre-receive hook declined".into());
                }
            }
        } else if let Some(name) =
            run_update_hooks(
                request.git_dir,
                &ok_commands,
                &quarantine_env,
                remote_stderr,
                capture_stderr,
            )?
        {
            for state in &mut command_states {
                if state.error_string.is_none() {
                    if use_atomic {
                        state.error_string = Some("atomic push failure".into());
                    } else if state.command.name == name {
                        state.error_string = Some("hook declined".into());
                    }
                }
            }
        }
    }

    if unpack_error.is_none() && run_proc_receive {
        let output = run_proc_receive_hook(ProcReceiveHookInput {
            git_dir: request.git_dir,
            format: request.format,
            commands: &command_states,
            push_options,
            use_atomic,
            use_push_options: request_uses_push_options(request.header),
            remote_stderr,
            capture_stderr,
        })?;
        command_states = output.commands;
        apply_proc_receive_hook_failure(&mut command_states, use_atomic, output.hook_failed);
    }

    if unpack_error.is_none() {
        if let Err(err) = apply_command_updates(request.git_dir, request.format, &command_states) {
            let message = err.to_string();
            for state in &mut command_states {
                if state.error_string.is_none() && !state.defer_ref_update() {
                    state.error_string = Some(message.clone());
                }
            }
        }
    }

    if request.options.run_post_hooks {
        run_receive_pack_post_hooks(
            request.git_dir,
            &command_states,
            push_options,
            remote_stderr,
            capture_stderr,
        );
    }

    let report = build_report(&command_states, unpack_error, use_report_v2)?;
    Ok(ReceivePackServerOutcome {
        report,
        command_states,
    })
}

pub fn run_receive_pack_post_hooks(
    git_dir: &Path,
    command_states: &[ReceivePackCommandState],
    push_options: &[String],
    remote_stderr: &mut Vec<u8>,
    capture_stderr: bool,
) {
    let landed: Vec<ReceivePackCommandState> = command_states
        .iter()
        .filter(|state| {
            state.error_string.is_none() && state.command.old_id != state.command.new_id
        })
        .cloned()
        .collect();
    if landed.is_empty() {
        return;
    }
    let _ = run_post_receive(
        git_dir,
        &landed,
        push_options,
        remote_stderr,
        capture_stderr,
    );
    let _ = run_post_update(git_dir, &landed, remote_stderr, capture_stderr);
}

pub fn receive_pack_server_report_v1(report: &ReceivePackServerReport) -> ReceivePackReportStatus {
    match report {
        ReceivePackServerReport::V1(status) => status.clone(),
        ReceivePackServerReport::V2(status) => ReceivePackReportStatus {
            unpack: status.unpack.clone(),
            commands: status
                .commands
                .iter()
                .map(|command| match command {
                    ReceivePackCommandStatusV2::Ok { name, .. } => {
                        ReceivePackCommandStatus::Ok { name: name.clone() }
                    }
                    ReceivePackCommandStatusV2::Ng { name, message } => {
                        ReceivePackCommandStatus::Ng {
                            name: name.clone(),
                            message: message.clone(),
                        }
                    }
                })
                .collect(),
        },
    }
}

pub fn write_receive_pack_server_report(
    writer: &mut impl Write,
    report: &ReceivePackServerReport,
    use_sideband: bool,
) -> Result<()> {
    match report {
        ReceivePackServerReport::V1(status) => {
            if use_sideband {
                write_report_sideband(writer, |buf| write_receive_pack_report_status(buf, status))
            } else {
                write_receive_pack_report_status(writer, status)
            }
        }
        ReceivePackServerReport::V2(status) => {
            if use_sideband {
                write_report_sideband(writer, |buf| {
                    write_receive_pack_report_status_v2(buf, status)
                })
            } else {
                write_receive_pack_report_status_v2(writer, status)
            }
        }
    }
}

pub fn request_uses_sideband(header: &ReceivePackPushRequestHeader) -> bool {
    header
        .commands
        .capabilities
        .iter()
        .any(|cap| cap.name == "side-band-64k")
}

fn write_report_sideband(
    writer: &mut impl Write,
    write_report: impl FnOnce(&mut Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut payload = Vec::new();
    write_report(&mut payload)?;
    for chunk in payload.chunks(65516) {
        write_sideband_packet(
            writer,
            &SideBandPacket {
                channel: SideBandChannel::Data,
                data: chunk.to_vec(),
            },
        )?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

fn request_uses_atomic(header: &ReceivePackPushRequestHeader) -> bool {
    header
        .commands
        .capabilities
        .iter()
        .any(|cap| cap.name == "atomic")
}

fn request_uses_report_status_v2(header: &ReceivePackPushRequestHeader) -> bool {
    header
        .commands
        .capabilities
        .iter()
        .any(|cap| cap.name == "report-status-v2")
}

fn request_uses_push_options(header: &ReceivePackPushRequestHeader) -> bool {
    header
        .commands
        .capabilities
        .iter()
        .any(|cap| cap.name == "push-options")
}

fn receive_pre_hooks_may_run(git_dir: &Path) -> bool {
    hook_exists(git_dir, "pre-receive") || hook_exists(git_dir, "update")
}

fn quarantine_hook_env(git_dir: &Path, object_dir: &Path) -> Vec<(String, String)> {
    let alternate = repository_common_dir(git_dir)
        .join("objects")
        .to_string_lossy()
        .into_owned();
    let object_dir = object_dir.to_string_lossy().into_owned();
    vec![
        ("GIT_OBJECT_DIRECTORY".into(), object_dir.clone()),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES".into(), alternate),
        ("GIT_QUARANTINE_PATH".into(), object_dir),
    ]
}

fn needs_pack_data(states: &[ReceivePackCommandState]) -> bool {
    states.iter().any(|state| {
        state.error_string.is_none() && !state.command.new_id.is_null()
    })
}

fn install_pack_from_reader(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &mut dyn Read,
) -> Result<()> {
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(err) => return Err(err.into()),
    }
    if &prefix != b"PACK" {
        return Err(GitError::InvalidFormat(
            "receive-pack packfile must start with PACK".into(),
        ));
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut stream = Cursor::new(prefix).chain(reader);
    db.install_raw_pack_from_reader(&mut stream).map(|_| ())
}

fn apply_command_updates(
    git_dir: &Path,
    format: ObjectFormat,
    states: &[ReceivePackCommandState],
) -> Result<()> {
    let applicable: Vec<ReceivePackCommand> = states
        .iter()
        .filter(|state| state.error_string.is_none() && !state.defer_ref_update())
        .map(|state| state.command.clone())
        .collect();
    if applicable.is_empty() {
        return Ok(());
    }

    let store = FileRefStore::new(git_dir, format);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    for command in applicable.iter().filter(|c| !c.new_id.is_null()) {
        if !db.contains(&command.new_id)? {
            return Err(GitError::InvalidObject(format!(
                "receive-pack packfile did not provide {}",
                command.new_id
            )));
        }
    }

    for command in applicable.iter().filter(|c| c.new_id.is_null()) {
        let current = store.read_ref(&command.name)?;
        if !command.old_id.is_null() {
            let Some(sley_refs::RefTarget::Direct(oid)) = current else {
                return Err(GitError::Transaction(format!(
                    "expected ref {} to match",
                    command.name
                )));
            };
            if oid != command.old_id {
                return Err(GitError::Transaction(format!(
                    "expected ref {} to match",
                    command.name
                )));
            }
        }
    }

    let updates: Vec<ReceivePackCommand> = applicable
        .iter()
        .filter(|c| !c.new_id.is_null())
        .cloned()
        .collect();
    apply_receive_pack_ref_transaction(git_dir, format, &store, &updates, &applicable)?;
    Ok(())
}

fn build_report(
    command_states: &[ReceivePackCommandState],
    unpack_error: Option<String>,
    use_report_v2: bool,
) -> Result<ReceivePackServerReport> {
    let unpack = match unpack_error {
        None => ReceivePackUnpackStatus::Ok,
        Some(message) => ReceivePackUnpackStatus::Error(message),
    };
    if use_report_v2 {
        let mut commands = Vec::new();
        for state in command_states {
            if let Some(message) = &state.error_string {
                commands.push(ReceivePackCommandStatusV2::Ng {
                    name: state.command.name.clone(),
                    message: message.clone(),
                });
                continue;
            }
            commands.push(ReceivePackCommandStatusV2::Ok {
                name: state.command.name.clone(),
                options: ReceivePackCommandStatusV2Options::default(),
            });
            for (index, report) in state.reports.iter().enumerate() {
                if index > 0 {
                    commands.push(ReceivePackCommandStatusV2::Ok {
                        name: state.command.name.clone(),
                        options: ReceivePackCommandStatusV2Options::default(),
                    });
                }
                if let Some(ReceivePackCommandStatusV2::Ok { options, .. }) = commands.last_mut() {
                    options.refname = report.refname.clone();
                    options.old_oid = report.old_oid.clone();
                    options.new_oid = report.new_oid.clone();
                    options.forced_update = report.forced_update;
                }
            }
        }
        Ok(ReceivePackServerReport::V2(ReceivePackReportStatusV2 { unpack, commands }))
    } else {
        let commands = command_states
            .iter()
            .map(|state| {
                if let Some(message) = &state.error_string {
                    ReceivePackCommandStatus::Ng {
                        name: state.command.name.clone(),
                        message: message.clone(),
                    }
                } else {
                    ReceivePackCommandStatus::Ok {
                        name: state.command.name.clone(),
                    }
                }
            })
            .collect();
        Ok(ReceivePackServerReport::V1(ReceivePackReportStatus { unpack, commands }))
    }
}