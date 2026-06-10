use std::fs;
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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differed for {args:?}"
    );
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin is piped"),
        stdin,
    );
    let output = child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"));
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_rs_with_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin(env!("CARGO_BIN_EXE_sley"), cwd, args, stdin)
}

fn git_with_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin("git", cwd, args, stdin)
}

fn git_with_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Vec<u8> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn expected_log(cwd: &Path, rev: &str) -> Vec<u8> {
    expected_log_args(cwd, &[rev])
}

fn expected_log_args(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let mut git_args = vec!["log", "--format=commit %H%nAuthor: %an <%ae>%n%n    %s"];
    git_args.extend_from_slice(args);
    git(cwd, &git_args)
}

#[test]
fn log_minimal_format_matches_upstream_git() {
    let root = unique_temp_dir("log-minimal-format");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("hello.txt"), b"hello\n").expect("write fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "initial subject",
                "-q",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "v1.0",
                "-m",
                "release",
            ],
        );
        fs::write(root.join("hello.txt"), b"hello again\n").expect("update fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "second subject",
                "-q",
            ],
        );
        for rev in ["HEAD", "refs/tags/v1.0"] {
            let expected = expected_log(&root, rev);
            let actual = git_rs(&root, &["log", rev]);
            assert_eq!(actual, expected, "sley log output differed for {rev}");
        }
        for args in [
            vec!["-1", "HEAD"],
            vec!["--max-count=1", "HEAD"],
            vec!["--max-count", "1", "HEAD"],
            vec!["-n", "1", "HEAD"],
            vec!["-n1", "HEAD"],
            vec!["--max-count=0", "HEAD"],
            vec!["--skip=0", "HEAD"],
            vec!["--skip=1", "HEAD"],
            vec!["--skip", "1", "HEAD"],
            vec!["--skip=1", "--max-count=1", "HEAD"],
            vec!["--default", "HEAD~1"],
            vec!["--default", "HEAD~1", "HEAD"],
            vec!["--no-walk", "HEAD"],
            vec!["--no-walk=sorted", "HEAD"],
            vec!["--no-walk=unsorted", "HEAD"],
            vec!["--do-walk", "--no-walk", "HEAD"],
            vec!["--no-walk", "--do-walk", "HEAD"],
            vec!["--first-parent", "HEAD"],
            vec!["--sparse", "HEAD"],
            vec!["--dense", "HEAD"],
            vec!["--remove-empty", "HEAD"],
            vec!["--unpacked", "HEAD"],
            vec!["--full-history", "HEAD"],
            vec!["--simplify-merges", "HEAD"],
            vec!["--show-pulls", "HEAD"],
            vec!["--no-merges", "HEAD"],
            vec!["--no-min-parents", "HEAD"],
            vec!["--no-max-parents", "HEAD"],
            vec!["--min-parents=0", "HEAD"],
            vec!["--max-parents=1", "HEAD"],
            vec!["--use-mailmap", "HEAD"],
            vec!["--mailmap", "HEAD"],
            vec!["--no-use-mailmap", "--use-mailmap", "HEAD"],
            vec!["HEAD", "--mailmap"],
            vec!["--notes", "HEAD"],
            vec!["--notes=commits", "HEAD"],
            vec!["--show-notes", "HEAD"],
            vec!["--show-notes=commits", "HEAD"],
            vec!["--no-notes", "HEAD"],
            vec!["--encoding=UTF-8", "HEAD"],
            vec!["--encoding=none", "HEAD"],
            vec!["--encoding=", "HEAD"],
            vec!["--show-signature", "HEAD"],
            vec!["--no-show-signature", "HEAD"],
            vec!["--no-color", "HEAD"],
            vec!["--color", "HEAD"],
            vec!["--color=always", "HEAD"],
            vec!["--color=auto", "HEAD"],
            vec!["--color=never", "HEAD"],
            vec!["--clear-decorations", "HEAD"],
            vec!["HEAD", "--clear-decorations"],
            vec!["--clear-decorations", "--no-decorate", "HEAD"],
            vec!["--decorate=auto", "HEAD"],
            vec!["HEAD", "--decorate=auto"],
            vec!["--decorate=", "HEAD"],
            vec!["--decorate=false", "HEAD"],
            vec!["--decorate=0", "HEAD"],
            vec!["--decorate=off", "HEAD"],
            vec!["--decorate-refs", "refs/tags/*", "HEAD"],
            vec!["--decorate-refs=refs/tags/*", "HEAD"],
            vec!["--decorate-refs-exclude", "refs/tags/*", "HEAD"],
            vec!["--decorate-refs-exclude=refs/tags/*", "HEAD"],
            vec!["--no-decorate-refs", "HEAD"],
            vec!["--no-decorate-refs-exclude", "HEAD"],
            vec!["--do-walk", "HEAD"],
            vec!["--date=raw", "HEAD"],
            vec!["--date=unix", "HEAD"],
            vec!["--date=relative", "HEAD"],
            vec!["--date=local", "HEAD"],
            vec!["--date=iso", "HEAD"],
            vec!["--date=iso-local", "HEAD"],
            vec!["--date=iso-strict", "HEAD"],
            vec!["--date=iso-strict-local", "HEAD"],
            vec!["--date=rfc", "HEAD"],
            vec!["--date=rfc2822-local", "HEAD"],
            vec!["--date=short", "HEAD"],
            vec!["--date=default-local", "HEAD"],
            vec!["--date=human", "HEAD"],
            vec!["--date=format:%Y", "HEAD"],
            vec!["--date=format-local:%Y", "HEAD"],
            vec!["--date=auto:bad", "HEAD"],
            vec!["--date", "raw", "HEAD"],
            vec!["--no-patch", "HEAD"],
            vec!["--no-diff-merges", "HEAD"],
            vec!["--diff-merges=off", "HEAD"],
            vec!["--diff-merges=none", "HEAD"],
            vec!["--diff-merges", "off", "HEAD"],
            vec!["--diff-merges", "none", "HEAD"],
            vec!["--full-diff", "HEAD"],
            vec!["--relative", "HEAD"],
            vec!["--relative=sub", "HEAD"],
            vec!["--relative=", "HEAD"],
            vec!["--no-relative", "HEAD"],
            vec!["--ext-diff", "HEAD"],
            vec!["--no-ext-diff", "HEAD"],
            vec!["--no-renames", "HEAD"],
            vec!["--find-renames", "HEAD"],
            vec!["--find-renames=50%", "HEAD"],
            vec!["--find-renames=", "HEAD"],
            vec!["-M", "HEAD"],
            vec!["-M50%", "HEAD"],
            vec!["--find-copies", "HEAD"],
            vec!["--find-copies=50%", "HEAD"],
            vec!["--find-copies=", "HEAD"],
            vec!["--find-copies-harder", "HEAD"],
            vec!["--no-find-copies-harder", "HEAD"],
            vec!["-C", "HEAD"],
            vec!["-C50%", "HEAD"],
            vec!["--minimal", "HEAD"],
            vec!["--patience", "HEAD"],
            vec!["--histogram", "HEAD"],
            vec!["--diff-algorithm=default", "HEAD"],
            vec!["--diff-algorithm=myers", "HEAD"],
            vec!["--diff-algorithm=minimal", "HEAD"],
            vec!["--diff-algorithm=patience", "HEAD"],
            vec!["--diff-algorithm=histogram", "HEAD"],
            vec!["--diff-algorithm", "myers", "HEAD"],
            vec!["--indent-heuristic", "HEAD"],
            vec!["--no-indent-heuristic", "HEAD"],
            vec!["--anchored=hello", "HEAD"],
            vec!["--anchored=", "HEAD"],
            vec!["--anchored", "hello", "HEAD"],
            vec!["HEAD~1", "--anchored", "hello"],
            vec!["--ignore-space-at-eol", "HEAD"],
            vec!["--ignore-cr-at-eol", "HEAD"],
            vec!["--ignore-space-change", "HEAD"],
            vec!["--ignore-all-space", "HEAD"],
            vec!["--ignore-blank-lines", "HEAD"],
            vec!["-b", "HEAD"],
            vec!["-w", "HEAD"],
            vec!["-bw", "HEAD"],
            vec!["-wb", "HEAD"],
            vec!["--inter-hunk-context=3", "HEAD"],
            vec!["--inter-hunk-context=1k", "HEAD"],
            vec!["--inter-hunk-context=1K", "HEAD"],
            vec!["--inter-hunk-context", "3", "HEAD"],
            vec!["HEAD~1", "--inter-hunk-context", "3"],
            vec!["--function-context", "HEAD"],
            vec!["-W", "HEAD"],
            vec!["--src-prefix=old/", "HEAD"],
            vec!["--src-prefix=", "HEAD"],
            vec!["--src-prefix", "old/", "HEAD"],
            vec!["--dst-prefix=new/", "HEAD"],
            vec!["--dst-prefix=", "HEAD"],
            vec!["--dst-prefix", "new/", "HEAD"],
            vec!["--no-prefix", "HEAD"],
            vec!["--default-prefix", "HEAD"],
            vec!["--output-indicator-new=>", "HEAD"],
            vec!["--output-indicator-new=", "HEAD"],
            vec!["--output-indicator-new", ">", "HEAD"],
            vec!["--output-indicator-old=<", "HEAD"],
            vec!["--output-indicator-context=.", "HEAD"],
            vec!["--full-index", "HEAD"],
            vec!["--abbrev", "HEAD"],
            vec!["--abbrev=12", "HEAD"],
            vec!["--abbrev=bad", "HEAD"],
            vec!["--abbrev=", "HEAD"],
            vec!["--no-abbrev", "HEAD"],
            vec!["--no-abbrev-commit", "HEAD"],
            vec!["--break-rewrites", "HEAD"],
            vec!["--break-rewrites=50%", "HEAD"],
            vec!["--break-rewrites=", "HEAD"],
            vec!["--break-rewrites=50/60", "HEAD"],
            vec!["--break-rewrites=/60", "HEAD"],
            vec!["--break-rewrites=50/", "HEAD"],
            vec!["-B", "HEAD"],
            vec!["-B50%", "HEAD"],
            vec!["-B50/60", "HEAD"],
            vec!["-B/60", "HEAD"],
            vec!["-B50/", "HEAD"],
            vec!["-D", "HEAD"],
            vec!["-m", "HEAD"],
            vec!["-s", "HEAD"],
            vec!["--irreversible-delete", "HEAD"],
            vec!["--textconv", "HEAD"],
            vec!["--no-textconv", "HEAD"],
            vec!["--submodule", "HEAD"],
            vec!["--submodule=short", "HEAD"],
            vec!["--submodule=log", "HEAD"],
            vec!["--submodule=diff", "HEAD"],
            vec!["--ignore-submodules", "HEAD"],
            vec!["--ignore-submodules=none", "HEAD"],
            vec!["--ignore-submodules=untracked", "HEAD"],
            vec!["--ignore-submodules=dirty", "HEAD"],
            vec!["--ignore-submodules=all", "HEAD"],
            vec!["--color-moved", "HEAD"],
            vec!["--color-moved=", "HEAD"],
            vec!["--color-moved=no", "HEAD"],
            vec!["--color-moved=true", "HEAD"],
            vec!["--color-moved=1", "HEAD"],
            vec!["--color-moved=on", "HEAD"],
            vec!["--color-moved=yes", "HEAD"],
            vec!["--color-moved=false", "HEAD"],
            vec!["--color-moved=0", "HEAD"],
            vec!["--color-moved=off", "HEAD"],
            vec!["--color-moved=default", "HEAD"],
            vec!["--color-moved=blocks", "HEAD"],
            vec!["--color-moved=zebra", "HEAD"],
            vec!["--color-moved=dimmed-zebra", "HEAD"],
            vec!["--color-moved=plain", "HEAD"],
            vec!["--no-color-moved", "HEAD"],
            vec!["--color-moved-ws=no", "HEAD"],
            vec!["--color-moved-ws=ignore-space-change", "HEAD"],
            vec!["--color-moved-ws=ignore-space-at-eol", "HEAD"],
            vec!["--color-moved-ws=ignore-all-space", "HEAD"],
            vec!["--color-moved-ws=allow-indentation-change", "HEAD"],
            vec![
                "--color-moved-ws=ignore-space-change,ignore-space-at-eol",
                "HEAD",
            ],
            vec!["--color-moved-ws", "ignore-all-space", "HEAD"],
            vec!["--ws-error-highlight=", "HEAD"],
            vec!["--ws-error-highlight=all", "HEAD"],
            vec!["--ws-error-highlight=default", "HEAD"],
            vec!["--ws-error-highlight=none", "HEAD"],
            vec!["--ws-error-highlight=old", "HEAD"],
            vec!["--ws-error-highlight=new", "HEAD"],
            vec!["--ws-error-highlight=context", "HEAD"],
            vec!["--ws-error-highlight=old,new,context", "HEAD"],
            vec!["--ws-error-highlight", "all", "HEAD"],
            vec!["--ita-visible-in-index", "HEAD"],
            vec!["--ita-invisible-in-index", "HEAD"],
            vec!["--pickaxe-all", "HEAD"],
            vec!["--pickaxe-regex", "HEAD"],
        ] {
            let expected = expected_log_args(&root, &args);
            let mut git_rs_args = vec!["log"];
            git_rs_args.extend_from_slice(&args);
            let actual = git_rs(&root, &git_rs_args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }
        for (args, stdin) in [
            (vec!["log", "--stdin", "--format=%s"], b"HEAD\n".to_vec()),
            (
                vec!["log", "--stdin", "--default", "HEAD~1", "--format=%s"],
                Vec::new(),
            ),
            (
                vec!["log", "--stdin", "--format=%s"],
                b"HEAD~1..HEAD\n".to_vec(),
            ),
            (
                vec!["log", "--stdin", "--format=%s", "HEAD"],
                b"^HEAD~1\n".to_vec(),
            ),
            (
                vec!["log", "--stdin", "--format=%s"],
                b"--not\nHEAD~1\n--not\nHEAD\n".to_vec(),
            ),
        ] {
            assert_eq!(
                git_rs_with_stdin(&root, &args, &stdin),
                git_with_stdin(&root, &args, &stdin),
                "sley log output differed for {args:?} with stdin {:?}",
                String::from_utf8_lossy(&stdin)
            );
        }
        for args in [
            vec!["log", "--use-mailmap=value", "HEAD"],
            vec!["log", "--mailmap=value", "HEAD"],
            vec!["log", "--no-use-mailmap=value", "HEAD"],
            vec!["log", "--no-mailmap=value", "HEAD"],
            vec!["log", "--no-notes=value", "HEAD"],
            vec!["log", "--no-notes=", "HEAD"],
            vec!["log", "--no-show-signature=value", "HEAD"],
            vec!["log", "--no-show-signature=", "HEAD"],
            vec!["log", "--no-color=value", "HEAD"],
            vec!["log", "--color=", "HEAD"],
            vec!["log", "--color=no", "HEAD"],
            vec!["log", "--color=false", "HEAD"],
            vec!["log", "--color=invalid", "HEAD"],
            vec!["log", "--clear-decorations=value", "HEAD"],
            vec!["log", "--no-decorate-refs=value", "HEAD"],
            vec!["log", "--no-decorate-refs-exclude=value", "HEAD"],
            vec!["log", "--do-walk=value", "HEAD"],
            vec!["log", "--do-walk=", "HEAD"],
            vec!["log", "--no-walk=", "HEAD"],
            vec!["log", "--no-walk=bad", "HEAD"],
            vec!["log", "--no-walk=value", "HEAD"],
            vec!["log", "--first-parent=value", "HEAD"],
            vec!["log", "--no-first-parent", "HEAD"],
            vec!["log", "--no-first-parent=value", "HEAD"],
            vec!["log", "--merges=value", "HEAD"],
            vec!["log", "--no-merges=value", "HEAD"],
            vec!["log", "--no-min-parents=value", "HEAD"],
            vec!["log", "--no-max-parents=value", "HEAD"],
            vec!["log", "--min-parents=bad", "HEAD"],
            vec!["log", "--min-parents=", "HEAD"],
            vec!["log", "--max-parents=bad", "HEAD"],
            vec!["log", "--max-parents=", "HEAD"],
            vec!["log", "--parents=value", "HEAD"],
            vec!["log", "--parents=", "HEAD"],
            vec!["log", "--no-parents", "HEAD"],
            vec!["log", "--no-parents=value", "HEAD"],
            vec!["log", "--children=value", "HEAD"],
            vec!["log", "--children=", "HEAD"],
            vec!["log", "--no-children", "HEAD"],
            vec!["log", "--no-children=value", "HEAD"],
            vec!["log", "--parents", "--children", "HEAD"],
            vec!["log", "--date"],
            vec!["log", "--date=", "HEAD"],
            vec!["log", "--date=bad", "HEAD"],
            vec!["log", "--date=auto", "HEAD"],
            vec!["log", "--date", "bad", "HEAD"],
            vec!["log", "--no-patch=value", "HEAD"],
            vec!["log", "--no-patch=", "HEAD"],
            vec!["log", "--no-diff-merges=value", "HEAD"],
            vec!["log", "--diff-merges"],
            vec!["log", "--diff-merges=", "HEAD"],
            vec!["log", "--diff-merges=bad", "HEAD"],
            vec!["log", "--diff-merges", "bad", "HEAD"],
            vec!["log", "--no-relative=value", "HEAD"],
            vec!["log", "--ext-diff=value", "HEAD"],
            vec!["log", "--no-ext-diff=value", "HEAD"],
            vec!["log", "--no-renames=value", "HEAD"],
            vec!["log", "--find-renames=foo", "HEAD"],
            vec!["log", "-Mfoo", "HEAD"],
            vec!["log", "--find-copies=foo", "HEAD"],
            vec!["log", "-Cfoo", "HEAD"],
            vec!["log", "--find-copies-harder=value", "HEAD"],
            vec!["log", "--no-find-copies-harder=value", "HEAD"],
            vec!["log", "--diff-algorithm=bad", "HEAD"],
            vec!["log", "--diff-algorithm=", "HEAD"],
            vec!["log", "--diff-algorithm"],
            vec!["log", "--indent-heuristic=value", "HEAD"],
            vec!["log", "--no-indent-heuristic=value", "HEAD"],
            vec!["log", "--anchored"],
            vec!["log", "--ignore-space-at-eol=value", "HEAD"],
            vec!["log", "--ignore-cr-at-eol=value", "HEAD"],
            vec!["log", "--ignore-space-change=value", "HEAD"],
            vec!["log", "--ignore-all-space=value", "HEAD"],
            vec!["log", "--ignore-blank-lines=value", "HEAD"],
            vec!["log", "--inter-hunk-context"],
            vec!["log", "--inter-hunk-context=", "HEAD"],
            vec!["log", "--inter-hunk-context=bad", "HEAD"],
            vec!["log", "--inter-hunk-context=1x", "HEAD"],
            vec!["log", "--function-context=value", "HEAD"],
            vec!["log", "--src-prefix"],
            vec!["log", "--dst-prefix"],
            vec!["log", "--no-prefix=value", "HEAD"],
            vec!["log", "--default-prefix=value", "HEAD"],
            vec!["log", "--output-indicator-new"],
            vec!["log", "--output-indicator-new=abc", "HEAD"],
            vec!["log", "--output-indicator-old=abc", "HEAD"],
            vec!["log", "--output-indicator-context=abc", "HEAD"],
            vec!["log", "--output-indicator-new", "HEAD"],
            vec!["log", "--full-index=value", "HEAD"],
            vec!["log", "--no-abbrev=value", "HEAD"],
            vec!["log", "--abbrev-commit=value", "HEAD"],
            vec!["log", "--no-abbrev-commit=value", "HEAD"],
            vec!["log", "--topo-order=value", "HEAD"],
            vec!["log", "--date-order=value", "HEAD"],
            vec!["log", "--author-date-order=value", "HEAD"],
            vec!["log", "--sparse=value", "HEAD"],
            vec!["log", "--dense=value", "HEAD"],
            vec!["log", "--remove-empty=value", "HEAD"],
            vec!["log", "--unpacked=value", "HEAD"],
            vec!["log", "--full-history=value", "HEAD"],
            vec!["log", "--simplify-merges=value", "HEAD"],
            vec!["log", "--show-pulls=value", "HEAD"],
            vec!["log", "--all=value", "HEAD"],
            vec!["log", "--no-all", "HEAD"],
            vec!["log", "--no-all=value", "HEAD"],
            vec!["log", "--no-branches", "HEAD"],
            vec!["log", "--no-branches=value", "HEAD"],
            vec!["log", "--no-tags", "HEAD"],
            vec!["log", "--no-tags=value", "HEAD"],
            vec!["log", "--no-remotes", "HEAD"],
            vec!["log", "--no-remotes=value", "HEAD"],
            vec!["log", "--break-rewrites=foo", "HEAD"],
            vec!["log", "--break-rewrites=1x2", "HEAD"],
            vec!["log", "-Bfoo", "HEAD"],
            vec!["log", "-B=50", "HEAD"],
            vec!["log", "--irreversible-delete=value", "HEAD"],
            vec!["log", "--textconv=value", "HEAD"],
            vec!["log", "--no-textconv=value", "HEAD"],
            vec!["log", "--submodule=", "HEAD"],
            vec!["log", "--submodule=bad", "HEAD"],
            vec!["log", "--ignore-submodules=", "HEAD"],
            vec!["log", "--ignore-submodules=bad", "HEAD"],
            vec!["log", "--color-moved=bad", "HEAD"],
            vec!["log", "--no-color-moved=value", "HEAD"],
            vec!["log", "--color-moved-ws"],
            vec!["log", "--color-moved-ws=", "HEAD"],
            vec!["log", "--color-moved-ws=bad", "HEAD"],
            vec![
                "log",
                "--color-moved-ws=allow-indentation-change,ignore-all-space",
                "HEAD",
            ],
            vec!["log", "--ws-error-highlight"],
            vec!["log", "--ws-error-highlight=bad", "HEAD"],
            vec!["log", "--ws-error-highlight=new,bad", "HEAD"],
            vec!["log", "--ita-visible-in-index=value", "HEAD"],
            vec!["log", "--ita-invisible-in-index=value", "HEAD"],
            vec!["log", "--pickaxe-all=value", "HEAD"],
            vec!["log", "--pickaxe-regex=value", "HEAD"],
            vec!["log", "--decorate-refs"],
            vec!["log", "--decorate-refs-exclude"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_oneline_matches_upstream_git() {
    let root = unique_temp_dir("log-oneline");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("hello.txt"), b"hello\n").expect("write fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "initial subject",
                "-q",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "v1.0",
                "-m",
                "release",
            ],
        );
        fs::write(root.join("hello.txt"), b"hello again\n").expect("update fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "second subject",
                "-q",
            ],
        );
        git(&root, &["branch", "side"]);

        for args in [
            vec!["log", "--oneline", "HEAD"],
            vec!["log", "--oneline", "-1", "HEAD"],
            vec!["log", "--oneline", "--max-count=1", "HEAD"],
            vec!["log", "--abbrev=12", "--oneline", "-1", "HEAD"],
            vec!["log", "--no-abbrev", "--oneline", "-1", "HEAD"],
            vec!["log", "--oneline", "refs/tags/v1.0"],
            vec!["log", "--decorate", "--oneline", "HEAD"],
            vec!["log", "--decorate=short", "--oneline", "HEAD"],
            vec!["log", "--decorate=true", "--oneline", "HEAD"],
            vec!["log", "--decorate=1", "--oneline", "HEAD"],
            vec!["log", "--decorate=on", "--oneline", "HEAD"],
            vec!["log", "--decorate=yes", "--oneline", "HEAD"],
            vec!["log", "--decorate=full", "--oneline", "HEAD"],
            vec!["log", "--decorate", "--pretty=oneline", "-1", "HEAD"],
            vec!["log", "--decorate", "--format=%H", "-1", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_pretty_oneline_aliases_match_upstream_git() {
    let root = unique_temp_dir("log-pretty-oneline");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("hello.txt"), b"hello\n").expect("write fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "initial subject",
                "-q",
            ],
        );
        fs::write(root.join("hello.txt"), b"hello again\n").expect("update fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "second subject",
                "-q",
            ],
        );

        for args in [
            vec!["log", "--pretty=oneline", "HEAD"],
            vec!["log", "--abbrev-commit", "--pretty=oneline", "-1", "HEAD"],
            vec![
                "log",
                "--abbrev=12",
                "--abbrev-commit",
                "--pretty=oneline",
                "-1",
                "HEAD",
            ],
            vec![
                "log",
                "--abbrev-commit",
                "--no-abbrev",
                "--pretty=oneline",
                "-1",
                "HEAD",
            ],
            vec!["log", "--format=oneline", "-1", "HEAD"],
            vec!["log", "--abbrev-commit", "--pretty=short", "-1", "HEAD"],
            vec![
                "log",
                "--abbrev=12",
                "--abbrev-commit",
                "--format=short",
                "-1",
                "HEAD",
            ],
            vec![
                "log",
                "--abbrev-commit",
                "--no-abbrev",
                "--pretty=short",
                "-1",
                "HEAD",
            ],
            vec![
                "log",
                "--abbrev-commit",
                "--no-abbrev-commit",
                "--pretty=short",
                "-1",
                "HEAD",
            ],
            vec!["log", "--pretty=short", "-1", "HEAD"],
            vec!["log", "--format=short", "-1", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_first_parent_matches_upstream_git() {
    let root = unique_temp_dir("log-first-parent");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("base.txt"), b"base\n").expect("write fixture");
        git(&root, &["add", "base.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );
        git(&root, &["checkout", "-qb", "side"]);
        fs::write(root.join("side.txt"), b"side\n").expect("write side fixture");
        git(&root, &["add", "side.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "side",
                "-q",
            ],
        );
        git(&root, &["tag", "v-side", "side"]);
        git(&root, &["checkout", "-q", "main"]);
        fs::write(root.join("main.txt"), b"main\n").expect("write main fixture");
        git(&root, &["add", "main.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "main",
                "-q",
            ],
        );
        git(&root, &["merge", "--no-ff", "side", "-m", "merge", "-q"]);
        git(&root, &["update-ref", "refs/remotes/origin/side", "side"]);
        git(&root, &["checkout", "-qb", "hidden", "HEAD~1"]);
        fs::write(root.join("hidden.txt"), b"hidden\n").expect("write hidden fixture");
        git(&root, &["add", "hidden.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "hidden",
                "-q",
            ],
        );
        git(&root, &["checkout", "-q", "main"]);
        git(
            &root,
            &["config", "--add", "transfer.hideRefs", "refs/heads/hidden"],
        );

        for args in [
            vec!["log", "--format=%s", "HEAD"],
            vec!["log", "--all", "--format=%s"],
            vec!["log", "--exclude-hidden=fetch", "--all", "--format=%s"],
            vec!["log", "--exclude-hidden", "receive", "--all", "--format=%s"],
            vec!["log", "--exclude-hidden=uploadpack", "--all", "--format=%s"],
            vec!["log", "--exclude=refs/heads/side", "--all", "--format=%s"],
            vec!["log", "--all", "--format=%s", "--max-count=2"],
            vec!["log", "--branches", "--format=%s"],
            vec!["log", "--exclude=side", "--branches", "--format=%s"],
            vec!["log", "--branches=s*", "--format=%s"],
            vec!["log", "--tags", "--format=%s"],
            vec!["log", "--exclude=v*", "--tags", "--format=%s"],
            vec!["log", "--tags=v*", "--format=%s"],
            vec!["log", "--remotes", "--format=%s"],
            vec!["log", "--exclude=origin/side", "--remotes", "--format=%s"],
            vec!["log", "--remotes=origin/*", "--format=%s"],
            vec!["log", "--glob=refs/heads/*", "--format=%s"],
            vec!["log", "--glob", "refs/heads/s*", "--format=%s"],
            vec![
                "log",
                "--exclude=refs/heads/side",
                "--glob=refs/heads/*",
                "--format=%s",
            ],
            vec!["log", "--format=%s", "HEAD", "refs/heads/side"],
            vec!["log", "--format=%s", "HEAD", "^refs/heads/side"],
            vec!["log", "--format=%s", "HEAD", "--not", "refs/heads/side"],
            vec!["log", "--format=%s", "HEAD~1..HEAD"],
            vec!["log", "--format=%s", "refs/heads/side..HEAD"],
            vec!["log", "--format=%s", "HEAD...refs/heads/side"],
            vec!["log", "--topo-order", "--format=%s", "HEAD"],
            vec!["log", "--date-order", "--format=%s", "HEAD"],
            vec!["log", "--author-date-order", "--format=%s", "HEAD"],
            vec![
                "log",
                "--topo-order",
                "--format=%s",
                "--max-count=2",
                "HEAD",
            ],
            vec![
                "log",
                "--date-order",
                "--format=%s",
                "--max-count=2",
                "HEAD",
            ],
            vec![
                "log",
                "--author-date-order",
                "--format=%s",
                "--max-count=2",
                "HEAD",
            ],
            vec!["log", "--first-parent", "--format=%s", "HEAD"],
            vec!["log", "--no-walk", "--first-parent", "--format=%s", "HEAD"],
            vec!["log", "--merges", "--format=%s", "HEAD"],
            vec!["log", "--no-merges", "--format=%s", "HEAD"],
            vec!["log", "--min-parents=2", "--format=%s", "HEAD"],
            vec!["log", "--max-parents=1", "--format=%s", "HEAD"],
            vec!["log", "--merges", "--no-min-parents", "--format=%s", "HEAD"],
            vec!["log", "--parents", "--oneline", "-1", "HEAD"],
            vec!["log", "--parents", "--pretty=oneline", "-1", "HEAD"],
            vec!["log", "--parents", "--format=%H %s", "-1", "HEAD"],
            vec!["log", "--children", "--oneline", "HEAD"],
            vec!["log", "--children", "--pretty=oneline", "HEAD"],
            vec!["log", "--children", "--format=%H %s", "HEAD"],
            vec![
                "log",
                "--no-merges",
                "--no-max-parents",
                "--format=%s",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_author_filter_matches_upstream_git() {
    let root = unique_temp_dir("log-author-filter");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        for (author, subject) in [
            ("Alpha Author <alpha@example.invalid>", "alpha"),
            ("Beta Writer <beta@example.invalid>", "beta"),
            ("Gamma Author <gamma@example.invalid>", "gamma"),
        ] {
            git(
                &root,
                &[
                    "-c",
                    "user.name=Committer User",
                    "-c",
                    "user.email=committer@example.invalid",
                    "commit",
                    "--allow-empty",
                    "--author",
                    author,
                    "-m",
                    subject,
                    "-q",
                ],
            );
        }

        for args in [
            vec!["log", "--author=Alpha Author", "--format=%s"],
            vec!["log", "--author", "beta@example.invalid", "--format=%s"],
            vec!["log", "--author=Alpha", "--author=Gamma", "--format=%s"],
            vec!["log", r"--author=Alpha\|Gamma", "--format=%s"],
            vec!["log", "--author=[AB]eta", "--format=%s"],
            vec!["log", "--author=", "--format=%s"],
            vec!["log", "--author", "--format=%s"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }

        for args in [
            vec!["log", "--author"],
            vec!["log", "--author=["],
            vec!["log", "--no-author"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_committer_filter_matches_upstream_git() {
    let root = unique_temp_dir("log-committer-filter");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        for (committer_name, committer_email, subject) in [
            ("Alpha Committer", "alpha-commit@example.invalid", "alpha"),
            ("Beta Committer", "beta-commit@example.invalid", "beta"),
            ("Gamma Committer", "gamma-commit@example.invalid", "gamma"),
        ] {
            git(
                &root,
                &[
                    "-c",
                    &format!("user.name={committer_name}"),
                    "-c",
                    &format!("user.email={committer_email}"),
                    "commit",
                    "--allow-empty",
                    "--author",
                    "Shared Author <author@example.invalid>",
                    "-m",
                    subject,
                    "-q",
                ],
            );
        }

        for args in [
            vec!["log", "--committer=Alpha Committer", "--format=%s"],
            vec![
                "log",
                "--committer",
                "beta-commit@example.invalid",
                "--format=%s",
            ],
            vec![
                "log",
                "--committer=Alpha",
                "--committer=Gamma",
                "--format=%s",
            ],
            vec!["log", r"--committer=Alpha\|Gamma", "--format=%s"],
            vec!["log", "--committer=[AB]eta", "--format=%s"],
            vec!["log", "--committer=", "--format=%s"],
            vec!["log", "--committer", "--format=%s"],
            vec![
                "log",
                "--author",
                "Shared",
                "--committer",
                "Gamma",
                "--format=%s",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }

        for args in [
            vec!["log", "--committer"],
            vec!["log", "--committer=["],
            vec!["log", "--no-committer"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_epoch_age_filters_match_upstream_git() {
    let root = unique_temp_dir("log-age-filter");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        for (timestamp, subject) in [(1000, "one"), (2000, "two"), (3000, "three")] {
            let date = format!("@{timestamp} +0000");
            git_with_env(
                &root,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-m",
                    subject,
                    "-q",
                ],
                &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
            );
        }

        for args in [
            vec!["log", "--max-age=2000", "--format=%s"],
            vec!["log", "--max-age", "2000", "--format=%s"],
            vec!["log", "--min-age=2000", "--format=%s"],
            vec!["log", "--min-age", "2000", "--format=%s"],
            vec!["log", "--max-age=2000", "--min-age=2000", "--format=%s"],
            vec!["log", "--since=@2000 +0000", "--format=%s"],
            vec!["log", "--after", "@2000 +0000", "--format=%s"],
            vec!["log", "--until=@2000 +0000", "--format=%s"],
            vec!["log", "--before", "@2000 +0000", "--format=%s"],
            vec!["log", "--since=1970-01-01 00:33:20 +0000", "--format=%s"],
            vec!["log", "--after", "1970-01-01 01:33:20 +0100", "--format=%s"],
            vec!["log", "--until=1970-01-01T00:33:20 +0000", "--format=%s"],
            vec![
                "log",
                "--before",
                "1970-01-01T01:33:20 +0100",
                "--format=%s",
            ],
            vec![
                "log",
                "--since=@2000 +0000",
                "--until=@2000 +0000",
                "--format=%s",
            ],
            vec!["log", "--reverse", "--min-age=2000", "--format=%s"],
            vec!["log", "--max-count=1", "--min-age=2000", "--format=%s"],
            vec!["log", "--grep=two", "--max-age=2000", "--format=%s"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }

        for args in [
            vec!["log", "--max-age"],
            vec!["log", "--min-age"],
            vec!["log", "--max-age=", "--format=%s"],
            vec!["log", "--min-age=bad", "--format=%s"],
            vec!["log", "--no-max-age", "--format=%s"],
            vec!["log", "--no-min-age", "--format=%s"],
            vec!["log", "--since"],
            vec!["log", "--before"],
            vec!["log", "--no-since", "--format=%s"],
            vec!["log", "--no-before", "--format=%s"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_regexp_ignore_case_filters_match_upstream_git() {
    let root = unique_temp_dir("log-regexp-ignore-case");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        for (author, committer_name, committer_email, subject, body) in [
            (
                "Alpha Author <alpha-author@example.invalid>",
                "Alpha Committer",
                "alpha-commit@example.invalid",
                "Alpha Subject",
                "Shared Body",
            ),
            (
                "Beta Writer <beta-author@example.invalid>",
                "Beta Committer",
                "beta-commit@example.invalid",
                "Beta Subject",
                "Other Body",
            ),
        ] {
            git(
                &root,
                &[
                    "-c",
                    &format!("user.name={committer_name}"),
                    "-c",
                    &format!("user.email={committer_email}"),
                    "commit",
                    "--allow-empty",
                    "--author",
                    author,
                    "-m",
                    subject,
                    "-m",
                    body,
                    "-q",
                ],
            );
        }

        for args in [
            vec!["log", "-i", "--grep", "alpha", "--format=%s"],
            vec![
                "log",
                "--regexp-ignore-case",
                "--grep",
                "shared",
                "--format=%s",
            ],
            vec!["log", "-i", "--author", "alpha author", "--format=%s"],
            vec![
                "log",
                "--regexp-ignore-case",
                "--committer",
                "beta committer",
                "--format=%s",
            ],
            vec!["log", "-i", "--grep", "[ab]eta", "--format=%s"],
            vec!["log", "-i", "--format=%s"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }

        for args in [
            vec!["log", "--regexp-ignore-case=yes", "--format=%s"],
            vec!["log", "--no-regexp-ignore-case", "--format=%s"],
            vec!["log", "--no-regexp-ignore-case=yes", "--format=%s"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_fixed_string_filters_match_upstream_git() {
    let root = unique_temp_dir("log-fixed-strings");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        for (author, committer_name, committer_email, subject) in [
            (
                "A.B Author <ab-author@example.invalid>",
                "A.B Committer",
                "ab-commit@example.invalid",
                "A.B subject",
            ),
            (
                "AXB Author <axb-author@example.invalid>",
                "AXB Committer",
                "axb-commit@example.invalid",
                "AXB subject",
            ),
            (
                "Bracket Author <bracket-author@example.invalid>",
                "Bracket Committer",
                "bracket-commit@example.invalid",
                "[",
            ),
        ] {
            git(
                &root,
                &[
                    "-c",
                    &format!("user.name={committer_name}"),
                    "-c",
                    &format!("user.email={committer_email}"),
                    "commit",
                    "--allow-empty",
                    "--author",
                    author,
                    "-m",
                    subject,
                    "-q",
                ],
            );
        }

        for args in [
            vec!["log", "--fixed-strings", "--grep", "A.B", "--format=%s"],
            vec!["log", "-F", "--grep", "A.B", "--format=%s"],
            vec!["log", "--grep", "A.B", "--fixed-strings", "--format=%s"],
            vec![
                "log",
                "--fixed-strings",
                "--author",
                "A.B Author",
                "--format=%s",
            ],
            vec![
                "log",
                "--fixed-strings",
                "--committer",
                "A.B Committer",
                "--format=%s",
            ],
            vec![
                "log",
                "--fixed-strings",
                "--grep",
                r"A.B\|AXB",
                "--format=%s",
            ],
            vec!["log", "--grep", "[", "--fixed-strings", "--format=%s"],
            vec![
                "log",
                "--fixed-strings",
                "--basic-regexp",
                "--grep",
                "A.B",
                "--format=%s",
            ],
            vec![
                "log",
                "--fixed-strings",
                "--extended-regexp",
                "--grep",
                "A.B",
                "--format=%s",
            ],
            vec![
                "log",
                "--fixed-strings",
                "-i",
                "--grep",
                "a.b",
                "--format=%s",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }

        for args in [
            vec!["log", "--fixed-strings=yes", "--format=%s"],
            vec!["log", "--no-fixed-strings", "--format=%s"],
            vec!["log", "--basic-regexp=yes", "--format=%s"],
            vec!["log", "--no-basic-regexp", "--format=%s"],
            vec!["log", "--extended-regexp=yes", "--format=%s"],
            vec!["log", "--no-extended-regexp", "--format=%s"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_grep_filter_matches_upstream_git() {
    let root = unique_temp_dir("log-grep-filter");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        for (author, subject, body) in [
            (
                "Alpha Author <alpha@example.invalid>",
                "alpha subject",
                "shared body",
            ),
            (
                "Beta Writer <beta@example.invalid>",
                "beta subject",
                "other body",
            ),
            (
                "Gamma Author <gamma@example.invalid>",
                "gamma subject",
                "shared tail",
            ),
        ] {
            git(
                &root,
                &[
                    "-c",
                    "user.name=Committer User",
                    "-c",
                    "user.email=committer@example.invalid",
                    "commit",
                    "--allow-empty",
                    "--author",
                    author,
                    "-m",
                    subject,
                    "-m",
                    body,
                    "-q",
                ],
            );
        }

        for args in [
            vec!["log", "--grep=alpha", "--format=%s"],
            vec!["log", "--grep", "shared", "--format=%s"],
            vec!["log", "--grep", "alpha", "--grep", "gamma", "--format=%s"],
            vec![
                "log",
                "--grep",
                "subject",
                "--grep",
                "shared",
                "--all-match",
                "--format=%s",
            ],
            vec![
                "log",
                "--all-match",
                "--grep",
                "subject",
                "--grep",
                "shared",
                "--format=%s",
            ],
            vec!["log", "--grep", "beta", "--invert-grep", "--format=%s"],
            vec!["log", "--invert-grep", "--grep", "beta", "--format=%s"],
            vec![
                "log",
                "--author",
                "Alpha",
                "--grep",
                "shared",
                "--format=%s",
            ],
            vec!["log", "--author", "Alpha", "--grep", "beta", "--format=%s"],
            vec!["log", r"--grep=alpha\|gamma", "--format=%s"],
            vec!["log", "--grep=[AB]eta", "--format=%s"],
            vec!["log", "--grep=", "--format=%s"],
            vec!["log", "--grep", "--format=%s"],
            vec!["log", "--invert-grep", "--format=%s"],
            vec!["log", "--all-match", "--format=%s"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }

        for args in [
            vec!["log", "--grep"],
            vec!["log", "--grep=["],
            vec!["log", "--no-grep"],
            vec!["log", "--invert-grep=yes"],
            vec!["log", "--all-match=yes"],
            vec!["log", "--no-all-match"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_custom_format_placeholders_match_upstream_git() {
    let root = unique_temp_dir("log-custom-format");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("hello.txt"), b"hello\n").expect("write fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Committer User",
                "-c",
                "user.email=committer@example.invalid",
                "commit",
                "--author",
                "Author User <author@example.invalid>",
                "-m",
                "initial subject",
                "-m",
                "initial body",
                "-q",
            ],
        );
        fs::write(root.join("hello.txt"), b"hello again\n").expect("update fixture");
        git(&root, &["add", "hello.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Committer User",
                "-c",
                "user.email=committer@example.invalid",
                "commit",
                "--author",
                "Author User <author@example.invalid>",
                "-m",
                "second subject",
                "-m",
                "second body one",
                "-m",
                "second body two",
                "-q",
            ],
        );
        let tree = String::from_utf8(git(&root, &["write-tree"]))
            .expect("tree oid is utf8")
            .trim()
            .to_string();
        let parent = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("parent oid is utf8")
            .trim()
            .to_string();
        let encoded = String::from_utf8(git(
            &root,
            &[
                "-c",
                "i18n.commitEncoding=ISO-8859-1",
                "commit-tree",
                &tree,
                "-p",
                &parent,
                "-m",
                "encoded subject",
            ],
        ))
        .expect("encoded commit oid is utf8")
        .trim()
        .to_string();
        git(&root, &["update-ref", "HEAD", &encoded]);
        git(&root, &["tag", "v-second"]);
        git(&root, &["branch", "side"]);

        for args in [
            vec!["log", "--format=%H %h %s", "-2", "HEAD"],
            vec!["log", "--abbrev=12", "--format=%H %h %s", "-1", "HEAD"],
            vec!["log", "--abbrev=bad", "--format=%h %s", "-1", "HEAD"],
            vec!["log", "--no-abbrev", "--format=%h %s", "-1", "HEAD"],
            vec!["log", "--quiet", "--format=%H %s", "-1", "HEAD"],
            vec![
                "log",
                "--quiet",
                "--no-quiet",
                "--format=%H %s",
                "-1",
                "HEAD",
            ],
            vec!["log", "--no-source", "--format=%H %s", "-1", "HEAD"],
            vec!["log", "--no-use-mailmap", "--format=%H %s", "-1", "HEAD"],
            vec!["log", "--no-mailmap", "--format=%H %s", "-1", "HEAD"],
            vec!["log", "--no-decorate", "--format=%H %s", "-1", "HEAD"],
            vec!["log", "--decorate=no", "--format=%H %s", "-1", "HEAD"],
            vec!["log", "--format=%T %t %P %p %s", "-2", "HEAD"],
            vec!["log", "--format=%m|%s", "-1", "HEAD"],
            vec!["log", "--format=%an <%ae> %s", "-1", "HEAD"],
            vec!["log", "--format=%cn <%ce> %s", "-1", "HEAD"],
            vec!["log", "--format=%an|%ae|%cn|%ce|%s", "-1", "HEAD"],
            vec![
                "log",
                "--format=%aN|%aE|%al|%aL|%cN|%cE|%cl|%cL",
                "-1",
                "HEAD",
            ],
            vec!["log", "--format=%at|%ct|%s", "-1", "HEAD"],
            vec!["log", "--format=%ad|%cd", "-1", "HEAD"],
            vec!["log", "--date=raw", "--format=%ad|%cd", "-1", "HEAD"],
            vec!["log", "--date=unix", "--format=%ad|%cd", "-1", "HEAD"],
            vec!["log", "--date=short", "--format=%ad|%cd", "-1", "HEAD"],
            vec!["log", "--date=iso", "--format=%ad|%cd", "-1", "HEAD"],
            vec!["log", "--date=iso-strict", "--format=%ad|%cd", "-1", "HEAD"],
            vec!["log", "--date=rfc", "--format=%ad|%cd", "-1", "HEAD"],
            vec!["log", "--format=%e|%s", "-1", "HEAD"],
            vec!["log", "--format=%N|%s", "-1", "HEAD"],
            vec!["log", "--format=%S|%s", "-1", "HEAD"],
            vec![
                "log",
                "--no-color",
                "--format=%Credred%Creset|%C(auto)%C(red)%s",
                "-1",
                "HEAD",
            ],
            vec!["log", "--format=%G?|%GS|%GK|%GF|%GP|%GT|%GG", "-1", "HEAD"],
            vec![
                "log",
                "--format=%gD|%gd|%gn|%gN|%ge|%gE|%gs|%s",
                "-1",
                "HEAD",
            ],
            vec!["log", "--format=%f", "-1", "HEAD"],
            vec!["log", "--format=A%x20B%x2fC%x0aD", "-1", "HEAD"],
            vec!["log", "--format=bad:%xZZ:end|short:%x0:end", "-1", "HEAD"],
            vec![
                "log",
                "--format=%ai|%aI|%as|%aD|%ci|%cI|%cs|%cD",
                "-1",
                "HEAD",
            ],
            vec!["log", "--format=%b", "-1", "HEAD"],
            vec!["log", "--format=%B", "-1", "HEAD"],
            vec!["log", "--format=%d", "-1", "HEAD"],
            vec!["log", "--format=%D", "-1", "HEAD"],
            vec!["log", "--decorate=full", "--format=%d", "-1", "HEAD"],
            vec!["log", "--decorate=full", "--format=%D", "-1", "HEAD"],
            vec!["log", "--decorate=no", "--format=%d", "-1", "HEAD"],
            vec!["log", "--format=commit %H%nsubject %s", "-1", "HEAD"],
            vec!["log", "--format=%H %% %s", "-1", "HEAD"],
            vec!["log", "--pretty=format:%T|%t|%P|%p|%s", "-2", "HEAD"],
            vec!["log", "--pretty=format:x%bY", "-1", "HEAD"],
            vec!["log", "--pretty=format:x%BY", "-1", "HEAD"],
            vec!["log", "--pretty=format:%H %s", "-2", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_reverse_matches_upstream_git() {
    let root = unique_temp_dir("log-reverse");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        for (path, subject) in [
            ("one.txt", "first subject"),
            ("two.txt", "second subject"),
            ("three.txt", "third subject"),
        ] {
            fs::write(root.join(path), format!("{subject}\n")).expect("write fixture");
            git(&root, &["add", path]);
            git(
                &root,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "-m",
                    subject,
                    "-q",
                ],
            );
        }

        for args in [
            vec!["log", "--reverse", "--oneline", "HEAD"],
            vec!["log", "--reverse", "--oneline", "--max-count=2", "HEAD"],
            vec!["log", "--reverse", "--format=%s", "HEAD"],
            vec!["log", "--reverse", "--pretty=format:%H %s", "-2", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley log output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}
