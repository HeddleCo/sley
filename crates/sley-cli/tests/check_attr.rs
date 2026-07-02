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

fn run(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args, None);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_output(program: &str, cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    run_output_with_env(program, cwd, args, stdin, &[], &[])
}

fn run_output_with_env(
    program: &str,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    envs: &[(&str, &Path)],
    env_remove: &[&str],
) -> Output {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    for key in env_remove {
        command.env_remove(key);
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    if let Some(stdin) = stdin {
        sley_testkit::write_stdin_tolerating_early_exit(
            child.stdin.as_mut().expect("stdin pipe"),
            stdin,
        );
    }
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn sley(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    run_output(sley_testkit::sley_bin!(), cwd, args, stdin)
}

fn git(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    run_output(sley_testkit::oracle_git(), cwd, args, stdin)
}

fn sley_with_env(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    envs: &[(&str, &Path)],
    env_remove: &[&str],
) -> Output {
    run_output_with_env(
        sley_testkit::sley_bin!(),
        cwd,
        args,
        stdin,
        envs,
        env_remove,
    )
}

fn git_with_env(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    envs: &[(&str, &Path)],
    env_remove: &[&str],
) -> Output {
    run_output_with_env(
        sley_testkit::oracle_git(),
        cwd,
        args,
        stdin,
        envs,
        env_remove,
    )
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

fn fixture(root: &Path) {
    run(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    run(
        sley_testkit::oracle_git(),
        root,
        &["config", "core.attributesFile", "global-attributes"],
    );
    fs::write(
        root.join("global-attributes"),
        b"global.data globalattr shared=global\n",
    )
    .expect("write global attributes");
    fs::write(
        root.join(".gitattributes"),
        b"*.txt text eol=lf diff\n*.bin -text binary\n/doc/** linguist-docs\ninfo.data text treeattr\nglobal.data shared=root rootattr\n[attr]vendored linguist-vendored -diff macrovalue=value\nmacro.js vendored\nmacro.css -vendored\n",
    )
    .expect("write attributes");
    fs::write(
        root.join(".git/info/attributes"),
        b"info.data -text infovalue=info infoattr\n",
    )
    .expect("write info attributes");
    fs::create_dir_all(root.join("doc")).expect("create doc");
    fs::create_dir_all(root.join("sub/nested")).expect("create sub");
    fs::write(
        root.join("sub/.gitattributes"),
        b"*.md subattr custom=value\n",
    )
    .expect("write nested attributes");
    fs::write(root.join("a.txt"), b"text\n").expect("write txt");
    fs::write(root.join("a.bin"), b"bin\n").expect("write bin");
    fs::write(root.join("doc/a.md"), b"doc\n").expect("write doc");
    fs::write(root.join("none.md"), b"none\n").expect("write none");
    fs::write(root.join("info.data"), b"info\n").expect("write info data");
    fs::write(root.join("global.data"), b"global\n").expect("write global data");
    fs::write(root.join("macro.js"), b"macro\n").expect("write macro js");
    fs::write(root.join("macro.css"), b"macro\n").expect("write macro css");
    fs::write(root.join("sub/readme.md"), b"sub\n").expect("write sub");
    fs::write(root.join("sub/nested/readme.md"), b"nested\n").expect("write nested");
}

#[test]
fn check_attr_matches_upstream_git() {
    let root = unique_temp_dir("check-attr");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);

        for (args, stdin) in [
            (
                vec![
                    "check-attr",
                    "text",
                    "diff",
                    "merge",
                    "binary",
                    "eol",
                    "linguist-docs",
                    "subattr",
                    "custom",
                    "infovalue",
                    "infoattr",
                    "treeattr",
                    "globalattr",
                    "shared",
                    "rootattr",
                    "vendored",
                    "linguist-vendored",
                    "macrovalue",
                    "--",
                    "a.txt",
                    "a.bin",
                    "doc/a.md",
                    "none.md",
                    "info.data",
                    "global.data",
                    "macro.js",
                    "macro.css",
                    "sub/readme.md",
                    "sub/nested/readme.md",
                ],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--all",
                    "--",
                    "a.txt",
                    "a.bin",
                    "doc/a.md",
                    "none.md",
                    "info.data",
                    "global.data",
                    "macro.js",
                    "macro.css",
                    "sub/readme.md",
                ],
                None,
            ),
            (
                vec!["check-attr", "-a", "--", "a.txt", "sub/readme.md"],
                None,
            ),
            (
                vec!["check-attr", "--stdin", "text", "binary"],
                Some(&b"a.txt\na.bin\nnone.md\n"[..]),
            ),
            (
                vec![
                    "check-attr",
                    "-z",
                    "text",
                    "eol",
                    "binary",
                    "--",
                    "a.txt",
                    "a.bin",
                ],
                None,
            ),
            (
                vec!["check-attr", "-az", "--stdin"],
                Some(
                    &b"a.txt\0a.bin\0doc/a.md\0info.data\0global.data\0macro.js\0macro.css\0sub/readme.md\0"[..],
                ),
            ),
        ] {
            let expected = git(&upstream, &args, stdin);
            let actual = sley(&rust, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_attr_cached_toggle_matches_upstream_git() {
    let root = unique_temp_dir("check-attr-cached");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);
        run(sley_testkit::oracle_git(), &upstream, &["add", "."]);
        run(sley_testkit::oracle_git(), &rust, &["add", "."]);
        for repo in [&upstream, &rust] {
            fs::write(
                repo.join(".gitattributes"),
                b"*.txt -text\nmacro.js -vendored\n",
            )
            .expect("dirty worktree attributes");
        }

        for (args, stdin) in [
            (
                vec![
                    "check-attr",
                    "--cached",
                    "text",
                    "binary",
                    "--",
                    "a.txt",
                    "a.bin",
                ],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--cached",
                    "--no-cached",
                    "text",
                    "--",
                    "a.txt",
                ],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--no-cached",
                    "--cached",
                    "--all",
                    "--",
                    "info.data",
                    "global.data",
                ],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--cached",
                    "--no-cached",
                    "text",
                    "vendored",
                    "--",
                    "a.txt",
                    "macro.js",
                ],
                None,
            ),
            (
                vec!["check-attr", "--cached", "--all", "--", "macro.js"],
                None,
            ),
            (
                vec!["check-attr", "--cached", "--stdin", "text", "binary"],
                Some(&b"a.txt\na.bin\nnone.md\n"[..]),
            ),
            (
                vec!["check-attr", "--cached", "-az", "--stdin"],
                Some(&b"a.txt\0a.bin\0macro.js\0"[..]),
            ),
            (
                vec!["check-attr", "--no-source", "text", "--", "a.txt"],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--no-source",
                    "--all",
                    "--",
                    "a.txt",
                    "macro.js",
                ],
                None,
            ),
            (
                vec!["check-attr", "--no-source", "--stdin", "text", "binary"],
                Some(&b"a.txt\na.bin\nnone.md\n"[..]),
            ),
        ] {
            let expected = git(&upstream, &args, stdin);
            let actual = sley(&rust, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_attr_source_option_matches_upstream_when_tree_matches_worktree() {
    let root = unique_temp_dir("check-attr-source");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);
        for repo in [&upstream, &rust] {
            run(sley_testkit::oracle_git(), repo, &["add", "."]);
            run(
                sley_testkit::oracle_git(),
                repo,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "-q",
                    "-m",
                    "attributes",
                ],
            );
            fs::write(
                repo.join(".gitattributes"),
                b"*.txt -text\nmacro.js -vendored\n",
            )
            .expect("change index attributes");
            run(sley_testkit::oracle_git(), repo, &["add", ".gitattributes"]);
            fs::write(repo.join(".gitattributes"), b"*.txt custom\n")
                .expect("change worktree attributes");
        }

        for (args, stdin) in [
            (
                vec!["check-attr", "--source=HEAD", "text", "--", "a.txt"],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--source",
                    "HEAD",
                    "--all",
                    "--",
                    "a.txt",
                    "macro.js",
                ],
                None,
            ),
            (
                vec!["check-attr", "--source=HEAD", "--stdin", "text", "binary"],
                Some(&b"a.txt\na.bin\nnone.md\n"[..]),
            ),
            (
                vec![
                    "check-attr",
                    "--source=HEAD",
                    "--cached",
                    "text",
                    "custom",
                    "--",
                    "a.txt",
                ],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--cached",
                    "--source=HEAD",
                    "--no-cached",
                    "text",
                    "custom",
                    "--",
                    "a.txt",
                ],
                None,
            ),
            (
                vec![
                    "check-attr",
                    "--source=HEAD",
                    "--no-source",
                    "text",
                    "--",
                    "a.txt",
                ],
                None,
            ),
        ] {
            let expected = git(&upstream, &args, stdin);
            let actual = sley(&rust, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_attr_cached_honors_git_index_file_like_upstream_git() {
    let root = unique_temp_dir("check-attr-index-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    let upstream_index = root.join("upstream-index");
    let rust_index = root.join("rust-index");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);
        for (repo, index) in [(&upstream, &upstream_index), (&rust, &rust_index)] {
            let index_env = [("GIT_INDEX_FILE", index.as_path())];
            let output = run_output_with_env(
                sley_testkit::oracle_git(),
                repo,
                &["add", ".gitattributes", "a.txt", "macro.js"],
                None,
                &index_env,
                &[],
            );
            assert!(
                output.status.success(),
                "git add with alternate index failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            fs::write(
                repo.join(".gitattributes"),
                b"*.txt -text\nmacro.js custom\n",
            )
            .expect("dirty worktree attributes");
        }

        let args = [
            "check-attr",
            "--cached",
            "text",
            "vendored",
            "custom",
            "--",
            "a.txt",
            "macro.js",
        ];
        let expected = git_with_env(
            &upstream,
            &args,
            None,
            &[("GIT_INDEX_FILE", &upstream_index)],
            &[],
        );
        let actual = sley_with_env(&rust, &args, None, &[("GIT_INDEX_FILE", &rust_index)], &[]);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_attr_default_global_attributes_match_upstream_git() {
    let root = unique_temp_dir("check-attr-default-global");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    let home = root.join("home");
    let xdg = root.join("xdg");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    fs::create_dir_all(home.join(".config/git")).expect("create home attributes dir");
    fs::create_dir_all(xdg.join("git")).expect("create xdg attributes dir");
    {
        run(
            sley_testkit::oracle_git(),
            &upstream,
            &["init", "-q", "-b", "main"],
        );
        run(
            sley_testkit::oracle_git(),
            &rust,
            &["init", "-q", "-b", "main"],
        );
        fs::write(
            home.join(".config/git/attributes"),
            b"*.data homeattr shared=home\n",
        )
        .expect("write home attributes");
        fs::write(xdg.join("git/attributes"), b"*.data xdgattr shared=xdg\n")
            .expect("write xdg attributes");
        fs::write(upstream.join("a.data"), b"data\n").expect("write upstream data");
        fs::write(rust.join("a.data"), b"data\n").expect("write rust data");

        let args = [
            "check-attr",
            "homeattr",
            "xdgattr",
            "shared",
            "--",
            "a.data",
        ];
        let expected = git_with_env(
            &upstream,
            &args,
            None,
            &[("HOME", &home)],
            &["XDG_CONFIG_HOME"],
        );
        let actual = sley_with_env(&rust, &args, None, &[("HOME", &home)], &["XDG_CONFIG_HOME"]);
        assert_same_output(actual, expected, &args);

        let expected = git_with_env(
            &upstream,
            &args,
            None,
            &[("HOME", &home), ("XDG_CONFIG_HOME", &xdg)],
            &[],
        );
        let actual = sley_with_env(
            &rust,
            &args,
            None,
            &[("HOME", &home), ("XDG_CONFIG_HOME", &xdg)],
            &[],
        );
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}
