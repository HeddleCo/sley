//! Object read parity (commits, tags, blobs).

use sley::{GitObjectType, Repository};
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase, git_oid_line};

#[test]
fn read_commit_tree_matches_oracle() {
    EngineParityCase::new("objects-commit-tree").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.rev_parse("HEAD").expect("HEAD");
            let commit = repo.read_commit(&head).expect("read_commit");
            EngineOutput::stdout(git_oid_line(commit.tree.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}

#[test]
fn read_commit_parent_matches_oracle() {
    EngineParityCase::new("objects-commit-parent").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.commit_paths("first", &["one.txt"]);
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("second", &["two.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.rev_parse("HEAD").expect("HEAD");
            let commit = repo.read_commit(&head).expect("read_commit");
            let parent = commit.parents.first().expect("parent");
            EngineOutput::stdout(git_oid_line(parent.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^"]),
    );
}

#[test]
fn peel_tag_to_commit_matches_oracle() {
    EngineParityCase::new("objects-peel-tag-commit").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tag = repo.rev_parse("refs/tags/v1").expect("tag");
            let commit = repo.peel_to_commit_oid(tag).expect("peel");
            EngineOutput::stdout(git_oid_line(commit.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/tags/v1^{commit}"]),
    );
}

#[test]
fn blob_read_matches_cat_file() {
    EngineParityCase::new("objects-blob-read").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.oracle_ok(&["hash-object", "-w", "payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid_hex = fixture.oracle_ok(&["hash-object", "payload.txt"]);
            let oid = sley::ObjectId::from_hex(
                repo.object_format(),
                String::from_utf8_lossy(&oid_hex).trim(),
            )
            .expect("parse oid");
            let bytes = repo.blobs().read(oid).expect("read blob");
            EngineOutput::stdout(bytes)
        },
        |fixture| {
            let oid = String::from_utf8_lossy(&fixture.oracle_ok(&["hash-object", "payload.txt"]))
                .trim()
                .to_string();
            fixture.oracle(&["cat-file", "-p", &oid])
        },
    );
}

#[test]
fn load_object_type_matches_oracle() {
    EngineParityCase::new("objects-load-type").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.rev_parse("HEAD").expect("HEAD");
            let loaded = repo.load_object(&head).expect("load_object");
            let (object_type, _) = loaded.header();
            assert_eq!(object_type, GitObjectType::Commit);
            let mut stdout = object_type.as_str().as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| fixture.oracle(&["cat-file", "-t", "HEAD"]),
    );
}

#[test]
fn read_tree_entry_count_matches_oracle() {
    EngineParityCase::new("objects-tree-entry-count").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("alpha.txt", b"alpha\n");
            fixture.write_file("beta.txt", b"beta\n");
            fixture.commit_paths("initial", &["alpha.txt", "beta.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree_oid = repo.rev_parse("HEAD^{tree}").expect("tree");
            let tree = repo.read_tree(&tree_oid).expect("read_tree");
            EngineOutput::stdout(format!("{}\n", tree.entries.len()).into_bytes())
        },
        |fixture| {
            let output = fixture.oracle_ok(&["ls-tree", "--name-only", "HEAD^{tree}"]);
            let count = output
                .split(|b| b == &b'\n')
                .filter(|line| !line.is_empty())
                .count();
            EngineOutput::stdout(format!("{count}\n").into_bytes())
        },
    );
}
