//! `config` engine parity (ported from `sley-cli/tests/config.rs`).

use sley::{ConfigEditScope, Repository};
use sley_testkit::engine_parity::{
    EngineOutput, EngineParityCase, assert_stdout_eq, git_config_get_all_lines, git_config_line,
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

#[test]
fn repositoryformatversion_matches_oracle() {
    EngineParityCase::new("config-repo-format-version").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("core", None, "repositoryformatversion");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "core.repositoryformatversion"]),
    );
}

#[test]
fn set_core_editor_matches_oracle() {
    EngineParityCase::new("config-set-core-editor").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let plan = repo
                .plan_config_set("core.editor", "vim", ConfigEditScope::Local)
                .expect("plan set");
            repo.apply_config_edit_plan(plan).expect("apply set");
            let config = repo.config().expect("config");
            let value = config.get("core", None, "editor");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| {
            fixture.oracle_ok(&["config", "core.editor", "vim"]);
            fixture.oracle(&["config", "--get", "core.editor"])
        },
    );
}

#[test]
fn unset_user_name_matches_oracle() {
    EngineParityCase::new("config-unset-user-name").run_with_compare(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&["config", "user.name", "Ada Lovelace"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let plan = repo
                .plan_config_unset("user.name", ConfigEditScope::Local)
                .expect("plan unset");
            repo.apply_config_edit_plan(plan).expect("apply unset");
            let config = repo.config().expect("config");
            let value = config.get("user", None, "name");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "user.name"]),
        |sley, oracle| {
            // `git config --get` exits 1 when the key is absent; the library
            // reports `None` as empty stdout with exit 0.
            assert_stdout_eq(sley, oracle, "config-unset-user-name: stdout differed");
            assert_eq!(sley.exit_code, 0);
            assert_eq!(oracle.exit_code, 1);
        },
    );
}

#[test]
fn get_bool_core_filemode_matches_oracle() {
    EngineParityCase::new("config-get-bool-filemode").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config
                .get_bool("core", None, "filemode")
                .map(|enabled| if enabled { "true" } else { "false" });
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "core.filemode"]),
    );
}

#[test]
fn set_bool_feature_matches_oracle() {
    EngineParityCase::new("config-set-bool-feature").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let plan = repo
                .plan_config_set("feature.enabled", "true", ConfigEditScope::Local)
                .expect("plan set");
            repo.apply_config_edit_plan(plan).expect("apply set");
            let config = repo.config().expect("config");
            let value = config.get("feature", None, "enabled");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| {
            fixture.oracle_ok(&["config", "feature.enabled", "true"]);
            fixture.oracle(&["config", "--get", "feature.enabled"])
        },
    );
}

#[test]
fn remote_origin_url_matches_oracle() {
    EngineParityCase::new("config-remote-origin-url").run(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&[
                "config",
                "remote.origin.url",
                "https://example.invalid/repo.git",
            ]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("remote", Some("origin"), "url");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "remote.origin.url"]),
    );
}

#[test]
fn get_all_remote_pushurl_matches_oracle() {
    EngineParityCase::new("config-get-all-pushurl").run(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&[
                "config",
                "--add",
                "remote.origin.pushurl",
                "ssh://git@example.invalid/a.git",
            ]);
            fixture.oracle_ok(&[
                "config",
                "--add",
                "remote.origin.pushurl",
                "ssh://git@example.invalid/b.git",
            ]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let values = config.get_all("remote", Some("origin"), "pushurl");
            EngineOutput::stdout(git_config_get_all_lines(&values))
        },
        |fixture| fixture.oracle(&["config", "--get-all", "remote.origin.pushurl"]),
    );
}

#[test]
fn core_logallrefupdates_matches_oracle() {
    EngineParityCase::new("config-core-logallrefupdates").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("core", None, "logallrefupdates");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "core.logallrefupdates"]),
    );
}

#[test]
fn init_default_branch_matches_oracle() {
    EngineParityCase::new("config-init-defaultbranch").run_with_compare(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("init", None, "defaultBranch");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "init.defaultBranch"]),
        |sley, oracle| {
            assert_stdout_eq(sley, oracle, "config-init-defaultbranch: stdout differed");
            assert_eq!(sley.exit_code, 0);
            assert_eq!(oracle.exit_code, 1);
        },
    );
}

#[test]
fn core_abbrev_matches_oracle() {
    EngineParityCase::new("config-core-abbrev").run_with_compare(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("core", None, "abbrev");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "core.abbrev"]),
        |sley, oracle| {
            assert_stdout_eq(sley, oracle, "config-core-abbrev: stdout differed");
            assert_eq!(sley.exit_code, 0);
            assert_eq!(oracle.exit_code, 1);
        },
    );
}

#[test]
fn set_user_email_matches_oracle() {
    EngineParityCase::new("config-set-user-email").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let plan = repo
                .plan_config_set("user.email", "ada@example.com", ConfigEditScope::Local)
                .expect("plan set");
            repo.apply_config_edit_plan(plan).expect("apply set");
            let config = repo.config().expect("config");
            let value = config.get("user", None, "email");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| {
            fixture.oracle_ok(&["config", "user.email", "ada@example.com"]);
            fixture.oracle(&["config", "--get", "user.email"])
        },
    );
}

#[test]
fn branch_upstream_remote_matches_oracle() {
    EngineParityCase::new("config-branch-upstream-remote").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok(&["config", "branch.main.remote", "."]);
            fixture.oracle_ok(&["config", "branch.main.merge", "refs/heads/main"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("branch", Some("main"), "remote");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "branch.main.remote"]),
    );
}

#[test]
fn branch_upstream_merge_matches_oracle() {
    EngineParityCase::new("config-branch-upstream-merge").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("payload.txt", b"payload\n");
            fixture.commit_paths("initial", &["payload.txt"]);
            fixture.oracle_ok(&["config", "branch.main.remote", "."]);
            fixture.oracle_ok(&["config", "branch.main.merge", "refs/heads/main"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("branch", Some("main"), "merge");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "branch.main.merge"]),
    );
}

#[test]
fn config_string_api_matches_oracle() {
    EngineParityCase::new("config-string-api").run(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&["config", "custom.value", "alpha"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let value = repo
                .config_string("custom", "value")
                .expect("config_string");
            EngineOutput::stdout(git_config_line(value.as_deref()))
        },
        |fixture| fixture.oracle(&["config", "--get", "custom.value"]),
    );
}

#[test]
fn core_autocrlf_matches_oracle() {
    EngineParityCase::new("config-core-autocrlf").run_with_compare(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let value = config.get("core", None, "autocrlf");
            EngineOutput::stdout(git_config_line(value))
        },
        |fixture| fixture.oracle(&["config", "--get", "core.autocrlf"]),
        |sley, oracle| {
            assert_stdout_eq(sley, oracle, "config-core-autocrlf: stdout differed");
            assert_eq!(sley.exit_code, 0);
            assert_eq!(oracle.exit_code, 1);
        },
    );
}

#[test]
fn get_all_same_key_matches_oracle() {
    EngineParityCase::new("config-get-all-same-key").run(
        |fixture| {
            fixture.init_default();
            fixture.oracle_ok(&["config", "--add", "test.var", "one"]);
            fixture.oracle_ok(&["config", "--add", "test.var", "two"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let config = repo.config().expect("config");
            let values = config.get_all("test", None, "var");
            EngineOutput::stdout(git_config_get_all_lines(&values))
        },
        |fixture| fixture.oracle(&["config", "--get-all", "test.var"]),
    );
}
