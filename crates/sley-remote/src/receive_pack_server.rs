//! Full receive-pack server: hooks, proc-receive, ref updates, and status reports.

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
use sley_refs::{FileRefStore, RefTarget};

use crate::local::{apply_receive_pack_ref_transaction, receive_pack_features};
use crate::proc_receive::{
    ProcReceiveHookInput, ReceivePackCommandState, apply_proc_receive_hook_failure,
    mark_proc_receive_commands, parse_proc_receive_refs, run_proc_receive_hook,
};
use crate::receive_hooks::{
    run_post_receive, run_post_update, run_pre_receive, run_push_to_checkout, run_update_hooks,
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
    /// Receive/transfer fsck policy resolved before advertising refs, so
    /// malformed configuration fails before protocol I/O.
    pub validation: &'a sley_fsck::FsckPolicy,
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

pub fn serve_receive_pack(
    request: ReceivePackServerRequest<'_>,
) -> Result<ReceivePackServerOutcome> {
    let mut discard_stderr = Vec::new();
    let capture_stderr = request.options.remote_stderr.is_some();
    let remote_stderr = request.options.remote_stderr.unwrap_or(&mut discard_stderr);

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

    // transfer.hideRefs / receive.hideRefs: deny updates matching patterns
    // using git's full-vs-stripped subject rules (namespace-aware).
    let hidden_patterns = crate::local::transfer_receive_hidden_ref_patterns(request.config);
    if !hidden_patterns.is_empty() {
        for state in &mut command_states {
            if state.error_string.is_some() {
                continue;
            }
            let logical = state.command.name.as_str();
            let physical = sley_core::expand_namespace(logical);
            if sley_core::ref_is_hidden(Some(logical), &physical, &hidden_patterns) {
                let reason = if state.command.new_id.is_null() {
                    "deny deleting a hidden ref"
                } else {
                    "deny updating a hidden ref"
                };
                state.error_string = Some(reason.into());
            }
        }
    }

    // receive.denyCurrentBranch: refuse (or allow with updateInstead) updates
    // to the currently checked-out branch. Matching is on the logical
    // (namespace-stripped) name against the worktree HEAD, same as git.
    for state in &mut command_states {
        if state.error_string.is_some() || state.command.new_id.is_null() {
            continue;
        }
        if !state.command.name.starts_with("refs/heads/") {
            continue;
        }
        if receive_denies_current_branch_for_server(
            request.git_dir,
            request.format,
            request.config,
            &state.command.name,
        ) {
            state.error_string = Some("branch is currently checked out".into());
        }
    }

    let proc_patterns = parse_proc_receive_refs(request.config);
    let run_proc_receive = mark_proc_receive_commands(&proc_patterns, &mut command_states);

    let mut unpack_error = None;
    let mut quarantine = None;
    if needs_pack_data(&command_states) {
        let incoming = sley_odb::IncomingPackQuarantine::new(request.git_dir, request.format)?;
        let incoming_db = incoming.database();
        match install_pack_from_reader(&incoming_db, request.pack_reader) {
            Ok(installed) => {
                let roots = command_states
                    .iter()
                    .filter(|state| !state.command.new_id.is_null())
                    .map(|state| state.command.new_id)
                    .collect::<Vec<_>>();
                let validation = validate_receive_objects(
                    &incoming_db,
                    request.format,
                    &roots,
                    &installed,
                    request.validation,
                    remote_stderr,
                );
                if request.validation.enabled {
                    if validation.is_err() {
                        unpack_error = Some("unpacker error".to_string());
                    } else {
                        quarantine = Some(incoming);
                    }
                } else if validation.is_err() {
                    for state in &mut command_states {
                        if state.error_string.is_none() && !state.command.new_id.is_null() {
                            state.error_string = Some("missing necessary objects".into());
                        }
                    }
                } else {
                    quarantine = Some(incoming);
                }
            }
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
        } else if let Some(name) = run_update_hooks(
            request.git_dir,
            &ok_commands,
            &quarantine_env,
            remote_stderr,
            capture_stderr,
        )? {
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
            quarantine_env: &quarantine_env,
            remote_stderr,
            capture_stderr,
        })?;
        command_states = output.commands;
        apply_proc_receive_hook_failure(&mut command_states, use_atomic, output.hook_failed);
    }

    let accepts_incoming_objects = command_states
        .iter()
        .any(|state| state.error_string.is_none() && !state.command.new_id.is_null());
    if unpack_error.is_none()
        && accepts_incoming_objects
        && let Some(incoming) = quarantine.take()
        && let Err(err) = incoming.promote()
    {
        let message = err.to_string();
        unpack_error = Some(message.clone());
        for state in &mut command_states {
            if state.error_string.is_none() && !state.command.new_id.is_null() {
                state.error_string = Some(message.clone());
            }
        }
    }

    if unpack_error.is_none()
        && let Err(err) = apply_command_updates(request.git_dir, request.format, &command_states)
    {
        let message = err.to_string();
        for state in &mut command_states {
            if state.error_string.is_none() && !state.defer_ref_update() {
                state.error_string = Some(message.clone());
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
    let _ = run_push_to_checkout(git_dir, remote_stderr, capture_stderr);
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

/// Write hook stderr captured during receive-pack as sideband-64k progress packets.
pub fn write_receive_pack_sideband_stderr(writer: &mut impl Write, stderr: &[u8]) -> Result<()> {
    if stderr.is_empty() {
        return Ok(());
    }
    for chunk in stderr.chunks(SIDEBAND_PAYLOAD_CHUNK) {
        write_sideband_packet(
            writer,
            &SideBandPacket {
                channel: SideBandChannel::Progress,
                data: chunk.to_vec(),
            },
        )?;
    }
    Ok(())
}

/// Flush a sideband-64k receive-pack response stream (after progress + report packets).
pub fn flush_receive_pack_sideband(writer: &mut impl Write) -> Result<()> {
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn write_receive_pack_server_report(
    writer: &mut impl Write,
    report: &ReceivePackServerReport,
    use_sideband: bool,
    flush_stream: bool,
) -> Result<()> {
    match report {
        ReceivePackServerReport::V1(status) => {
            if use_sideband {
                write_report_sideband(
                    writer,
                    |buf| write_receive_pack_report_status(buf, status),
                    flush_stream,
                )
            } else {
                write_receive_pack_report_status(writer, status)
            }
        }
        ReceivePackServerReport::V2(status) => {
            if use_sideband {
                write_report_sideband(
                    writer,
                    |buf| write_receive_pack_report_status_v2(buf, status),
                    flush_stream,
                )
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

/// Maximum sideband payload bytes per packet (git `LARGE_PACKET_MAX - 5`).
const SIDEBAND_PAYLOAD_CHUNK: usize = 65_515;

fn write_report_sideband(
    writer: &mut impl Write,
    write_report: impl FnOnce(&mut Vec<u8>) -> Result<()>,
    flush_stream: bool,
) -> Result<()> {
    let mut payload = Vec::new();
    write_report(&mut payload)?;
    for chunk in payload.chunks(SIDEBAND_PAYLOAD_CHUNK) {
        write_sideband_packet(
            writer,
            &SideBandPacket {
                channel: SideBandChannel::Data,
                data: chunk.to_vec(),
            },
        )?;
    }
    if flush_stream {
        writer.write_all(b"0000")?;
    }
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

/// Whether receive-pack should refuse an update to the currently checked-out
/// branch under `receive.denyCurrentBranch`. Unconfigured / refuse / true deny;
/// `updateInstead` / `warn` / `ignore` allow the update through.
/// Checks the main worktree and every linked worktree (t5516 worktrees).
fn receive_denies_current_branch_for_server(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    logical_ref: &str,
) -> bool {
    if !logical_ref.starts_with("refs/heads/") {
        return false;
    }
    let deny = config
        .get("receive", None, "denycurrentbranch")
        .unwrap_or("refuse");
    let denies = matches!(
        deny.to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1" | "refuse"
    );
    if !denies {
        return false;
    }
    branch_checked_out_anywhere(git_dir, format, logical_ref)
}

fn branch_checked_out_anywhere(git_dir: &Path, format: ObjectFormat, logical_ref: &str) -> bool {
    let store = FileRefStore::new(git_dir, format);
    if matches!(
        store.read_ref("HEAD").ok().flatten(),
        Some(RefTarget::Symbolic(target)) if target == logical_ref
    ) && sley_worktree::worktree_root_for_git_dir(git_dir)
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }
    let common = sley_odb::repository_common_dir(git_dir);
    let worktrees_dir = common.join("worktrees");
    let Ok(entries) = std::fs::read_dir(worktrees_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let wt_git_dir = entry.path();
        if !wt_git_dir.is_dir() {
            continue;
        }
        let wt_store = FileRefStore::new(&wt_git_dir, format);
        if matches!(
            wt_store.read_ref("HEAD").ok().flatten(),
            Some(RefTarget::Symbolic(target)) if target == logical_ref
        ) {
            return true;
        }
    }
    false
}

fn needs_pack_data(states: &[ReceivePackCommandState]) -> bool {
    states
        .iter()
        .any(|state| state.error_string.is_none() && !state.command.new_id.is_null())
}

fn install_pack_from_reader(
    db: &FileObjectDatabase,
    reader: &mut dyn Read,
) -> Result<Vec<sley_core::ObjectId>> {
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    }
    if &prefix != b"PACK" {
        return Err(GitError::InvalidFormat(
            "receive-pack packfile must start with PACK".into(),
        ));
    }
    let mut stream = Cursor::new(prefix).chain(reader);
    db.install_raw_pack_from_reader(&mut stream)
        .map(|installed| installed.object_ids)
}

fn validate_receive_objects(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    roots: &[sley_core::ObjectId],
    installed: &[sley_core::ObjectId],
    policy: &sley_fsck::FsckPolicy,
    remote_stderr: &mut Vec<u8>,
) -> Result<()> {
    let report = sley_fsck::fsck_objects_with_options(
        db,
        format,
        roots.iter().copied(),
        installed.iter().copied(),
        policy.fsck_options(policy.enabled),
    );
    for diagnostic in &policy.diagnostics {
        remote_stderr.extend_from_slice(diagnostic.as_bytes());
        remote_stderr.push(b'\n');
    }
    for issue in &report.issues {
        remote_stderr.extend_from_slice(issue.message.as_bytes());
        remote_stderr.push(b'\n');
    }
    if report.is_ok() {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

fn apply_command_updates(
    git_dir: &Path,
    format: ObjectFormat,
    states: &[ReceivePackCommandState],
) -> Result<()> {
    // Expand logical client-side names into the active namespace before writing
    // (git's receive-pack `namespaced_name = namespace + name`).
    let applicable: Vec<ReceivePackCommand> = states
        .iter()
        .filter(|state| state.error_string.is_none() && !state.defer_ref_update())
        .map(|state| {
            let mut command = state.command.clone();
            command.name = sley_core::expand_namespace(&command.name);
            command
        })
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
        Ok(ReceivePackServerReport::V2(ReceivePackReportStatusV2 {
            unpack,
            commands,
        }))
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
        Ok(ReceivePackServerReport::V1(ReceivePackReportStatus {
            unpack,
            commands,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_object::{EncodedObject, ObjectType, Tree};
    use sley_pack::PackFile;
    use sley_refs::RefTarget;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn receive_quarantine_discards_rejected_and_promotes_accepted_pack() {
        let format = ObjectFormat::Sha1;
        let rejected_repo = temp_repo("receive-quarantine-rejected");
        let tree = EncodedObject::new(ObjectType::Tree, Tree { entries: vec![] }.write());
        let tree_oid = tree.object_id(format).expect("tree oid");
        let malformed = EncodedObject::new(
            ObjectType::Commit,
            format!(
                "tree {tree_oid}\nauthor Bugs Bunny 1 +0000\ncommitter Bugs <bugs@example.invalid> 1 +0000\n\nbad\n"
            )
            .into_bytes(),
        );
        let malformed_oid = malformed.object_id(format).expect("malformed oid");
        let rejected_pack =
            PackFile::write_packed(&[&tree, &malformed], format).expect("rejected pack");
        let strict_config =
            GitConfig::parse(b"[receive]\n\tfsckObjects = true\n").expect("strict receive config");
        let strict_policy = sley_fsck::FsckPolicy::from_config(
            &strict_config,
            sley_fsck::FsckConfigKind::Receive,
            format,
            &rejected_repo,
            false,
        )
        .expect("strict receive policy");
        let rejected = serve_test_pack(
            &rejected_repo,
            format,
            malformed_oid,
            rejected_pack.pack,
            &strict_config,
            &strict_policy,
        );
        assert!(matches!(
            rejected.report,
            ReceivePackServerReport::V1(ReceivePackReportStatus {
                unpack: ReceivePackUnpackStatus::Error(_),
                ..
            })
        ));
        let rejected_db = FileObjectDatabase::from_git_dir(&rejected_repo, format);
        assert!(
            !rejected_db
                .contains(&malformed_oid)
                .expect("rejected object lookup")
        );
        assert_eq!(
            FileRefStore::new(&rejected_repo, format)
                .read_ref("refs/heads/main")
                .expect("rejected ref lookup"),
            None
        );

        let accepted_repo = temp_repo("receive-quarantine-accepted");
        let valid = EncodedObject::new(
            ObjectType::Commit,
            format!(
                "tree {tree_oid}\nauthor A <a@example.invalid> 1 +0000\ncommitter A <a@example.invalid> 1 +0000\n\nok\n"
            )
            .into_bytes(),
        );
        let valid_oid = valid.object_id(format).expect("valid oid");
        let accepted_pack =
            PackFile::write_packed(&[&tree, &valid], format).expect("accepted pack");
        let accepted = serve_test_pack(
            &accepted_repo,
            format,
            valid_oid,
            accepted_pack.pack,
            &strict_config,
            &strict_policy,
        );
        assert!(matches!(
            accepted.report,
            ReceivePackServerReport::V1(ReceivePackReportStatus {
                unpack: ReceivePackUnpackStatus::Ok,
                ..
            })
        ));
        let accepted_db = FileObjectDatabase::from_git_dir(&accepted_repo, format);
        assert!(
            accepted_db
                .contains(&valid_oid)
                .expect("accepted object lookup")
        );
        assert_eq!(
            FileRefStore::new(&accepted_repo, format)
                .read_ref("refs/heads/main")
                .expect("accepted ref lookup"),
            Some(RefTarget::Direct(valid_oid))
        );
    }

    fn serve_test_pack(
        git_dir: &Path,
        format: ObjectFormat,
        new_id: sley_core::ObjectId,
        pack: Vec<u8>,
        config: &GitConfig,
        policy: &sley_fsck::FsckPolicy,
    ) -> ReceivePackServerOutcome {
        let header = ReceivePackPushRequestHeader {
            commands: sley_protocol::ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: sley_core::ObjectId::null(format),
                    new_id,
                    name: "refs/heads/main".into(),
                }],
                ..Default::default()
            },
            push_options: None,
        };
        let mut reader = Cursor::new(pack);
        serve_receive_pack(ReceivePackServerRequest {
            git_dir,
            format,
            header: &header,
            pack_reader: &mut reader,
            config,
            validation: policy,
            options: ReceivePackServerOptions {
                quiet: true,
                remote_stderr: None,
                run_post_hooks: false,
            },
        })
        .expect("serve receive pack")
    }

    fn temp_repo(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sley-{name}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(path.join("objects/pack")).expect("repo object directory");
        fs::create_dir_all(path.join("refs/heads")).expect("repo refs directory");
        fs::write(path.join("HEAD"), b"ref: refs/heads/main\n").expect("repo HEAD");
        path
    }
}
