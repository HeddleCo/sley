//! `update-index` engine parity.

use sley::Repository;
use sley::plumbing::sley_worktree::UpdateIndexOptions;
use sley_testkit::engine_parity::EngineParityCase;

use super::common::{index_stage_output, run_update_index};

fn default_options() -> UpdateIndexOptions {
    UpdateIndexOptions {
        add: false,
        remove: false,
        force_remove: false,
        chmod: None,
        info_only: false,
        ignore_skip_worktree_entries: false,
        allow_skip_worktree_entries: false,
    }
}

#[test]
fn add_new_file_matches_oracle() {
    EngineParityCase::new("update-index-add").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["new.txt"],
                UpdateIndexOptions {
                    add: true,
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--add", "new.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn remove_tracked_file_matches_oracle() {
    EngineParityCase::new("update-index-remove").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["one.txt"],
                UpdateIndexOptions {
                    remove: true,
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--remove", "one.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn refresh_modified_file_matches_oracle() {
    EngineParityCase::new("update-index-refresh").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(&repo, &["keep.txt"], default_options())
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "keep.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn force_remove_matches_oracle() {
    EngineParityCase::new("update-index-force-remove").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["keep.txt"],
                UpdateIndexOptions {
                    force_remove: true,
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--force-remove", "keep.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn chmod_plus_x_matches_oracle() {
    EngineParityCase::new("update-index-chmod-plus-x").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["keep.txt"],
                UpdateIndexOptions {
                    chmod: Some(true),
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--chmod=+x", "keep.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn chmod_minus_x_matches_oracle() {
    EngineParityCase::new("update-index-chmod-minus-x").run(
        |fixture| {
            fixture.seed_update_index_fixture();
            fixture.oracle_ok(&["update-index", "--chmod=+x", "keep.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["keep.txt"],
                UpdateIndexOptions {
                    chmod: Some(false),
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--chmod=-x", "keep.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn add_multiple_paths_matches_oracle() {
    EngineParityCase::new("update-index-add-multiple").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["new.txt", "z.txt"],
                UpdateIndexOptions {
                    add: true,
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--add", "new.txt", "z.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn remove_and_add_sequence_matches_oracle() {
    EngineParityCase::new("update-index-remove-then-add").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["one.txt"],
                UpdateIndexOptions {
                    remove: true,
                    ..default_options()
                },
            );
            run_update_index(
                &repo,
                &["new.txt"],
                UpdateIndexOptions {
                    add: true,
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--remove", "one.txt"]);
            fixture.oracle_ok(&["update-index", "--add", "new.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn quiet_refresh_matches_oracle() {
    EngineParityCase::new("update-index-quiet-refresh").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(&repo, &["keep.txt"], default_options())
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "-q", "keep.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn add_then_refresh_matches_oracle() {
    EngineParityCase::new("update-index-add-then-refresh").run(
        |fixture| fixture.seed_update_index_fixture(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["new.txt"],
                UpdateIndexOptions {
                    add: true,
                    ..default_options()
                },
            );
            fixture.write_file("new.txt", b"updated-new");
            run_update_index(&repo, &["new.txt"], default_options())
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--add", "new.txt"]);
            fixture.write_file("new.txt", b"updated-new");
            fixture.oracle_ok(&["update-index", "new.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn staged_tree_after_add_matches_oracle() {
    EngineParityCase::new("update-index-staged-tree").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("alpha.txt", b"alpha\n");
            fixture.write_file("beta.txt", b"beta\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["alpha.txt", "beta.txt"],
                UpdateIndexOptions {
                    add: true,
                    ..default_options()
                },
            )
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--add", "alpha.txt", "beta.txt"]);
            fixture.index_stage_output()
        },
    );
}

#[test]
fn read_index_matches_oracle_stage_after_add() {
    EngineParityCase::new("update-index-read-index").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("solo.txt", b"solo\n");
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            run_update_index(
                &repo,
                &["solo.txt"],
                UpdateIndexOptions {
                    add: true,
                    ..default_options()
                },
            );
            index_stage_output(&repo)
        },
        |fixture| {
            fixture.oracle_ok(&["update-index", "--add", "solo.txt"]);
            fixture.index_stage_output()
        },
    );
}