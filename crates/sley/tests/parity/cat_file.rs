//! `cat-file` engine parity (ported from `sley-cli/tests/cat_file.rs`).

use sley::Repository;
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase};

#[test]
fn blob_type_matches_oracle() {
    EngineParityCase::new("cat-file-blob-type").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.oracle_ok(&["hash-object", "-w", "hello.txt"]);
        },
        |fixture| {
            let oid_hex = fixture.oracle_ok(&["hash-object", "hello.txt"]);
            let oid = sley::ObjectId::from_hex(
                Repository::discover(fixture.path())
                    .expect("discover")
                    .object_format(),
                String::from_utf8_lossy(&oid_hex).trim(),
            )
            .expect("parse oid");
            let repo = Repository::discover(fixture.path()).expect("discover");
            let object = repo.read_object(&oid).expect("read blob");
            let mut stdout = object.object_type.as_str().as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "hello.txt"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-t", &oid])
        },
    );
}

#[test]
fn blob_pretty_matches_oracle() {
    EngineParityCase::new("cat-file-blob-pretty").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.oracle_ok(&["hash-object", "-w", "hello.txt"]);
        },
        |fixture| {
            let oid_hex = fixture.oracle_ok(&["hash-object", "hello.txt"]);
            let oid = sley::ObjectId::from_hex(
                Repository::discover(fixture.path())
                    .expect("discover")
                    .object_format(),
                String::from_utf8_lossy(&oid_hex).trim(),
            )
            .expect("parse oid");
            let repo = Repository::discover(fixture.path()).expect("discover");
            let object = repo.read_object(&oid).expect("read blob");
            EngineOutput::stdout(object.body.to_vec())
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "hello.txt"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-p", &oid])
        },
    );
}

#[test]
fn commit_pretty_matches_oracle() {
    EngineParityCase::new("cat-file-commit-pretty").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD").expect("HEAD");
            let object = repo.read_object(&oid).expect("read commit");
            EngineOutput::stdout(object.body.to_vec())
        },
        |fixture| fixture.oracle(&["cat-file", "-p", "HEAD"]),
    );
}