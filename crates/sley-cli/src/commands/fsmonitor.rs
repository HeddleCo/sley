//! Thin CLI adapter for the engine-owned fsmonitor daemon lifecycle.

use crate::session::CliSession;
use sley::plumbing::sley_worktree::{FsmonitorDaemonSession, FsmonitorDaemonState};
use sley::{GitError, Result};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: git fsmonitor--daemon start [<options>]\n   or: git fsmonitor--daemon run [<options>]\n   or: git fsmonitor--daemon stop\n   or: git fsmonitor--daemon status";

#[derive(Debug)]
struct DaemonArgs {
    subcommand: String,
    detach: bool,
    ipc_threads: usize,
    start_timeout: Duration,
}

pub(crate) fn cmd_fsmonitor_daemon(cli_session: &CliSession, args: &[String]) -> Result<()> {
    let parsed = parse_args(args)?;
    let repository = cli_session.open_repository()?;
    let Some(worktree) = repository.workdir() else {
        eprintln!("fatal: fsmonitor--daemon does not support bare repositories");
        return Err(GitError::Exit(128));
    };
    let daemon = FsmonitorDaemonSession::new(repository.git_dir());

    match parsed.subcommand.as_str() {
        "start" => start_daemon(&daemon, &worktree, &parsed),
        "run" => run_daemon(&daemon, &worktree, parsed.detach),
        "stop" => stop_daemon(&daemon),
        "status" => status_daemon(&daemon, &worktree),
        other => {
            eprintln!("fatal: Unhandled subcommand '{other}'");
            Err(GitError::Exit(128))
        }
    }
}

fn parse_args(args: &[String]) -> Result<DaemonArgs> {
    let mut subcommand = None;
    let mut detach = false;
    let mut ipc_threads = 8_usize;
    let mut start_timeout = 60_u64;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-h" | "--help" => return usage(),
            "--detach" => {
                detach = true;
                index += 1;
            }
            "--ipc-threads" => {
                let value = option_value(args, &mut index, "ipc-threads")?;
                ipc_threads = parse_positive(&value, "ipc-threads")?;
            }
            value if value.starts_with("--ipc-threads=") => {
                ipc_threads = parse_positive(&value["--ipc-threads=".len()..], "ipc-threads")?;
                index += 1;
            }
            "--start-timeout" => {
                let value = option_value(args, &mut index, "start-timeout")?;
                start_timeout = parse_nonnegative(&value, "start-timeout")?;
            }
            value if value.starts_with("--start-timeout=") => {
                start_timeout =
                    parse_nonnegative(&value["--start-timeout=".len()..], "start-timeout")?;
                index += 1;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return usage();
            }
            value if subcommand.is_none() => {
                subcommand = Some(value.to_string());
                index += 1;
            }
            _ => return usage(),
        }
    }
    let Some(subcommand) = subcommand else {
        return usage();
    };
    Ok(DaemonArgs {
        subcommand,
        detach,
        ipc_threads,
        start_timeout: Duration::from_secs(start_timeout),
    })
}

fn option_value(args: &[String], index: &mut usize, name: &str) -> Result<String> {
    let Some(value) = args.get(*index + 1) else {
        eprintln!("error: option `{name}' requires a value");
        return usage();
    };
    *index += 2;
    Ok(value.clone())
}

fn parse_positive(value: &str, name: &str) -> Result<usize> {
    let parsed = value.parse::<usize>().unwrap_or(0);
    if parsed == 0 {
        eprintln!("fatal: invalid '{name}' value ({value})");
        return Err(GitError::Exit(128));
    }
    Ok(parsed)
}

fn parse_nonnegative(value: &str, name: &str) -> Result<u64> {
    value.parse::<u64>().map_err(|_| {
        eprintln!("fatal: invalid '{name}' value ({value})");
        GitError::Exit(128)
    })
}

fn usage<T>() -> Result<T> {
    eprintln!("{USAGE}");
    Err(GitError::Exit(129))
}

fn start_daemon(daemon: &FsmonitorDaemonSession, worktree: &Path, args: &DaemonArgs) -> Result<()> {
    if daemon.state()? == FsmonitorDaemonState::Listening {
        eprintln!(
            "fatal: fsmonitor--daemon is already running '{}'",
            worktree.display()
        );
        return Err(GitError::Exit(128));
    }

    let executable = daemon_executable()?;
    let mut command = Command::new(executable);
    command
        .arg(format!("--git-dir={}", daemon.git_dir().display()))
        .arg(format!("--work-tree={}", worktree.display()))
        .arg("fsmonitor--daemon")
        .arg("run")
        .arg("--detach")
        .arg(format!("--ipc-threads={}", args.ipc_threads))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(worktree);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|err| GitError::Io(format!("could not start fsmonitor daemon: {err}")))?;

    let started = Instant::now();
    loop {
        if daemon.state()? == FsmonitorDaemonState::Listening {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|err| GitError::Io(err.to_string()))?
        {
            eprintln!("error: daemon terminated with status {status}");
            return Err(GitError::Exit(1));
        }
        if started.elapsed() >= args.start_timeout {
            eprintln!("error: daemon not online yet");
            return Err(GitError::Exit(1));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_daemon(daemon: &FsmonitorDaemonSession, worktree: &Path, _detach: bool) -> Result<()> {
    if daemon.state()? == FsmonitorDaemonState::Listening {
        eprintln!(
            "fatal: fsmonitor--daemon is already running '{}'",
            worktree.display()
        );
        return Err(GitError::Exit(128));
    }
    daemon.serve()
}

fn stop_daemon(daemon: &FsmonitorDaemonSession) -> Result<()> {
    if daemon.state()? != FsmonitorDaemonState::Listening {
        eprintln!("fatal: fsmonitor--daemon is not running");
        return Err(GitError::Exit(128));
    }
    daemon.request_stop(Duration::from_secs(30))
}

fn status_daemon(daemon: &FsmonitorDaemonSession, worktree: &Path) -> Result<()> {
    if daemon.state()? == FsmonitorDaemonState::Listening {
        println!("fsmonitor-daemon is watching '{}'", worktree.display());
        Ok(())
    } else {
        println!("fsmonitor-daemon is not watching '{}'", worktree.display());
        Err(GitError::Exit(1))
    }
}

/// Resolve the main Sley executable for the detached server process. Normally
/// `fsmonitor--daemon start` is entered through Sley itself, but Scalar invokes
/// the same native adapter in-process. Spawning Scalar with Git arguments would
/// immediately terminate, so prefer the harness-provided binary and otherwise
/// locate a sibling `sley` executable. Deliberately do not fall back to a
/// sibling or PATH-resolved `git`: Scalar must never borrow installed Git.
fn daemon_executable() -> Result<PathBuf> {
    let current = env::current_exe().map_err(|err| GitError::Io(err.to_string()))?;
    daemon_executable_from(
        &current,
        env::var_os("SLEY_BIN").map(PathBuf::from).as_deref(),
    )
}

fn daemon_executable_from(current: &Path, configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(executable) = configured.filter(|path| path.is_file()) {
        return Ok(executable.to_path_buf());
    }
    if current
        .file_stem()
        .is_some_and(|name| name.eq_ignore_ascii_case("scalar"))
    {
        let directory = current
            .parent()
            .ok_or_else(|| GitError::Io("Scalar executable has no parent directory".into()))?;
        let mut sibling = directory.join("sley");
        if cfg!(windows) {
            sibling.set_extension("exe");
        }
        if sibling.is_file() {
            return Ok(sibling);
        }
        return Err(GitError::Io(
            "could not locate Sley beside the Scalar executable".into(),
        ));
    }
    Ok(current.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_options_around_the_subcommand() {
        let parsed = parse_args(&[
            "--start-timeout=2".into(),
            "start".into(),
            "--ipc-threads".into(),
            "3".into(),
        ])
        .expect("parse daemon args");
        assert_eq!(parsed.subcommand, "start");
        assert_eq!(parsed.ipc_threads, 3);
        assert_eq!(parsed.start_timeout, Duration::from_secs(2));
    }

    #[test]
    fn rejects_zero_ipc_threads() {
        let err = parse_args(&["run".into(), "--ipc-threads=0".into()])
            .expect_err("zero threads must fail");
        assert_eq!(err, GitError::Exit(128));
    }

    #[cfg(unix)]
    #[test]
    fn scalar_daemon_resolution_never_borrows_sibling_git() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root =
            env::temp_dir().join(format!("sley-fsmonitor-bin-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).expect("temporary binary directory");
        let scalar = root.join("scalar");
        std::fs::write(&scalar, b"scalar").expect("scalar placeholder");
        std::fs::write(root.join("git"), b"installed git").expect("git placeholder");

        let error = daemon_executable_from(&scalar, None)
            .expect_err("a sibling Git must not satisfy native daemon resolution");
        assert!(error.to_string().contains("locate Sley"), "{error}");

        let sley = root.join("sley");
        std::fs::write(&sley, b"sley").expect("sley placeholder");
        assert_eq!(
            daemon_executable_from(&scalar, None).expect("native sibling"),
            sley
        );
        std::fs::remove_dir_all(root).ok();
    }
}
