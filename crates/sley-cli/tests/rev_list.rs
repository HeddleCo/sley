use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
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
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin)
        .expect("write stdin");
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

fn commit_file(root: &Path, name: &str, body: &str, message: &str) {
    fs::write(root.join(name), body).expect("write fixture");
    git(root, &["add", name]);
    git(
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            message,
            "-q",
        ],
    );
}

fn commit_file_at(root: &Path, name: &str, body: &str, message: &str, timestamp: i64) {
    fs::write(root.join(name), body).expect("write fixture");
    git(root, &["add", name]);
    let date = format!("@{timestamp} +0000");
    git_with_env(
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            message,
            "-q",
        ],
        &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
    );
}

fn commit_empty_with_identities(
    root: &Path,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
    subject: &str,
    body: &str,
) {
    git_with_env(
        root,
        &[
            "-c",
            "user.name=Fallback User",
            "-c",
            "user.email=fallback@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            subject,
            "-m",
            body,
            "-q",
        ],
        &[
            ("GIT_AUTHOR_NAME", author_name),
            ("GIT_AUTHOR_EMAIL", author_email),
            ("GIT_COMMITTER_NAME", committer_name),
            ("GIT_COMMITTER_EMAIL", committer_email),
        ],
    );
}

fn rev_parse(root: &Path, rev: &str) -> String {
    String::from_utf8(git(root, &["rev-parse", rev]))
        .expect("rev-parse output is utf8")
        .trim()
        .to_string()
}

fn current_branch(root: &Path) -> String {
    String::from_utf8(git(root, &["branch", "--show-current"]))
        .expect("branch output is utf8")
        .trim()
        .to_string()
}

#[test]
fn rev_list_linear_history_matches_upstream_git() {
    let root = unique_temp_dir("rev-list-linear");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        commit_file(&root, "file", "one\n", "one");
        commit_file(&root, "file", "two\n", "two");
        commit_file(&root, "file", "three\n", "three");
        let tree = rev_parse(&root, "HEAD^{tree}");
        let parent = rev_parse(&root, "HEAD");
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
                "encoded",
            ],
        ))
        .expect("encoded commit oid is utf8")
        .trim()
        .to_string();
        git(&root, &["update-ref", "HEAD", &encoded]);
        git(&root, &["tag", "v-three"]);
        git(&root, &["branch", "side"]);
        let base = rev_parse(&root, "HEAD~2");
        let middle = rev_parse(&root, "HEAD~1");
        let exclude_base = format!("^{base}");
        let base_to_head = format!("{base}..HEAD");
        let middle_to_head = format!("{middle}..HEAD");
        let base_to_implicit_head = format!("{base}..");
        let base_symmetric_head = format!("{base}...HEAD");

        let cases = [
            vec!["rev-list", "HEAD"],
            vec!["rev-list", "--max-count=2", "HEAD"],
            vec!["rev-list", "--max-count", "2", "HEAD"],
            vec!["rev-list", "-n", "2", "HEAD"],
            vec!["rev-list", "-n2", "HEAD"],
            vec!["rev-list", "-2", "HEAD"],
            vec!["rev-list", "--default", "HEAD"],
            vec!["rev-list", "--default", "HEAD", "HEAD~1"],
            vec!["rev-list", "--default", "HEAD", exclude_base.as_str()],
            vec!["rev-list", "--not", "--default", "HEAD"],
            vec!["rev-list", "--skip=1", "HEAD"],
            vec!["rev-list", "--skip", "2", "HEAD"],
            vec!["rev-list", "--skip=1", "--max-count=1", "HEAD"],
            vec!["rev-list", "--max-count=1", "--skip=1", "HEAD"],
            vec!["rev-list", "--skip=1", "--count", "HEAD"],
            vec!["rev-list", "--reverse", "--max-count=2", "HEAD"],
            vec!["rev-list", "--reverse", "--skip=1", "--max-count=1", "HEAD"],
            vec!["rev-list", "--sparse", "HEAD"],
            vec!["rev-list", "--dense", "HEAD"],
            vec!["rev-list", "--remove-empty", "HEAD"],
            vec!["rev-list", "--unpacked", "HEAD"],
            vec!["rev-list", "--full-history", "HEAD"],
            vec!["rev-list", "--simplify-merges", "HEAD"],
            vec!["rev-list", "--show-pulls", "HEAD"],
            vec!["rev-list", "--author-date-order", "HEAD"],
            vec!["rev-list", "--exclude-promisor-objects", "HEAD"],
            vec![
                "rev-list",
                "--sparse",
                "--dense",
                "--remove-empty",
                "--unpacked",
                "--full-history",
                "--simplify-merges",
                "--show-pulls",
                "--exclude-promisor-objects",
                "HEAD",
            ],
            vec!["rev-list", "--parents", "HEAD"],
            vec!["rev-list", "--children", "HEAD"],
            vec!["rev-list", "--children", "--abbrev-commit", "HEAD"],
            vec!["rev-list", "--children", "--max-count=2", "HEAD"],
            vec!["rev-list", "-z", "HEAD"],
            vec!["rev-list", "-z", "--parents", "HEAD"],
            vec!["rev-list", "-z", "--children", "HEAD"],
            vec!["rev-list", "-z", "--count", "HEAD"],
            vec!["rev-list", "--object-names", "HEAD"],
            vec!["rev-list", "--no-object-names", "HEAD"],
            vec!["rev-list", "--object-names", "--no-object-names", "HEAD"],
            vec!["rev-list", "--objects", "HEAD"],
            vec![
                "rev-list",
                "--objects-edge-aggressive",
                base_to_head.as_str(),
            ],
            vec!["rev-list", "--objects", "--filter=blob:none", "HEAD"],
            vec!["rev-list", "--filter=blob:none", "--objects", "HEAD"],
            vec![
                "rev-list",
                "--objects",
                "--filter=blob:none",
                "--no-filter",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--filter=blob:none",
                "--no-filter",
                "--objects",
                "HEAD",
            ],
            vec!["rev-list", "--no-filter", "HEAD"],
            vec!["rev-list", "--objects", "--no-filter", "HEAD"],
            vec!["rev-list", "--objects", "--filter=blob:limit=1", "HEAD"],
            vec!["rev-list", "--objects", "--filter=blob:limit=5", "HEAD"],
            vec!["rev-list", "--filter=blob:limit=5", "--objects", "HEAD"],
            vec!["rev-list", "--objects", "--filter=tree:0", "HEAD"],
            vec!["rev-list", "--filter=tree:0", "--objects", "HEAD"],
            vec!["rev-list", "--objects", "--filter=tree:1", "HEAD"],
            vec!["rev-list", "--filter=tree:1", "--objects", "HEAD"],
            vec!["rev-list", "--objects", "--filter=tree:2", "HEAD"],
            vec!["rev-list", "--objects", "--filter=object:type=blob", "HEAD"],
            vec!["rev-list", "--filter=object:type=blob", "--objects", "HEAD"],
            vec!["rev-list", "--objects", "--filter=object:type=tree", "HEAD"],
            vec![
                "rev-list",
                "--objects",
                "--filter=object:type=commit",
                "HEAD",
            ],
            vec!["rev-list", "--objects", "--no-object-names", "HEAD"],
            vec!["rev-list", "--objects", "--object-names", "HEAD"],
            vec!["rev-list", "--objects", "--count", "HEAD"],
            vec!["rev-list", "--objects", "--max-count=2", "HEAD"],
            vec!["rev-list", "--objects", "--reverse", "HEAD"],
            vec!["rev-list", "--objects", "-z", "HEAD"],
            vec!["rev-list", "--disk-usage", "HEAD"],
            vec!["rev-list", "--disk-usage=human", "HEAD"],
            vec!["rev-list", "--objects", "--disk-usage", "HEAD"],
            vec!["rev-list", "--header", "--max-count=1", "HEAD"],
            vec!["rev-list", "--header", "--parents", "--max-count=1", "HEAD"],
            vec!["rev-list", "--header", "--count", "HEAD"],
            vec!["rev-list", "--header", "--objects", "--max-count=1", "HEAD"],
            vec!["rev-list", "--pretty=oneline", "--max-count=2", "HEAD"],
            vec!["rev-list", "--format=oneline", "--max-count=2", "HEAD"],
            vec!["rev-list", "--oneline", "--max-count=2", "HEAD"],
            vec![
                "rev-list",
                "--pretty=oneline",
                "--parents",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--pretty=oneline",
                "--objects",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--header",
                "--pretty=oneline",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--pretty=oneline",
                "--abbrev-commit",
                "--abbrev=12",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--pretty=short", "--max-count=1", "HEAD"],
            vec!["rev-list", "--format=short", "--max-count=1", "HEAD"],
            vec![
                "rev-list",
                "--pretty=short",
                "--parents",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--pretty=short",
                "--objects",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--header",
                "--pretty=short",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--pretty=short",
                "--abbrev-commit",
                "--abbrev=12",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--format=%H|%P|%s", "--max-count=2", "HEAD"],
            vec!["rev-list", "--format=%m|%s", "--max-count=1", "HEAD"],
            vec!["rev-list", "--format=%d|%D", "--max-count=1", "HEAD"],
            vec![
                "rev-list",
                "--format=%aN|%aE|%al|%aL|%cN|%cE|%cl|%cL",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--format=%ad|%cd", "--max-count=1", "HEAD"],
            vec![
                "rev-list",
                "--date=raw",
                "--format=%ad|%cd",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--date=unix",
                "--format=%ad|%cd",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--date=short",
                "--format=%ad|%cd",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--date=iso",
                "--format=%ad|%cd",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--date=iso-strict",
                "--format=%ad|%cd",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--date=rfc",
                "--format=%ad|%cd",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--format=%e|%s", "--max-count=1", "HEAD"],
            vec!["rev-list", "--format=%N|%s", "--max-count=1", "HEAD"],
            vec!["rev-list", "--format=%S|%s", "--max-count=1", "HEAD"],
            vec![
                "rev-list",
                "--format=%G?|%GS|%GK|%GF|%GP|%GT|%GG",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--format=%gD|%gd|%gn|%gN|%ge|%gE|%gs|%s",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--format=%f", "--max-count=1", "HEAD"],
            vec![
                "rev-list",
                "--format=A%x20B%x2fC%x0aD",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--format=bad:%xZZ:end|short:%x0:end",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--format=%ai|%aI|%as|%aD|%ci|%cI|%cs|%cD",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--pretty=format:%H|%s", "--max-count=2", "HEAD"],
            vec![
                "rev-list",
                "--format=%H|%s",
                "--parents",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--format=%H|%s",
                "--objects",
                "--max-count=1",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--header",
                "--format=%H|%s",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--quiet", "HEAD"],
            vec!["rev-list", "--ignore-missing", "missing"],
            vec!["rev-list", "--ignore-missing", "HEAD", "missing"],
            vec!["rev-list", "--ignore-missing", "missing..HEAD"],
            vec!["rev-list", "--ignore-missing", "HEAD", "^missing"],
            vec!["rev-list", "--quiet", "--children", "HEAD"],
            vec!["rev-list", "--quiet", "--parents", "HEAD"],
            vec!["rev-list", "--count", "HEAD"],
            vec!["rev-list", "--count", "--children", "HEAD"],
            vec!["rev-list", "--quiet", "--count", "HEAD"],
            vec!["rev-list", "--abbrev-commit", "HEAD"],
            vec!["rev-list", "--abbrev-commit", "--abbrev=12", "HEAD"],
            vec!["rev-list", "--abbrev-commit", "--parents", "HEAD"],
            vec!["rev-list", "--abbrev-commit", "--no-abbrev-commit", "HEAD"],
            vec!["rev-list", "--abbrev-commit", "--no-abbrev", "HEAD"],
            vec![
                "rev-list",
                "--abbrev-commit",
                "--no-abbrev",
                "--abbrev=12",
                "HEAD",
            ],
            vec!["rev-list", "--timestamp", "HEAD"],
            vec!["rev-list", "--timestamp", "--parents", "HEAD"],
            vec!["rev-list", "--timestamp", "--abbrev-commit", "HEAD"],
            vec!["rev-list", "--timestamp", "--oneline", "HEAD"],
            vec!["rev-list", "--timestamp", "--pretty=oneline", "HEAD"],
            vec!["rev-list", "--timestamp", "--pretty=short", "HEAD"],
            vec!["rev-list", "--timestamp", "--format=%H|%s", "HEAD"],
            vec![
                "rev-list",
                "--timestamp",
                "--header",
                "--max-count=1",
                "HEAD",
            ],
            vec!["rev-list", "--all"],
            vec!["rev-list", "--count", "--all"],
            vec!["rev-list", "--exclude-hidden=fetch", "--all", "--count"],
            vec![
                "rev-list",
                "--exclude-hidden",
                "receive",
                "--all",
                "--count",
            ],
            vec![
                "rev-list",
                "--exclude-hidden=uploadpack",
                "--all",
                "--count",
            ],
            vec!["rev-list", "HEAD", exclude_base.as_str()],
            vec!["rev-list", "HEAD", "--not", "HEAD~1"],
            vec!["rev-list", "--not", "HEAD~1", "--not", "HEAD"],
            vec![
                "rev-list",
                "--count",
                "--not",
                exclude_base.as_str(),
                "--not",
                "HEAD",
            ],
            vec!["rev-list", "--not", base_to_head.as_str()],
            vec!["rev-list", "--parents", "HEAD", exclude_base.as_str()],
            vec!["rev-list", "--count", "HEAD", exclude_base.as_str()],
            vec!["rev-list", base_to_head.as_str()],
            vec!["rev-list", "--objects-edge", base_to_head.as_str()],
            vec![
                "rev-list",
                "--objects-edge",
                "--no-object-names",
                base_to_head.as_str(),
            ],
            vec![
                "rev-list",
                "--objects-edge",
                "--count",
                base_to_head.as_str(),
            ],
            vec![
                "rev-list",
                "--objects-edge",
                "--reverse",
                base_to_head.as_str(),
            ],
            vec!["rev-list", middle_to_head.as_str()],
            vec!["rev-list", "--boundary", base_to_head.as_str()],
            vec![
                "rev-list",
                "--timestamp",
                "--boundary",
                base_to_head.as_str(),
            ],
            vec!["rev-list", "--boundary", "--parents", base_to_head.as_str()],
            vec!["rev-list", "--boundary", "--oneline", base_to_head.as_str()],
            vec![
                "rev-list",
                "--timestamp",
                "--boundary",
                "--oneline",
                base_to_head.as_str(),
            ],
            vec![
                "rev-list",
                "--boundary",
                "--abbrev-commit",
                "--abbrev=12",
                base_to_head.as_str(),
            ],
            vec!["rev-list", "--boundary", "--count", base_to_head.as_str()],
            vec![
                "rev-list",
                "--boundary",
                "--format=%m|%s",
                base_to_head.as_str(),
            ],
            vec!["rev-list", "--disk-usage", base_to_head.as_str()],
            vec![
                "rev-list",
                "--boundary",
                "--disk-usage",
                base_to_head.as_str(),
            ],
            vec![
                "rev-list",
                "--boundary",
                "--objects",
                "--disk-usage",
                base_to_head.as_str(),
            ],
            vec!["rev-list", "--count", base_to_implicit_head.as_str()],
            vec!["rev-list", base_symmetric_head.as_str()],
            vec!["rev-list", "--count", base_symmetric_head.as_str()],
        ];
        for args in cases {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }

        for (args, stdin) in [
            (vec!["rev-list", "--stdin"], b"HEAD\n".to_vec()),
            (vec!["rev-list", "--stdin", "--count"], b"HEAD\n\n".to_vec()),
            (
                vec!["rev-list", "--stdin", "--parents"],
                format!("HEAD\n^{base}\n").into_bytes(),
            ),
            (
                vec!["rev-list", "--stdin", "--count"],
                format!("{base}..HEAD\n").into_bytes(),
            ),
            (
                vec!["rev-list", "--stdin", "--objects"],
                format!("{base}..HEAD\n").into_bytes(),
            ),
            (
                vec!["rev-list", "--stdin", "--objects-edge"],
                format!("{base}..HEAD\n").into_bytes(),
            ),
            (
                vec!["rev-list", "--stdin", "HEAD"],
                format!("^{middle}\n").into_bytes(),
            ),
            (
                vec!["rev-list", "--stdin", "--not", "HEAD~1"],
                b"HEAD\n".to_vec(),
            ),
            (
                vec!["rev-list", "--stdin"],
                b"--not\nHEAD~1\n--not\nHEAD\n".to_vec(),
            ),
            (
                vec!["rev-list", "--stdin"],
                format!("--not\n{base}..HEAD\n").into_bytes(),
            ),
            (vec!["rev-list", "--stdin", "--default", "HEAD"], Vec::new()),
            (
                vec!["rev-list", "--stdin", "--default", "HEAD"],
                b"HEAD~1\n".to_vec(),
            ),
        ] {
            assert_eq!(
                git_rs_with_stdin(&root, &args, &stdin),
                git_with_stdin(&root, &args, &stdin),
                "rev-list output differed for {args:?} with stdin {:?}",
                String::from_utf8_lossy(&stdin)
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_list_epoch_age_filters_match_upstream_git() {
    let root = unique_temp_dir("rev-list-age-filters");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        commit_file_at(&root, "file", "old\n", "old", 1000);
        commit_file_at(&root, "file", "middle\n", "middle", 2000);
        commit_file_at(&root, "file", "new\n", "new", 3000);

        for args in [
            vec!["rev-list", "--max-age=2000", "HEAD"],
            vec!["rev-list", "--max-age", "2000", "HEAD"],
            vec!["rev-list", "--min-age=2000", "HEAD"],
            vec!["rev-list", "--min-age", "2000", "HEAD"],
            vec!["rev-list", "--max-age=2000", "--min-age=2000", "HEAD"],
            vec!["rev-list", "--since=@2000 +0000", "HEAD"],
            vec!["rev-list", "--after", "@2000 +0000", "HEAD"],
            vec!["rev-list", "--until=@2000 +0000", "HEAD"],
            vec!["rev-list", "--before", "@2000 +0000", "HEAD"],
            vec!["rev-list", "--since=1970-01-01 00:33:20 +0000", "HEAD"],
            vec!["rev-list", "--after", "1970-01-01 01:33:20 +0100", "HEAD"],
            vec!["rev-list", "--until=1970-01-01T00:33:20 +0000", "HEAD"],
            vec!["rev-list", "--before", "1970-01-01T01:33:20 +0100", "HEAD"],
            vec![
                "rev-list",
                "--since=@2000 +0000",
                "--until=@2000 +0000",
                "HEAD",
            ],
            vec!["rev-list", "--count", "--max-age=2000", "HEAD"],
            vec!["rev-list", "--count", "--since=@2000 +0000", "HEAD"],
            vec!["rev-list", "--reverse", "--min-age=2000", "HEAD"],
            vec!["rev-list", "--reverse", "--until=@2000 +0000", "HEAD"],
            vec!["rev-list", "--max-count=1", "--min-age=2000", "HEAD"],
            vec!["rev-list", "--max-count=1", "--since=@2000 +0000", "HEAD"],
            vec!["rev-list", "--no-walk", "HEAD", "HEAD~1"],
            vec!["rev-list", "--no-walk=sorted", "HEAD~1", "HEAD"],
            vec!["rev-list", "--no-walk=unsorted", "HEAD~1", "HEAD"],
            vec!["rev-list", "--no-walk", "HEAD", "^HEAD"],
            vec!["rev-list", "--no-walk", "--do-walk", "--count", "HEAD"],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_list_identity_and_message_filters_match_upstream_git() {
    let root = unique_temp_dir("rev-list-filters");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        commit_empty_with_identities(
            &root,
            "Alpha Author",
            "alpha@example.invalid",
            "Alpha Committer",
            "alpha-commit@example.invalid",
            "Alpha Subject",
            "Shared Body",
        );
        commit_empty_with_identities(
            &root,
            "A.B Author",
            "literal@example.invalid",
            "A.B Committer",
            "literal-commit@example.invalid",
            "A.B Subject",
            "Literal A.B Body",
        );
        commit_empty_with_identities(
            &root,
            "Beta Author",
            "beta@example.invalid",
            "Beta Committer",
            "beta-commit@example.invalid",
            "Beta Subject",
            "Other Body",
        );

        for args in [
            vec!["rev-list", "--author=Alpha", "--format=%s", "HEAD"],
            vec![
                "rev-list",
                "--author",
                "Alpha Author",
                "--format=%s",
                "HEAD",
            ],
            vec!["rev-list", "--committer=Beta", "--format=%s", "HEAD"],
            vec![
                "rev-list",
                "--committer",
                "Beta Committer",
                "--format=%s",
                "HEAD",
            ],
            vec!["rev-list", "--grep=Shared", "--format=%s", "HEAD"],
            vec!["rev-list", "--grep", "Other Body", "--format=%s", "HEAD"],
            vec![
                "rev-list",
                "--grep=Subject",
                "--all-match",
                "--grep=Beta",
                "--format=%s",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--invert-grep",
                "--grep=Beta",
                "--format=%s",
                "HEAD",
            ],
            vec!["rev-list", "-i", "--author=alpha", "--format=%s", "HEAD"],
            vec![
                "rev-list",
                "--regexp-ignore-case",
                "--grep=shared body",
                "--format=%s",
                "HEAD",
            ],
            vec!["rev-list", "--grep=A.B", "--format=%s", "HEAD"],
            vec![
                "rev-list",
                "--fixed-strings",
                "--grep=A.B",
                "--format=%s",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--grep=A.B",
                "--fixed-strings",
                "--format=%s",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--fixed-strings",
                "--author=A.B Author",
                "--format=%s",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--fixed-strings",
                "--committer=A.B Committer",
                "--format=%s",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--fixed-strings",
                "--basic-regexp",
                "--grep=A.B",
                "--format=%s",
                "HEAD",
            ],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_list_annotated_tag_start_matches_upstream_git() {
    let root = unique_temp_dir("rev-list-tag");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        commit_file(&root, "file", "one\n", "one");
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "v1",
                "-m",
                "release",
            ],
        );
        for args in [
            vec!["rev-list", "v1"],
            vec!["rev-list", "--objects", "v1"],
            vec!["rev-list", "--objects", "--filter=object:type=tag", "v1"],
            vec!["rev-list", "--objects", "--filter=object:type=commit", "v1"],
            vec!["rev-list", "--objects", "--filter=object:type=blob", "v1"],
            vec!["rev-list", "--objects", "--filter=blob:none", "v1"],
            vec!["rev-list", "--objects", "--filter=tree:0", "v1"],
            vec!["rev-list", "--objects", "--filter=blob:limit=1", "v1"],
            vec!["rev-list", "--objects", "--tags"],
            vec![
                "rev-list",
                "--objects",
                "--filter=object:type=tag",
                "--tags",
            ],
            vec!["rev-list", "--objects", "--all"],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_list_ref_selector_modes_match_upstream_git() {
    let root = unique_temp_dir("rev-list-ref-selectors");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        commit_file(&root, "base-file", "base\n", "base");
        let main = current_branch(&root);
        let base = rev_parse(&root, "HEAD");
        git(&root, &["checkout", "-qb", "topic-branch"]);
        commit_file(&root, "topic-file", "topic\n", "topic");
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "main-file", "main\n", "main");
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "v-base",
                &base,
                "-m",
                "base release",
            ],
        );
        git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let tree = String::from_utf8(git(&root, &["write-tree"])).expect("tree is utf8");
        let tree = tree.trim();
        let isolated = String::from_utf8(git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit-tree",
                tree,
                "-m",
                "isolated",
            ],
        ))
        .expect("commit is utf8");
        let isolated = isolated.trim().to_string();
        git(&root, &["update-ref", "refs/tags/v-isolated", &isolated]);
        git(
            &root,
            &[
                "config",
                "--add",
                "transfer.hideRefs",
                "refs/heads/topic-branch",
            ],
        );
        git(
            &root,
            &[
                "config",
                "--add",
                "uploadpack.hideRefs",
                "refs/tags/v-isolated",
            ],
        );

        for args in [
            vec!["rev-list", "--branches", "--count"],
            vec!["rev-list", "--branches=topic-branch", "--count"],
            vec!["rev-list", "--branches=topic*", "--count"],
            vec!["rev-list", "--branches=refs/heads/topic*", "--count"],
            vec![
                "rev-list",
                "--exclude=topic-branch",
                "--branches",
                "--count",
            ],
            vec![
                "rev-list",
                "--exclude=refs/heads/topic-branch",
                "--all",
                "--count",
            ],
            vec!["rev-list", "--tags", "--count"],
            vec!["rev-list", "--tags=v-*", "--count"],
            vec!["rev-list", "--exclude=v-base", "--tags", "--count"],
            vec![
                "rev-list",
                "--exclude=v-isolated",
                "--branches",
                "--tags",
                "--count",
            ],
            vec![
                "rev-list",
                "--branches",
                "--exclude=v-isolated",
                "--tags",
                "--count",
            ],
            vec!["rev-list", "--remotes", "--count"],
            vec!["rev-list", "--remotes=origin", "--count"],
            vec!["rev-list", "--remotes=origin/*", "--count"],
            vec!["rev-list", "--exclude=origin/main", "--remotes", "--count"],
            vec!["rev-list", "--glob=refs/heads/*", "--count"],
            vec!["rev-list", "--glob", "refs/heads/*", "--count"],
            vec!["rev-list", "--glob=refs/heads", "--count"],
            vec!["rev-list", "--glob=refs/heads/", "--count"],
            vec!["rev-list", "--glob=refs/heads/topic-branch", "--count"],
            vec!["rev-list", "--glob=refs/heads/topic*", "--count"],
            vec!["rev-list", "--glob=heads/topic*", "--count"],
            vec!["rev-list", "--glob=refs/tags/v-*", "--count"],
            vec!["rev-list", "--glob=refs/remotes/origin/*", "--count"],
            vec![
                "rev-list",
                "--exclude=refs/heads/topic-branch",
                "--glob=refs/heads/*",
                "--count",
            ],
            vec![
                "rev-list",
                "--exclude=v-isolated",
                "--branches",
                "--glob=refs/tags/*",
                "--count",
            ],
            vec![
                "rev-list",
                "--branches",
                "--exclude=v-isolated",
                "--glob=refs/tags/*",
                "--count",
            ],
            vec!["rev-list", "--branches", "--tags", "--count"],
            vec![
                "rev-list",
                "--exclude-hidden",
                "receive",
                "--all",
                "--count",
            ],
            vec!["rev-list", "--exclude-hidden=fetch", "--all", "--count"],
            vec![
                "rev-list",
                "--exclude-hidden=uploadpack",
                "--glob=refs/tags/*",
                "--count",
            ],
            vec!["rev-list", "--branches", "--remotes", "--count"],
            vec![
                "rev-list",
                "--branches=topic*",
                "--tags=v-*",
                "--remotes=origin/*",
                "--count",
            ],
            vec!["rev-list", "--all", "--not", "--branches=topic*", "--count"],
            vec!["rev-list", "--all", "--not", "--tags=v-isolated", "--count"],
            vec![
                "rev-list",
                "--all",
                "--not",
                "--glob=refs/heads/topic*",
                "--count",
            ],
            vec![
                "rev-list",
                "--all",
                "--not",
                "--glob=refs/tags/v-isolated",
                "--count",
            ],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_list_symmetric_branch_range_count_matches_upstream_git() {
    let root = unique_temp_dir("rev-list-symmetric-branches");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        commit_file_at(&root, "file", "base\n", "base", 1000);
        git(&root, &["checkout", "-qb", "left-branch"]);
        commit_file_at(&root, "left", "left\n", "left", 3000);
        git(&root, &["checkout", "-qb", "right-branch", "HEAD~1"]);
        commit_file_at(&root, "right", "right\n", "right", 2000);

        for args in [
            vec!["rev-list", "--count", "left-branch...right-branch"],
            vec!["rev-list", "--left-right", "left-branch...right-branch"],
            vec![
                "rev-list",
                "--left-right",
                "--format=%m|%s",
                "left-branch...right-branch",
            ],
            vec!["rev-list", "--left-only", "left-branch...right-branch"],
            vec!["rev-list", "--right-only", "left-branch...right-branch"],
            vec![
                "rev-list",
                "--left-right",
                "--left-only",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--left-right",
                "--right-only",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--count",
                "--left-only",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--count",
                "--right-only",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--left-right",
                "--count",
                "--left-only",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--left-right",
                "--count",
                "--right-only",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--left-right",
                "--abbrev-commit",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--left-right",
                "--reverse",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--left-right",
                "--count",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--quiet",
                "--left-right",
                "left-branch...right-branch",
            ],
            vec![
                "rev-list",
                "--quiet",
                "--left-right",
                "--count",
                "left-branch...right-branch",
            ],
            vec!["rev-list", "--left-right", "right-branch"],
            vec!["rev-list", "--objects-edge", "left-branch...right-branch"],
            vec![
                "rev-list",
                "--count",
                "left-branch",
                "right-branch",
                "^HEAD~1",
            ],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_list_first_parent_merge_history_matches_upstream_git() {
    let root = unique_temp_dir("rev-list-first-parent");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let main = current_branch(&root);
        commit_file(&root, "file", "base\n", "base");
        git(&root, &["checkout", "-qb", "side"]);
        commit_file(&root, "side", "side\n", "side");
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "main", "main\n", "main");
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "merge",
                "--no-ff",
                "side",
                "-m",
                "merge side",
                "-q",
            ],
        );

        for args in [
            vec!["rev-list", "--first-parent", "HEAD"],
            vec!["rev-list", "--parents", "--first-parent", "HEAD"],
            vec!["rev-list", "--count", "--first-parent", "HEAD"],
            vec!["rev-list", "--reverse", "--first-parent", "HEAD"],
            vec!["rev-list", "--topo-order", "HEAD"],
            vec!["rev-list", "--topo-order", "--max-count=2", "HEAD"],
            vec!["rev-list", "--topo-order", "--reverse", "HEAD"],
            vec!["rev-list", "--topo-order", "--count", "HEAD"],
            vec!["rev-list", "--merges", "HEAD"],
            vec!["rev-list", "--no-merges", "HEAD"],
            vec!["rev-list", "--min-parents=2", "--count", "HEAD"],
            vec!["rev-list", "--max-parents=0", "HEAD"],
            vec![
                "rev-list",
                "--min-parents=1",
                "--max-parents=1",
                "--count",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--min-parents=2",
                "--no-min-parents",
                "--count",
                "HEAD",
            ],
            vec![
                "rev-list",
                "--max-parents=0",
                "--no-max-parents",
                "--count",
                "HEAD",
            ],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn assert_rev_list_date_order_merge_case(name: &str, side_timestamp: i64, main_timestamp: i64) {
    let root = unique_temp_dir(name);
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let main = current_branch(&root);
        commit_file_at(&root, "file", "base\n", "base", 1000);
        git(&root, &["checkout", "-qb", "side"]);
        commit_file_at(&root, "side", "side\n", "side", side_timestamp);
        git(&root, &["checkout", "-q", &main]);
        commit_file_at(&root, "main", "main\n", "main", main_timestamp);
        git_with_env(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "merge",
                "--no-ff",
                "side",
                "-m",
                "merge side",
                "-q",
            ],
            &[
                ("GIT_AUTHOR_DATE", "@3000 +0000"),
                ("GIT_COMMITTER_DATE", "@3000 +0000"),
            ],
        );

        for args in [
            vec!["rev-list", "--date-order", "HEAD"],
            vec!["rev-list", "--date-order", "--max-count=3", "HEAD"],
            vec!["rev-list", "--date-order", "--reverse", "HEAD"],
            vec!["rev-list", "--date-order", "--count", "HEAD"],
            vec!["rev-list", "--topo-order", "--date-order", "HEAD"],
            vec!["rev-list", "--date-order", "--topo-order", "HEAD"],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "rev-list output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_list_date_order_merge_history_matches_upstream_git() {
    assert_rev_list_date_order_merge_case("rev-list-date-order-newer-side", 4000, 2000);
    assert_rev_list_date_order_merge_case("rev-list-date-order-older-side", 1500, 2000);
}
