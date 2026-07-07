//! Reference and `HEAD` engine parity.

use sley::{ReferenceTarget, Repository};
use sley_testkit::engine_parity::{
    git_oid_line, git_symbolic_ref_line, EngineOutput, EngineParityCase,
};

fn reference_exists_output(repo: &Repository, name: &str) -> EngineOutput {
    let exists = repo.reference_exists(name).expect("reference_exists");
    EngineOutput {
        exit_code: if exists { 0 } else { 1 },
        ..EngineOutput::default()
    }
}

fn head_symbolic_output(repo: &Repository) -> EngineOutput {
    let head = repo.head().expect("head");
    let target = head
        .symbolic_target
        .expect("symbolic HEAD")
        .to_string();
    EngineOutput::stdout(git_symbolic_ref_line(&target))
}

fn head_oid_output(repo: &Repository) -> EngineOutput {
    let head = repo.head().expect("head");
    let oid = head.oid.expect("HEAD resolves");
    EngineOutput::stdout(git_oid_line(oid.to_hex()))
}

fn find_reference_peeled_output(repo: &Repository, name: &str) -> EngineOutput {
    let reference = repo
        .find_reference(name)
        .expect("find_reference")
        .expect("reference must exist");
    let oid = reference
        .peeled_oid(repo)
        .expect("peel")
        .expect("peeled oid");
    EngineOutput::stdout(git_oid_line(oid.to_hex()))
}

fn head_state_attached_output(repo: &Repository) -> EngineOutput {
    let state = repo.head_state().expect("head_state");
    assert!(state.is_attached());
    let mut stdout = git_symbolic_ref_line(state.symbolic_target().expect("target").as_str());
    stdout.extend(git_oid_line(state.oid().expect("oid").to_hex()));
    EngineOutput::stdout(stdout)
}

#[test]
fn head_symbolic_unborn_matches_oracle() {
    EngineParityCase::new("refs-head-symbolic-unborn").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_output(&repo)
        },
        |fixture| fixture.oracle(&["symbolic-ref", "HEAD"]),
    );
}

#[test]
fn head_symbolic_after_commit_matches_oracle() {
    EngineParityCase::new("refs-head-symbolic-attached").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_output(&repo)
        },
        |fixture| fixture.oracle(&["symbolic-ref", "HEAD"]),
    );
}

#[test]
fn head_oid_after_commit_matches_oracle() {
    EngineParityCase::new("refs-head-oid-attached").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_oid_output(&repo)
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD"]),
    );
}

#[test]
fn head_branch_name_unborn_matches_oracle() {
    EngineParityCase::new("refs-head-branch-unborn").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.head().expect("head");
            assert!(head.is_unborn());
            let branch = head.branch_name().expect("branch name");
            let mut stdout = branch.as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| {
            let sym = fixture.oracle_ok(&["symbolic-ref", "HEAD"]);
            let sym = String::from_utf8_lossy(&sym);
            let branch = sym
                .trim()
                .strip_prefix("refs/heads/")
                .expect("branch ref");
            let mut stdout = branch.as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
    );
}

#[test]
fn reference_exists_main_before_commit_matches_oracle() {
    EngineParityCase::new("refs-exists-main-unborn").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reference_exists_output(&repo, "refs/heads/main")
        },
        |fixture| fixture.oracle(&["show-ref", "--verify", "--quiet", "refs/heads/main"]),
    );
}

#[test]
fn reference_exists_main_after_commit_matches_oracle() {
    EngineParityCase::new("refs-exists-main-attached").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reference_exists_output(&repo, "refs/heads/main")
        },
        |fixture| fixture.oracle(&["show-ref", "--verify", "--quiet", "refs/heads/main"]),
    );
}

#[test]
fn reference_exists_missing_branch_matches_oracle() {
    EngineParityCase::new("refs-exists-missing-branch").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reference_exists_output(&repo, "refs/heads/missing")
        },
        |fixture| fixture.oracle(&["show-ref", "--verify", "--quiet", "refs/heads/missing"]),
    );
}

#[test]
fn reference_exists_tag_after_create_matches_oracle() {
    EngineParityCase::new("refs-exists-tag").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reference_exists_output(&repo, "refs/tags/v1")
        },
        |fixture| fixture.oracle(&["show-ref", "--verify", "--quiet", "refs/tags/v1"]),
    );
}

#[test]
fn reference_exists_missing_tag_matches_oracle() {
    EngineParityCase::new("refs-exists-missing-tag").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reference_exists_output(&repo, "refs/tags/missing")
        },
        |fixture| fixture.oracle(&["show-ref", "--verify", "--quiet", "refs/tags/missing"]),
    );
}

#[test]
fn find_reference_head_symbolic_unborn_matches_oracle() {
    EngineParityCase::new("refs-find-head-unborn").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let reference = repo
                .find_reference("HEAD")
                .expect("find_reference")
                .expect("HEAD exists");
            let target = match reference.immediate_target() {
                ReferenceTarget::Symbolic(name) => name.clone(),
                other => panic!("expected symbolic HEAD, got {other:?}"),
            };
            EngineOutput::stdout(git_symbolic_ref_line(&target))
        },
        |fixture| fixture.oracle(&["symbolic-ref", "HEAD"]),
    );
}

#[test]
fn find_reference_main_oid_after_commit_matches_oracle() {
    EngineParityCase::new("refs-find-main-oid").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            find_reference_peeled_output(&repo, "refs/heads/main")
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/heads/main"]),
    );
}

#[test]
fn find_reference_missing_branch_matches_oracle() {
    EngineParityCase::new("refs-find-missing-branch").run_with_compare(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            assert!(
                repo.find_reference("refs/heads/missing")
                    .expect("find_reference")
                    .is_none()
            );
            match repo.rev_parse("refs/heads/missing") {
                Ok(oid) => EngineOutput::stdout(git_oid_line(oid.to_hex())),
                Err(_) => EngineOutput {
                    exit_code: 128,
                    ..EngineOutput::default()
                },
            }
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/heads/missing"]),
        |sley, oracle| {
            assert_eq!(
                sley.exit_code, oracle.exit_code,
                "refs-find-missing-branch: exit code differed"
            );
        },
    );
}

#[test]
fn head_state_unborn_symbolic_matches_oracle() {
    EngineParityCase::new("refs-head-state-unborn").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let state = repo.head_state().expect("head_state");
            assert!(state.is_unborn());
            let target = state.symbolic_target().expect("target");
            EngineOutput::stdout(git_symbolic_ref_line(target.as_str()))
        },
        |fixture| fixture.oracle(&["symbolic-ref", "HEAD"]),
    );
}

#[test]
fn head_state_attached_after_commit_matches_oracle() {
    EngineParityCase::new("refs-head-state-attached").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_state_attached_output(&repo)
        },
        |fixture| {
            let mut stdout = fixture.oracle_ok(&["symbolic-ref", "HEAD"]);
            stdout.extend_from_slice(&fixture.oracle_ok(&["rev-parse", "HEAD"]));
            EngineOutput::stdout(stdout)
        },
    );
}

#[test]
fn head_state_detached_rev_parse_matches_oracle() {
    EngineParityCase::new("refs-head-state-detached").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head).trim().to_string();
            fixture.oracle_ok(&["checkout", "--detach", &head]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let state = repo.head_state().expect("head_state");
            assert!(state.is_detached());
            let oid = state.oid().expect("detached oid");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD"]),
    );
}

#[test]
fn head_detached_symbolic_ref_fails_like_oracle() {
    EngineParityCase::new("refs-head-detached-symbolic").run_with_compare(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head).trim().to_string();
            fixture.oracle_ok(&["checkout", "--detach", &head]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.head().expect("head");
            assert!(head.is_detached());
            EngineOutput {
                exit_code: 128,
                ..EngineOutput::default()
            }
        },
        |fixture| fixture.oracle(&["symbolic-ref", "HEAD"]),
        |sley, oracle| {
            assert_eq!(
                sley.exit_code, oracle.exit_code,
                "refs-head-detached-symbolic: exit code differed"
            );
        },
    );
}

#[test]
fn show_ref_main_line_matches_oracle() {
    EngineParityCase::new("refs-show-ref-main-line").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let reference = repo
                .find_reference("refs/heads/main")
                .expect("find_reference")
                .expect("main exists");
            let oid = reference
                .peeled_oid(&repo)
                .expect("peel")
                .expect("main oid");
            let mut stdout = oid.to_hex().into_bytes();
            stdout.extend_from_slice(b" refs/heads/main\n");
            EngineOutput::stdout(stdout)
        },
        |fixture| fixture.oracle(&["show-ref", "refs/heads/main"]),
    );
}

#[test]
fn lightweight_tag_ref_matches_oracle() {
    EngineParityCase::new("refs-lightweight-tag").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head).trim().to_string();
            fixture.oracle_ok(&["tag", "v-lite", &head]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            find_reference_peeled_output(&repo, "refs/tags/v-lite")
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/tags/v-lite"]),
    );
}

#[test]
fn show_ref_heads_matches_oracle() {
    EngineParityCase::new("refs-show-ref-heads").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let reference = repo
                .find_reference("refs/heads/main")
                .expect("find_reference")
                .expect("main exists");
            let oid = reference
                .peeled_oid(&repo)
                .expect("peel")
                .expect("main oid");
            let mut stdout = oid.to_hex().into_bytes();
            stdout.extend_from_slice(b" refs/heads/main\n");
            EngineOutput::stdout(stdout)
        },
        |fixture| fixture.oracle(&["show-ref", "--heads"]),
    );
}

#[test]
fn show_ref_tags_matches_oracle() {
    EngineParityCase::new("refs-show-ref-tags").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let reference = repo
                .find_reference("refs/tags/v1")
                .expect("find_reference")
                .expect("tag exists");
            let oid = reference.direct_target().expect("direct");
            let mut stdout = oid.to_hex().into_bytes();
            stdout.extend_from_slice(b" refs/tags/v1\n");
            EngineOutput::stdout(stdout)
        },
        |fixture| fixture.oracle(&["show-ref", "--tags"]),
    );
}

#[test]
fn symbolic_ref_topic_branch_matches_oracle() {
    EngineParityCase::new("refs-symbolic-topic").run(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&["checkout", "-b", "topic"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_output(&repo)
        },
        |fixture| fixture.oracle(&["symbolic-ref", "HEAD"]),
    );
}

#[test]
fn reference_exists_head_matches_oracle() {
    EngineParityCase::new("refs-exists-head").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reference_exists_output(&repo, "HEAD")
        },
        |fixture| fixture.oracle(&["show-ref", "--verify", "--quiet", "HEAD"]),
    );
}

#[test]
fn head_after_checkout_branch_matches_oracle() {
    EngineParityCase::new("refs-head-after-checkout").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok(&["branch", "topic"]);
            fixture.oracle_ok(&["checkout", "topic"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_output(&repo)
        },
        |fixture| fixture.oracle(&["symbolic-ref", "HEAD"]),
    );
}

#[test]
fn find_reference_tag_direct_matches_oracle() {
    EngineParityCase::new("refs-find-tag-direct").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let reference = repo
                .find_reference("refs/tags/v1")
                .expect("find_reference")
                .expect("tag exists");
            let oid = reference.direct_target().expect("direct");
            EngineOutput::stdout(git_oid_line(oid.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "refs/tags/v1"]),
    );
}

#[test]
fn show_ref_verify_tag_matches_oracle() {
    EngineParityCase::new("refs-show-ref-verify-tag").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok_with_identity(&["tag", "-a", "v1", "-m", "release"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reference_exists_output(&repo, "refs/tags/v1")
        },
        |fixture| fixture.oracle(&["show-ref", "--verify", "--quiet", "refs/tags/v1"]),
    );
}

#[test]
fn head_branch_name_after_commit_matches_oracle() {
    EngineParityCase::new("refs-head-branch-attached").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let head = repo.head().expect("head");
            let branch = head.branch_name().expect("branch");
            let mut stdout = branch.as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
        |fixture| {
            let sym = fixture.oracle_ok(&["symbolic-ref", "HEAD"]);
            let sym = String::from_utf8_lossy(&sym);
            let branch = sym
                .trim()
                .strip_prefix("refs/heads/")
                .expect("branch ref");
            let mut stdout = branch.as_bytes().to_vec();
            stdout.push(b'\n');
            EngineOutput::stdout(stdout)
        },
    );
}

#[test]
fn find_reference_head_after_commit_matches_oracle() {
    EngineParityCase::new("refs-find-head-attached").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            find_reference_peeled_output(&repo, "HEAD")
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD"]),
    );
}