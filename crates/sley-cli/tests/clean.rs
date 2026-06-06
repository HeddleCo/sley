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

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_sley"), cwd, args)
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
    git(root, &["init", "-q"]);
    fs::create_dir_all(root.join("tracked")).expect("create tracked dir");
    fs::write(root.join("tracked/keep.txt"), b"keep\n").expect("write tracked fixture");
    git(root, &["add", "tracked/keep.txt"]);
    fs::write(root.join("top.txt"), b"top\n").expect("write untracked top");
    fs::write(root.join("tracked/extra.txt"), b"extra\n").expect("write nested extra");
    fs::create_dir_all(root.join("scratch")).expect("create scratch dir");
    fs::write(root.join("scratch/file.txt"), b"scratch\n").expect("write scratch file");
}

fn exclude_fixture(root: &Path) {
    git(root, &["init", "-q"]);
    fs::write(root.join("drop.tmp"), b"drop\n").expect("write drop fixture");
    fs::write(root.join("keep.tmp"), b"keep\n").expect("write keep fixture");
    fs::create_dir_all(root.join("scratch")).expect("create scratch dir");
    fs::write(root.join("scratch/file.tmp"), b"scratch\n").expect("write scratch file");
}

fn ignore_fixture(root: &Path) {
    git(root, &["init", "-q"]);
    fs::write(
        root.join(".gitignore"),
        b"\\#ignored.hash\n\\!ignored.bang\n\\*.literal\nliteral-\\?.tmp\nliteral-\\[ab\\].tmp\ntrailing.log   \nliteral-space\\ \nclass-[ab].tmp\nrange-[0-2].tmp\nnegclass-[!z].tmp\nslash/*.tmp\nslash/**/*.deep\nslash/**/wild.tmp\n*.log\n!important.log\nignored-dir/\n",
    )
    .expect("write gitignore");
    fs::write(
        root.join(".git/info/exclude"),
        b"*.cache\n!important.cache\ninfo-dir/\n",
    )
    .expect("write exclude");
    fs::write(
        root.join("global-excludes"),
        b"*.global\n!important.global\n",
    )
    .expect("write configured excludes");
    git(root, &["config", "core.excludesFile", "global-excludes"]);
    fs::write(root.join("ignored.log"), b"ignored\n").expect("write ignored fixture");
    fs::write(root.join("ignored.cache"), b"ignored\n").expect("write info ignored fixture");
    fs::write(root.join("ignored.global"), b"ignored\n").expect("write global ignored fixture");
    fs::write(root.join("#ignored.hash"), b"ignored\n").expect("write escaped hash fixture");
    fs::write(root.join("!ignored.bang"), b"ignored\n").expect("write escaped bang fixture");
    fs::write(root.join("*.literal"), b"ignored\n").expect("write escaped star fixture");
    fs::write(root.join("wild.literal"), b"visible\n").expect("write escaped star visible fixture");
    fs::write(root.join("literal-?.tmp"), b"ignored\n").expect("write escaped question fixture");
    fs::write(root.join("literal-a.tmp"), b"visible\n")
        .expect("write escaped question visible fixture");
    fs::write(root.join("literal-[ab].tmp"), b"ignored\n").expect("write escaped class fixture");
    fs::write(root.join("trailing.log"), b"ignored\n").expect("write trailing-space fixture");
    fs::write(root.join("literal-space "), b"ignored\n").expect("write literal-space fixture");
    fs::write(root.join("class-a.tmp"), b"ignored\n").expect("write class fixture");
    fs::write(root.join("class-c.tmp"), b"visible\n").expect("write class visible fixture");
    fs::write(root.join("range-1.tmp"), b"ignored\n").expect("write range fixture");
    fs::write(root.join("range-9.tmp"), b"visible\n").expect("write range visible fixture");
    fs::write(root.join("negclass-a.tmp"), b"ignored\n").expect("write negated class fixture");
    fs::write(root.join("negclass-z.tmp"), b"visible\n")
        .expect("write negated class visible fixture");
    fs::create_dir_all(root.join("slash/deep")).expect("create slash wildcard fixture");
    fs::write(root.join("slash/file.tmp"), b"ignored\n").expect("write slash star fixture");
    fs::write(root.join("slash/deep/file.tmp"), b"visible\n")
        .expect("write slash star visible fixture");
    fs::write(root.join("slash/file.deep"), b"ignored\n").expect("write double-star fixture");
    fs::write(root.join("slash/deep/file.deep"), b"ignored\n")
        .expect("write nested double-star fixture");
    fs::write(root.join("slash/wild.tmp"), b"ignored\n").expect("write direct double-star fixture");
    fs::write(root.join("slash/deep/wild.tmp"), b"ignored\n")
        .expect("write nested wildcard fixture");
    fs::write(root.join("important.log"), b"visible\n").expect("write negated fixture");
    fs::write(root.join("important.cache"), b"visible\n").expect("write info negated fixture");
    fs::write(root.join("important.global"), b"visible\n").expect("write global negated fixture");
    fs::write(root.join("visible.tmp"), b"visible\n").expect("write visible fixture");
    fs::create_dir_all(root.join("ignored-dir")).expect("create ignored dir");
    fs::write(root.join("ignored-dir/file.txt"), b"ignored\n").expect("write ignored dir file");
    fs::create_dir_all(root.join("info-dir")).expect("create info ignored dir");
    fs::write(root.join("info-dir/file.txt"), b"ignored\n").expect("write info ignored file");
    fs::create_dir_all(root.join("local")).expect("create local ignore dir");
    fs::write(
        root.join("local/.gitignore"),
        b"*.local\n!important.local\n",
    )
    .expect("write local gitignore");
    fs::write(root.join("local/hidden.local"), b"ignored\n").expect("write local ignored file");
    fs::write(root.join("local/important.local"), b"visible\n").expect("write local negated file");
}

fn files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_files(root, root, &mut out);
    out.sort();
    out
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("read entry");
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out);
        } else {
            out.push(
                path.strip_prefix(root)
                    .expect("strip prefix")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

#[test]
fn clean_long_option_negations_match_upstream_git() {
    let root = unique_temp_dir("clean-long-option-negations");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);

        let args = ["clean", "-n", "--no-dry-run", "-f", "top.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            files(&rust),
            files(&upstream),
            "filesystem differed after clean --no-dry-run"
        );

        let args = ["clean", "-q", "--no-quiet", "-f", "tracked/extra.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            files(&rust),
            files(&upstream),
            "filesystem differed after clean --no-quiet"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_no_force_and_no_interactive_match_upstream_git() {
    let root = unique_temp_dir("clean-no-force");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);

        let args = ["clean", "-f", "--no-force"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            files(&rust),
            files(&upstream),
            "clean --no-force changed files"
        );

        let args = ["clean", "--no-interactive", "-n", "top.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_dry_run_and_force_match_upstream_git() {
    let root = unique_temp_dir("clean");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);

        for args in [
            ["clean", "-n"].as_slice(),
            ["clean", "-nd"].as_slice(),
            ["clean", "-dn"].as_slice(),
        ] {
            assert_eq!(
                git(&upstream, args),
                git_rs(&rust, args),
                "sley clean output differed for {args:?}"
            );
        }
        assert_eq!(files(&rust), files(&upstream), "dry-run changed files");

        assert_eq!(
            git(&upstream, &["clean", "-f"]),
            git_rs(&rust, &["clean", "-f"])
        );
        assert_eq!(
            files(&rust),
            files(&upstream),
            "clean -f filesystem differed"
        );
        assert!(
            rust.join("scratch/file.txt").exists(),
            "clean -f removed an untracked directory without -d"
        );

        assert_eq!(
            git(&upstream, &["clean", "-fd"]),
            git_rs(&rust, &["clean", "-fd"])
        );
        assert_eq!(
            files(&rust),
            files(&upstream),
            "clean -fd filesystem differed"
        );
        assert_eq!(files(&rust), vec!["tracked/keep.txt"]);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_include_ignored_option_matches_upstream_git_without_ignored_files() {
    let root = unique_temp_dir("clean-include-ignored");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);

        for args in [
            ["clean", "-nx"].as_slice(),
            ["clean", "-nxd"].as_slice(),
            ["clean", "-fx"].as_slice(),
            ["clean", "-fxd"].as_slice(),
        ] {
            let expected = run_output("git", &upstream, args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
            assert_same_output(actual, expected, args);
            assert_eq!(
                files(&rust),
                files(&upstream),
                "filesystem differed after {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_exclude_patterns_match_upstream_git() {
    let root = unique_temp_dir("clean-excludes");
    for args in [
        ["clean", "-n", "-e", "keep.tmp"].as_slice(),
        ["clean", "-n", "-e", "*.tmp"].as_slice(),
        ["clean", "-f", "-e", "keep.tmp"].as_slice(),
        ["clean", "-fd", "-e", "scratch"].as_slice(),
        ["clean", "-n", "--exclude=keep.tmp"].as_slice(),
        ["clean", "-n", "--exclude", "keep.tmp"].as_slice(),
    ] {
        let upstream = root.join(format!("upstream-{}", args.join("-")));
        let rust = root.join(format!("rust-{}", args.join("-")));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        exclude_fixture(&upstream);
        exclude_fixture(&rust);

        let expected = run_output("git", &upstream, args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
        assert_same_output(actual, expected, args);
        assert_eq!(
            files(&rust),
            files(&upstream),
            "filesystem differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_respects_root_gitignore_and_x_matches_upstream_git() {
    let root = unique_temp_dir("clean-gitignore");
    for args in [
        ["clean", "-n"].as_slice(),
        ["clean", "-nx"].as_slice(),
        ["clean", "-f"].as_slice(),
        ["clean", "-fx"].as_slice(),
        ["clean", "-nd"].as_slice(),
        ["clean", "-nxd"].as_slice(),
    ] {
        let upstream = root.join(format!("upstream-{}", args.join("-")));
        let rust = root.join(format!("rust-{}", args.join("-")));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        ignore_fixture(&upstream);
        ignore_fixture(&rust);

        let expected = run_output("git", &upstream, args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
        assert_same_output(actual, expected, args);
        assert_eq!(
            files(&rust),
            files(&upstream),
            "filesystem differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_requires_force_or_dry_run_like_upstream_git() {
    let root = unique_temp_dir("clean-requires-force");
    fs::create_dir_all(&root).expect("create repo");
    {
        fixture(&root);
        let expected = run_output("git", &root, &["clean"]);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &["clean"]);
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "sley clean status differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_pathspecs_match_upstream_git() {
    let root = unique_temp_dir("clean-pathspecs");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);

        for args in [
            ["clean", "-n", "tracked"].as_slice(),
            ["clean", "-n", "scratch"].as_slice(),
            ["clean", "-n", "scratch/"].as_slice(),
            ["clean", "-n", "scratch/file.txt"].as_slice(),
            ["clean", "-n", "--", "tracked/extra.txt"].as_slice(),
            ["clean", "-n", "missing"].as_slice(),
        ] {
            assert_eq!(
                git(&upstream, args),
                git_rs(&rust, args),
                "sley clean output differed for {args:?}"
            );
        }

        assert_eq!(
            git(&upstream, &["clean", "-f", "scratch/file.txt"]),
            git_rs(&rust, &["clean", "-f", "scratch/file.txt"])
        );
        assert_eq!(
            files(&rust),
            files(&upstream),
            "clean -f pathspec filesystem differed"
        );
        assert!(
            rust.join("scratch").exists(),
            "file pathspec removed the parent directory"
        );

        assert_eq!(
            git(&upstream, &["clean", "-f", "scratch"]),
            git_rs(&rust, &["clean", "-f", "scratch"])
        );
        assert_eq!(
            files(&rust),
            files(&upstream),
            "clean -f directory pathspec filesystem differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clean_require_force_false_matches_upstream_git() {
    let root = unique_temp_dir("clean-require-force-false");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);
        git(&upstream, &["config", "clean.requireForce", "false"]);
        git_rs(&rust, &["config", "clean.requireForce", "false"]);

        assert_eq!(
            git(&upstream, &["clean"]),
            git_rs(&rust, &["clean"]),
            "sley clean output differed with clean.requireForce=false"
        );
        assert_eq!(
            files(&rust),
            files(&upstream),
            "clean with clean.requireForce=false filesystem differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}
