//! FIX-C regression coverage for the sley-gc cluster (review #232):
//!
//! * M2 — repack/gc traversal roots must include linked-worktree HEADs,
//!   indexes, and reflogs (upstream `pack-objects --all --reflog
//!   --indexed-objects` examines every worktree).
//! * M3 — `gc --no-prune` / `--prune=never` must keep unreachable packed
//!   objects (upstream `repack -A -d` semantics), not delete them.
//! * S6 — gc.pid acquisition is single-step with pid liveness: a crashed
//!   holder is recovered immediately; a live holder blocks.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    let mut command = sley_testkit::hermetic_git_command_with_identity(program);
    command.current_dir(cwd).args(args);
    command
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

fn git(cwd: &Path, args: &[&str]) -> Output {
    run(sley_testkit::oracle_git(), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) -> Output {
    success(sley_testkit::oracle_git(), cwd, args)
}

fn stdout_trimmed(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    fs::create_dir_all(&repo).expect("create repo dir");
    git_ok(&root, &["init", "-q", "-b", "main", repo.to_str().expect("utf8")]);
    fs::write(repo.join("base.txt"), b"base\n").expect("write base file");
    git_ok(&repo, &["add", "base.txt"]);
    git_ok(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

/// Write a blob into the object database and return its oid.
fn write_blob(cwd: &Path, contents: &[u8]) -> String {
    let source = cwd.join("unreachable-source");
    fs::write(&source, contents).expect("write blob source");
    let output = git_ok(cwd, &["hash-object", "-w", "unreachable-source"]);
    fs::remove_file(&source).expect("remove blob source");
    stdout_trimmed(&output)
}

/// Pack exactly one object into its own pack file and drop the loose copy, so
/// it exists only inside that pack.
fn isolate_blob_in_pack(repo: &Path, oid: &str) {
    let mut child = Command::new(sley_testkit::oracle_git())
        .current_dir(repo)
        .args(["pack-objects", ".git/objects/pack/pack"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pack-objects");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(format!("{oid}\n").as_bytes())
        .expect("feed pack-objects");
    let output = child.wait_with_output().expect("pack-objects");
    assert!(
        output.status.success(),
        "pack-objects failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let loose = repo
        .join(".git")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    if loose.exists() {
        fs::remove_file(&loose).expect("remove loose copy");
    }
}

fn object_is_present(cwd: &Path, oid: &str) -> bool {
    git(cwd, &["cat-file", "-e", oid]).status.success()
}

/// M2: a commit reachable only from a linked worktree's detached HEAD must
/// survive an all-into-one repack run from the main worktree.
#[test]
fn repack_all_keeps_linked_worktree_detached_head_commit() {
    let root = unique_temp_dir("gc-wt-detached");
    let repo = init_repo(&root, "repo");

    let worktree_b = root.join("worktree-b");
    git_ok(&repo, &["worktree", "add", "--detach", worktree_b.to_str().expect("utf8")]);

    // Commit X exists only in worktree B's detached HEAD.
    fs::write(worktree_b.join("only-in-b.txt"), b"precious\n").expect("write file");
    git_ok(&worktree_b, &["add", "only-in-b.txt"]);
    git_ok(&worktree_b, &["commit", "-q", "-m", "worktree-only commit"]);
    let commit_x = stdout_trimmed(&git_ok(&worktree_b, &["rev-parse", "HEAD"]));

    // Park X inside a pack so losing it means losing data, not just a loose
    // file: upstream repack (run from B) packs X because it examines every
    // worktree.
    git_ok(&worktree_b, &["repack", "-a", "-d"]);

    // The main-worktree repack must treat B's HEAD as a traversal root.
    success(sley_testkit::sley_bin!(), &repo, &["repack", "-a", "-d"]);

    assert!(
        object_is_present(&worktree_b, &commit_x),
        "commit X ({commit_x}) reachable only from linked worktree B was dropped by repack -a -d"
    );
    let fsck = git(&repo, &["fsck", "--no-progress"]);
    assert!(
        fsck.status.success(),
        "fsck failed after repack: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    fs::remove_dir_all(&root).ok();
}

/// M2: a staged blob that exists only in a linked worktree's index must be
/// treated as an indexed-objects root by repack.
#[test]
fn repack_all_keeps_staged_blob_of_linked_worktree() {
    let root = unique_temp_dir("gc-wt-index");
    let repo = init_repo(&root, "repo");

    let worktree_b = root.join("worktree-b");
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature-b",
            worktree_b.to_str().expect("utf8"),
        ],
    );

    // Stage (but do not commit) a brand-new blob in worktree B.
    let blob_oid = write_blob(&worktree_b, b"staged only in worktree b\n");
    git_ok(
        &worktree_b,
        &["update-index", "--add", "--cacheinfo", &format!("100644,{blob_oid},staged.txt")],
    );
    // Only copy of the blob lives in a pack now.
    isolate_blob_in_pack(&repo, &blob_oid);

    success(sley_testkit::sley_bin!(), &repo, &["repack", "-a", "-d"]);

    assert!(
        object_is_present(&worktree_b, &blob_oid),
        "staged blob {blob_oid} present only in linked worktree B's index was dropped"
    );

    fs::remove_dir_all(&root).ok();
}

/// M3: with cruft packs disabled, `gc --no-prune` must loosen unreachable
/// packed objects instead of deleting them with their source pack.
#[test]
fn gc_no_prune_keeps_unreachable_packed_objects() {
    assert_gc_keeps_unreachable_packed_object(&["gc", "--no-prune"]);
}

/// M3: same for the explicit `--prune=never` spelling.
#[test]
fn gc_prune_never_keeps_unreachable_packed_objects() {
    assert_gc_keeps_unreachable_packed_object(&["gc", "--prune=never"]);
}

fn assert_gc_keeps_unreachable_packed_object(gc_args: &[&str]) {
    let root = unique_temp_dir("gc-no-prune");
    let repo = init_repo(&root, "repo");
    let sley = sley_testkit::sley_bin!();

    let blob_oid = write_blob(&repo, b"unreachable but precious\n");
    isolate_blob_in_pack(&repo, &blob_oid);
    assert!(
        object_is_present(&repo, &blob_oid),
        "setup failed: packed blob not readable"
    );

    // cruftPacks=false selects the Reachable gc mode where the bug lived.
    git_ok(&repo, &["config", "gc.cruftPacks", "false"]);
    success(sley, &repo, gc_args);

    assert!(
        object_is_present(&repo, &blob_oid),
        "{gc_args:?} deleted unreachable packed object {blob_oid}"
    );

    fs::remove_dir_all(&root).ok();
}

/// M3 control: default gc (expire 2.weeks.ago) still expires old unreachable
/// packed objects in Reachable mode.
#[test]
fn gc_default_still_expires_old_unreachable_packed_objects() {
    let root = unique_temp_dir("gc-default-expire");
    let repo = init_repo(&root, "repo");

    let blob_oid = write_blob(&repo, b"unreachable and expired\n");
    isolate_blob_in_pack(&repo, &blob_oid);

    // Age the object beyond gc.pruneExpire before packing so no recent-root
    // protection applies.
    let pack_dir = repo.join(".git").join("objects").join("pack");
    let pack_path = fs::read_dir(&pack_dir)
        .expect("read pack dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("pack"))
        .expect("created pack");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64
        - 30 * 24 * 3600;
    filetime::set_file_mtime(
        &pack_path,
        filetime::FileTime::from_unix_time(stamp, 0),
    )
    .expect("backdate pack mtime");

    git_ok(&repo, &["config", "gc.cruftPacks", "false"]);
    success(sley_testkit::sley_bin!(), &repo, &["gc"]);

    assert!(
        !object_is_present(&repo, &blob_oid),
        "default gc should have expired old unreachable object {blob_oid}"
    );

    fs::remove_dir_all(&root).ok();
}

/// Default gc (cruft mode) expires aged unreachable LOOSE objects exactly like
/// upstream's `repack --cruft --cruft-expiration`, while fresh unreachable
/// loose objects survive.
#[test]
fn gc_cruft_mode_expires_aged_unreachable_loose_objects() {
    let root = unique_temp_dir("gc-cruft-loose");
    let repo = init_repo(&root, "repo");

    let aged = write_blob(&repo, b"aged unreachable loose\n");
    let fresh = write_blob(&repo, b"fresh unreachable loose\n");
    let objects_dir = repo.join(".git").join("objects");
    let backdate = |oid: &str| {
        let path = objects_dir.join(&oid[..2]).join(&oid[2..]);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs() as i64
            - 30 * 24 * 3600;
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(stamp, 0))
            .expect("backdate object");
    };
    backdate(&aged);

    success(sley_testkit::sley_bin!(), &repo, &["gc"]);

    assert!(
        !object_is_present(&repo, &aged),
        "default gc kept an unreachable loose object expired 30 days ago ({aged})"
    );
    assert!(
        object_is_present(&repo, &fresh),
        "default gc destroyed a recent unreachable object ({fresh})"
    );

    fs::remove_dir_all(&root).ok();
}

/// S6: a crashed gc's stale gc.pid (dead pid) must be recovered immediately;
/// a live holder blocks manual gc; --force proceeds without disturbing the
/// foreign lock.
#[test]
fn gc_pid_lock_liveness_and_recovery() {
    let root = unique_temp_dir("gc-pid-lock");
    let repo = init_repo(&root, "repo");
    let sley = sley_testkit::sley_bin!();
    let pid_file = repo.join(".git").join("gc.pid");

    // Crashed holder: dead-but-recorded pid recovers without waiting 12h.
    let mut crashed = Command::new("sleep").arg("30").spawn().expect("spawn sleep");
    let dead_pid = crashed.id();
    crashed.kill().expect("kill sleep");
    crashed.wait().expect("reap sleep");
    fs::write(&pid_file, format!("{dead_pid} crashed-host\n")).expect("write stale gc.pid");

    success(sley, &repo, &["gc"]);
    assert!(
        !pid_file.exists(),
        "our own gc.pid should be cleaned up after a successful run"
    );

    // Live holder: manual gc refuses, foreign file untouched, --force proceeds.
    let mut live = Command::new("sleep").arg("60").spawn().expect("spawn sleep");
    let live_pid = live.id();
    fs::write(&pid_file, format!("{live_pid} live-host\n")).expect("write live gc.pid");

    let blocked = run(sley, &repo, &["gc"]);
    assert_eq!(
        blocked.status.code(),
        Some(128),
        "manual gc did not refuse a live lock: {}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("gc is already running"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert_eq!(
        fs::read_to_string(&pid_file).expect("foreign lock survives"),
        format!("{live_pid} live-host\n"),
        "a refused gc disturbed the live holder's gc.pid"
    );

    success(sley, &repo, &["gc", "--force"]);
    assert_eq!(
        fs::read_to_string(&pid_file).expect("foreign lock survives --force"),
        format!("{live_pid} live-host\n"),
        "--force clobbered or removed the live holder's gc.pid"
    );

    live.kill().ok();
    live.wait().ok();
    fs::remove_dir_all(&root).ok();
}
