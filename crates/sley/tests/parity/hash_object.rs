//! `hash-object` engine parity via [`Repository::write_blob`].

use sley::Repository;
use sley_testkit::engine_parity::{git_oid_line, EngineOutput, EngineParityCase};

fn blob_hash_output(repo: &Repository, bytes: &[u8]) -> EngineOutput {
    let oid = repo.write_blob(bytes).expect("write_blob");
    EngineOutput::stdout(git_oid_line(oid.to_hex()))
}

#[test]
fn empty_blob_matches_oracle() {
    EngineParityCase::new("hash-object-empty").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("empty.bin", b"");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"")
        },
        |fixture| fixture.oracle(&["hash-object", "empty.bin"]),
    );
}

#[test]
fn hello_newline_matches_oracle() {
    EngineParityCase::new("hash-object-hello-newline").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"hello\n")
        },
        |fixture| fixture.oracle(&["hash-object", "hello.txt"]),
    );
}

#[test]
fn hello_without_newline_matches_oracle() {
    EngineParityCase::new("hash-object-hello-no-nl").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("plain.txt", b"hello");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"hello")
        },
        |fixture| fixture.oracle(&["hash-object", "plain.txt"]),
    );
}

#[test]
fn single_byte_matches_oracle() {
    EngineParityCase::new("hash-object-single-byte").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.bin", b"x");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"x")
        },
        |fixture| fixture.oracle(&["hash-object", "one.bin"]),
    );
}

#[test]
fn binary_payload_matches_oracle() {
    EngineParityCase::new("hash-object-binary").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("data.bin", &[0u8, 255, 127, 1, 2, 3]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, &[0u8, 255, 127, 1, 2, 3])
        },
        |fixture| fixture.oracle(&["hash-object", "data.bin"]),
    );
}

#[test]
fn multiline_text_matches_oracle() {
    EngineParityCase::new("hash-object-multiline").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file(
                "lines.txt",
                b"alpha\nbeta\ngamma\n",
            );
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"alpha\nbeta\ngamma\n")
        },
        |fixture| fixture.oracle(&["hash-object", "lines.txt"]),
    );
}

#[test]
fn unicode_utf8_matches_oracle() {
    EngineParityCase::new("hash-object-unicode").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("unicode.txt", "café 🚀\n".as_bytes());
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, "café 🚀\n".as_bytes())
        },
        |fixture| fixture.oracle(&["hash-object", "unicode.txt"]),
    );
}

#[test]
fn whitespace_only_matches_oracle() {
    EngineParityCase::new("hash-object-whitespace").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("spaces.txt", b"   \t  \n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"   \t  \n")
        },
        |fixture| fixture.oracle(&["hash-object", "spaces.txt"]),
    );
}

#[test]
fn repeated_write_same_oid_matches_oracle() {
    EngineParityCase::new("hash-object-idempotent").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("stable.txt", b"stable payload\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let first = repo.write_blob(b"stable payload\n").expect("first");
            let second = repo.write_blob(b"stable payload\n").expect("second");
            assert_eq!(first, second);
            EngineOutput::stdout(git_oid_line(first.to_hex()))
        },
        |fixture| fixture.oracle(&["hash-object", "stable.txt"]),
    );
}

#[test]
fn tab_and_crlf_matches_oracle() {
    EngineParityCase::new("hash-object-tab-crlf").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("mixed.txt", b"a\tb\r\nc\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"a\tb\r\nc\n")
        },
        |fixture| fixture.oracle(&["hash-object", "mixed.txt"]),
    );
}

#[test]
fn kilobyte_payload_matches_oracle() {
    EngineParityCase::new("hash-object-1k").run(
        |fixture| {
            fixture.init_default();
            let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
            fixture.write_file("kilo.bin", &payload);
        },
        |fixture| {
            let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, &payload)
        },
        |fixture| fixture.oracle(&["hash-object", "kilo.bin"]),
    );
}

#[test]
fn json_blob_matches_oracle() {
    EngineParityCase::new("hash-object-json").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("doc.json", br#"{"ok":true,"n":3}"#);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, br#"{"ok":true,"n":3}"#)
        },
        |fixture| fixture.oracle(&["hash-object", "doc.json"]),
    );
}

#[test]
fn written_blob_type_matches_oracle() {
    EngineParityCase::new("hash-object-written-type").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("typed.txt", b"typed\n");
            fixture.oracle_ok(&["hash-object", "-w", "typed.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.write_blob(b"typed\n").expect("write_blob");
            let object = repo.read_object(&oid).expect("read_object");
            let mut stdout = object.object_type.as_str().as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "typed.txt"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-t", &oid])
        },
    );
}

#[test]
fn stdin_content_matches_oracle() {
    EngineParityCase::new("hash-object-stdin").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("stdin.txt", b"from stdin\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"from stdin\n")
        },
        |fixture| fixture.oracle(&["hash-object", "stdin.txt"]),
    );
}

#[test]
fn written_blob_exists_matches_oracle() {
    EngineParityCase::new("hash-object-written-exists").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("persisted.txt", b"persisted\n");
            fixture.oracle_ok(&["hash-object", "-w", "persisted.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.write_blob(b"persisted\n").expect("write_blob");
            repo.read_object_header(&oid)
                .expect("header")
                .expect("exists");
            EngineOutput::stdout(Vec::new())
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "persisted.txt"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-e", &oid])
        },
    );
}

#[test]
fn lf_only_blob_matches_oracle() {
    EngineParityCase::new("hash-object-lf-only").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("lf.txt", b"\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, b"\n")
        },
        |fixture| fixture.oracle(&["hash-object", "lf.txt"]),
    );
}

#[test]
fn long_line_blob_matches_oracle() {
    EngineParityCase::new("hash-object-long-line").run(
        |fixture| {
            fixture.init_default();
            let payload = format!("{}\n", "x".repeat(512));
            fixture.write_file("long.txt", payload.as_bytes());
        },
        |fixture| {
            let payload = format!("{}\n", "x".repeat(512));
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, payload.as_bytes())
        },
        |fixture| fixture.oracle(&["hash-object", "long.txt"]),
    );
}

#[test]
fn hash_object_w_matches_rev_parse() {
    EngineParityCase::new("hash-object-write-roundtrip").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("round.txt", b"roundtrip\n");
            fixture.oracle_ok(&["hash-object", "-w", "round.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.write_blob(b"roundtrip\n").expect("write_blob");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| {
            let oid = fixture.oracle_ok(&["hash-object", "round.txt"]);
            EngineOutput::stdout(oid)
        },
    );
}

#[test]
fn blob_size_header_matches_oracle() {
    EngineParityCase::new("hash-object-blob-size").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("sized.txt", b"12345\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.write_blob(b"12345\n").expect("write_blob");
            let (_, size) = repo
                .read_object_header(&oid)
                .expect("header")
                .expect("exists");
            EngineOutput::stdout(format!("{size}\n").into_bytes())
        },
        |fixture| {
            let oid = String::from_utf8_lossy(
                &fixture.oracle_ok(&["hash-object", "sized.txt"]),
            )
            .trim()
            .to_string();
            fixture.oracle(&["cat-file", "-s", &oid])
        },
    );
}

#[test]
fn null_byte_blob_matches_oracle() {
    EngineParityCase::new("hash-object-null-byte").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("null.bin", &[b'a', 0, b'b']);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            blob_hash_output(&repo, &[b'a', 0, b'b'])
        },
        |fixture| fixture.oracle(&["hash-object", "null.bin"]),
    );
}