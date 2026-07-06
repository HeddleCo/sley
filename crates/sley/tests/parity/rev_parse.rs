//! `rev-parse` engine parity (ported from `sley-cli/tests/rev_parse.rs`).

use sley::Repository;
use sley_testkit::engine_parity::{
    EngineOutput, EngineParityCase, git_bool_line, git_oid_line,
};

#[test]
fn is_shallow_repository_matches_oracle() {
    EngineParityCase::new("rev-parse-is-shallow").run(
        |fixture| {
            fixture.init_default();
            fixture.write_shallow_marker(&fixture.path().join(".git"));
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            EngineOutput::stdout(git_bool_line(repo.is_shallow()))
        },
        |fixture| fixture.oracle(&["rev-parse", "--is-shallow-repository"]),
    );
}

#[test]
fn head_resolution_matches_oracle() {
    EngineParityCase::new("rev-parse-head").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD").expect("rev-parse HEAD");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| {
            let output = fixture.oracle(&["rev-parse", "HEAD"]);
            EngineOutput {
                stdout: output.stdout,
                ..output
            }
        },
    );
}

#[test]
fn abbreviated_object_id_matches_oracle() {
    EngineParityCase::new("rev-parse-abbrev").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.rev_parse("HEAD").expect("HEAD");
            let prefix = &head.to_hex()[..8];
            let oid = repo.rev_parse(prefix).expect("abbrev");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| {
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head);
            let prefix = &head.trim()[..8];
            fixture.oracle(&["rev-parse", prefix])
        },
    );
}

#[test]
fn main_branch_matches_oracle() {
    EngineParityCase::new("rev-parse-main").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("main").expect("main");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "main"]),
    );
}

#[test]
fn full_ref_name_matches_oracle() {
    EngineParityCase::new("rev-parse-full-ref").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("refs/heads/main").expect("full ref");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/heads/main"]),
    );
}

#[test]
fn head_tree_matches_oracle() {
    EngineParityCase::new("rev-parse-head-tree").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD^{tree}").expect("tree");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}

#[test]
fn head_commit_matches_oracle() {
    EngineParityCase::new("rev-parse-head-commit").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD^{commit}").expect("commit");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{commit}"]),
    );
}

#[test]
fn parent_commit_matches_oracle() {
    EngineParityCase::new("rev-parse-parent").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.commit_paths("first", &["one.txt"]);
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("second", &["two.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD~1").expect("parent");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD~1"]),
    );
}

#[test]
fn caret_zero_matches_oracle() {
    EngineParityCase::new("rev-parse-caret-zero").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD^0").expect("caret zero");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^0"]),
    );
}

#[test]
fn tag_peel_matches_oracle() {
    EngineParityCase::new("rev-parse-tag-peel").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("refs/tags/v1^{commit}").expect("peeled");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/tags/v1^{commit}"]),
    );
}

#[test]
fn object_format_name_matches_oracle() {
    EngineParityCase::new("rev-parse-object-format").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            EngineOutput::stdout(git_oid_line(repo.object_format().name()))
        },
        |fixture| fixture.oracle(&["rev-parse", "--show-object-format"]),
    );
}

#[test]
fn not_shallow_repository_matches_oracle() {
    EngineParityCase::new("rev-parse-not-shallow").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            EngineOutput::stdout(git_bool_line(repo.is_shallow()))
        },
        |fixture| fixture.oracle(&["rev-parse", "--is-shallow-repository"]),
    );
}

#[test]
fn double_resolution_matches_oracle() {
    EngineParityCase::new("rev-parse-double").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let first = repo.rev_parse("HEAD").expect("first");
            let second = repo.rev_parse(&first.to_hex()).expect("second");
            EngineOutput::stdout(git_oid_line(second.to_hex()))
        },
        |fixture| {
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head).trim().to_string();
            fixture.oracle(&["rev-parse", &head])
        },
    );
}

#[test]
fn head_equals_main_matches_oracle() {
    EngineParityCase::new("rev-parse-head-equals-main").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.rev_parse("HEAD").expect("HEAD");
            let main = repo.rev_parse("main").expect("main");
            assert_eq!(head, main);
            EngineOutput::stdout(git_oid_line(head.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD"]),
    );
}