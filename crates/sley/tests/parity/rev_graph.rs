//! Revision graph parity via [`Repository::rev_graph`].

use sley::Repository;
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase};

fn is_ancestor_output(repo: &Repository, ancestor: &str, descendant: &str) -> EngineOutput {
    let ancestor = repo.rev_parse(ancestor).expect("ancestor");
    let descendant = repo.rev_parse(descendant).expect("descendant");
    let is_ancestor = repo
        .rev_graph()
        .is_ancestor(ancestor, descendant)
        .expect("is_ancestor");
    EngineOutput {
        exit_code: if is_ancestor { 0 } else { 1 },
        ..EngineOutput::default()
    }
}

fn ahead_behind_output(repo: &Repository, left: &str, right: &str) -> EngineOutput {
    let left = repo.rev_parse(left).expect("left");
    let right = repo.rev_parse(right).expect("right");
    let (ahead, behind) = repo
        .rev_graph()
        .ahead_behind(left, right)
        .expect("ahead_behind");
    EngineOutput::stdout(format!("{ahead}\t{behind}\n").into_bytes())
}

fn reachable_count_output(repo: &Repository, tip: &str) -> EngineOutput {
    let tip = repo.rev_parse(tip).expect("tip");
    let commits = repo
        .rev_graph()
        .collect_reachable_commits([tip], Default::default())
        .expect("reachable");
    EngineOutput::stdout(format!("{}\n", commits.len()).into_bytes())
}

fn seed_linear_commits(fixture: &mut sley_testkit::engine_parity::HermeticRepo, n: usize) {
    fixture.init_default();
    for i in 0..n {
        fixture.write_file(&format!("file{i}.txt"), format!("content{i}\n").as_bytes());
        fixture.commit_paths(&format!("commit {i}"), &[&format!("file{i}.txt")]);
    }
}

#[test]
fn is_ancestor_first_to_head_matches_oracle() {
    EngineParityCase::new("rev-graph-ancestor-first-head").run(
        |fixture| seed_linear_commits(fixture, 3),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            is_ancestor_output(&repo, "HEAD~2", "HEAD")
        },
        |fixture| fixture.oracle(&["merge-base", "--is-ancestor", "HEAD~2", "HEAD"]),
    );
}

#[test]
fn is_ancestor_head_not_to_first_matches_oracle() {
    EngineParityCase::new("rev-graph-ancestor-head-not-first").run(
        |fixture| seed_linear_commits(fixture, 3),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            is_ancestor_output(&repo, "HEAD", "HEAD~2")
        },
        |fixture| fixture.oracle(&["merge-base", "--is-ancestor", "HEAD", "HEAD~2"]),
    );
}

#[test]
fn is_ancestor_same_commit_matches_oracle() {
    EngineParityCase::new("rev-graph-ancestor-self").run(
        |fixture| seed_linear_commits(fixture, 2),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            is_ancestor_output(&repo, "HEAD", "HEAD")
        },
        |fixture| fixture.oracle(&["merge-base", "--is-ancestor", "HEAD", "HEAD"]),
    );
}

#[test]
fn is_ancestor_parent_chain_matches_oracle() {
    EngineParityCase::new("rev-graph-ancestor-parent-chain").run(
        |fixture| seed_linear_commits(fixture, 4),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            is_ancestor_output(&repo, "HEAD~1", "HEAD")
        },
        |fixture| fixture.oracle(&["merge-base", "--is-ancestor", "HEAD~1", "HEAD"]),
    );
}

#[test]
fn ahead_behind_equal_tips_matches_oracle() {
    EngineParityCase::new("rev-graph-ahead-behind-equal").run(
        |fixture| seed_linear_commits(fixture, 2),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            ahead_behind_output(&repo, "HEAD", "HEAD")
        },
        |fixture| {
            let head = fixture.oracle_ok(&["rev-parse", "HEAD"]);
            let head = String::from_utf8_lossy(&head).trim().to_string();
            fixture.oracle(&["rev-list", "--left-right", "--count", &format!("{head}...{head}")])
        },
    );
}

#[test]
fn ahead_behind_linear_matches_oracle() {
    EngineParityCase::new("rev-graph-ahead-behind-linear").run(
        |fixture| seed_linear_commits(fixture, 3),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            ahead_behind_output(&repo, "HEAD~1", "HEAD")
        },
        |fixture| fixture.oracle(&["rev-list", "--left-right", "--count", "HEAD~1...HEAD"]),
    );
}

#[test]
fn ahead_behind_diverged_branches_matches_oracle() {
    EngineParityCase::new("rev-graph-ahead-behind-diverged").run(
        |fixture| {
            seed_linear_commits(fixture, 1);
            fixture.oracle_ok(&["branch", "feature"]);
            fixture.write_file("feature.txt", b"feature\n");
            fixture.commit_paths("feature commit", &["feature.txt"]);
            fixture.oracle_ok(&["checkout", "main"]);
            fixture.write_file("main.txt", b"main\n");
            fixture.commit_paths("main commit", &["main.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            ahead_behind_output(&repo, "main", "feature")
        },
        |fixture| fixture.oracle(&["rev-list", "--left-right", "--count", "main...feature"]),
    );
}

#[test]
fn reachable_count_single_commit_matches_oracle() {
    EngineParityCase::new("rev-graph-count-single").run(
        |fixture| seed_linear_commits(fixture, 1),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reachable_count_output(&repo, "HEAD")
        },
        |fixture| fixture.oracle(&["rev-list", "--count", "HEAD"]),
    );
}

#[test]
fn reachable_count_three_commits_matches_oracle() {
    EngineParityCase::new("rev-graph-count-three").run(
        |fixture| seed_linear_commits(fixture, 3),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reachable_count_output(&repo, "HEAD")
        },
        |fixture| fixture.oracle(&["rev-list", "--count", "HEAD"]),
    );
}

#[test]
fn reachable_count_from_root_matches_oracle() {
    EngineParityCase::new("rev-graph-count-root").run(
        |fixture| seed_linear_commits(fixture, 3),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            reachable_count_output(&repo, "HEAD~2")
        },
        |fixture| fixture.oracle(&["rev-list", "--count", "HEAD~2"]),
    );
}

#[test]
fn is_ancestor_on_feature_branch_matches_oracle() {
    EngineParityCase::new("rev-graph-ancestor-feature").run(
        |fixture| {
            seed_linear_commits(fixture, 1);
            fixture.oracle_ok(&["branch", "feature"]);
            fixture.write_file("feature.txt", b"feature\n");
            fixture.commit_paths("feature", &["feature.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            is_ancestor_output(&repo, "HEAD~1", "feature")
        },
        |fixture| fixture.oracle(&["merge-base", "--is-ancestor", "HEAD~1", "feature"]),
    );
}

#[test]
fn ahead_behind_feature_vs_main_matches_oracle() {
    EngineParityCase::new("rev-graph-ahead-behind-feature-main").run(
        |fixture| {
            seed_linear_commits(fixture, 1);
            fixture.oracle_ok(&["branch", "feature"]);
            fixture.write_file("feature.txt", b"feature\n");
            fixture.commit_paths("feature", &["feature.txt"]);
            fixture.oracle_ok(&["checkout", "main"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            ahead_behind_output(&repo, "feature", "main")
        },
        |fixture| fixture.oracle(&["rev-list", "--left-right", "--count", "feature...main"]),
    );
}