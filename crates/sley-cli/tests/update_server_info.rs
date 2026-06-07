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

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create destination");
    for entry in fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("read source entry");
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_dir(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).expect("copy file");
        }
    }
}

fn create_update_server_info_fixture(root: &Path) {
    fs::create_dir_all(root).expect("create repo root");
    let init = run_output("git", root, &["init", "-b", "main"]);
    assert!(
        init.status.success(),
        "git init failed\nstderr:\n{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(root.join("file.txt"), b"payload\n").expect("write file");
    let add = run_output("git", root, &["add", "file.txt"]);
    assert!(add.status.success(), "git add failed");
    let commit = Command::new("git")
        .current_dir(root)
        .args(["commit", "-m", "one"])
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
        .env("GIT_AUTHOR_DATE", "@1 +0000")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.invalid")
        .env("GIT_COMMITTER_DATE", "@1 +0000")
        .output()
        .expect("run git commit");
    assert!(
        commit.status.success(),
        "git commit failed\nstderr:\n{}",
        String::from_utf8_lossy(&commit.stderr)
    );

    for args in [
        vec!["branch", "feature"],
        vec!["tag", "lightweight"],
        vec!["tag", "-a", "annotated", "-m", "annotated"],
        vec!["symbolic-ref", "refs/heads/sym", "refs/heads/main"],
        vec!["gc"],
    ] {
        let output = run_output("git", root, &args);
        assert!(
            output.status.success(),
            "git {args:?} failed\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let pack_dir = root.join(".git").join("objects").join("pack");
    fs::write(pack_dir.join("notpack.pack"), b"ignored").expect("write ignored pack");
    fs::write(pack_dir.join("pack-deadbeef.pack"), b"ignored").expect("write short pack");
    fs::write(
        pack_dir.join("pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack"),
        b"ignored",
    )
    .expect("write orphan pack");
}

fn assert_status_stdout_stderr_match(upstream: &Path, actual: &Path, args: &[&str]) {
    let expected = run_output("git", upstream, args);
    let actual_output = run_output(env!("CARGO_BIN_EXE_sley"), actual, args);
    assert_eq!(
        actual_output.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual_output.stdout),
        String::from_utf8_lossy(&actual_output.stderr)
    );
    assert_eq!(
        actual_output.stdout, expected.stdout,
        "sley stdout differed for {args:?}"
    );
    assert_eq!(
        actual_output.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

fn assert_info_files_match(upstream: &Path, actual: &Path) {
    for path in [
        &[".git", "info", "refs"][..],
        &[".git", "objects", "info", "packs"][..],
    ] {
        let expected = fs::read(
            path.iter()
                .fold(upstream.to_path_buf(), |base, part| base.join(part)),
        )
        .expect("read upstream info file");
        let actual = fs::read(
            path.iter()
                .fold(actual.to_path_buf(), |base, part| base.join(part)),
        )
        .expect("read sley info file");
        assert_eq!(actual, expected, "generated {} differed", path.join("/"));
    }
}

#[test]
fn update_server_info_matches_upstream_git() {
    let root = unique_temp_dir("update-server-info");
    let base = root.join("base");
    let upstream = root.join("upstream");
    let actual = root.join("actual");
    create_update_server_info_fixture(&base);
    copy_dir(&base, &upstream);
    copy_dir(&base, &actual);

    for args in [
        vec!["update-server-info", "--bad"],
        vec!["update-server-info", "-x"],
        vec!["update-server-info", "--force=false"],
        vec!["update-server-info", "extra"],
        vec!["update-server-info", "--", "extra"],
    ] {
        assert_status_stdout_stderr_match(&upstream, &actual, &args);
    }

    for args in [
        vec!["update-server-info"],
        vec!["update-server-info", "--force"],
        vec!["update-server-info", "--no-force"],
        vec!["update-server-info", "-ff"],
        vec!["update-server-info", "--"],
    ] {
        assert_status_stdout_stderr_match(&upstream, &actual, &args);
        assert_info_files_match(&upstream, &actual);
    }

    let refs = fs::read_to_string(actual.join(".git").join("info").join("refs"))
        .expect("read generated refs");
    assert!(
        refs.contains("refs/tags/annotated^{}"),
        "expected peeled annotated tag line in info/refs"
    );
    assert!(
        refs.contains("refs/heads/sym"),
        "expected symbolic ref resolved in info/refs"
    );
    let packs = fs::read_to_string(
        actual
            .join(".git")
            .join("objects")
            .join("info")
            .join("packs"),
    )
    .expect("read generated packs");
    assert!(
        !packs.contains("notpack.pack")
            && !packs.contains("pack-deadbeef.pack")
            && !packs.contains("pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack"),
        "invalid and orphan pack filenames should be ignored"
    );

    let _ = fs::remove_dir_all(&root);
}
