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