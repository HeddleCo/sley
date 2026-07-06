//! `config` engine parity (ported from `sley-cli/tests/config.rs`).

use sley::{ConfigEditScope, Repository};
use sley_testkit::engine_parity::{
    EngineOutput, EngineParityCase, git_config_get_all_lines, git_config_line,
};

#[test]
fn get_user_name_after_set_matches_oracle() {
    EngineParityCase::new("config-get-user-name").run(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&["config", "user.name", "Ada Lovelace"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("user", None, "name");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "user.name"]),
    );
}

#[test]
fn set_user_name_via_library_matches_oracle() {
    EngineParityCase::new("config-set-user-name").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let plan = repo
                .plan_config_set("user.name", "Ada Lovelace", ConfigEditScope::Local)
                .expect("plan set");
            repo.apply_config_edit_plan(plan).expect("apply set");
            let config = repo.config().expect("config");
            let value = config.get("user", None, "name");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| {
            fixture.oracle_ok(&["config", "user.name", "Ada Lovelace"]);
            fixture.oracle(&["config", "--get", "user.name"])
        },
    );
}

#[test]
fn get_all_remote_fetch_matches_oracle() {
    EngineParityCase::new("config-get-all-fetch").run(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/heads/main:refs/remotes/origin/main",
            ]);
            fixture.oracle_ok(&[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/heads/dev:refs/remotes/origin/dev",
            ]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let values = config.get_all("remote", Some("origin"), "fetch");
            EngineOutput::stdout(git_config_get_all_lines(&values))
        },
        |fixture| fixture.oracle(&["config", "--get-all", "remote.origin.fetch"]),
    );
}

#[test]
fn core_bare_default_matches_oracle() {
    EngineParityCase::new("config-core-bare-default").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("core", None, "bare");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "core.bare"]),
    );
}