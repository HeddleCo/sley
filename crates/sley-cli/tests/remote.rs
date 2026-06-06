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

fn assert_remote_config_matches(expected: &Path, actual: &Path, label: &str) {
    let expected_output = run(
        "git",
        expected,
        &["config", "--get-regexp", "^remote\\.origin\\."],
    );
    let actual_output = run(
        "git",
        actual,
        &["config", "--get-regexp", "^remote\\.origin\\."],
    );
    assert_eq!(
        actual_output, expected_output,
        "remote config differed after {label}"
    );
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

#[test]
fn remote_add_list_get_url_and_remove_match_upstream_git() {
    let root = unique_temp_dir("remote");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);

        let add_args = [
            "remote",
            "add",
            "origin",
            "https://example.invalid/repo.git",
        ];
        assert_eq!(git(&upstream, &add_args), git_rs(&rust, &add_args));
        let backup_args = ["remote", "add", "backup", "../backup.git"];
        assert_eq!(git(&upstream, &backup_args), git_rs(&rust, &backup_args));

        for args in [
            ["remote"].as_slice(),
            ["remote", "-v"].as_slice(),
            ["remote", "--verbose"].as_slice(),
            ["remote", "--no-verbose"].as_slice(),
            ["remote", "--no-verbose", "--verbose"].as_slice(),
            ["remote", "--verbose", "--no-verbose"].as_slice(),
            ["remote", "-v", "--verbose"].as_slice(),
            ["remote", "get-url", "origin"].as_slice(),
            ["remote", "-v", "get-url", "origin"].as_slice(),
            ["remote", "--no-verbose", "get-url", "origin"].as_slice(),
            ["remote", "get-url", "backup"].as_slice(),
        ] {
            let expected = git(&upstream, args);
            let actual = git_rs(&rust, args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        let remove_args = ["remote", "remove", "backup"];
        assert_eq!(git(&upstream, &remove_args), git_rs(&rust, &remove_args));
        let expected = git(&upstream, &["remote", "-v"]);
        let actual = git_rs(&rust, &["remote", "-v"]);
        assert_eq!(actual, expected, "sley output differed after remove");

        let set_url_args = [
            "remote",
            "set-url",
            "origin",
            "https://example.invalid/renamed.git",
        ];
        assert_eq!(git(&upstream, &set_url_args), git_rs(&rust, &set_url_args));
        let expected = git(&upstream, &["remote", "get-url", "origin"]);
        let actual = git_rs(&rust, &["remote", "get-url", "origin"]);
        assert_eq!(actual, expected, "sley output differed after set-url");

        let add_url_args = [
            "remote",
            "set-url",
            "--add",
            "origin",
            "https://example.invalid/mirror.git",
        ];
        assert_eq!(git(&upstream, &add_url_args), git_rs(&rust, &add_url_args));
        let replace_url_args = [
            "remote",
            "set-url",
            "origin",
            "https://example.invalid/replaced.git",
            "mirror",
        ];
        assert_eq!(
            git(&upstream, &replace_url_args),
            git_rs(&rust, &replace_url_args)
        );
        let expected = git(&upstream, &["remote", "get-url", "--all", "origin"]);
        let actual = git_rs(&rust, &["remote", "get-url", "--all", "origin"]);
        assert_eq!(
            actual, expected,
            "sley output differed after old-url replacement"
        );

        let delete_url_args = [
            "remote",
            "set-url",
            "--delete",
            "origin",
            "https://example.invalid/renamed.git",
        ];
        assert_eq!(
            git(&upstream, &delete_url_args),
            git_rs(&rust, &delete_url_args)
        );
        let expected = git(&upstream, &["remote", "get-url", "--all", "origin"]);
        let actual = git_rs(&rust, &["remote", "get-url", "--all", "origin"]);
        assert_eq!(actual, expected, "sley output differed after --delete");

        let set_branches_args = ["remote", "set-branches", "origin", "main", "dev"];
        assert_eq!(
            git(&upstream, &set_branches_args),
            git_rs(&rust, &set_branches_args)
        );
        let add_branch_args = ["remote", "set-branches", "--add", "origin", "release"];
        assert_eq!(
            git(&upstream, &add_branch_args),
            git_rs(&rust, &add_branch_args)
        );
        let expected = git(&upstream, &["config", "--get-all", "remote.origin.fetch"]);
        let actual = git(&rust, &["config", "--get-all", "remote.origin.fetch"]);
        assert_eq!(
            actual, expected,
            "sley fetch refspec config differed after set-branches"
        );
        for args in [
            ["remote", "show"].as_slice(),
            ["remote", "show", "-n"].as_slice(),
            ["remote", "show", "-n", "origin"].as_slice(),
            ["remote", "-v", "show", "-n", "origin"].as_slice(),
            ["remote", "show", "-n", "missing"].as_slice(),
        ] {
            let expected = git(&upstream, args);
            let actual = git_rs(&rust, args);
            assert_eq!(
                actual, expected,
                "sley remote show output differed for {args:?}"
            );
        }

        let set_push_url_args = [
            "remote",
            "set-url",
            "--push",
            "origin",
            "ssh://example.invalid/renamed.git",
        ];
        assert_eq!(
            git(&upstream, &set_push_url_args),
            git_rs(&rust, &set_push_url_args)
        );
        for args in [
            ["remote", "get-url", "--push", "origin"].as_slice(),
            ["remote", "get-url", "--push", "--no-push", "origin"].as_slice(),
            ["remote", "get-url", "--all", "origin"].as_slice(),
            ["remote", "get-url", "--all", "--no-all", "origin"].as_slice(),
            ["remote", "get-url", "--push", "--all", "origin"].as_slice(),
            ["remote", "get-url", "--all", "--no-all", "--push", "origin"].as_slice(),
            [
                "remote",
                "get-url",
                "--push",
                "--no-push",
                "--all",
                "origin",
            ]
            .as_slice(),
        ] {
            let expected = git(&upstream, args);
            let actual = git_rs(&rust, args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        let expected = git(&upstream, &["remote", "-v"]);
        let actual = git_rs(&rust, &["remote", "-v"]);
        assert_eq!(
            actual, expected,
            "sley verbose output differed after pushurl"
        );

        let add_push_url_args = [
            "remote",
            "set-url",
            "--push",
            "--add",
            "origin",
            "ssh://example.invalid/mirror.git",
        ];
        assert_eq!(
            git(&upstream, &add_push_url_args),
            git_rs(&rust, &add_push_url_args)
        );
        let replace_push_url_args = [
            "remote",
            "set-url",
            "--push",
            "origin",
            "ssh://example.invalid/replaced-mirror.git",
            "mirror",
        ];
        assert_eq!(
            git(&upstream, &replace_push_url_args),
            git_rs(&rust, &replace_push_url_args)
        );
        let delete_push_url_args = [
            "remote",
            "set-url",
            "--push",
            "--delete",
            "origin",
            "ssh://example.invalid/renamed.git",
        ];
        assert_eq!(
            git(&upstream, &delete_push_url_args),
            git_rs(&rust, &delete_push_url_args)
        );
        let expected = git(
            &upstream,
            &["remote", "get-url", "--push", "--all", "origin"],
        );
        let actual = git_rs(&rust, &["remote", "get-url", "--push", "--all", "origin"]);
        assert_eq!(
            actual, expected,
            "sley output differed after pushurl delete"
        );

        for repo in [&upstream, &rust] {
            git(repo, &["remote", "add", "rewrite", "alias/repo.git"]);
            git(
                repo,
                &["config", "url.https://example.invalid/.insteadOf", "alias/"],
            );
            git(
                repo,
                &[
                    "config",
                    "url.ssh://example.invalid/.pushInsteadOf",
                    "alias/",
                ],
            );
        }
        for args in [
            ["remote", "get-url", "rewrite"].as_slice(),
            ["remote", "get-url", "--push", "rewrite"].as_slice(),
            ["remote", "get-url", "--all", "rewrite"].as_slice(),
            ["remote", "get-url", "--push", "--all", "rewrite"].as_slice(),
        ] {
            let expected = git(&upstream, args);
            let actual = git_rs(&rust, args);
            assert_eq!(
                actual, expected,
                "sley rewritten remote URL output differed for {args:?}"
            );
        }

        fs::write(upstream.join("marker"), b"marker\n").expect("write upstream marker");
        fs::write(rust.join("marker"), b"marker\n").expect("write rust marker");
        let upstream_oid = git(&upstream, &["hash-object", "-w", "marker"]);
        let rust_oid = git(&rust, &["hash-object", "-w", "marker"]);
        assert_eq!(rust_oid, upstream_oid, "marker object ids differed");
        let oid = std::str::from_utf8(&upstream_oid)
            .expect("upstream oid utf8")
            .trim();
        for repo in [&upstream, &rust] {
            git(repo, &["update-ref", "refs/remotes/origin/main", oid]);
            git(repo, &["update-ref", "refs/remotes/origin/dev", oid]);
            git(
                repo,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/main",
                ],
            );
            git(repo, &["config", "remote.pushDefault", "origin"]);
            git(repo, &["config", "branch.main.remote", "origin"]);
            git(repo, &["config", "branch.main.merge", "refs/heads/main"]);
            git(repo, &["config", "branch.main.pushRemote", "origin"]);
            git(repo, &["config", "branch.keep.remote", "backup"]);
            git(repo, &["config", "branch.keep.merge", "refs/heads/keep"]);
            git(repo, &["config", "branch.keep.pushRemote", "origin"]);
        }

        let rename_args = ["remote", "rename", "origin", "upstream"];
        assert_eq!(git(&upstream, &rename_args), git_rs(&rust, &rename_args));
        for args in [
            ["remote"].as_slice(),
            ["remote", "-v"].as_slice(),
            ["remote", "get-url", "--all", "upstream"].as_slice(),
            ["remote", "get-url", "--push", "--all", "upstream"].as_slice(),
        ] {
            let expected = git(&upstream, args);
            let actual = git_rs(&rust, args);
            assert_eq!(
                actual, expected,
                "sley output differed after rename for {args:?}"
            );
        }
        let expected = git(&upstream, &["config", "--get-all", "remote.upstream.fetch"]);
        let actual = git(&rust, &["config", "--get-all", "remote.upstream.fetch"]);
        assert_eq!(
            actual, expected,
            "sley fetch refspec config differed after rename"
        );
        let expected = git(
            &upstream,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        let actual = git(
            &rust,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        assert_eq!(
            actual, expected,
            "sley remote-tracking refs differed after rename"
        );
        let expected = git(
            &upstream,
            &["config", "--local", "--get-regexp", "^(branch|remote)\\."],
        );
        let actual = git(
            &rust,
            &["config", "--local", "--get-regexp", "^(branch|remote)\\."],
        );
        assert_eq!(
            actual, expected,
            "sley branch/remote config differed after rename"
        );
        assert!(
            !rust.join(".git/refs/remotes/origin").exists(),
            "sley left old remote-tracking ref directory after rename"
        );

        fs::write(upstream.join("marker"), b"marker\n").expect("write upstream marker");
        fs::write(rust.join("marker"), b"marker\n").expect("write rust marker");
        let upstream_oid = git(&upstream, &["hash-object", "-w", "marker"]);
        let rust_oid = git(&rust, &["hash-object", "-w", "marker"]);
        let upstream_oid = std::str::from_utf8(&upstream_oid)
            .expect("upstream oid utf8")
            .trim();
        let rust_oid = std::str::from_utf8(&rust_oid)
            .expect("rust oid utf8")
            .trim();
        git(
            &upstream,
            &["update-ref", "refs/remotes/upstream/main", upstream_oid],
        );
        git(
            &rust,
            &["update-ref", "refs/remotes/upstream/main", rust_oid],
        );
        git(
            &upstream,
            &["update-ref", "refs/remotes/upstream/dev", upstream_oid],
        );
        git(
            &rust,
            &["update-ref", "refs/remotes/upstream/dev", rust_oid],
        );
        for repo in [&upstream, &rust] {
            git(repo, &["config", "branch.main.remote", "upstream"]);
            git(repo, &["config", "branch.main.merge", "refs/heads/main"]);
            git(repo, &["config", "branch.dev.remote", "upstream"]);
            git(repo, &["config", "branch.dev.merge", "refs/heads/dev"]);
        }
        let expected = git(&upstream, &["remote", "show", "-n", "upstream"]);
        let actual = git_rs(&rust, &["remote", "show", "-n", "upstream"]);
        assert_eq!(
            actual, expected,
            "sley remote show output differed with local remote refs"
        );
        let set_head_args = ["remote", "set-head", "upstream", "main"];
        assert_eq!(
            git(&upstream, &set_head_args),
            git_rs(&rust, &set_head_args)
        );
        let expected = git(&upstream, &["symbolic-ref", "refs/remotes/upstream/HEAD"]);
        let actual = git(&rust, &["symbolic-ref", "refs/remotes/upstream/HEAD"]);
        assert_eq!(actual, expected, "sley remote HEAD target differed");

        let delete_head_args = ["remote", "set-head", "upstream", "-d"];
        assert_eq!(
            git(&upstream, &delete_head_args),
            git_rs(&rust, &delete_head_args)
        );
        assert!(
            !rust.join(".git/refs/remotes/upstream/HEAD").exists(),
            "sley did not delete remote HEAD symbolic ref"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_rename_moves_packed_remote_tracking_refs_match_upstream_git() {
    let root = unique_temp_dir("remote-rename-packed");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q"]);
            git(
                repo,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/repo.git",
                ],
            );
            fs::write(repo.join("marker"), b"packed marker\n").expect("write marker");
        }
        let upstream_oid = git(&upstream, &["hash-object", "-w", "marker"]);
        let rust_oid = git(&rust, &["hash-object", "-w", "marker"]);
        assert_eq!(rust_oid, upstream_oid, "marker object ids differed");
        let oid = std::str::from_utf8(&upstream_oid)
            .expect("upstream oid utf8")
            .trim();
        for repo in [&upstream, &rust] {
            git(repo, &["update-ref", "refs/remotes/origin/main", oid]);
            git(repo, &["update-ref", "refs/remotes/origin/dev", oid]);
            git(repo, &["pack-refs", "--all", "--prune"]);
            assert!(
                !repo.join(".git/refs/remotes/origin/main").exists(),
                "remote-tracking ref was not packed"
            );
        }

        let args = ["remote", "rename", "origin", "upstream"];
        assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        let expected = git(
            &upstream,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/remotes",
            ],
        );
        let actual = git(
            &rust,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/remotes",
            ],
        );
        assert_eq!(
            actual, expected,
            "sley packed remote-tracking refs differed after rename"
        );
        let packed_refs =
            fs::read_to_string(rust.join(".git/packed-refs")).expect("read packed-refs");
        assert!(
            !packed_refs.contains("refs/remotes/origin/"),
            "sley left old packed remote-tracking refs after rename"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_remove_cleans_refs_and_branch_config_match_upstream_git() {
    let root = unique_temp_dir("remote-remove-cleanup");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q"]);
            git(
                repo,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/repo.git",
                ],
            );
            git(
                repo,
                &[
                    "remote",
                    "add",
                    "other",
                    "https://example.invalid/other.git",
                ],
            );
            git(repo, &["config", "branch.main.remote", "origin"]);
            git(repo, &["config", "branch.main.merge", "refs/heads/main"]);
            git(repo, &["config", "branch.main.pushRemote", "origin"]);
            git(repo, &["config", "branch.main.description", "keep"]);
            git(repo, &["config", "branch.keep.remote", "other"]);
            git(repo, &["config", "branch.keep.merge", "refs/heads/keep"]);
            git(repo, &["config", "remote.pushDefault", "origin"]);
            fs::write(repo.join("marker"), b"remove marker\n").expect("write marker");
        }
        let upstream_oid = git(&upstream, &["hash-object", "-w", "marker"]);
        let rust_oid = git(&rust, &["hash-object", "-w", "marker"]);
        assert_eq!(rust_oid, upstream_oid, "marker object ids differed");
        let oid = std::str::from_utf8(&upstream_oid)
            .expect("upstream oid utf8")
            .trim();
        for repo in [&upstream, &rust] {
            git(repo, &["update-ref", "refs/remotes/origin/main", oid]);
            git(repo, &["update-ref", "refs/remotes/origin/dev", oid]);
            git(
                repo,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/main",
                ],
            );
            git(repo, &["pack-refs", "--all", "--prune"]);
        }

        let args = ["remote", "remove", "origin"];
        assert_same_output(
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args),
            run_output("git", &upstream, &args),
            &args,
        );
        let expected = git(
            &upstream,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        let actual = git(
            &rust,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        assert_eq!(
            actual, expected,
            "sley remote-tracking refs differed after remove"
        );
        let expected = git(
            &upstream,
            &["config", "--local", "--get-regexp", "^branch\\."],
        );
        let actual = git(&rust, &["config", "--local", "--get-regexp", "^branch\\."]);
        assert_eq!(
            actual, expected,
            "sley branch config differed after remote remove"
        );
        let expected = run_output(
            "git",
            &upstream,
            &["config", "--local", "--get-regexp", "^remote\\."],
        );
        let actual = run_output(
            "git",
            &rust,
            &["config", "--local", "--get-regexp", "^remote\\."],
        );
        assert_same_output(
            actual,
            expected,
            &["config", "--local", "--get-regexp", "^remote\\."],
        );
        let expected = git(&upstream, &["remote", "-v"]);
        let actual = git(&rust, &["remote", "-v"]);
        assert_eq!(actual, expected, "sley remote output differed after remove");
        let packed_refs =
            fs::read_to_string(rust.join(".git/packed-refs")).expect("read packed-refs");
        assert!(
            !packed_refs.contains("refs/remotes/origin/"),
            "sley left old packed remote-tracking refs after remove"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_set_url_old_url_errors_match_upstream_git() {
    let root = unique_temp_dir("remote-set-url-errors");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);
        for repo in [&upstream, &rust] {
            git(repo, &["remote", "add", "origin", "one"]);
            git(repo, &["remote", "set-url", "--add", "origin", "two"]);
            git(repo, &["remote", "set-url", "--add", "origin", "three"]);
            git(
                repo,
                &["remote", "set-url", "--push", "--add", "origin", "push-one"],
            );
            git(
                repo,
                &["remote", "set-url", "--push", "--add", "origin", "push-two"],
            );
        }

        for args in [
            [
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/new.git",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/new.git",
                "missing",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/new.git",
                "t.*",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "--push",
                "--no-push",
                "origin",
                "https://example.invalid/new.git",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "--add",
                "--no-add",
                "origin",
                "https://example.invalid/new.git",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "--delete",
                "--no-delete",
                "origin",
                "https://example.invalid/new.git",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "--push",
                "--no-push",
                "--add",
                "origin",
                "https://example.invalid/added-by-no-push.git",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "--push",
                "origin",
                "ssh://example.invalid/new.git",
            ]
            .as_slice(),
            [
                "remote",
                "set-url",
                "--push",
                "origin",
                "ssh://example.invalid/new.git",
                "push-.*",
            ]
            .as_slice(),
            ["remote", "set-url", "--delete", "origin", "missing"].as_slice(),
        ] {
            let expected = run_output("git", &upstream, args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
            assert_same_output(actual, expected, args);
        }

        for args in [
            ["remote", "set-url", "--delete", "origin", "t.*"].as_slice(),
            ["remote", "set-url", "--delete", "origin", ".*"].as_slice(),
            [
                "remote", "set-url", "--push", "--delete", "origin", "push-t.*",
            ]
            .as_slice(),
            [
                "remote", "set-url", "--push", "--delete", "origin", "push-.*",
            ]
            .as_slice(),
        ] {
            let expected = run_output("git", &upstream, args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
            assert_same_output(actual, expected, args);
        }
        let expected = run_output(
            "git",
            &upstream,
            &["config", "--local", "--get-regexp", "^remote\\.origin\\."],
        );
        let actual = run_output(
            "git",
            &rust,
            &["config", "--local", "--get-regexp", "^remote\\.origin\\."],
        );
        assert_same_output(
            actual,
            expected,
            &["config", "--local", "--get-regexp", "^remote\\.origin\\."],
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_set_branches_negations_match_upstream_git() {
    let root = unique_temp_dir("remote-set-branches-negations");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);
        for repo in [&upstream, &rust] {
            git(
                repo,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/repo.git",
                ],
            );
        }

        for args in [
            ["remote", "set-branches", "origin", "main", "dev"].as_slice(),
            ["remote", "set-branches", "--add", "origin", "release"].as_slice(),
            [
                "remote",
                "set-branches",
                "--add",
                "--no-add",
                "origin",
                "only",
            ]
            .as_slice(),
            [
                "remote",
                "set-branches",
                "--no-add",
                "--add",
                "origin",
                "extra",
            ]
            .as_slice(),
            ["remote", "set-branches", "--no-add", "origin", "reset"].as_slice(),
        ] {
            let expected = run_output("git", &upstream, args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
            assert_same_output(actual, expected, args);
            let expected = git(&upstream, &["config", "--get-all", "remote.origin.fetch"]);
            let actual = git(&rust, &["config", "--get-all", "remote.origin.fetch"]);
            assert_eq!(
                actual, expected,
                "sley fetch refspecs differed after {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_set_head_option_order_matches_upstream_git() {
    let root = unique_temp_dir("remote-set-head-options");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q"]);
            git(
                repo,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/repo.git",
                ],
            );
            fs::write(repo.join("marker"), b"set-head marker\n").expect("write marker");
        }
        let upstream_oid = git(&upstream, &["hash-object", "-w", "marker"]);
        let rust_oid = git(&rust, &["hash-object", "-w", "marker"]);
        assert_eq!(rust_oid, upstream_oid, "marker object ids differed");
        let oid = std::str::from_utf8(&upstream_oid)
            .expect("upstream oid utf8")
            .trim();
        for repo in [&upstream, &rust] {
            git(repo, &["update-ref", "refs/remotes/origin/main", oid]);
        }

        for args in [
            ["remote", "set-head", "origin", "main"].as_slice(),
            ["remote", "set-head", "--delete", "origin"].as_slice(),
            ["remote", "set-head", "origin", "main"].as_slice(),
            ["remote", "set-head", "origin", "--delete"].as_slice(),
            [
                "remote",
                "set-head",
                "--delete",
                "--no-delete",
                "origin",
                "main",
            ]
            .as_slice(),
            ["remote", "set-head", "--no-delete", "--delete", "origin"].as_slice(),
            [
                "remote",
                "set-head",
                "--auto",
                "--no-auto",
                "origin",
                "main",
            ]
            .as_slice(),
            ["remote", "set-head", "--no-auto", "origin", "main"].as_slice(),
            ["remote", "set-head", "-a", "--no-auto", "origin", "main"].as_slice(),
        ] {
            let expected = run_output("git", &upstream, args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
            assert_same_output(actual, expected, args);
            let expected = run_output(
                "git",
                &upstream,
                &["symbolic-ref", "refs/remotes/origin/HEAD"],
            );
            let actual = run_output("git", &rust, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
            assert_same_output(
                actual,
                expected,
                &["symbolic-ref", "refs/remotes/origin/HEAD"],
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_set_head_auto_local_remote_matches_upstream_git() {
    let root = unique_temp_dir("remote-set-head-auto-local");
    let expected_remote = root.join("expected-remote");
    let actual_remote = root.join("actual-remote");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    for repo in [&expected_remote, &actual_remote, &upstream, &rust] {
        fs::create_dir_all(repo).expect("create repo dir");
    }
    {
        for repo in [&expected_remote, &actual_remote] {
            git(repo, &["init", "-q"]);
            git(repo, &["checkout", "-q", "-b", "main"]);
            git(repo, &["config", "user.email", "a@b.c"]);
            git(repo, &["config", "user.name", "A"]);
            fs::write(repo.join("f"), b"remote head\n").expect("write remote file");
            git(repo, &["add", "f"]);
            git(repo, &["commit", "-qm", "init"]);
        }
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);
        git(
            &upstream,
            &["remote", "add", "origin", "../expected-remote"],
        );
        git(&rust, &["remote", "add", "origin", "../actual-remote"]);
        git(&upstream, &["fetch", "-q", "origin", "main"]);
        git(&rust, &["fetch", "-q", "origin", "main"]);

        for args in [
            ["remote", "set-head", "-a", "origin"].as_slice(),
            ["remote", "set-head", "--auto", "origin"].as_slice(),
        ] {
            let expected = run_output("git", &upstream, args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
            assert_same_output(actual, expected, args);
            let expected = git(&upstream, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
            let actual = git(&rust, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
            assert_eq!(
                actual, expected,
                "sley remote HEAD target differed after {args:?}"
            );
        }

        for repo in [&upstream, &rust] {
            git(
                repo,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/dev",
                ],
            );
        }
        let args = ["remote", "set-head", "-a", "origin"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        let expected = git(&upstream, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
        let actual = git(&rust, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
        assert_eq!(
            actual, expected,
            "sley remote HEAD target differed after changed auto set-head"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_show_local_remote_matches_upstream_git() {
    let root = unique_temp_dir("remote-show-local");
    let remote = root.join("remote");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    for repo in [&remote, &upstream, &rust] {
        fs::create_dir_all(repo).expect("create repo dir");
    }
    {
        git(&remote, &["init", "-q"]);
        git(&remote, &["checkout", "-q", "-b", "main"]);
        git(&remote, &["config", "user.email", "a@b.c"]);
        git(&remote, &["config", "user.name", "A"]);
        fs::write(remote.join("f"), b"main\n").expect("write main file");
        git(&remote, &["add", "f"]);
        git(&remote, &["commit", "-qm", "main"]);
        git(&remote, &["checkout", "-q", "-b", "dev"]);
        fs::write(remote.join("f"), b"dev\n").expect("write dev file");
        git(&remote, &["commit", "-am", "dev", "-q"]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q"]);
            git(repo, &["remote", "add", "origin", "../remote"]);
            git(repo, &["fetch", "-q", "origin", "main", "dev"]);
        }

        for args in [
            ["remote", "show", "origin"].as_slice(),
            ["remote", "show", "--", "origin"].as_slice(),
            ["remote", "show", "-n", "--", "origin"].as_slice(),
        ] {
            assert_same_output(
                run_output(env!("CARGO_BIN_EXE_sley"), &rust, args),
                run_output("git", &upstream, args),
                args,
            );
        }

        for repo in [&upstream, &rust] {
            git(repo, &["checkout", "-q", "-b", "main", "origin/main"]);
            git(repo, &["checkout", "-q", "-b", "dev", "origin/dev"]);
        }
        let args = ["remote", "show", "origin"];
        assert_same_output(
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args),
            run_output("git", &upstream, &args),
            &args,
        );

        for repo in [&upstream, &rust] {
            git(repo, &["update-ref", "-d", "refs/remotes/origin/dev"]);
            git(
                repo,
                &[
                    "update-ref",
                    "refs/remotes/origin/old",
                    "refs/remotes/origin/main",
                ],
            );
        }
        assert_same_output(
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args),
            run_output("git", &upstream, &args),
            &args,
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_prune_local_remote_matches_upstream_git() {
    let root = unique_temp_dir("remote-prune-local");
    let remote = root.join("remote");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    for repo in [&remote, &upstream, &rust] {
        fs::create_dir_all(repo).expect("create repo dir");
    }
    {
        git(&remote, &["init", "-q"]);
        git(&remote, &["checkout", "-q", "-b", "main"]);
        git(&remote, &["config", "user.email", "a@b.c"]);
        git(&remote, &["config", "user.name", "A"]);
        fs::write(remote.join("f"), b"main\n").expect("write main file");
        git(&remote, &["add", "f"]);
        git(&remote, &["commit", "-qm", "main"]);
        git(&remote, &["checkout", "-q", "-b", "dev"]);
        fs::write(remote.join("f"), b"dev\n").expect("write dev file");
        git(&remote, &["commit", "-am", "dev", "-q"]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q"]);
            git(repo, &["remote", "add", "origin", "../remote"]);
            git(repo, &["fetch", "-q", "origin", "main", "dev"]);
            git(repo, &["update-ref", "-d", "refs/remotes/origin/dev"]);
            git(
                repo,
                &[
                    "update-ref",
                    "refs/remotes/origin/old",
                    "refs/remotes/origin/main",
                ],
            );
        }

        let dry_run_args = ["remote", "prune", "--dry-run", "--", "origin"];
        assert_same_output(
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &dry_run_args),
            run_output("git", &upstream, &dry_run_args),
            &dry_run_args,
        );
        let expected = git(
            &upstream,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        let actual = git(
            &rust,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        assert_eq!(
            actual, expected,
            "sley dry-run prune changed remote-tracking refs"
        );

        let dry_run_reset_args = ["remote", "prune", "--dry-run", "--no-dry-run", "origin"];
        assert_same_output(
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &dry_run_reset_args),
            run_output("git", &upstream, &dry_run_reset_args),
            &dry_run_reset_args,
        );

        for repo in [&upstream, &rust] {
            git(
                repo,
                &[
                    "update-ref",
                    "refs/remotes/origin/old",
                    "refs/remotes/origin/main",
                ],
            );
            git(
                repo,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/old",
                ],
            );
        }
        let no_dry_run_reset_args = ["remote", "prune", "--no-dry-run", "--dry-run", "origin"];
        assert_same_output(
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &no_dry_run_reset_args),
            run_output("git", &upstream, &no_dry_run_reset_args),
            &no_dry_run_reset_args,
        );
        let prune_args = ["remote", "prune", "origin"];
        assert_same_output(
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &prune_args),
            run_output("git", &upstream, &prune_args),
            &prune_args,
        );
        let expected = git(
            &upstream,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        let actual = git(
            &rust,
            &[
                "for-each-ref",
                "--format=%(refname) %(symref)",
                "refs/remotes",
            ],
        );
        assert_eq!(
            actual, expected,
            "sley remote-tracking refs differed after prune"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_add_track_branches_match_upstream_git() {
    let root = unique_temp_dir("remote-add-track");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);

        let args = [
            "remote",
            "add",
            "-t",
            "main",
            "--track=dev",
            "origin",
            "https://example.invalid/repo.git",
        ];
        assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        let expected = git(&upstream, &["config", "--get-all", "remote.origin.fetch"]);
        let actual = git(&rust, &["config", "--get-all", "remote.origin.fetch"]);
        assert_eq!(
            actual, expected,
            "sley tracked remote fetch refspecs differed"
        );
        let expected = git(&upstream, &["remote", "-v"]);
        let actual = git_rs(&rust, &["remote", "-v"]);
        assert_eq!(actual, expected, "sley verbose remote output differed");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn remote_add_config_options_match_upstream_git() {
    let root = unique_temp_dir("remote-add-options");
    fs::create_dir_all(&root).expect("create temp root");
    for (label, args) in [
        (
            "master",
            vec![
                "remote",
                "add",
                "-m",
                "main",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "master-equals",
            vec![
                "remote",
                "add",
                "--master=main",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "tags",
            vec![
                "remote",
                "add",
                "--tags",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "no-tags",
            vec![
                "remote",
                "add",
                "--no-tags",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "tags-reset",
            vec![
                "remote",
                "add",
                "--tags",
                "--no-tags",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "track-reset",
            vec![
                "remote",
                "add",
                "-t",
                "main",
                "--no-track",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "master-reset",
            vec![
                "remote",
                "add",
                "-m",
                "main",
                "--no-master",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "mirror-fetch",
            vec![
                "remote",
                "add",
                "--mirror=fetch",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "mirror-push",
            vec![
                "remote",
                "add",
                "--mirror=push",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "deprecated-mirror",
            vec![
                "remote",
                "add",
                "--mirror",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "mirror-fetch-reset",
            vec![
                "remote",
                "add",
                "--mirror=fetch",
                "--no-mirror",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "mirror-push-reset",
            vec![
                "remote",
                "add",
                "--mirror=push",
                "--no-mirror",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
        (
            "combined",
            vec![
                "remote",
                "add",
                "--tags",
                "-t",
                "main",
                "-m",
                "main",
                "origin",
                "https://example.invalid/repo.git",
            ],
        ),
    ] {
        let expected = root.join(format!("expected-{label}"));
        let actual = root.join(format!("actual-{label}"));
        fs::create_dir_all(&expected).expect("create expected repo");
        fs::create_dir_all(&actual).expect("create actual repo");
        git(&expected, &["init", "-q"]);
        git(&actual, &["init", "-q"]);

        let expected_output = run_output("git", &expected, &args);
        let actual_output = run_output(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_remote_config_matches(&expected, &actual, label);
        let expected_head = fs::read(expected.join(".git/refs/remotes/origin/HEAD")).ok();
        let actual_head = fs::read(actual.join(".git/refs/remotes/origin/HEAD")).ok();
        assert_eq!(
            actual_head, expected_head,
            "remote HEAD differed after {label}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
