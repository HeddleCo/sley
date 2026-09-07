//! Process status and command diagnostics belong to this executable adapter.
use sley_core::{CallbackError, GitError, RejectionKind};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExit {
    Ok,
    UserError,
    Usage,
    Custom(i32),
}
impl CliExit {
    pub const fn code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::UserError => 128,
            Self::Usage => 129,
            Self::Custom(code) => code,
        }
    }
}

#[derive(Debug)]
struct CliOutcome {
    status: i32,
    message: Option<String>,
}
impl fmt::Display for CliOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.message {
            Some(message) => f.write_str(message),
            None => write!(f, "exit {}", self.status),
        }
    }
}
impl std::error::Error for CliOutcome {}

pub fn cli_exit(status: i32) -> GitError {
    GitError::Callback(CallbackError::new(CliOutcome {
        status,
        message: None,
    }))
}
pub fn cli_diagnostic(kind: CliExit, message: impl Into<String>) -> GitError {
    GitError::Callback(CallbackError::new(CliOutcome {
        status: kind.code(),
        message: Some(message.into()),
    }))
}
pub fn cli_usage(message: impl Into<String>) -> GitError {
    cli_diagnostic(CliExit::Usage, message)
}
pub fn cli_user_error(message: impl Into<String>) -> GitError {
    cli_diagnostic(CliExit::UserError, message)
}

fn outcome(error: &GitError) -> Option<&CliOutcome> {
    match error {
        GitError::Callback(error) => error.downcast_ref(),
        _ => None,
    }
}
pub fn cli_message(error: &GitError) -> Option<&str> {
    outcome(error)?.message.as_deref()
}
/// Status for an already-reported operation. Suppresses duplicate CLI output.
pub fn cli_reported_status(error: &GitError) -> Option<i32> {
    match error {
        GitError::Rejected(kind) => Some(match kind {
            RejectionKind::InvalidArguments => 129,
            RejectionKind::Refused => 128,
            RejectionKind::Incomplete => 1,
        }),
        GitError::ChildProcessFailed { status } => Some(status.unwrap_or(1)),
        GitError::EmptyPreferredPack { .. } => Some(255),
        _ => outcome(error)
            .filter(|outcome| outcome.message.is_none())
            .map(|outcome| outcome.status),
    }
}
pub fn cli_exit_code(error: &GitError) -> i32 {
    cli_reported_status(error)
        .or_else(|| outcome(error).map(|outcome| outcome.status))
        .unwrap_or_else(|| match error {
            GitError::Cancelled => 130,
            GitError::RemoteHelperAborted { .. } => 128,
            _ => 1,
        })
}

/// The executable owns the decision to render diagnostics to process streams.
pub(crate) struct CliDiagnostics;
impl sley_core::diagnostics::DiagnosticSink for CliDiagnostics {
    fn write(
        &self,
        stream: sley_core::diagnostics::DiagnosticStream,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        use std::io::Write;
        match stream {
            sley_core::diagnostics::DiagnosticStream::Stdout => {
                std::io::stdout().lock().write_all(bytes)
            }
            sley_core::diagnostics::DiagnosticStream::Stderr => {
                std::io::stderr().lock().write_all(bytes)
            }
        }
    }
    fn flush(&self, stream: sley_core::diagnostics::DiagnosticStream) -> std::io::Result<()> {
        use std::io::Write;
        match stream {
            sley_core::diagnostics::DiagnosticStream::Stdout => std::io::stdout().lock().flush(),
            sley_core::diagnostics::DiagnosticStream::Stderr => std::io::stderr().lock().flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_status_and_library_rejections_retain_git_exit_codes() {
        for (error, status) in [
            (cli_exit(0), 0),
            (cli_exit(2), 2),
            (cli_usage("bad option"), 129),
            (cli_user_error("not a repository"), 128),
            (GitError::Rejected(RejectionKind::InvalidArguments), 129),
            (GitError::Rejected(RejectionKind::Refused), 128),
            (GitError::Rejected(RejectionKind::Incomplete), 1),
            (GitError::ChildProcessFailed { status: Some(7) }, 7),
            (GitError::ChildProcessFailed { status: None }, 1),
            (GitError::Cancelled, 130),
        ] {
            assert_eq!(cli_exit_code(&error), status, "{error:?}");
        }
        assert_eq!(cli_message(&cli_usage("bad option")), Some("bad option"));
        assert_eq!(cli_reported_status(&cli_usage("bad option")), None);
        assert_eq!(cli_reported_status(&cli_exit(2)), Some(2));
    }
}
