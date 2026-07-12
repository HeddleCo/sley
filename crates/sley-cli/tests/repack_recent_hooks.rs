use filetime::FileTime;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_repo(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sley-{name}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&path).expect("create repository directory");
    path
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Recent Hook Test")
        .env("GIT_AUTHOR_EMAIL", "recent@example.invalid")
        .env("GIT_COMMITTER_NAME", "Recent Hook Test")
        .env("GIT_COMMITTER_EMAIL", "recent@example.invalid")
        .output()
        .unwrap_or_else(|error| panic!("run {program} {args:?}: {error}"))
}

fn success(program: &str, cwd: &Path, args: &[&str]) -> Output {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn initialize_repo(name: &str) -> PathBuf {
    let repo = temp_repo(name);
    let git = sley_testkit::oracle_git();
    success(git, &repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("base"), b"base\n").expect("write base");
    success(git, &repo, &["add", "base"]);
    success(git, &repo, &["commit", "-q", "-m", "base"]);
    repo
}

fn write_unreachable_blob(repo: &Path, contents: &[u8]) -> String {
    let path = repo.join("unreachable");
    fs::write(&path, contents).expect("write unreachable source");
    let output = success(
        sley_testkit::oracle_git(),
        repo,
        &["hash-object", "-w", "unreachable"],
    );
    fs::remove_file(path).expect("remove unreachable source");
    String::from_utf8(output.stdout)
        .expect("object id utf8")
        .trim()
        .to_string()
}

#[test]
fn recent_hook_is_lazy_and_preserves_hook_stderr() {
    let repo = initialize_repo("recent-hook-lazy");
    let git = sley_testkit::oracle_git();
    let sley = sley_testkit::sley_bin!();
    let hook = "echo SENTINEL-HOOK-ERR 1>&2 && exit 1";
    success(git, &repo, &["config", "gc.recentObjectsHook", hook]);

    // No unreachable candidate means pack-objects never invokes the hook.
    success(
        sley,
        &repo,
        &["repack", "-a", "-d", "--cruft", "--cruft-expiration=now"],
    );

    write_unreachable_blob(&repo, b"candidate\n");
    let output = run(
        sley,
        &repo,
        &["repack", "-a", "-d", "--cruft", "--cruft-expiration=now"],
    );
    assert_eq!(output.status.code(), Some(128));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SENTINEL-HOOK-ERR"), "stderr: {stderr}");
    assert!(
        stderr.contains("unable to enumerate additional recent objects"),
        "stderr: {stderr}"
    );
    fs::remove_dir_all(repo).ok();
}

#[test]
fn gc_cruft_expiration_respects_recent_hook_roots() {
    let repo = initialize_repo("gc-recent-hook");
    let git = sley_testkit::oracle_git();
    let sley = sley_testkit::sley_bin!();
    let oid = write_unreachable_blob(&repo, b"precious unreachable\n");
    let loose = repo.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    let old = FileTime::from_unix_time(946_684_800, 0);
    filetime::set_file_mtime(&loose, old).expect("age unreachable object");
    success(git, &repo, &["repack", "-a", "-d", "--cruft"]);
    success(
        git,
        &repo,
        &["config", "gc.recentObjectsHook", &format!("echo {oid}")],
    );

    success(sley, &repo, &["gc", "--prune=1.day.ago"]);
    success(git, &repo, &["cat-file", "-e", &oid]);
    fs::remove_dir_all(repo).ok();
}
