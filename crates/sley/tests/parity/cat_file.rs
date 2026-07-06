//! `cat-file` engine parity (ported from `sley-cli/tests/cat_file.rs`).

use sley::{GitObjectType, Repository};
use sley_testkit::engine_parity::{git_size_line, EngineOutput, EngineParityCase};

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

#[test]
fn tree_type_matches_oracle() {
    EngineParityCase::new("cat-file-tree-type").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree = repo.rev_parse("HEAD^{tree}").expect("tree");
            let object = repo.read_object(&tree).expect("read tree");
            let mut stdout = object.object_type.as_str().as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| fixture.oracle(&["cat-file", "-t", "HEAD^{tree}"]),
    );
}

#[test]
fn blob_size_matches_oracle() {
    EngineParityCase::new("cat-file-blob-size").run(
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
            let (_, size) = repo
                .read_object_header(&oid)
                .expect("header")
                .expect("object exists");
            EngineOutput::stdout(git_size_line(size))
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "hello.txt"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-s", &oid])
        },
    );
}

#[test]
fn commit_size_matches_oracle() {
    EngineParityCase::new("cat-file-commit-size").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD").expect("HEAD");
            let (_, size) = repo
                .read_object_header(&oid)
                .expect("header")
                .expect("object exists");
            EngineOutput::stdout(git_size_line(size))
        },
        |fixture| fixture.oracle(&["cat-file", "-s", "HEAD"]),
    );
}

#[test]
fn tree_size_matches_oracle() {
    EngineParityCase::new("cat-file-tree-size").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree = repo.rev_parse("HEAD^{tree}").expect("tree");
            let (_, size) = repo
                .read_object_header(&tree)
                .expect("header")
                .expect("object exists");
            EngineOutput::stdout(git_size_line(size))
        },
        |fixture| fixture.oracle(&["cat-file", "-s", "HEAD^{tree}"]),
    );
}

#[test]
fn annotated_tag_type_matches_oracle() {
    EngineParityCase::new("cat-file-tag-type").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tag = repo.rev_parse("refs/tags/v1").expect("tag");
            let object = repo.read_object(&tag).expect("read tag");
            let mut stdout = object.object_type.as_str().as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| fixture.oracle(&["cat-file", "-t", "refs/tags/v1"]),
    );
}

#[test]
fn annotated_tag_pretty_matches_oracle() {
    EngineParityCase::new("cat-file-tag-pretty").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tag = repo.rev_parse("refs/tags/v1").expect("tag");
            let object = repo.read_object(&tag).expect("read tag");
            EngineOutput::stdout(object.body.to_vec())
        },
        |fixture| fixture.oracle(&["cat-file", "-p", "refs/tags/v1"]),
    );
}

#[test]
fn commit_type_matches_oracle() {
    EngineParityCase::new("cat-file-commit-type").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD").expect("HEAD");
            let object = repo.read_object(&oid).expect("read commit");
            assert_eq!(object.object_type, GitObjectType::Commit);
            let mut stdout = object.object_type.as_str().as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| fixture.oracle(&["cat-file", "-t", "HEAD"]),
    );
}

#[test]
fn peeled_tag_commit_pretty_matches_oracle() {
    EngineParityCase::new("cat-file-tag-peel-pretty").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let commit = repo.rev_parse("refs/tags/v1^{commit}").expect("peeled");
            let object = repo.read_object(&commit).expect("read commit");
            EngineOutput::stdout(object.body.to_vec())
        },
        |fixture| fixture.oracle(&["cat-file", "-p", "refs/tags/v1^{commit}"]),
    );
}

#[test]
fn tree_raw_matches_oracle() {
    EngineParityCase::new("cat-file-tree-raw").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree = repo.rev_parse("HEAD^{tree}").expect("tree");
            let object = repo.read_object(&tree).expect("read tree");
            EngineOutput::stdout(object.body.to_vec())
        },
        |fixture| fixture.oracle(&["cat-file", "tree", "HEAD^{tree}"]),
    );
}

#[test]
fn annotated_tag_size_matches_oracle() {
    EngineParityCase::new("cat-file-tag-size").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tag = repo.rev_parse("refs/tags/v1").expect("tag");
            let (_, size) = repo
                .read_object_header(&tag)
                .expect("header")
                .expect("object exists");
            EngineOutput::stdout(git_size_line(size))
        },
        |fixture| fixture.oracle(&["cat-file", "-s", "refs/tags/v1"]),
    );
}

#[test]
fn tree_exists_is_empty_stdout_like_oracle() {
    EngineParityCase::new("cat-file-tree-exists").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree = repo.rev_parse("HEAD^{tree}").expect("tree");
            repo.read_object_header(&tree)
                .expect("header")
                .expect("object exists");
            EngineOutput::stdout(Vec::new())
        },
        |fixture| fixture.oracle(&["cat-file", "-e", "HEAD^{tree}"]),
    );
}

#[test]
fn commit_exists_is_empty_stdout_like_oracle() {
    EngineParityCase::new("cat-file-commit-exists").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD").expect("HEAD");
            repo.read_object_header(&oid)
                .expect("header")
                .expect("object exists");
            EngineOutput::stdout(Vec::new())
        },
        |fixture| fixture.oracle(&["cat-file", "-e", "HEAD"]),
    );
}

#[test]
fn empty_blob_type_matches_oracle() {
    EngineParityCase::new("cat-file-empty-blob-type").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("empty.bin", b"");
            fixture.oracle_ok(&["hash-object", "-w", "empty.bin"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.write_blob(b"").expect("write empty blob");
            let object = repo.read_object(&oid).expect("read blob");
            let mut stdout = object.object_type.as_str().as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "empty.bin"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-t", &oid])
        },
    );
}

#[test]
fn nested_tree_raw_matches_oracle() {
    EngineParityCase::new("cat-file-nested-tree-raw").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("nested/dir/file.txt", b"nested\n");
            fixture.commit_paths("initial", &["nested/dir/file.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree = repo.rev_parse("HEAD^{tree}").expect("tree");
            let object = repo.read_object(&tree).expect("read tree");
            EngineOutput::stdout(object.body.to_vec())
        },
        |fixture| fixture.oracle(&["cat-file", "tree", "HEAD^{tree}"]),
    );
}

#[test]
fn blob_exists_is_empty_stdout_like_oracle() {
    EngineParityCase::new("cat-file-blob-exists").run(
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
            repo.read_object_header(&oid)
                .expect("header")
                .expect("object exists");
            EngineOutput::stdout(Vec::new())
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "hello.txt"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-e", &oid])
        },
    );
}