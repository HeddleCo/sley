//! `rev-parse` engine parity (ported from `sley-cli/tests/rev_parse.rs`).

use sley::Repository;
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase, git_bool_line, git_oid_line};

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

#[test]
fn grandparent_head_tilde_two_matches_oracle() {
    EngineParityCase::new("rev-parse-head-tilde-two").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.commit_paths("first", &["one.txt"]);
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("second", &["two.txt"]);
            fixture.write_file("three.txt", b"three\n");
            fixture.commit_paths("third", &["three.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD~2").expect("grandparent");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD~2"]),
    );
}

#[test]
fn caret_parent_matches_oracle() {
    EngineParityCase::new("rev-parse-head-caret").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.commit_paths("first", &["one.txt"]);
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("second", &["two.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD^").expect("parent");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^"]),
    );
}

#[test]
fn caret_one_explicit_matches_oracle() {
    EngineParityCase::new("rev-parse-head-caret-one").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.commit_paths("first", &["one.txt"]);
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("second", &["two.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD^1").expect("first parent");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^1"]),
    );
}

#[test]
fn main_branch_tree_matches_oracle() {
    EngineParityCase::new("rev-parse-main-tree").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("main^{tree}").expect("main tree");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "main^{tree}"]),
    );
}

#[test]
fn full_ref_tree_matches_oracle() {
    EngineParityCase::new("rev-parse-full-ref-tree").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("refs/heads/main^{tree}").expect("tree");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/heads/main^{tree}"]),
    );
}

#[test]
fn empty_repo_head_fails_like_oracle() {
    EngineParityCase::new("rev-parse-empty-head").run_with_compare(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            match repo.rev_parse("HEAD") {
                Ok(oid) => EngineOutput::stdout(git_oid_line(oid.to_hex())),
                Err(_) => EngineOutput {
                    exit_code: 128,
                    ..EngineOutput::default()
                },
            }
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD"]),
        |sley, oracle| {
            assert_eq!(
                sley.exit_code, oracle.exit_code,
                "rev-parse-empty-head: exit code differed"
            );
        },
    );
}

#[test]
fn empty_repo_main_fails_like_oracle() {
    EngineParityCase::new("rev-parse-empty-main").run_with_compare(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            match repo.rev_parse("main") {
                Ok(oid) => EngineOutput::stdout(git_oid_line(oid.to_hex())),
                Err(_) => EngineOutput {
                    exit_code: 128,
                    ..EngineOutput::default()
                },
            }
        },
        |fixture| fixture.oracle(&["rev-parse", "main"]),
        |sley, oracle| {
            assert_eq!(
                sley.exit_code, oracle.exit_code,
                "rev-parse-empty-main: exit code differed"
            );
        },
    );
}

#[test]
fn head_tilde_zero_matches_oracle() {
    EngineParityCase::new("rev-parse-head-tilde-zero").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD~0").expect("HEAD~0");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD~0"]),
    );
}

#[test]
fn second_branch_ref_matches_oracle() {
    EngineParityCase::new("rev-parse-second-branch").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.commit_paths("first", &["one.txt"]);
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("second", &["two.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD~1").expect("first commit");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD~1"]),
    );
}

#[test]
fn feature_branch_after_create_matches_oracle() {
    EngineParityCase::new("rev-parse-feature-branch").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("base.txt", b"base\n");
            fixture.commit_paths("base", &["base.txt"]);
            fixture.oracle_ok(&["branch", "feature"]);
            fixture.write_file("feature.txt", b"feature\n");
            fixture.commit_paths("feature work", &["feature.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("feature").expect("feature");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "feature"]),
    );
}

#[test]
fn tag_object_peel_matches_oracle() {
    EngineParityCase::new("rev-parse-tag-object").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("refs/tags/v1^{object}").expect("tag object");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/tags/v1^{object}"]),
    );
}

#[test]
fn long_abbrev_resolution_matches_oracle() {
    EngineParityCase::new("rev-parse-long-abbrev").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.rev_parse("HEAD").expect("HEAD");
            let prefix = &head.to_hex()[..12];
            let oid = repo.rev_parse(prefix).expect("long abbrev");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| {
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head);
            let prefix = &head.trim()[..12];
            fixture.oracle(&["rev-parse", prefix])
        },
    );
}

#[test]
fn great_grandparent_head_tilde_three_matches_oracle() {
    EngineParityCase::new("rev-parse-head-tilde-three").run(
        |fixture| {
            fixture.init_default();
            for i in 0..4 {
                fixture.write_file(&format!("f{i}.txt"), format!("v{i}\n").as_bytes());
                fixture.commit_paths(&format!("c{i}"), &[&format!("f{i}.txt")]);
            }
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD~3").expect("ancestor");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD~3"]),
    );
}

#[test]
fn caret_two_generations_matches_oracle() {
    EngineParityCase::new("rev-parse-head-caret-two").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.commit_paths("first", &["one.txt"]);
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("second", &["two.txt"]);
            fixture.write_file("three.txt", b"three\n");
            fixture.commit_paths("third", &["three.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD^^").expect("grandparent");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^^"]),
    );
}

#[test]
fn head_at_zero_matches_oracle() {
    EngineParityCase::new("rev-parse-head-at-zero").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("HEAD@{0}").expect("HEAD@{0}");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD@{0}"]),
    );
}

#[test]
fn upstream_ref_matches_oracle() {
    EngineParityCase::new("rev-parse-upstream").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok(&["config", "branch.main.remote", "."]);
            fixture.oracle_ok(&["config", "branch.main.merge", "refs/heads/main"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("@{upstream}").expect("upstream");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "@{upstream}"]),
    );
}

#[test]
fn branch_upstream_ref_matches_oracle() {
    EngineParityCase::new("rev-parse-branch-upstream").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok(&["config", "branch.main.remote", "."]);
            fixture.oracle_ok(&["config", "branch.main.merge", "refs/heads/main"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("main@{upstream}").expect("branch upstream");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "main@{upstream}"]),
    );
}

#[test]
fn merge_base_is_ancestor_matches_oracle() {
    EngineParityCase::new("rev-parse-merge-base-is-ancestor").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("base.txt", b"base\n");
            fixture.commit_paths("base", &["base.txt"]);
            fixture.oracle_ok(&["branch", "feature"]);
            fixture.write_file("feature.txt", b"feature\n");
            fixture.commit_paths("feature", &["feature.txt"]);
            fixture.oracle_ok(&["checkout", "main"]);
            fixture.write_file("main.txt", b"main\n");
            fixture.commit_paths("main", &["main.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let base = repo.rev_parse("main~1").expect("common base");
            let main = repo.rev_parse("main").expect("main");
            let is = repo
                .rev_graph()
                .is_ancestor(base, main)
                .expect("is_ancestor");
            EngineOutput {
                exit_code: if is { 0 } else { 1 },
                ..EngineOutput::default()
            }
        },
        |fixture| {
            let base = fixture.oracle_ok(&["merge-base", "main", "feature"]);
            let base = String::from_utf8_lossy(&base).trim().to_string();
            fixture.oracle(&["merge-base", "--is-ancestor", &base, "main"])
        },
    );
}

#[test]
fn short_prefix_seven_matches_oracle() {
    EngineParityCase::new("rev-parse-short-seven").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.rev_parse("HEAD").expect("HEAD");
            let prefix = &head.to_hex()[..7];
            let oid = repo.rev_parse(prefix).expect("short");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| {
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head);
            let prefix = &head.trim()[..7];
            fixture.oracle(&["rev-parse", prefix])
        },
    );
}

#[test]
fn topic_branch_tree_matches_oracle() {
    EngineParityCase::new("rev-parse-topic-tree").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok(&["branch", "topic"]);
            fixture.write_file("topic.txt", b"topic\n");
            fixture.commit_paths("topic", &["topic.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let oid = repo.rev_parse("topic^{tree}").expect("topic tree");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "topic^{tree}"]),
    );
}
