//! `proc-receive` hook protocol and `receive.procReceiveRefs` matching.
//!
//! Mirrors upstream `receive-pack.c` (`proc_receive_*`, `run_proc_receive_hook`,
//! `read_proc_receive_report`).

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::repository_common_dir;
use sley_protocol::{
    PktLineFrame, ReceivePackCommand, read_pkt_line_frame, read_pkt_line_frames_until_flush,
    write_pkt_line_payload,
};

const RUN_PROC_RECEIVE_SCHEDULED: u8 = 1;
const RUN_PROC_RECEIVE_RETURNED: u8 = 2;

/// One configured `receive.procReceiveRefs` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcReceiveRefPattern {
    pub prefix: String,
    pub want_add: bool,
    pub want_delete: bool,
    pub want_modify: bool,
    pub negative: bool,
}

/// Per-command proc-receive state carried through receive-pack execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcReceiveReport {
    pub refname: Option<String>,
    pub old_oid: Option<ObjectId>,
    pub new_oid: Option<ObjectId>,
    pub forced_update: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivePackCommandState {
    pub command: ReceivePackCommand,
    pub error_string: Option<String>,
    pub proc_receive: u8,
    pub was_proc_receive: bool,
    pub reports: Vec<ProcReceiveReport>,
}

impl ReceivePackCommandState {
    pub fn new(command: ReceivePackCommand) -> Self {
        Self {
            command,
            error_string: None,
            proc_receive: 0,
            was_proc_receive: false,
            reports: Vec::new(),
        }
    }

    pub fn scheduled_for_proc_receive(&self) -> bool {
        self.was_proc_receive
            && self.proc_receive & RUN_PROC_RECEIVE_RETURNED == 0
            && self.proc_receive & RUN_PROC_RECEIVE_SCHEDULED != 0
    }

    pub fn defer_ref_update(&self) -> bool {
        self.was_proc_receive && self.proc_receive != 0
    }

    pub fn expects_proc_receive_report(&self) -> bool {
        self.was_proc_receive && self.proc_receive != 0
    }
}

/// Load every `receive.procReceiveRefs` value from `config`.
pub fn parse_proc_receive_refs(config: &GitConfig) -> Vec<ProcReceiveRefPattern> {
    config
        .get_all("receive", None, "procReceiveRefs")
        .into_iter()
        .flatten()
        .map(parse_proc_receive_ref_value)
        .collect()
}

fn parse_proc_receive_ref_value(value: &str) -> ProcReceiveRefPattern {
    let (modifiers, prefix) = match value.split_once(':') {
        Some((mods, prefix)) => (mods, prefix),
        None => ("adm", value),
    };
    let mut pattern = ProcReceiveRefPattern {
        prefix: trim_ref_prefix(prefix),
        want_add: false,
        want_delete: false,
        want_modify: false,
        negative: false,
    };
    if modifiers.is_empty() {
        pattern.want_add = true;
        pattern.want_delete = true;
        pattern.want_modify = true;
    } else {
        for ch in modifiers.chars() {
            match ch {
                'a' => pattern.want_add = true,
                'd' => pattern.want_delete = true,
                'm' => pattern.want_modify = true,
                '!' => pattern.negative = true,
                _ => {}
            }
        }
    }
    if !pattern.want_add && !pattern.want_delete && !pattern.want_modify {
        pattern.want_add = true;
        pattern.want_delete = true;
        pattern.want_modify = true;
    }
    pattern
}

fn trim_ref_prefix(prefix: &str) -> String {
    let mut out = prefix.to_string();
    while out.ends_with('/') {
        out.pop();
    }
    out
}

pub fn proc_receive_ref_matches(patterns: &[ProcReceiveRefPattern], command: &ReceivePackCommand) -> bool {
    for pattern in patterns {
        if !pattern.want_add && command.old_id.is_null() {
            continue;
        }
        if !pattern.want_delete && command.new_id.is_null() {
            continue;
        }
        if !pattern.want_modify && !command.old_id.is_null() && !command.new_id.is_null() {
            continue;
        }
        let matched = command.name.starts_with(&pattern.prefix)
            && (command.name.len() == pattern.prefix.len()
                || command.name.as_bytes().get(pattern.prefix.len()) == Some(&b'/'));
        if pattern.negative {
            if !matched {
                return true;
            }
        } else if matched {
            return true;
        }
    }
    false
}

pub fn mark_proc_receive_commands(
    patterns: &[ProcReceiveRefPattern],
    commands: &mut [ReceivePackCommandState],
) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let mut any = false;
    for state in commands.iter_mut() {
        if state.error_string.is_some() {
            continue;
        }
        if proc_receive_ref_matches(patterns, &state.command) {
            state.proc_receive = RUN_PROC_RECEIVE_SCHEDULED;
            state.was_proc_receive = true;
            any = true;
        }
    }
    any
}

pub struct ProcReceiveHookInput<'a> {
    pub git_dir: &'a Path,
    pub format: ObjectFormat,
    pub commands: &'a [ReceivePackCommandState],
    pub push_options: &'a [String],
    pub use_atomic: bool,
    pub use_push_options: bool,
    pub remote_stderr: &'a mut Vec<u8>,
    pub capture_stderr: bool,
}

pub struct ProcReceiveHookOutput {
    pub commands: Vec<ReceivePackCommandState>,
    pub hook_failed: bool,
}

pub fn run_proc_receive_hook(input: ProcReceiveHookInput<'_>) -> Result<ProcReceiveHookOutput> {
    let hook_path = find_proc_receive_hook(input.git_dir)?;
    let Some(hook_path) = hook_path else {
        if input.capture_stderr {
            input
                .remote_stderr
                .extend_from_slice(b"error: cannot find hook 'proc-receive'\n");
        } else {
            eprintln!("error: cannot find hook 'proc-receive'");
        }
        let mut commands = input.commands.to_vec();
        for state in &mut commands {
            if state.scheduled_for_proc_receive() {
                state.error_string = Some("fail to run proc-receive hook".into());
            }
        }
        return Ok(ProcReceiveHookOutput {
            commands,
            hook_failed: true,
        });
    };

    let capture_stderr = input.capture_stderr;
    let mut child = Command::new(&hook_path);
    child
        .current_dir(input.git_dir)
        .env("GIT_DIR", input.git_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });

    for (index, option) in input.push_options.iter().enumerate() {
        if index == 0 {
            child.env("GIT_PUSH_OPTION_COUNT", input.push_options.len().to_string());
        }
        child.env(format!("GIT_PUSH_OPTION_{index}"), option);
    }

    let mut child = child.spawn().map_err(|err| {
        GitError::Io(format!("cannot spawn proc-receive hook: {err}"))
    })?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        GitError::Io("proc-receive hook stdin unavailable".into())
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        GitError::Io("proc-receive hook stdout unavailable".into())
    })?;

    let mut hook_failed = false;
    if let Err(err) = write_proc_receive_version(
        &mut stdin,
        input.use_atomic,
        input.use_push_options,
    ) {
        hook_failed = true;
        eprintln!("error: {}", proc_receive_remote_error_message(&err));
    }

    if !hook_failed {
        for state in input.commands {
            if !state.scheduled_for_proc_receive() || state.error_string.is_some() {
                continue;
            }
            let cmd = &state.command;
            let line = format!("{} {} {}\n", cmd.old_id, cmd.new_id, cmd.name);
            if write_pkt_line_payload(&mut stdin, line.as_bytes()).is_err() {
                hook_failed = true;
                eprintln!("error: fail to write commands to proc-receive hook");
                break;
            }
        }
        if !hook_failed {
            stdin.write_all(b"0000").map_err(|err| GitError::Io(err.to_string()))?;
        }
    }

    let mut hook_use_push_options = false;
    if !hook_failed {
        match read_proc_receive_version_response(&mut stdout) {
            Ok(use_push_options) => hook_use_push_options = use_push_options,
            Err(err) => {
                hook_failed = true;
                let message = proc_receive_remote_error_message(&err);
                if input.capture_stderr {
                    input
                        .remote_stderr
                        .extend_from_slice(format!("error: {message}\n").as_bytes());
                } else {
                    eprintln!("error: {message}");
                }
            }
        }
        if hook_use_push_options && input.use_push_options {
            for option in input.push_options {
                let mut line = option.as_bytes().to_vec();
                line.push(b'\n');
                if write_pkt_line_payload(&mut stdin, &line).is_err() {
                    hook_failed = true;
                    eprintln!("error: fail to write push-options to proc-receive hook");
                    break;
                }
            }
            if !hook_failed {
                stdin.write_all(b"0000").map_err(|err| GitError::Io(err.to_string()))?;
            }
        }
    }

    drop(stdin);

    let mut commands = input.commands.to_vec();
    if !hook_failed {
        let mut reader = stdout;
        let outcome = read_proc_receive_report(input.format, &mut reader, &mut commands);
        for message in &outcome.protocol_messages {
            let message = proc_receive_remote_error_message(message);
            if input.capture_stderr {
                input
                    .remote_stderr
                    .extend_from_slice(format!("error: {message}\n").as_bytes());
            } else {
                eprintln!("error: {message}");
            }
        }
        if outcome.hook_failed {
            hook_failed = true;
        }
    }

    let status = child.wait().map_err(|err| GitError::Io(err.to_string()))?;
    if capture_stderr {
        if let Some(mut stderr) = child.stderr.take() {
            let _ = std::io::copy(&mut stderr, input.remote_stderr);
        }
    }
    if !status.success() {
        hook_failed = true;
    }

    Ok(ProcReceiveHookOutput {
        commands,
        hook_failed,
    })
}

fn find_proc_receive_hook(git_dir: &Path) -> Result<Option<std::path::PathBuf>> {
    let common = repository_common_dir(git_dir);
    let path = common.join("hooks").join("proc-receive");
    if path.is_file() {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

fn write_proc_receive_version(
    writer: &mut impl Write,
    use_atomic: bool,
    use_push_options: bool,
) -> Result<()> {
    let mut caps = Vec::new();
    if use_atomic {
        caps.push("atomic");
    }
    if use_push_options {
        caps.push("push-options");
    }
    let payload = if caps.is_empty() {
        b"version=1\n".to_vec()
    } else {
        let mut out = b"version=1\0".to_vec();
        out.extend_from_slice(caps.join(" ").as_bytes());
        out.push(b'\n');
        out
    };
    write_pkt_line_payload(writer, &payload)?;
    writer.write_all(b"0000")?;
    Ok(())
}

fn read_proc_receive_version_response(reader: &mut impl Read) -> Result<bool> {
    let mut hook_use_push_options = false;
    loop {
        let Some(frame) = read_pkt_line_frame(reader)? else {
            return Err(GitError::InvalidFormat(
                "fail to negotiate version with proc-receive hook".into(),
            ));
        };
        match frame {
            PktLineFrame::Flush => return Ok(hook_use_push_options),
            PktLineFrame::Data(payload) => {
                let text = pkt_line_text(&payload)?;
                if let Some(version) = text.strip_prefix("version=") {
                    let version = version
                        .split('\0')
                        .next()
                        .unwrap_or(version)
                        .parse::<u32>()
                        .map_err(|_| {
                            GitError::InvalidFormat(format!(
                                "proc-receive version '{version}' is not supported"
                            ))
                        })?;
                    if version != 0 && version != 1 {
                        return Err(GitError::InvalidFormat(format!(
                            "proc-receive version '{version}' is not supported"
                        )));
                    }
                    if text.contains('\0') {
                        let features = text.split('\0').nth(1).unwrap_or_default();
                        for feature in features.split_whitespace() {
                            if feature == "push-options" {
                                hook_use_push_options = true;
                            }
                        }
                    }
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "proc-receive version negotiation contains unexpected control packet".into(),
                ));
            }
        }
    }
}

struct ProcReceiveReportOutcome {
    hook_failed: bool,
    protocol_messages: Vec<String>,
}

fn read_proc_receive_report(
    format: ObjectFormat,
    reader: &mut impl Read,
    commands: &mut [ReceivePackCommandState],
) -> ProcReceiveReportOutcome {
    let mut outcome = ProcReceiveReportOutcome {
        hook_failed: false,
        protocol_messages: Vec::new(),
    };
    let frames = match read_pkt_line_frames_until_flush(reader) {
        Ok(frames) => frames,
        Err(err) => {
            outcome.hook_failed = true;
            outcome
                .protocol_messages
                .push(proc_receive_remote_error_message(&err));
            return outcome;
        }
    };
    let mut hint_index: Option<usize> = None;
    let mut new_report = false;
    let mut option_without_ok = false;

    for frame in frames {
        let PktLineFrame::Data(payload) = frame else {
            continue;
        };
        let text = match std::str::from_utf8(pkt_line_bytes(&payload)) {
            Ok(text) => text.to_string(),
            Err(err) => {
                outcome.hook_failed = true;
                outcome.protocol_messages.push(err.to_string());
                return outcome;
            }
        };

        if text.starts_with("option ") {
            if hint_index.is_none() || (!new_report && commands[hint_index.unwrap()].reports.is_empty())
            {
                if !option_without_ok {
                    option_without_ok = true;
                    outcome.protocol_messages.push(
                        "proc-receive reported 'option' without a matching 'ok/ng' directive"
                            .into(),
                    );
                }
                outcome.hook_failed = true;
                continue;
            }
            let idx = hint_index.unwrap();
            if new_report {
                commands[idx].reports.push(ProcReceiveReport {
                    refname: None,
                    old_oid: None,
                    new_oid: None,
                    forced_update: false,
                });
                new_report = false;
            }
            let report = commands[idx].reports.last_mut().unwrap();
            if let Some(rest) = text.strip_prefix("option refname ") {
                report.refname = Some(rest.to_string());
            } else if let Some(rest) = text.strip_prefix("option old-oid ") {
                match ObjectId::from_hex(format, rest) {
                    Ok(oid) => report.old_oid = Some(oid),
                    Err(err) => {
                        outcome.hook_failed = true;
                        outcome.protocol_messages.push(err.to_string());
                    }
                }
            } else if let Some(rest) = text.strip_prefix("option new-oid ") {
                match ObjectId::from_hex(format, rest) {
                    Ok(oid) => report.new_oid = Some(oid),
                    Err(err) => {
                        outcome.hook_failed = true;
                        outcome.protocol_messages.push(err.to_string());
                    }
                }
            } else if text == "option forced-update" {
                report.forced_update = true;
            } else if text == "option fall-through" {
                commands[idx].proc_receive = 0;
            } else {
                outcome.hook_failed = true;
            }
            continue;
        }

        new_report = false;
        let Some((head, rest)) = text.split_once(' ') else {
            outcome.hook_failed = true;
            outcome.protocol_messages.push(format!(
                "proc-receive reported incomplete status line: '{text}'"
            ));
            continue;
        };
        let (refname, message) = match rest.split_once(' ') {
            Some((refname, message)) => (refname, Some(message)),
            None => (rest, None),
        };

        if head != "ok" && head != "ng" {
            outcome.hook_failed = true;
            outcome.protocol_messages.push(format!(
                "proc-receive reported bad status '{head}' on ref '{refname}'"
            ));
            continue;
        }

        let Some(idx) = find_command_index(commands, refname, hint_index) else {
            outcome.hook_failed = true;
            outcome.protocol_messages.push(format!(
                "proc-receive reported status on unknown ref: {refname}"
            ));
            continue;
        };
        if !commands[idx].expects_proc_receive_report() {
            outcome.hook_failed = true;
            outcome.protocol_messages.push(format!(
                "proc-receive reported status on unexpected ref: {refname}"
            ));
            continue;
        }

        hint_index = Some(idx);
        commands[idx].proc_receive |= RUN_PROC_RECEIVE_RETURNED;
        if head == "ng" {
            commands[idx].error_string = Some(message.unwrap_or("failed").to_string());
            outcome.hook_failed = true;
            continue;
        }
        new_report = true;
    }

    for state in commands.iter_mut() {
        if state.scheduled_for_proc_receive()
            && state.error_string.is_none()
            && state.proc_receive & RUN_PROC_RECEIVE_RETURNED == 0
        {
            state.error_string = Some("proc-receive failed to report status".into());
            outcome.hook_failed = true;
        }
    }

    outcome
}

fn find_command_index(
    commands: &[ReceivePackCommandState],
    refname: &str,
    hint: Option<usize>,
) -> Option<usize> {
    if let Some(hint) = hint {
        if commands
            .get(hint)
            .is_some_and(|state| state.command.name == refname)
        {
            return Some(hint);
        }
    }
    commands
        .iter()
        .position(|state| state.command.name == refname)
}

fn pkt_line_bytes(payload: &[u8]) -> &[u8] {
    payload.strip_suffix(b"\n").unwrap_or(payload)
}

fn pkt_line_text(payload: &[u8]) -> Result<&str> {
    std::str::from_utf8(pkt_line_bytes(payload)).map_err(|err| GitError::InvalidFormat(err.to_string()))
}

fn proc_receive_remote_error_message(err: impl std::fmt::Display) -> String {
    let message = err.to_string();
    message
        .strip_prefix("invalid format: ")
        .unwrap_or(&message)
        .to_string()
}

pub fn apply_proc_receive_hook_failure(
    commands: &mut [ReceivePackCommandState],
    atomic: bool,
    hook_failed: bool,
) {
    if !hook_failed {
        return;
    }
    for state in commands.iter_mut() {
        if state.error_string.is_some() {
            continue;
        }
        if state.scheduled_for_proc_receive()
            && state.proc_receive & RUN_PROC_RECEIVE_RETURNED == 0
        {
            state.error_string = Some("fail to run proc-receive hook".into());
        } else if atomic {
            state.error_string = Some("fail to run proc-receive hook".into());
        }
    }
}