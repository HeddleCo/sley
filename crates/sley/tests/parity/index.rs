//! Index read parity via [`Repository::read_index`].

use sley::Repository;
use sley_testkit::engine_parity::EngineParityCase;

use super::common::index_stage_output;

#[test]
fn empty_index_after_init_matches_oracle() {
    EngineParityCase::new("index-empty-after-init").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn single_file_staged_matches_oracle() {
    EngineParityCase::new("index-single-file").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("solo.txt", b"solo\n");
            fixture.oracle_ok(&["add", "solo.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn two_files_staged_matches_oracle() {
    EngineParityCase::new("index-two-files").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("alpha.txt", b"alpha\n");
            fixture.write_file("beta.txt", b"beta\n");
            fixture.oracle_ok(&["add", "alpha.txt", "beta.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn nested_directory_file_matches_oracle() {
    EngineParityCase::new("index-nested-path").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("nested/dir/file.txt", b"nested\n");
            fixture.oracle_ok(&["add", "nested/dir/file.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn executable_bit_staged_matches_oracle() {
    EngineParityCase::new("index-executable").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("run.sh", b"#!/bin/sh\necho hi\n");
            fixture.oracle_ok(&["add", "--chmod=+x", "run.sh"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn multiple_add_sequence_matches_oracle() {
    EngineParityCase::new("index-add-sequence").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.write_file("two.txt", b"two\n");
            fixture.oracle_ok(&["add", "one.txt"]);
            fixture.oracle_ok(&["add", "two.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn modified_restage_matches_oracle() {
    EngineParityCase::new("index-modified-restage").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("keep.txt", b"keep\n");
            fixture.oracle_ok(&["add", "keep.txt"]);
            fixture.write_file("keep.txt", b"changed\n");
            fixture.oracle_ok(&["add", "keep.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn subdirectory_only_matches_oracle() {
    EngineParityCase::new("index-subdir-only").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("pkg/mod.rs", b"pub fn ok() {}\n");
            fixture.oracle_ok(&["add", "pkg/mod.rs"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn deep_nested_path_matches_oracle() {
    EngineParityCase::new("index-deep-nested").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("a/b/c/d.txt", b"deep\n");
            fixture.oracle_ok(&["add", "a/b/c/d.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn empty_file_staged_matches_oracle() {
    EngineParityCase::new("index-empty-file").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("empty.txt", b"");
            fixture.oracle_ok(&["add", "empty.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn binary_file_staged_matches_oracle() {
    EngineParityCase::new("index-binary-file").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("data.bin", &[0u8, 1, 2, 255, 128]);
            fixture.oracle_ok(&["add", "data.bin"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}

#[test]
fn sorted_paths_matches_oracle() {
    EngineParityCase::new("index-sorted-paths").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("z-last.txt", b"z\n");
            fixture.write_file("a-first.txt", b"a\n");
            fixture.write_file("m-middle.txt", b"m\n");
            fixture.oracle_ok(&["add", "z-last.txt", "a-first.txt", "m-middle.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            index_stage_output(&repo)
        },
        |fixture| fixture.index_stage_output(),
    );
}