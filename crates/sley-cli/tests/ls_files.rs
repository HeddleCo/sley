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

fn run_with_global_env(
    program: &str,
    cwd: &Path,
    args: &[&str],
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> Vec<u8> {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .args(args)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(xdg_config_home) = xdg_config_home {
        command.env("XDG_CONFIG_HOME", xdg_config_home);
    } else {
        command.env_remove("XDG_CONFIG_HOME");
    }
    let output = command
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

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
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

fn run_status(program: &str, cwd: &Path, args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    (
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    )
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

fn prepare_unmerged_index(root: &Path) {
    git(root, &["init", "-q"]);
    let one = String::from_utf8(run_with_stdin(
        "git",
        root,
        &["hash-object", "-w", "--stdin"],
        b"one",
    ))
    .expect("first blob oid utf8")
    .trim()
    .to_string();
    let two = String::from_utf8(run_with_stdin(
        "git",
        root,
        &["hash-object", "-w", "--stdin"],
        b"two",
    ))
    .expect("second blob oid utf8")
    .trim()
    .to_string();
    let input = format!(
        "0 0000000000000000000000000000000000000000\tconflict\n100644 {one} 1\tconflict\n100644 {two} 2\tconflict\n100644 {one}\tnormal\n"
    );
    run_with_stdin(
        "git",
        root,
        &["update-index", "--index-info"],
        input.as_bytes(),
    );
}

fn prepare_ignored_fixture(root: &Path) {
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
    fs::write(root.join("tracked.log"), b"tracked\n").expect("write tracked ignored fixture");
    fs::write(root.join("tracked.global"), b"tracked\n").expect("write tracked global fixture");
    fs::write(root.join("visible.tmp"), b"visible\n").expect("write visible fixture");
    fs::write(root.join("excludes.txt"), b"*.log\n").expect("write exclude file");
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
    fs::write(root.join("local/tracked.local"), b"tracked\n").expect("write local tracked file");
    git(
        root,
        &[
            "add",
            "-f",
            "tracked.log",
            "tracked.global",
            "local/tracked.local",
        ],
    );
}

fn prepare_default_global_excludes_fixture(root: &Path) {
    git(root, &["init", "-q"]);
    fs::write(root.join("ignored.global"), b"ignored\n").expect("write xdg ignored fixture");
    fs::write(root.join("important.global"), b"visible\n").expect("write xdg negated fixture");
    fs::write(root.join("home-only.home"), b"visible\n").expect("write home-only fixture");
    fs::write(root.join("visible.tmp"), b"visible\n").expect("write visible fixture");
    fs::create_dir_all(root.join("global-dir")).expect("create global ignored dir");
    fs::write(root.join("global-dir/file.txt"), b"ignored\n").expect("write global ignored file");
}

fn prepare_per_directory_exclude_fixture(root: &Path) {
    git(root, &["init", "-q"]);
    fs::write(root.join(".ignore"), b"*.root\n").expect("write root exclude");
    fs::write(root.join("hidden.root"), b"ignored\n").expect("write root ignored fixture");
    fs::write(root.join("tracked.root"), b"tracked\n").expect("write root tracked fixture");
    fs::write(root.join("keep.tmp"), b"visible\n").expect("write visible fixture");
    fs::create_dir_all(root.join("nested")).expect("create nested dir");
    fs::write(root.join("nested/.ignore"), b"*.tmp\n").expect("write nested exclude");
    fs::write(root.join("nested/hidden.tmp"), b"ignored\n").expect("write nested ignored fixture");
    fs::write(root.join("nested/tracked.tmp"), b"tracked\n").expect("write nested tracked fixture");
    fs::write(root.join("nested/keep.log"), b"visible\n").expect("write nested visible fixture");
    git(root, &["add", "-f", "tracked.root", "nested/tracked.tmp"]);
}

#[test]
fn ls_files_short_option_aliases_match_upstream_git() {
    let root = unique_temp_dir("ls-files-short-options");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete");
        fs::write(root.join("--dash.txt"), b"dash\n").expect("write dash path");
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify");
        fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked");
        git(
            &root,
            &[
                "add",
                "--",
                "--dash.txt",
                "delete.txt",
                "modify.txt",
                "tracked.txt",
            ],
        );
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
        fs::remove_file(root.join("delete.txt")).expect("delete tracked file");
        fs::write(root.join("modify.txt"), b"after\n").expect("modify tracked file");
        fs::write(root.join("other.txt"), b"other\n").expect("write untracked");

        for args in [
            vec!["ls-files", "-s"],
            vec!["ls-files", "--stage", "--no-stage"],
            vec!["ls-files", "--no-stage"],
            vec!["ls-files", "-s", "-z"],
            vec!["ls-files", "-c"],
            vec!["ls-files", "--cached", "--no-cached"],
            vec!["ls-files", "--no-cached", "--cached"],
            vec!["ls-files", "-c", "-z"],
            vec!["ls-files", "-o"],
            vec!["ls-files", "--others", "--no-others"],
            vec!["ls-files", "--no-others", "--others"],
            vec!["ls-files", "-o", "-z"],
            vec!["ls-files", "-d"],
            vec!["ls-files", "--deleted", "--no-deleted"],
            vec!["ls-files", "--no-deleted", "--deleted"],
            vec!["ls-files", "-d", "-z"],
            vec!["ls-files", "-m"],
            vec!["ls-files", "--modified", "--no-modified"],
            vec!["ls-files", "--no-modified", "--modified"],
            vec!["ls-files", "-m", "-z"],
            vec!["ls-files", "-s", "-d"],
            vec!["ls-files", "-s", "-m"],
            vec!["ls-files", "--deduplicate", "--no-deduplicate"],
            vec!["ls-files", "--no-deduplicate", "--deduplicate"],
            vec!["ls-files", "--recurse-submodules"],
            vec!["ls-files", "--no-recurse-submodules"],
            vec!["ls-files", "--sparse"],
            vec!["ls-files", "--no-sparse"],
            vec!["ls-files", "--no-eol"],
            vec!["ls-files", "--no-ignored"],
            vec!["ls-files", "--no-killed"],
            vec!["ls-files", "--no-resolve-undo"],
            vec!["ls-files", "--no-debug"],
            vec!["ls-files", "-s", "--abbrev"],
            vec!["ls-files", "-s", "--abbrev=12"],
            vec!["ls-files", "-s", "--abbrev=1"],
            vec!["ls-files", "-s", "--abbrev=0"],
            vec!["ls-files", "--no-abbrev"],
            vec!["ls-files", "--", "tracked.txt"],
            vec!["ls-files", "--", "--dash.txt"],
            vec!["ls-files", "-s", "--", "--dash.txt"],
            vec!["ls-files", "-z", "--", "--dash.txt"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_sha256_stage_cached_and_modified_match_upstream_git() {
    let root = unique_temp_dir("ls-files-sha256");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "--object-format=sha256"]);
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify fixture");
        fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked fixture");
        git(&root, &["add", "modify.txt", "tracked.txt"]);
        fs::write(root.join("modify.txt"), b"after\n").expect("modify tracked fixture");

        for args in [
            vec!["ls-files", "--stage"],
            vec!["ls-files", "--stage", "--abbrev"],
            vec!["ls-files", "--stage", "--abbrev=12"],
            vec!["ls-files", "--stage", "--abbrev=1"],
            vec!["ls-files", "--stage", "--abbrev=0"],
            vec!["ls-files", "--stage", "--no-abbrev"],
            vec!["ls-files", "--cached"],
            vec!["ls-files", "--modified"],
            vec!["ls-files", "--stage", "--modified"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_unmerged_matches_upstream_git() {
    let root = unique_temp_dir("ls-files-unmerged");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    {
        prepare_unmerged_index(&expected);
        prepare_unmerged_index(&actual);

        for args in [
            vec!["ls-files", "--unmerged"],
            vec!["ls-files", "--unmerged", "--no-unmerged"],
            vec!["ls-files", "--no-unmerged", "--unmerged"],
            vec!["ls-files", "-u"],
            vec!["ls-files", "-u", "-z"],
            vec!["ls-files", "-u", "conflict"],
            vec!["ls-files", "-u", "normal"],
        ] {
            let expected_output = git(&expected, &args);
            let actual_output = git_rs(&actual, &args);
            assert_eq!(
                actual_output, expected_output,
                "sley output differed for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_directory_matches_upstream_git() {
    let root = unique_temp_dir("ls-files-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    {
        for repo in [&expected, &actual] {
            git(repo, &["init", "-q"]);
            fs::create_dir_all(repo.join("dir").join("sub")).expect("create untracked dir");
            fs::create_dir_all(repo.join("empty")).expect("create empty dir");
            fs::create_dir_all(repo.join("tracked-dir")).expect("create tracked dir");
            fs::write(repo.join("dir").join("a"), b"a").expect("write nested file");
            fs::write(repo.join("dir").join("sub").join("b"), b"b").expect("write nested file");
            fs::write(repo.join("file"), b"file").expect("write file");
            fs::write(repo.join("tracked-dir").join("tracked"), b"tracked")
                .expect("write tracked file");
            fs::write(repo.join("tracked-dir").join("untracked"), b"untracked")
                .expect("write untracked file below tracked dir");
            git(repo, &["add", "tracked-dir/tracked"]);
        }

        for args in [
            vec!["ls-files", "--others", "--directory"],
            vec!["ls-files", "--others", "--directory", "--no-directory"],
            vec!["ls-files", "--others", "--directory", "-z"],
            vec![
                "ls-files",
                "--others",
                "--directory",
                "--no-empty-directory",
            ],
            vec![
                "ls-files",
                "--others",
                "--directory",
                "--no-empty-directory",
                "--empty-directory",
            ],
            vec!["ls-files", "--others", "--directory", "dir"],
            vec!["ls-files", "--others", "--directory", "tracked-dir"],
        ] {
            let expected_output = git(&expected, &args);
            let actual_output = git_rs(&actual, &args);
            assert_eq!(
                actual_output, expected_output,
                "sley output differed for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_ignored_and_exclude_standard_match_upstream_git() {
    let root = unique_temp_dir("ls-files-ignored");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_ignored_fixture(&upstream);
        prepare_ignored_fixture(&rust);

        for args in [
            vec!["ls-files", "--others", "--exclude-standard"],
            vec!["ls-files", "--others", "--ignored", "--exclude-standard"],
            vec!["ls-files", "-o", "-i", "--exclude-standard", "-z"],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--no-ignored",
                "--exclude-standard",
            ],
            vec!["ls-files", "--cached", "--ignored", "--exclude-standard"],
            vec!["ls-files", "--others", "--exclude=*.log"],
            vec!["ls-files", "--others", "--exclude", "*.log"],
            vec!["ls-files", "--others", "--ignored", "--exclude=*.log"],
            vec!["ls-files", "--others", "-i", "-x", "*.log", "-z"],
            vec!["ls-files", "--others", "--exclude-from=excludes.txt"],
            vec!["ls-files", "--others", "--exclude-from", "excludes.txt"],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-from=excludes.txt",
            ],
            vec!["ls-files", "-o", "-i", "-X", "excludes.txt", "-z"],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude",
                "ignored-dir/",
                "--directory",
            ],
            vec!["ls-files", "--cached", "--ignored", "--exclude=*.log"],
        ] {
            let expected = git(&upstream, &args);
            let actual = git_rs(&rust, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        let args = ["ls-files", "--others", "--exclude-from=missing.txt"];
        let expected = run_status("git", &upstream, &args);
        let actual = run_status(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_eq!(actual, expected, "sley status/output differed for {args:?}");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_default_global_excludes_match_upstream_git() {
    let root = unique_temp_dir("ls-files-default-global-excludes");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    let home = root.join("home");
    let xdg = root.join("xdg");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    fs::create_dir_all(home.join(".config/git")).expect("create home git config");
    fs::create_dir_all(xdg.join("git")).expect("create xdg git config");
    {
        fs::write(home.join(".config/git/ignore"), b"*.home\nhome-dir/\n")
            .expect("write home fallback excludes");
        fs::write(
            xdg.join("git/ignore"),
            b"*.global\n!important.global\nglobal-dir/\n",
        )
        .expect("write xdg excludes");
        prepare_default_global_excludes_fixture(&upstream);
        prepare_default_global_excludes_fixture(&rust);

        for args in [
            vec!["ls-files", "--others", "--exclude-standard"],
            vec!["ls-files", "--others", "--ignored", "--exclude-standard"],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ],
            vec!["ls-files", "-o", "-i", "--exclude-standard", "-z"],
        ] {
            let expected_output = run_with_global_env("git", &upstream, &args, &home, Some(&xdg));
            let actual_output =
                run_with_global_env(env!("CARGO_BIN_EXE_sley"), &rust, &args, &home, Some(&xdg));
            assert_eq!(
                actual_output, expected_output,
                "sley XDG global excludes output differed for {args:?}"
            );
        }

        git(&upstream, &["config", "core.excludesFile", "repo-excludes"]);
        git(&rust, &["config", "core.excludesFile", "repo-excludes"]);
        fs::write(upstream.join("repo-excludes"), b"*.tmp\n").expect("write upstream excludes");
        fs::write(rust.join("repo-excludes"), b"*.tmp\n").expect("write rust excludes");
        let args = ["ls-files", "--others", "--exclude-standard"];
        let expected_output = run_with_global_env("git", &upstream, &args, &home, Some(&xdg));
        let actual_output =
            run_with_global_env(env!("CARGO_BIN_EXE_sley"), &rust, &args, &home, Some(&xdg));
        assert_eq!(
            actual_output, expected_output,
            "sley core.excludesFile override differed"
        );

        let fallback_root = root.join("home-fallback");
        let fallback_upstream = fallback_root.join("upstream");
        let fallback_rust = fallback_root.join("rust");
        fs::create_dir_all(&fallback_upstream).expect("create fallback upstream repo");
        fs::create_dir_all(&fallback_rust).expect("create fallback rust repo");
        prepare_default_global_excludes_fixture(&fallback_upstream);
        prepare_default_global_excludes_fixture(&fallback_rust);
        let args = ["ls-files", "--others", "--exclude-standard"];
        let expected_output = run_with_global_env("git", &fallback_upstream, &args, &home, None);
        let actual_output = run_with_global_env(
            env!("CARGO_BIN_EXE_sley"),
            &fallback_rust,
            &args,
            &home,
            None,
        );
        assert_eq!(
            actual_output, expected_output,
            "sley HOME fallback global excludes output differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_exclude_per_directory_matches_upstream_git() {
    let root = unique_temp_dir("ls-files-exclude-per-directory");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_per_directory_exclude_fixture(&upstream);
        prepare_per_directory_exclude_fixture(&rust);

        for args in [
            vec!["ls-files", "--others", "--exclude-per-directory=.ignore"],
            vec!["ls-files", "--others", "--exclude-per-directory", ".ignore"],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-per-directory=.ignore",
            ],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-per-directory=.ignore",
                "--directory",
            ],
            vec![
                "ls-files",
                "--cached",
                "--ignored",
                "--exclude-per-directory=.ignore",
            ],
            vec![
                "ls-files",
                "--cached",
                "--ignored",
                "--exclude-per-directory=.ignore",
                "--no-exclude-per-directory",
                "--exclude=*.root",
            ],
            vec![
                "ls-files",
                "--others",
                "--exclude-per-directory=.ignore",
                "--no-exclude-per-directory",
            ],
        ] {
            let expected = git(&upstream, &args);
            let actual = git_rs(&rust, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_quoted_paths_match_upstream_git() {
    let root = unique_temp_dir("ls-files-quoted-paths");
    fs::create_dir_all(&root).expect("create temp root");
    for (case, path) in [
        ("space", "space name.txt"),
        ("quote", "quote\"name.txt"),
        ("tab", "tab\tname.txt"),
    ] {
        let repo = root.join(case);
        fs::create_dir_all(&repo).expect("create case repo");
        git(&repo, &["init", "-q"]);
        let deleted = format!("deleted-{path}");
        let untracked = format!("untracked-{path}");
        fs::write(repo.join(path), b"base\n").expect("write tracked fixture");
        fs::write(repo.join(&deleted), b"delete\n").expect("write deleted fixture");
        git(&repo, &["add", path, deleted.as_str()]);
        git(
            &repo,
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
        fs::write(repo.join(path), b"modified\n").expect("modify tracked fixture");
        fs::remove_file(repo.join(&deleted)).expect("remove deleted fixture");
        fs::write(repo.join(&untracked), b"untracked\n").expect("write untracked fixture");

        for args in [
            vec!["ls-files"],
            vec!["ls-files", "-s"],
            vec!["ls-files", "--cached"],
            vec!["ls-files", "--modified"],
            vec!["ls-files", "--deleted"],
            vec!["ls-files", "--others"],
            vec!["ls-files", "-z"],
            vec!["ls-files", "-s", "-z"],
            vec!["ls-files", "--modified", "-z"],
            vec!["ls-files", "--deleted", "-z"],
            vec!["ls-files", "--others", "-z"],
        ] {
            let expected = git(&repo, &args);
            let actual = git_rs(&repo, &args);
            assert_eq!(
                actual, expected,
                "sley output differed for {args:?} path {path:?}"
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ls_files_error_unmatch_matches_upstream_git() {
    let root = unique_temp_dir("ls-files-error-unmatch");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked");
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete");
        fs::write(root.join("--dash.txt"), b"dash\n").expect("write dash path");
        git(
            &root,
            &["add", "--", "tracked.txt", "delete.txt", "--dash.txt"],
        );
        fs::remove_file(root.join("delete.txt")).expect("delete tracked file");
        fs::write(root.join("other.txt"), b"other\n").expect("write untracked");

        for args in [
            vec!["ls-files", "--error-unmatch", "tracked.txt"],
            vec![
                "ls-files",
                "--error-unmatch",
                "--no-error-unmatch",
                "missing",
            ],
            vec![
                "ls-files",
                "--no-error-unmatch",
                "--error-unmatch",
                "missing",
            ],
            vec!["ls-files", "--error-unmatch", "tracked.txt", "missing"],
            vec!["ls-files", "--error-unmatch", "a", "tracked.txt", "b"],
            vec![
                "ls-files",
                "-z",
                "--error-unmatch",
                "tracked.txt",
                "missing",
            ],
            vec!["ls-files", "-d", "--error-unmatch", "delete.txt"],
            vec!["ls-files", "-o", "--error-unmatch", "other.txt"],
            vec!["ls-files", "-o", "--error-unmatch", "tracked.txt"],
            vec!["ls-files", "--error-unmatch", "--", "--dash.txt"],
        ] {
            let expected = run_status("git", &root, &args);
            let actual = run_status(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_eq!(
                actual, expected,
                "sley status/stdout/stderr differed for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}
