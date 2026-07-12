//! `<rev>:<path>` engine parity via [`Repository::resolve_path`].

use sley::Repository;
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase, git_oid_line};

fn resolve_path_oid_output(repo: &Repository, rev: &str, path: &str) -> EngineOutput {
    let entry = repo.resolve_path(rev, path).expect("resolve_path");
    EngineOutput::stdout(git_oid_line(entry.oid.to_hex()))
}

#[test]
fn head_colon_file_matches_oracle() {
    EngineParityCase::new("resolve-path-head-file").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            resolve_path_oid_output(&repo, "HEAD", "hello.txt")
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD:hello.txt"]),
    );
}

#[test]
fn main_colon_file_matches_oracle() {
    EngineParityCase::new("resolve-path-main-file").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            resolve_path_oid_output(&repo, "main", "payload.txt")
        },
        |fixture| fixture.oracle(&["rev-parse", "main:payload.txt"]),
    );
}

#[test]
fn nested_path_matches_oracle() {
    EngineParityCase::new("resolve-path-nested").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("nested/dir/file.txt", b"nested\n");
            fixture.commit_paths("initial", &["nested/dir/file.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            resolve_path_oid_output(&repo, "HEAD", "nested/dir/file.txt")
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD:nested/dir/file.txt"]),
    );
}

#[test]
fn head_colon_directory_matches_oracle() {
    EngineParityCase::new("resolve-path-head-dir").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("nested/dir/file.txt", b"nested\n");
            fixture.commit_paths("initial", &["nested/dir/file.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            resolve_path_oid_output(&repo, "HEAD", "nested")
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD:nested"]),
    );
}

#[test]
fn head_colon_tree_matches_oracle() {
    EngineParityCase::new("resolve-path-head-tree").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            resolve_path_oid_output(&repo, "HEAD", "")
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD:"]),
    );
}

#[test]
fn tag_peel_colon_file_matches_oracle() {
    EngineParityCase::new("resolve-path-tag-file").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            resolve_path_oid_output(&repo, "refs/tags/v1^{commit}", "hello.txt")
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/tags/v1^{commit}:hello.txt"]),
    );
}

#[test]
fn full_ref_colon_file_matches_oracle() {
    EngineParityCase::new("resolve-path-full-ref-file").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("solo.txt", b"solo\n");
            fixture.commit_paths("initial", &["solo.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            resolve_path_oid_output(&repo, "refs/heads/main", "solo.txt")
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/heads/main:solo.txt"]),
    );
}

#[test]
fn missing_path_fails_like_oracle() {
    EngineParityCase::new("resolve-path-missing").run_with_compare(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            match repo.resolve_path("HEAD", "missing.txt") {
                Ok(entry) => EngineOutput::stdout(git_oid_line(entry.oid.to_hex())),
                Err(_) => EngineOutput {
                    exit_code: 128,
                    ..EngineOutput::default()
                },
            }
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD:missing.txt"]),
        |sley, oracle| {
            assert_eq!(
                sley.exit_code, oracle.exit_code,
                "resolve-path-missing: exit code differed"
            );
        },
    );
}
