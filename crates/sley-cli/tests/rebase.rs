use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_identity(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(cwd: &Path, args: &[&str]) {
    run_success(sley_testkit::oracle_git(), cwd, args);
}

fn git_with_identity(cwd: &Path, args: &[&str]) {
    let output = run_output_with_identity(sley_testkit::oracle_git(), cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout,
        expected.stdout,
        "stdout differed for {args:?}\nactual:\n{}\nexpected:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout)
    );
    assert_eq!(
        actual.stderr,
        expected.stderr,
        "stderr differed for {args:?}\nactual:\n{}\nexpected:\n{}",
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr)
    );
}

fn prepare_identity(root: &Path) {
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
}

fn rev_parse(program: &str, root: &Path, rev: &str) -> String {
    String::from_utf8(run_output(program, root, &["rev-parse", rev]).stdout)
        .expect("rev-parse output utf8")
        .trim()
        .to_string()
}

fn prepare_diverged_repos(upstream: &Path, rust: &Path) {
    for root in [upstream, rust] {
        git(root, &["init", "-q", "-b", "master"]);
        prepare_identity(root);
        fs::write(root.join("shared.txt"), b"base\n").expect("write shared file");
        git(root, &["add", "shared.txt"]);
        git_with_identity(root, &["commit", "-m", "base", "-q"]);
        let base = String::from_utf8(
            run_output(sley_testkit::oracle_git(), root, &["rev-parse", "HEAD"]).stdout,
        )
        .expect("base oid utf8")
        .trim()
        .to_string();
        git(root, &["checkout", "-b", "topic", &base, "-q"]);
        fs::write(root.join("topic.txt"), b"topic-only\n").expect("write topic file");
        git(root, &["add", "topic.txt"]);
        git_with_identity(root, &["commit", "-m", "topic", "-q"]);
        git(root, &["checkout", "master", "-q"]);
        fs::write(root.join("main.txt"), b"main-only\n").expect("write main file");
        git(root, &["add", "main.txt"]);
        git_with_identity(root, &["commit", "-m", "main", "-q"]);
        git(root, &["checkout", "topic", "-q"]);
    }
}

fn prepare_up_to_date_repos(upstream: &Path, rust: &Path) {
    for root in [upstream, rust] {
        git(root, &["init", "-q", "-b", "master"]);
        prepare_identity(root);
        fs::write(root.join("hello.txt"), b"base\n").expect("write base file");
        git(root, &["add", "hello.txt"]);
        git_with_identity(root, &["commit", "-m", "base", "-q"]);
        git(root, &["checkout", "-b", "topic", "-q"]);
        fs::write(root.join("topic.txt"), b"topic\n").expect("write topic file");
        git(root, &["add", "topic.txt"]);
        git_with_identity(root, &["commit", "-m", "topic", "-q"]);
        git(root, &["checkout", "master", "-q"]);
        git(root, &["merge", "topic", "-q"]);
        git(root, &["checkout", "topic", "-q"]);
    }
}

#[test]
fn rebase_keep_base_root_is_rejected() {
    let root = unique_temp_dir("rebase-keep-base-root");
    fs::create_dir_all(&root).expect("create repo");
    git(&root, &["init", "-q", "-b", "main"]);
    prepare_identity(&root);
    fs::write(root.join("file"), b"base\n").expect("write file");
    git(&root, &["add", "file"]);
    git_with_identity(&root, &["commit", "-m", "base", "-q"]);

    let args = ["rebase", "--keep-base", "--root"];
    let output = run_output_with_identity(sley_testkit::sley_bin!(), &root, &args);
    assert_eq!(output.status.code(), Some(128));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("fatal: options '--keep-base' and '--root' cannot be used together"),
        "stderr differed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join(".git/rebase-merge").exists());
    assert!(!root.join(".git/rebase-apply").exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_keep_base_reapplies_cherry_picks_by_default() {
    let root = unique_temp_dir("rebase-keep-base-reapply");
    fs::create_dir_all(&root).expect("create repo");
    git(&root, &["init", "-q", "-b", "main"]);
    prepare_identity(&root);

    fs::write(root.join("base"), b"base\n").expect("write base");
    git(&root, &["add", "base"]);
    git_with_identity(&root, &["commit", "-m", "base", "-q"]);
    git(&root, &["checkout", "-b", "topic", "-q"]);
    fs::write(root.join("f"), b"f\n").expect("write f");
    git(&root, &["add", "f"]);
    git_with_identity(&root, &["commit", "-m", "F", "-q"]);
    let f_oid = rev_parse(sley_testkit::oracle_git(), &root, "HEAD");
    fs::write(root.join("g"), b"g\n").expect("write g");
    git(&root, &["add", "g"]);
    git_with_identity(&root, &["commit", "-m", "G", "-q"]);
    let topic_oid = rev_parse(sley_testkit::oracle_git(), &root, "HEAD");

    git(&root, &["checkout", "main", "-q"]);
    git_with_identity(&root, &["cherry-pick", &f_oid]);

    let args = ["rebase", "-i", "--keep-base", "HEAD", &topic_oid];
    let output = run_output_with_identity(sley_testkit::sley_bin!(), &root, &args);
    assert!(
        output.status.success(),
        "sley rebase failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        rev_parse(sley_testkit::oracle_git(), &root, "HEAD"),
        topic_oid,
        "--keep-base should reapply clean cherry-picks unless explicitly disabled"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_clean_matches_upstream_git() {
    let root = unique_temp_dir("rebase-clean");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    prepare_diverged_repos(&upstream, &rust);
    let args = ["rebase", "master"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(sley_testkit::sley_bin!(), &rust, &args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for clean rebase"
    );
    assert!(
        actual.status.success(),
        "sley rebase failed: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &rust, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after clean rebase"
    );
    assert_eq!(
        run_output(sley_testkit::oracle_git(), &upstream, &["log", "--oneline"]).stdout,
        run_output(sley_testkit::sley_bin!(), &rust, &["log", "--oneline"]).stdout,
        "log order differed after clean rebase"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_already_up_to_date_matches_upstream_git() {
    let root = unique_temp_dir("rebase-up-to-date");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    prepare_up_to_date_repos(&upstream, &rust);
    let args = ["rebase", "master"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(sley_testkit::sley_bin!(), &rust, &args);
    assert_same_output(actual, expected, &args);
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &rust, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after up-to-date rebase"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_can_leave_gitlink_branch_with_dirty_populated_submodule() {
    let root = unique_temp_dir("rebase-autostash-gitlink-checkout");
    let child = root.join("child");
    let superproject = root.join("super");
    fs::create_dir_all(&child).expect("create child repo");
    fs::create_dir_all(&superproject).expect("create superproject repo");

    git(&child, &["init", "-q", "-b", "main"]);
    prepare_identity(&child);
    fs::write(child.join("file0"), b"child\n").expect("write child file");
    git(&child, &["add", "file0"]);
    git_with_identity(&child, &["commit", "-m", "child", "-q"]);

    git(&superproject, &["init", "-q", "-b", "main"]);
    prepare_identity(&superproject);
    fs::write(superproject.join("file0"), b"base\n").expect("write base file");
    git(&superproject, &["add", "file0"]);
    git_with_identity(&superproject, &["commit", "-m", "base", "-q"]);
    git(&superproject, &["checkout", "-b", "with-submodule", "-q"]);
    let child_arg = child.to_str().expect("child path utf8");
    git(
        &superproject,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child_arg,
            "sub",
        ],
    );
    git(&superproject, &["add", ".gitmodules", "sub"]);
    git_with_identity(&superproject, &["commit", "-m", "add submodule", "-q"]);

    fs::write(superproject.join("sub/file0"), b"changed\n").expect("dirty submodule");
    run_success(
        sley_testkit::sley_bin!(),
        &superproject,
        &["reset", "--hard"],
    );
    run_success(
        sley_testkit::sley_bin!(),
        &superproject,
        &["checkout", "main"],
    );
    assert_eq!(
        run_output(
            sley_testkit::sley_bin!(),
            &superproject,
            &["rev-parse", "--abbrev-ref", "HEAD"]
        )
        .stdout,
        b"main\n",
        "checkout should leave the gitlink branch"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_interactive_reads_sequence_editor_from_common_config_in_worktree() {
    // t3430: `test_config -C wt sequence.editor ...` must be honoured when
    // rebasing inside a linked worktree (config lives in the common gitdir).
    let root = unique_temp_dir("rebase-wt-sequence-editor");
    fs::create_dir_all(&root).expect("create temp");
    {
        git_with_identity(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("f"), b"f\n").expect("write");
        git_with_identity(&root, &["add", "f"]);
        git_with_identity(&root, &["commit", "-m", "c1", "-q"]);
        git_with_identity(&root, &["worktree", "add", "wt", "-q"]);
        let editor = root.join("replace-editor.sh");
        fs::write(
            &editor,
            "#!/bin/sh\nmv \"$1\" \"$(git rev-parse --git-path ORIGINAL-TODO)\"\ncp script-from-scratch \"$1\"\n",
        )
        .expect("write editor");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&editor).expect("meta").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&editor, perms).expect("chmod");
        }
        let wt = root.join("wt");
        fs::write(wt.join("script-from-scratch"), "label xyz\nexec true\n")
            .expect("write todo script");
        // Config goes to the common gitdir (same as `git -C wt config`).
        git_with_identity(
            &wt,
            &["config", "sequence.editor", editor.to_str().expect("utf8")],
        );
        // Clear env override so only config is consulted.
        let output = Command::new(sley_testkit::sley_bin!())
            .current_dir(&wt)
            .args(["rebase", "-i", "HEAD"])
            .env_remove("GIT_SEQUENCE_EDITOR")
            .env("GIT_EDITOR", "false")
            .env("GIT_AUTHOR_NAME", "Example User")
            .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
            .env("GIT_COMMITTER_NAME", "Example User")
            .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
            .output()
            .expect("run rebase");
        assert!(
            output.status.success(),
            "rebase -i failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // Editor should have saved ORIGINAL-TODO under the worktree admin dir.
        let original = root.join(".git/worktrees/wt/ORIGINAL-TODO");
        assert!(
            original.is_file(),
            "sequence.editor was not launched (missing {})\nstderr:\n{}",
            original.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let _ = fs::remove_dir_all(&root);
}
