//! `init` engine parity.

use sley::plumbing::sley_formats::RepositoryLayout;
use sley::{ObjectFormat, Repository};
use sley_testkit::engine_parity::{
    EngineOutput, EngineParityCase, git_bool_line, git_config_line, git_path_line,
    git_symbolic_ref_line,
};

fn head_symbolic_ref(repo: &Repository) -> EngineOutput {
    let head = repo.head().expect("head");
    let target = head.symbolic_target.expect("symbolic HEAD").to_string();
    EngineOutput::stdout(git_symbolic_ref_line(&target))
}

#[test]
fn default_init_head_matches_oracle() {
    EngineParityCase::new("init-default-head").run(
        |_| {},
        |fixture| {
            Repository::init(fixture.path()).expect("init");
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_ref(&repo)
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "-b", "main"]);
            fixture.oracle(&["symbolic-ref", "HEAD"])
        },
    );
}

#[test]
fn init_topic_branch_matches_oracle() {
    EngineParityCase::new("init-topic-branch").run(
        |_| {},
        |fixture| {
            RepositoryLayout::init_at_with_initial_branch(
                fixture.path(),
                ObjectFormat::Sha1,
                false,
                "topic",
            )
            .expect("init");
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_ref(&repo)
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "-b", "topic"]);
            fixture.oracle(&["symbolic-ref", "HEAD"])
        },
    );
}

#[test]
fn init_release_branch_matches_oracle() {
    EngineParityCase::new("init-release-branch").run(
        |_| {},
        |fixture| {
            RepositoryLayout::init_at_with_initial_branch(
                fixture.path(),
                ObjectFormat::Sha1,
                false,
                "release",
            )
            .expect("init");
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_ref(&repo)
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "--initial-branch", "release"]);
            fixture.oracle(&["symbolic-ref", "HEAD"])
        },
    );
}

#[test]
fn init_integration_branch_matches_oracle() {
    EngineParityCase::new("init-integration-branch").run(
        |_| {},
        |fixture| {
            RepositoryLayout::init_at_with_initial_branch(
                fixture.path(),
                ObjectFormat::Sha1,
                false,
                "integration",
            )
            .expect("init");
            let repo = Repository::discover(fixture.path()).expect("discover");
            head_symbolic_ref(&repo)
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "--initial-branch=integration"]);
            fixture.oracle(&["symbolic-ref", "HEAD"])
        },
    );
}

#[test]
fn bare_init_head_matches_oracle() {
    EngineParityCase::new("init-bare-head").run(
        |_| {},
        |fixture| {
            let bare = fixture.path().join("bare.git");
            Repository::init_bare(&bare).expect("init bare");
            let repo = Repository::open(&bare).expect("open bare");
            head_symbolic_ref(&repo)
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "--bare", "bare.git", "-b", "main"]);
            fixture.oracle_in(&fixture.path().join("bare.git"), &["symbolic-ref", "HEAD"])
        },
    );
}

#[test]
fn bare_init_topic_branch_matches_oracle() {
    EngineParityCase::new("init-bare-topic").run(
        |_| {},
        |fixture| {
            let bare = fixture.path().join("topic.git");
            RepositoryLayout::init_at_with_initial_branch(&bare, ObjectFormat::Sha1, true, "topic")
                .expect("init bare");
            let repo = Repository::open(&bare).expect("open bare");
            head_symbolic_ref(&repo)
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "--bare", "-b", "topic", "topic.git"]);
            fixture.oracle_in(&fixture.path().join("topic.git"), &["symbolic-ref", "HEAD"])
        },
    );
}

#[test]
fn bare_init_is_bare_repository_matches_oracle() {
    EngineParityCase::new("init-bare-flag").run(
        |_| {},
        |fixture| {
            let bare = fixture.path().join("bare.git");
            Repository::init_bare(&bare).expect("init bare");
            let repo = Repository::open(&bare).expect("open bare");
            let config = repo.config().expect("config");
            let is_bare = config.get_bool("core", None, "bare").unwrap_or(false);
            EngineOutput::stdout(git_bool_line(is_bare))
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "--bare", "bare.git", "-b", "main"]);
            fixture.oracle_in(
                &fixture.path().join("bare.git"),
                &["rev-parse", "--is-bare-repository"],
            )
        },
    );
}

#[test]
fn non_bare_init_worktree_matches_oracle() {
    EngineParityCase::new("init-worktree-root").run(
        |_| {},
        |fixture| {
            Repository::init(fixture.path()).expect("init");
            let repo = Repository::discover(fixture.path()).expect("discover");
            let workdir = repo.workdir().expect("workdir");
            EngineOutput::stdout(git_path_line(workdir))
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "-b", "main"]);
            fixture.oracle(&["rev-parse", "--show-toplevel"])
        },
    );
}

#[test]
fn init_core_bare_false_matches_oracle() {
    EngineParityCase::new("init-core-bare-false").run(
        |_| {},
        |fixture| {
            Repository::init(fixture.path()).expect("init");
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            EngineOutput::stdout(git_config_line(config.get("core", None, "bare")))
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "-b", "main"]);
            fixture.oracle(&["config", "--get", "core.bare"])
        },
    );
}

#[test]
fn bare_init_core_bare_true_matches_oracle() {
    EngineParityCase::new("init-bare-core-bare-true").run(
        |_| {},
        |fixture| {
            let bare = fixture.path().join("bare.git");
            Repository::init_bare(&bare).expect("init bare");
            let repo = Repository::open(&bare).expect("open bare");
            let config = repo.config().expect("config");
            EngineOutput::stdout(git_config_line(config.get("core", None, "bare")))
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "--bare", "bare.git", "-b", "main"]);
            fixture.oracle_in(
                &fixture.path().join("bare.git"),
                &["config", "--get", "core.bare"],
            )
        },
    );
}

#[test]
fn init_repositoryformatversion_matches_oracle() {
    EngineParityCase::new("init-repo-format-version").run(
        |_| {},
        |fixture| {
            Repository::init(fixture.path()).expect("init");
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            EngineOutput::stdout(git_config_line(config.get(
                "core",
                None,
                "repositoryformatversion",
            )))
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "-b", "main"]);
            fixture.oracle(&["config", "--get", "core.repositoryformatversion"])
        },
    );
}

#[test]
fn init_rediscover_matches_oracle_head() {
    EngineParityCase::new("init-rediscover-head").run(
        |_| {},
        |fixture| {
            let repo = Repository::init(fixture.path()).expect("init");
            let rediscovered = Repository::discover(fixture.path()).expect("rediscover");
            assert_eq!(repo.git_dir(), rediscovered.git_dir());
            head_symbolic_ref(&rediscovered)
        },
        |fixture| {
            fixture.oracle_ok(&["init", "-q", "-b", "main"]);
            fixture.oracle(&["symbolic-ref", "HEAD"])
        },
    );
}
