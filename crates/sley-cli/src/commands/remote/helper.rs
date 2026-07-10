//! CLI wiring for user-owned Git remote helpers.
//!
//! Protocol discovery and parsing live in `sley-remote`; this module supplies
//! the CLI runtime services: trace rendering and native Sley fast-import.

use super::fetch::{
    StdoutProgress, check_transport_allowed_url, repo_config_with_transport_policy,
};
use crate::*;
use sley::plumbing::sley_remote::FetchOptions;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) fn fetch_with_remote_helper(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<Option<sley_remote::FetchOutcome>> {
    let config = repo_config_with_transport_policy(git_dir)?;
    let Some(spec) = sley_remote::resolve_remote_helper(&config, source) else {
        return Ok(None);
    };
    check_transport_allowed_url(spec.url.as_deref().unwrap_or(source), Some(&config))?;
    sley_remote::check_transport_allowed(&spec.name, Some(&config), None).map_err(|error| {
        eprintln!("fatal: {error}");
        GitError::Exit(128)
    })?;
    trace_remote_helper(&spec);
    let ref_hook = crate::commands::refs::ReferenceTransactionHookRunner::new(git_dir);
    let mut credentials = sley_remote::NoCredentials;
    let mut progress = StdoutProgress;
    let mut plumbing = NativeRemoteHelperPlumbing;
    let mut events = CliRemoteHelperEvents;
    sley_remote::fetch_via_remote_helper(
        sley_remote::RemoteHelperFetchOperation {
            git_dir,
            format,
            config: &config,
            remote_name: source,
            spec: &spec,
            refspecs,
            options: &options,
        },
        sley_remote::RemoteHelperFetchServices {
            plumbing: &mut plumbing,
            events: &mut events,
            credentials: &mut credentials,
            progress: &mut progress,
            ref_hook: Some(&ref_hook),
        },
    )
    .map(Some)
}

pub(super) fn push_with_remote_helper(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    refspecs: &[String],
    options: sley_remote::RemoteHelperPushOptions,
    quiet: bool,
) -> Result<Option<()>> {
    let config = super::config::read_repo_config(git_dir)?;
    let Some(spec) = sley_remote::resolve_remote_helper(&config, remote) else {
        return Ok(None);
    };
    sley_remote::check_transport_allowed(&spec.name, Some(&config), None).map_err(|error| {
        eprintln!("fatal: {error}");
        GitError::Exit(128)
    })?;
    trace_remote_helper(&spec);
    let mut plumbing = NativeRemoteHelperPlumbing;
    let mut events = CliRemoteHelperEvents;
    let operation = sley_remote::push_via_remote_helper(
        sley_remote::RemoteHelperPushOperation {
            git_dir,
            format,
            config: &config,
            remote_name: remote,
            spec: &spec,
            refspecs,
            options,
        },
        sley_remote::RemoteHelperPushServices {
            plumbing: &mut plumbing,
            events: &mut events,
        },
    );
    let outcome = match operation {
        Ok(outcome) => outcome,
        Err(
            error @ (sley_remote::RemoteHelperPushError::RefspecRequired
            | sley_remote::RemoteHelperPushError::MarksRequired),
        ) => {
            eprintln!("fatal: {error}");
            return Err(GitError::Exit(128));
        }
        Err(sley_remote::RemoteHelperPushError::Engine(error)) => return Err(error),
    };
    if !quiet && !outcome.dry_run {
        eprintln!("To {}", spec.url.as_deref().unwrap_or(remote));
    }
    Ok(Some(()))
}

struct NativeRemoteHelperPlumbing;

impl sley_remote::RemoteHelperPlumbing for NativeRemoteHelperPlumbing {
    fn fast_import(&mut self, git_dir: &Path, stream: &[u8]) -> Result<()> {
        run_native_fast_import(git_dir, stream)
    }

    fn fast_export(
        &mut self,
        request: sley_remote::RemoteHelperExportRequest<'_>,
    ) -> Result<Vec<u8>> {
        run_native_fast_export(request)
    }
}

struct CliRemoteHelperEvents;

impl sley_remote::RemoteHelperEventSink for CliRemoteHelperEvents {
    fn event(&mut self, event: sley_remote::RemoteHelperEvent) {
        match event {
            sley_remote::RemoteHelperEvent::MissingRefspecCapability => {
                eprintln!("warning: this remote helper should implement refspec capability");
            }
            sley_remote::RemoteHelperEvent::FastImportFailed => {
                eprintln!("error: error while running fast-import");
            }
            sley_remote::RemoteHelperEvent::PushRejected(rest) => {
                eprintln!("error: remote helper rejected {rest}");
            }
        }
    }
}

fn run_native_fast_export(request: sley_remote::RemoteHelperExportRequest<'_>) -> Result<Vec<u8>> {
    let executable = native_sley_executable()?;
    let mut command = Command::new(&executable);
    command
        .arg("fast-export")
        .arg("--use-done-feature")
        .arg(if request.signed_tags {
            "--signed-tags=verbatim"
        } else {
            "--signed-tags=warn-strip"
        })
        .env("GIT_DIR", request.git_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path) = request.import_marks {
        command.arg(format!("--import-marks-if-exists={path}"));
    }
    if let Some(path) = request.export_marks {
        command.arg(format!("--export-marks={path}"));
    }
    let store = FileRefStore::new(request.git_dir, request.format);
    let mut has_source = false;
    for refspec in request.refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        let source = body.split_once(':').map_or(body, |(source, _)| source);
        let destination = body
            .split_once(':')
            .map_or(body, |(_, destination)| destination);
        let export_source = if source == "HEAD" {
            store
                .current_branch_ref()?
                .unwrap_or_else(|| source.to_string())
        } else {
            source.to_string()
        };
        command.arg(format!("--refspec={export_source}:{destination}"));
        if !source.is_empty() {
            command.arg(source);
            has_source = true;
        }
    }
    if !has_source && request.refspecs.is_empty() {
        return Err(GitError::Command(
            "remote-helper push has no refspecs".into(),
        ));
    }
    let output = command.output().map_err(|err| {
        GitError::Command(format!(
            "could not start native Sley fast-export '{}': {err}",
            PathBuf::from(&executable).display()
        ))
    })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Exit(output.status.code().unwrap_or(1)))
    }
}

fn run_native_fast_import(git_dir: &Path, stream: &[u8]) -> Result<()> {
    let executable = native_sley_executable()?;
    let mut child = Command::new(&executable)
        .arg("fast-import")
        .arg("--quiet")
        // Remote helpers are allowed to advertise import/export mark paths.
        // Git runs this trusted protocol stream with unsafe features enabled;
        // the path still originates from the explicitly selected user helper.
        .arg("--allow-unsafe-features")
        .env("GIT_DIR", git_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| {
            GitError::Command(format!(
                "could not start native Sley fast-import '{}': {err}",
                PathBuf::from(&executable).display()
            ))
        })?;
    child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command("native fast-import has no stdin".into()))?
        .write_all(stream)?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        eprintln!("error: error while running fast-import");
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

fn native_sley_executable() -> Result<OsString> {
    select_native_sley_executable(std::env::current_exe(), std::env::var_os("SLEY_BIN"))
}

fn select_native_sley_executable(
    current: std::io::Result<PathBuf>,
    _untrusted_sley_bin: Option<OsString>,
) -> Result<OsString> {
    current.map(Into::into).map_err(|error| {
        GitError::Command(format!("cannot resolve current Sley executable: {error}"))
    })
}

fn trace_remote_helper(spec: &sley_remote::RemoteHelperSpec) {
    if !crate::setup::git_trace_enabled() {
        return;
    }
    let mut line = format!(
        "trace: run_command: git remote-{} {}",
        spec.name,
        crate::setup::trace_quote_sq(&spec.alias)
    );
    if let Some(url) = spec.url.as_deref() {
        line.push(' ');
        line.push_str(&crate::setup::trace_quote_sq(url));
    }
    crate::setup::git_trace_line("run-command.c:672", &line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_sley_bin_cannot_redirect_native_plumbing() {
        let current = PathBuf::from("/trusted/current-sley");
        assert_eq!(
            select_native_sley_executable(
                Ok(current.clone()),
                Some(OsString::from("/installed/git")),
            )
            .expect("selection"),
            current.into_os_string()
        );
    }
}
