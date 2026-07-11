#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_repository() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    PathBuf::from("/tmp").join(format!("sley-fsmonitor-cli-{}-{nanos}", std::process::id()))
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    sley_testkit::hermetic_git_command(sley_testkit::sley_bin!())
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run sley {args:?}: {err}"))
}

struct RepositoryGuard(PathBuf);

impl Drop for RepositoryGuard {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = run(&self.0, &["fsmonitor--daemon", "stop"]);
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[test]
fn start_status_stop_tracks_a_live_daemon() {
    let repo = temp_repository();
    fs::create_dir_all(&repo).expect("create repository directory");
    let repo = fs::canonicalize(repo).expect("canonical repository path");
    let _guard = RepositoryGuard(repo.clone());
    let init = run(&repo, &["init"]);
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let before = run(&repo, &["fsmonitor--daemon", "status"]);
    assert_eq!(before.status.code(), Some(1));
    assert_eq!(
        before.stdout,
        format!("fsmonitor-daemon is not watching '{}'\n", repo.display()).into_bytes()
    );

    let start = run(&repo, &["fsmonitor--daemon", "start", "--start-timeout=5"]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(repo.join(".git/fsmonitor--daemon.ipc").exists());

    let watching = run(&repo, &["fsmonitor--daemon", "status"]);
    assert!(watching.status.success());
    assert_eq!(
        watching.stdout,
        format!("fsmonitor-daemon is watching '{}'\n", repo.display()).into_bytes()
    );

    let stop = run(&repo, &["fsmonitor--daemon", "stop"]);
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(!repo.join(".git/fsmonitor--daemon.ipc").exists());
}
