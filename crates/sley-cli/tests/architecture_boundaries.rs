//! Architectural regression guards for the CLI-to-engine migration.
//!
//! These tests deliberately inspect source rather than runtime behavior. They
//! turn the intended dependency direction into a gate: commands which already
//! receive an explicit invocation session may not rediscover process-global
//! state, and command implementations may not increase their direct storage
//! wiring while operations move into the engine crates.

use std::fs;
use std::path::{Path, PathBuf};

const MAX_DIRECT_ODB_OPENINGS: usize = 178;
const MAX_DIRECT_REF_STORE_OPENINGS: usize = 179;
const MAX_REPOSITORY_CONTEXT_DISCOVERIES: usize = 0;
const MAX_COMPAT_SESSION_READS: usize = 0;
const MAX_COMPAT_GIT_DIR_DISCOVERIES: usize = 0;
const MAX_COMPAT_GIT_DIR_FROM_DISCOVERIES: usize = 0;

const EXPLICIT_SESSION_COMMANDS: &[&str] = &[
    "add_interactive.rs",
    "add_patch.rs",
    "alias.rs",
    "am.rs",
    "attrs.rs",
    "bisect.rs",
    "blame.rs",
    "cat_file.rs",
    "checkout.rs",
    "checkout_index.rs",
    "commit.rs",
    "config_cmd.rs",
    "credential.rs",
    "describe.rs",
    "diff.rs",
    "diff_files.rs",
    "diff_index.rs",
    "diff_tree.rs",
    "diagnose.rs",
    "difftool.rs",
    "format_patch.rs",
    "format_rev.rs",
    "grep.rs",
    "help.rs",
    "for_each_ref.rs",
    "for_each_repo.rs",
    "fast_export.rs",
    "fast_import.rs",
    "fetch_pack.rs",
    "hash_object.rs",
    "index.rs",
    "interpret_trailers.rs",
    "last_modified.rs",
    "log.rs",
    "merge_index.rs",
    "merge_file.rs",
    "merge_tree.rs",
    "merge_rebase/merge_base.rs",
    "merge_rebase/merge.rs",
    "merge_rebase/pull.rs",
    "mergetool.rs",
    "mktag.rs",
    "name_rev.rs",
    "notes.rs",
    "pack_objects.rs",
    "pack.rs",
    "patch_id.rs",
    "pull_strategy.rs",
    "range_diff.rs",
    "read_tree.rs",
    "rebase.rs",
    "refs.rs",
    "refs_verify.rs",
    "reset.rs",
    "remote/admin.rs",
    "remote/clone.rs",
    "remote/fetch.rs",
    "remote/helper.rs",
    "remote/http_backend.rs",
    "remote/ls_remote.rs",
    "remote/pack.rs",
    "remote/resolve.rs",
    "replay.rs",
    "rerere.rs",
    "rev_list.rs",
    "rev_parse.rs",
    "show.rs",
    "show_branch.rs",
    "shortlog.rs",
    "sparse_checkout.rs",
    "stash.rs",
    "submodule.rs",
    "tag.rs",
    "trees.rs",
    "utility.rs",
    "verify_commit.rs",
    "verify_tag.rs",
    "worktree.rs",
];

const EXPLICIT_SESSION_PLUMBING_COMMANDS: &[&str] = &[
    "add.rs",
    "apply.rs",
    "archive.rs",
    "bundle.rs",
    "clean.rs",
    "commit_tree.rs",
    "commit_graph.rs",
    "fsck.rs",
    "prune_packed.rs",
    "replace.rs",
    "worktree.rs",
];

const EXPLICIT_SESSION_COMMAND_DIRECTORIES: &[&str] = &["branch"];

#[test]
fn direct_engine_wiring_does_not_grow() {
    let source = cli_source();
    assert_budget(
        &source,
        "FileObjectDatabase::from_git_dir",
        MAX_DIRECT_ODB_OPENINGS,
        "open repositories through CliSession/sley::Repository or a typed engine operation",
    );
    assert_budget(
        &source,
        "FileRefStore::new",
        MAX_DIRECT_REF_STORE_OPENINGS,
        "reuse the repository facade or move the operation into the owning engine",
    );
    assert_budget(
        &source,
        "RepositoryContext::discover",
        MAX_REPOSITORY_CONTEXT_DISCOVERIES,
        "receive the invocation repository explicitly",
    );
    assert_budget(
        &source,
        "cli_session()",
        MAX_COMPAT_SESSION_READS,
        "thread CliSession through dispatch instead of reading the compatibility slot",
    );
    assert_budget(
        &source,
        "cli_git_dir()",
        MAX_COMPAT_GIT_DIR_DISCOVERIES,
        "resolve the invocation repository from the explicit CliSession",
    );
    assert_budget(
        &source,
        "cli_git_dir_from(",
        MAX_COMPAT_GIT_DIR_FROM_DISCOVERIES,
        "use CliSession::cwd/git_dir instead of rediscovering from process state",
    );
}

#[test]
fn explicitly_migrated_commands_do_not_rediscover_invocation_state() {
    let commands = manifest_dir().join("src").join("commands");
    for command in EXPLICIT_SESSION_COMMANDS {
        assert_command_uses_explicit_session(&commands.join(command));
    }
    let plumbing = commands.join("plumbing");
    for command in EXPLICIT_SESSION_PLUMBING_COMMANDS {
        assert_command_uses_explicit_session(&plumbing.join(command));
    }
    for directory in EXPLICIT_SESSION_COMMAND_DIRECTORIES {
        let path = commands.join(directory);
        let mut files = Vec::new();
        collect_rust_files(&path, &mut files);
        for file in files {
            assert_command_uses_explicit_session(&file);
        }
    }
}

fn assert_command_uses_explicit_session(path: &Path) {
    let source = fs::read_to_string(path).expect("read migrated command source");
    for forbidden in [
        "cli_session()",
        "RepositoryContext::discover",
        "cli_git_dir()",
        "cli_git_dir_from(",
    ] {
        assert!(
            !source.contains(forbidden),
            "{} reintroduced `{forbidden}`; use its explicit CliSession/repository instead",
            path.display()
        );
    }
}

#[test]
fn repository_context_wraps_the_public_repository_facade() {
    let path = manifest_dir().join("src").join("repository.rs");
    let source = fs::read_to_string(&path).expect("read repository context source");
    let context = source
        .split_once("pub(crate) struct RepositoryContext {")
        .and_then(|(_, tail)| tail.split_once("\n}"))
        .map(|(body, _)| body)
        .expect("find RepositoryContext fields");
    assert!(
        context.contains("repository: Repository"),
        "RepositoryContext must delegate repository ownership to sley::Repository"
    );
    for duplicate in ["git_dir: PathBuf", "format: ObjectFormat", "objects:"] {
        assert!(
            !context.contains(duplicate),
            "RepositoryContext reintroduced duplicate `{duplicate}` repository wiring"
        );
    }
}

fn assert_budget(source: &str, needle: &str, maximum: usize, guidance: &str) {
    let actual = source.match_indices(needle).count();
    assert!(
        actual <= maximum,
        "CLI architecture budget for `{needle}` grew from {maximum} to {actual}; {guidance}"
    );
}

fn cli_source() -> String {
    let mut files = Vec::new();
    collect_rust_files(&manifest_dir().join("src"), &mut files);
    files.sort();
    let mut source = String::new();
    for path in files {
        source.push_str(&fs::read_to_string(path).expect("read CLI source"));
        source.push('\n');
    }
    source
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read CLI source directory") {
        let entry = entry.expect("read CLI source entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
