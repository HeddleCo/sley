use std::fs;
use std::io::Write;
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

fn run_raw(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_raw_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(stdin)
        .expect("write stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run_raw(program, cwd, args);
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
    run(sley_testkit::oracle_git(), cwd, args)
}

fn git_rs_raw(cwd: &Path, args: &[&str]) -> Output {
    run_raw(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git_raw(cwd: &Path, args: &[&str]) -> Output {
    run_raw(sley_testkit::oracle_git(), cwd, args)
}

fn git_rs_raw_with_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    run_raw_with_stdin(env!("CARGO_BIN_EXE_sley"), cwd, args, stdin)
}

fn git_raw_with_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    run_raw_with_stdin(sley_testkit::oracle_git(), cwd, args, stdin)
}

#[test]
fn for_each_ref_minimal_formats_match_upstream_git() {
    let root = unique_temp_dir("for-each-ref-minimal");
    let linked = unique_temp_dir("for-each-ref-linked");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("file"), b"file\n").expect("write fixture");
        git(&root, &["add", "file"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "initial",
                "-q",
            ],
        );
        git(&root, &["branch", "feature/topic"]);
        git(&root, &["branch", "another"]);
        git(&root, &["branch", "Beta"]);
        git(&root, &["branch", "Zoo"]);
        git(&root, &["branch", "alpha"]);
        git(&root, &["tag", "v1.0"]);
        git(&root, &["tag", "v1.9"]);
        git(&root, &["tag", "v1.10"]);
        git(&root, &["tag", "release-2"]);
        git(&root, &["tag", "release-10"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "annotated",
                "-m",
                "annotated subject",
                "-m",
                "annotated body",
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
                "quoted",
                "-m",
                "quote ' and \\ slash",
                "-m",
                "body line1\nbody line2",
            ],
        );
        fs::write(root.join("file"), b"file\nsecond\n").expect("update fixture");
        git(&root, &["add", "file"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "second",
                "-q",
            ],
        );
        git(&root, &["checkout", "-q", "-b", "side", "v1.0"]);
        fs::write(root.join("side"), b"side\n").expect("write side fixture");
        git(&root, &["add", "side"]);
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
        git(&root, &["checkout", "-q", "main"]);
        git(
            &root,
            &["remote", "add", "origin", "https://example.invalid/repo"],
        );
        git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&root, &["branch", "--set-upstream-to=origin/main", "main"]);
        git(&root, &["branch", "local-upstream", "main"]);
        git(&root, &["config", "branch.local-upstream.remote", "."]);
        git(
            &root,
            &["config", "branch.local-upstream.merge", "refs/heads/main"],
        );
        git(&root, &["checkout", "-q", "-b", "remote-diverged", "main"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "remote diverged",
                "-q",
            ],
        );
        git(
            &root,
            &["update-ref", "refs/remotes/origin/diverged", "HEAD"],
        );
        git(&root, &["checkout", "-q", "-b", "diverged", "main"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "local diverged",
                "-q",
            ],
        );
        git(
            &root,
            &["branch", "--set-upstream-to=origin/diverged", "diverged"],
        );
        git(&root, &["checkout", "-q", "main"]);
        git(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().expect("linked worktree path is utf-8"),
                "side",
            ],
        );

        for args in [
            vec!["for-each-ref"],
            vec!["for-each-ref", "refs/heads"],
            vec!["for-each-ref", "--count=2"],
            vec!["for-each-ref", "--count", "2"],
            vec!["for-each-ref", "--count=1", "refs/heads"],
            vec![
                "for-each-ref",
                "--count=1",
                "--no-count",
                "--format=%(refname)",
            ],
            vec!["for-each-ref", "--count=0"],
            vec!["for-each-ref", "--format", "%(refname)"],
            vec!["for-each-ref", "--omit-empty", "--format="],
            vec![
                "for-each-ref",
                "--omit-empty",
                "--format=",
                "--no-omit-empty",
            ],
            vec![
                "for-each-ref",
                "--no-omit-empty",
                "--omit-empty",
                "--format=",
            ],
            vec!["for-each-ref", "--include-root-refs", "--format=%(refname)"],
            vec![
                "for-each-ref",
                "--include-root-refs",
                "--no-include-root-refs",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--include-root-refs",
                "--format=%(HEAD) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--include-root-refs",
                "--format=%(refname)|%(symref)|%(symref:short)",
            ],
            vec![
                "for-each-ref",
                "--include-root-refs",
                "--points-at=HEAD",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--include-root-refs",
                "refs/heads",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--start-after=refs/heads/another",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--start-after",
                "refs/heads/another",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--start-after=refs/heads/feature/topic",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--start-after=refs/heads/another",
                "--count=1",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--start-after=refs/heads/another",
                "--no-start-after",
                "--format=%(refname)",
            ],
            vec!["for-each-ref", "--sort=refname", "--format=%(refname)"],
            vec!["for-each-ref", "--sort=-refname", "--format=%(refname)"],
            vec![
                "for-each-ref",
                "--sort=-refname",
                "--no-sort",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=objectname",
                "--format=%(objectname) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-objectname",
                "--format=%(objectname) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=objecttype",
                "--format=%(objecttype) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-objecttype",
                "--format=%(objecttype) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=objectsize",
                "--format=%(objectsize) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-objectsize",
                "--format=%(objectsize) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=objectsize:disk",
                "--format=%(objectsize:disk) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-objectsize:disk",
                "--format=%(objectsize:disk) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=upstream",
                "--format=%(upstream) %(refname)",
                "refs/heads",
            ],
            vec![
                "for-each-ref",
                "--sort=-upstream",
                "--format=%(upstream) %(refname)",
                "refs/heads",
            ],
            vec![
                "for-each-ref",
                "--sort=push",
                "--format=%(push) %(refname)",
                "refs/heads",
            ],
            vec![
                "for-each-ref",
                "--sort=-push",
                "--format=%(push) %(refname)",
                "refs/heads",
            ],
            vec![
                "for-each-ref",
                "--include-root-refs",
                "--sort=symref",
                "--format=%(symref) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--include-root-refs",
                "--sort=-symref",
                "--format=%(symref) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=worktreepath",
                "--format=%(worktreepath) %(refname)",
                "refs/heads",
            ],
            vec![
                "for-each-ref",
                "--sort=-worktreepath",
                "--format=%(worktreepath) %(refname)",
                "refs/heads",
            ],
            vec![
                "for-each-ref",
                "--sort=tag",
                "--format=%(tag) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-tag",
                "--format=%(tag) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=type",
                "--format=%(type) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=object",
                "--format=%(object) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=subject",
                "--format=%(subject) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-contents:subject",
                "--format=%(contents:subject) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=contents:body",
                "--format=%(contents:body) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-body",
                "--format=%(body) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=contents:size",
                "--format=%(contents:size) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=*objectname",
                "--format=%(*objectname) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-*objecttype",
                "--format=%(*objecttype) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*objectsize",
                "--format=%(*objectsize) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*objectsize:disk",
                "--format=%(*objectsize:disk) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*deltabase",
                "--format=%(*deltabase) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*raw:size",
                "--format=%(*raw:size) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*subject",
                "--format=%(*subject) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-*contents:body",
                "--format=%(*contents:body) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*contents:size",
                "--format=%(*contents:size) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*tree",
                "--format=%(*tree) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-*parent",
                "--format=%(*parent) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*numparent",
                "--format=%(*numparent) %(refname)",
                "refs/tags",
            ],
            vec!["for-each-ref", "--sort=tree", "--format=%(tree) %(refname)"],
            vec![
                "for-each-ref",
                "--sort=-parent",
                "--format=%(parent) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=numparent",
                "--format=%(numparent) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=authordate",
                "--format=%(authordate:unix) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(committerdate:unix) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=taggerdate",
                "--format=%(taggerdate:unix) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-creatordate",
                "--format=%(creatordate:unix) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=*authordate",
                "--format=%(*authordate:unix) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-*committerdate",
                "--format=%(*committerdate:unix) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*taggerdate",
                "--format=%(*taggerdate:unix) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*creatordate",
                "--format=%(*creatordate:unix) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=authorname",
                "--format=%(authorname) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=-committeremail",
                "--format=%(committeremail) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=taggername",
                "--format=%(taggername) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=creator",
                "--format=%(creator) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--sort=*authorname",
                "--format=%(*authorname) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-*committeremail",
                "--format=%(*committeremail) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*taggeremail",
                "--format=%(*taggeremail) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=*creator",
                "--format=%(*creator) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=version:refname",
                "--format=%(refname:short)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-version:refname",
                "--format=%(refname:short)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=v:refname",
                "--format=%(refname:short)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=version:refname",
                "--sort=-refname",
                "--format=%(refname:short)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-refname",
                "--sort=version:refname",
                "--format=%(refname:short)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=objectsize",
                "--sort=-refname",
                "--format=%(objectsize) %(refname:short)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--sort=-refname",
                "--sort=objectsize",
                "--format=%(objectsize) %(refname:short)",
                "refs/tags",
            ],
            vec!["for-each-ref", "--ignore-case", "--format=%(refname)"],
            vec![
                "for-each-ref",
                "--ignore-case",
                "--sort=-refname",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--ignore-case",
                "refs/heads/zoo",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--ignore-case",
                "refs/heads/z*",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--ignore-case",
                "--exclude=refs/heads/zoo",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--ignore-case",
                "--exclude=refs/heads/z*",
                "--format=%(refname)",
            ],
            vec!["for-each-ref", "--sort", "refname", "--format=%(refname)"],
            vec!["for-each-ref", "--sort", "-refname", "--format=%(refname)"],
            vec![
                "for-each-ref",
                "--sort=-refname",
                "--count=2",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--exclude=refs/heads/another",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--exclude=refs/heads/another",
                "--no-exclude",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--exclude",
                "refs/heads/another",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--exclude=refs/heads/feature/*",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--exclude=refs/heads/*",
                "--format=%(refname)",
            ],
            vec![
                "for-each-ref",
                "--exclude=refs/heads/another",
                "--format=%(refname)",
                "refs/heads",
            ],
            vec!["for-each-ref", "--points-at=HEAD", "--format=%(refname)"],
            vec!["for-each-ref", "--points-at", "HEAD", "--format=%(refname)"],
            vec!["for-each-ref", "--points-at=v1.0", "--format=%(refname)"],
            vec![
                "for-each-ref",
                "--points-at=HEAD",
                "--points-at=v1.0",
                "--format=%(refname)",
            ],
            vec!["for-each-ref", "--merged=HEAD", "--format=%(refname)"],
            vec!["for-each-ref", "--format=%(refname)", "--merged", "HEAD"],
            vec!["for-each-ref", "--no-merged=HEAD", "--format=%(refname)"],
            vec!["for-each-ref", "--format=%(refname)", "--no-merged", "HEAD"],
            vec!["for-each-ref", "--format=%(refname)", "--contains", "v1.0"],
            vec!["for-each-ref", "--contains=v1.0", "--format=%(refname)"],
            vec!["for-each-ref", "--format=%(refname)", "--contains", "HEAD"],
            vec![
                "for-each-ref",
                "--format=%(refname)",
                "--no-contains",
                "HEAD",
            ],
            vec!["for-each-ref", "--no-contains=HEAD", "--format=%(refname)"],
            vec![
                "for-each-ref",
                "--format=%(refname)",
                "--contains",
                "v1.0",
                "--no-contains",
                "HEAD",
            ],
            vec!["for-each-ref", "--format=%(HEAD) %(refname:short)"],
            vec!["for-each-ref", "--format=%(refname)"],
            vec!["for-each-ref", "--format=%(refname:short)"],
            vec![
                "for-each-ref",
                "--format=A%00B%09C%0aD%0AE%2fF %(refname)",
                "refs/heads/main",
            ],
            vec![
                "for-each-ref",
                "--format=X%(color:red)Y%(color:reset)Z %(refname)",
            ],
            vec![
                "for-each-ref",
                "--shell",
                "--format=X%(refname)Y %(objecttype)",
                "refs/heads/main",
            ],
            vec![
                "for-each-ref",
                "-s",
                "--format=X%(refname)Y",
                "refs/heads/main",
            ],
            vec![
                "for-each-ref",
                "--shell",
                "--color",
                "--format=X%(color:red)Y%(color:reset)Z",
                "refs/heads/main",
            ],
            vec![
                "for-each-ref",
                "--python",
                "--format=%(subject)|%(contents:body)",
                "refs/tags/quoted",
            ],
            vec![
                "for-each-ref",
                "--perl",
                "--format=%(subject)|%(contents:body)",
                "refs/tags/quoted",
            ],
            vec![
                "for-each-ref",
                "-p",
                "--format=%(subject)|%(contents:body)",
                "refs/tags/quoted",
            ],
            vec![
                "for-each-ref",
                "--tcl",
                "--format=%(subject)|%(contents:body)",
                "refs/tags/quoted",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:green)Y%(color:bold red)Z%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:normal)Y%(color:reset)Z %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:dim)D%(color:ul)U%(color:blink)B%(color:reverse)R%(color:italic)I%(color:strike)S%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:brightred)R%(color:brightblue)B%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:ul red)Y%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:red bold)Y%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:red black)Y%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:brightred brightblack)Y%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color",
                "--format=X%(color:nobold)Y%(color:noitalic)Z%(color:reset) %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color=always",
                "--format=X%(color:red)Y%(color:reset)Z %(refname)",
            ],
            vec![
                "for-each-ref",
                "--color=never",
                "--format=X%(color:red)Y%(color:reset)Z %(refname)",
            ],
            vec![
                "for-each-ref",
                "--no-color",
                "--format=X%(color:red)Y%(color:reset)Z %(refname)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(refname:lstrip=1)|%(refname:lstrip=2)|%(refname:lstrip=-1)|%(refname:rstrip=1)|%(refname:rstrip=2)|%(refname:rstrip=-1)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(refname:strip=1)|%(refname:strip=2)|%(refname:strip=-1)|%(refname:strip=0)",
            ],
            vec!["for-each-ref", "--format=%(objectname:short)"],
            vec!["for-each-ref", "--format=%(objectname:short=12)"],
            vec!["for-each-ref", "--format=%(objectname:short=3)"],
            vec!["for-each-ref", "--format=%(deltabase) %(refname)"],
            vec![
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*objectname)|%(*objectname:short)|%(*objectname:short=12)",
                "refs/tags",
            ],
            vec!["for-each-ref", "--format=%(objectsize) %(refname:short)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*objecttype)|%(*objectsize)|%(*deltabase)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*raw:size)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(objectsize:disk) %(refname:short)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*objectsize:disk)",
                "refs/tags",
            ],
            vec!["for-each-ref", "--format=%(objecttype) %(refname)"],
            vec!["for-each-ref", "--format=%(raw:size) %(refname)"],
            vec![
                "for-each-ref",
                "--format=BEGIN %(refname)%n%(raw)%nEND",
                "refs/tags/annotated",
            ],
            vec![
                "for-each-ref",
                "--format=BEGIN %(refname)%n%(*raw)%nEND",
                "refs/tags/annotated",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(worktreepath)"],
            vec!["for-each-ref", "--format=%(refname)|%(symref)"],
            vec!["for-each-ref", "--format=%(refname)|%(symref:short)"],
            vec!["for-each-ref", "--format=%(refname)|%(upstream)"],
            vec!["for-each-ref", "--format=%(refname)|%(upstream:short)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(upstream:lstrip=1)|%(upstream:lstrip=2)|%(upstream:lstrip=-1)|%(upstream:rstrip=1)|%(upstream:rstrip=2)|%(upstream:rstrip=-1)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(upstream:strip=1)|%(upstream:strip=2)|%(upstream:strip=-1)|%(upstream:strip=0)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(upstream:remotename)|%(upstream:remoteref)",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(upstream:track)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(upstream:track,nobracket)",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(upstream:trackshort)"],
            vec!["for-each-ref", "--format=%(refname)|%(push)"],
            vec!["for-each-ref", "--format=%(refname)|%(push:short)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(push:lstrip=1)|%(push:lstrip=2)|%(push:lstrip=-1)|%(push:rstrip=1)|%(push:rstrip=2)|%(push:rstrip=-1)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(push:strip=1)|%(push:strip=2)|%(push:strip=-1)|%(push:strip=0)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(push:remotename)|%(push:remoteref)",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(push:track)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(push:track,nobracket)",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(push:trackshort)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(ahead-behind:HEAD)|%(ahead-behind:side)",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(subject)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*subject)|%(*contents:subject)|%(*contents:body)|%(*body)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*contents)|%(*contents:size)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*contents:lines=0)|%(*contents:lines=1)|%(*contents:lines=3)|END",
                "refs/tags",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(contents:subject)"],
            vec!["for-each-ref", "--format=%(refname)|%(contents:body)"],
            vec!["for-each-ref", "--format=%(refname)|%(body)"],
            vec!["for-each-ref", "--format=%(refname)|%(contents)"],
            vec!["for-each-ref", "--format=%(refname)|%(contents:size)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(contents:lines=0)|END",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(contents:lines=1)|END",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(contents:lines=3)|END",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(author)|%(committer)|%(tagger)|%(creator)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*author)|%(*committer)|%(*creator)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authorname)|%(committername)|%(taggername)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*authorname)|%(*committername)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authoremail)|%(committeremail)|%(taggeremail)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*authoremail)|%(*committeremail)|%(*authoremail:trim)|%(*committeremail:localpart)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authoremail:trim)|%(committeremail:trim)|%(taggeremail:trim)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authoremail:localpart)|%(committeremail:localpart)|%(taggeremail:localpart)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authordate:raw)|%(committerdate:raw)|%(taggerdate:raw)|%(creatordate:raw)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*authordate:raw)|%(*committerdate:raw)|%(*creatordate:raw)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authordate)|%(committerdate)|%(taggerdate)|%(creatordate)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*authordate)|%(*committerdate)|%(*creatordate)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authordate:unix)|%(committerdate:unix)|%(taggerdate:unix)|%(creatordate:unix)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*authordate:unix)|%(*committerdate:short)|%(*creatordate:rfc2822)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authordate:short)|%(committerdate:short)|%(taggerdate:short)|%(creatordate:short)",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(authordate:iso)|%(committerdate:iso8601)|%(taggerdate:iso8601-strict)|%(creatordate:rfc2822)",
            ],
            vec!["for-each-ref", "--format=%(refname)|%(tree)|%(parent)"],
            vec!["for-each-ref", "--format=%(refname)|%(parent)|%(numparent)"],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(*tree)|%(*parent)|%(*numparent)",
                "refs/tags",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(tag)|%(type)|%(object)",
            ],
            vec!["for-each-ref", "--format=%% %(refname)%n%(objecttype)"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        for (args, stdin) in [
            (
                vec!["for-each-ref", "--stdin", "--format=%(refname)"],
                b"refs/heads\n".as_slice(),
            ),
            (
                vec!["for-each-ref", "--stdin", "--format=%(refname)"],
                b"refs/heads/main\nrefs/tags\n".as_slice(),
            ),
            (
                vec!["for-each-ref", "--stdin", "--format=%(refname)"],
                b"\nrefs/heads/main\n\n".as_slice(),
            ),
            (
                vec![
                    "for-each-ref",
                    "--stdin",
                    "--no-stdin",
                    "--format=%(refname)",
                ],
                b"refs/heads/main\n".as_slice(),
            ),
        ] {
            let expected = git_raw_with_stdin(&root, &args, stdin);
            let actual = git_rs_raw_with_stdin(&root, &args, stdin);
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

        git(&root, &["config", "core.abbrev", "12"]);
        let args = ["for-each-ref", "--format=%(objectname:short)"];
        let expected = git(&root, &args);
        let actual = git_rs(&root, &args);
        assert_eq!(
            actual, expected,
            "sley output differed for core.abbrev-driven objectname:short"
        );

        for args in [
            ["for-each-ref", "--format=%(objectname:short=0)"],
            ["for-each-ref", "--format=%(objectname:short=-1)"],
            ["for-each-ref", "--format=%(objectname:short=abc)"],
            ["for-each-ref", "--format=%(*objectname:short=0)"],
            ["for-each-ref", "--format=%(*objectname:short=-1)"],
            ["for-each-ref", "--format=%(*objectname:short=abc)"],
        ] {
            let expected = git_raw(&root, &args);
            let actual = git_rs_raw(&root, &args);
            assert!(
                !expected.status.success(),
                "upstream git unexpectedly accepted {args:?}"
            );
            assert!(
                !actual.status.success(),
                "sley unexpectedly accepted {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn for_each_ref_direct_remote_refspecs_match_upstream_git() {
    let root = unique_temp_dir("for-each-ref-direct-refspecs");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("file"), b"file\n").expect("write fixture");
        git(&root, &["add", "file"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "initial",
                "-q",
            ],
        );
        git(
            &root,
            &["remote", "add", "origin", "https://example.invalid/repo"],
        );
        git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(
            &root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
        );
        git(&root, &["config", "remote.origin.push", "refs/heads/main"]);
        git(&root, &["branch", "--set-upstream-to=origin/main", "main"]);

        for args in [
            vec![
                "for-each-ref",
                "--format=%(refname)|%(upstream)|%(upstream:short)",
                "refs/heads/main",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(push)|%(push:short)",
                "refs/heads/main",
            ],
            vec![
                "for-each-ref",
                "--format=%(refname)|%(push:remotename)|%(push:remoteref)",
                "refs/heads/main",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}
