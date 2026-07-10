//! Tree diff parity via [`Repository::diff_name_status`].

use sley::{EntryKind, NameStatusEntry, Repository, TreeEditor};
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase};

fn format_name_status(entries: &[NameStatusEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(entry.line().as_bytes());
        out.push(b'\n');
    }
    out
}

#[test]
fn added_file_matches_oracle() {
    EngineParityCase::new("diff-name-status-added").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("base.txt", b"base\n");
            fixture.commit_paths("base", &["base.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let base = repo.rev_parse("HEAD^{tree}").expect("base tree");
            let blob = repo.write_blob(b"added\n").expect("blob");
            let mut editor = repo.edit_tree(&base).expect("edit_tree");
            editor.upsert("added.txt", EntryKind::Blob, blob);
            let new_tree = repo.write_tree(editor).expect("new tree");
            let entries = repo.diff_name_status(&base, &new_tree).expect("diff");
            EngineOutput::stdout(format_name_status(&entries))
        },
        |fixture| {
            let base = fixture.oracle_ok(&["rev-parse", "HEAD^{tree}"]);
            let base = String::from_utf8_lossy(&base).trim().to_string();
            fixture.write_file("added.txt", b"added\n");
            fixture.oracle_ok(&["add", "added.txt"]);
            let new_tree = fixture.oracle_ok(&["write-tree"]);
            let new_tree = String::from_utf8_lossy(&new_tree).trim().to_string();
            fixture.oracle(&["diff", "--name-status", &base, &new_tree])
        },
    );
}

#[test]
fn modified_file_matches_oracle() {
    EngineParityCase::new("diff-name-status-modified").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("keep.txt", b"old\n");
            fixture.commit_paths("base", &["keep.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let base = repo.rev_parse("HEAD^{tree}").expect("base tree");
            let blob = repo.write_blob(b"new\n").expect("blob");
            let mut editor = repo.edit_tree(&base).expect("edit_tree");
            editor.upsert("keep.txt", EntryKind::Blob, blob);
            let new_tree = repo.write_tree(editor).expect("new tree");
            let entries = repo.diff_name_status(&base, &new_tree).expect("diff");
            EngineOutput::stdout(format_name_status(&entries))
        },
        |fixture| {
            let base = fixture.oracle_ok(&["rev-parse", "HEAD^{tree}"]);
            let base = String::from_utf8_lossy(&base).trim().to_string();
            fixture.write_file("keep.txt", b"new\n");
            fixture.oracle_ok(&["add", "keep.txt"]);
            let new_tree = fixture.oracle_ok(&["write-tree"]);
            let new_tree = String::from_utf8_lossy(&new_tree).trim().to_string();
            fixture.oracle(&["diff", "--name-status", &base, &new_tree])
        },
    );
}

#[test]
fn deleted_file_matches_oracle() {
    EngineParityCase::new("diff-name-status-deleted").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("one.txt", b"one\n");
            fixture.write_file("two.txt", b"two\n");
            fixture.commit_paths("base", &["one.txt", "two.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let base = repo.rev_parse("HEAD^{tree}").expect("base tree");
            let mut editor = repo.edit_tree(&base).expect("edit_tree");
            editor.remove(b"two.txt");
            let new_tree = repo.write_tree(editor).expect("new tree");
            let entries = repo.diff_name_status(&base, &new_tree).expect("diff");
            EngineOutput::stdout(format_name_status(&entries))
        },
        |fixture| {
            let base = fixture.oracle_ok(&["rev-parse", "HEAD^{tree}"]);
            let base = String::from_utf8_lossy(&base).trim().to_string();
            fixture.oracle_ok(&["rm", "--cached", "two.txt"]);
            let new_tree = fixture.oracle_ok(&["write-tree"]);
            let new_tree = String::from_utf8_lossy(&new_tree).trim().to_string();
            fixture.oracle(&["diff", "--name-status", &base, &new_tree])
        },
    );
}

#[test]
fn identical_trees_empty_diff_matches_oracle() {
    EngineParityCase::new("diff-name-status-identical").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("same.txt", b"same\n");
            fixture.commit_paths("base", &["same.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree = repo.rev_parse("HEAD^{tree}").expect("tree");
            let entries = repo.diff_name_status(&tree, &tree).expect("diff");
            EngineOutput::stdout(format_name_status(&entries))
        },
        |fixture| {
            let tree = fixture.oracle_ok(&["rev-parse", "HEAD^{tree}"]);
            let tree = String::from_utf8_lossy(&tree).trim().to_string();
            fixture.oracle(&["diff", "--name-status", &tree, &tree])
        },
    );
}

#[test]
fn two_additions_matches_oracle() {
    EngineParityCase::new("diff-name-status-two-added").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("base.txt", b"base\n");
            fixture.commit_paths("base", &["base.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let base = repo.rev_parse("HEAD^{tree}").expect("base tree");
            let a = repo.write_blob(b"a\n").expect("a");
            let b = repo.write_blob(b"b\n").expect("b");
            let mut editor = repo.edit_tree(&base).expect("edit_tree");
            editor.upsert("a.txt", EntryKind::Blob, a);
            editor.upsert("b.txt", EntryKind::Blob, b);
            let new_tree = repo.write_tree(editor).expect("new tree");
            let entries = repo.diff_name_status(&base, &new_tree).expect("diff");
            EngineOutput::stdout(format_name_status(&entries))
        },
        |fixture| {
            let base = fixture.oracle_ok(&["rev-parse", "HEAD^{tree}"]);
            let base = String::from_utf8_lossy(&base).trim().to_string();
            fixture.write_file("a.txt", b"a\n");
            fixture.write_file("b.txt", b"b\n");
            fixture.oracle_ok(&["add", "a.txt", "b.txt"]);
            let new_tree = fixture.oracle_ok(&["write-tree"]);
            let new_tree = String::from_utf8_lossy(&new_tree).trim().to_string();
            fixture.oracle(&["diff", "--name-status", &base, &new_tree])
        },
    );
}

#[test]
fn nested_addition_matches_oracle() {
    EngineParityCase::new("diff-name-status-nested-added").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("root.txt", b"root\n");
            fixture.commit_paths("root", &["root.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let base = repo.rev_parse("HEAD^{tree}").expect("base tree");
            let blob = repo.write_blob(b"nested\n").expect("blob");
            let mut nested = TreeEditor::new();
            nested.upsert("file.txt", EntryKind::Blob, blob);
            let nested_tree = repo.write_tree(nested).expect("nested tree");
            let mut editor = repo.edit_tree(&base).expect("edit_tree");
            editor.upsert("nested", EntryKind::Tree, nested_tree);
            let new_tree = repo.write_tree(editor).expect("new tree");
            let entries = repo.diff_name_status(&base, &new_tree).expect("diff");
            EngineOutput::stdout(format_name_status(&entries))
        },
        |fixture| {
            let base = fixture.oracle_ok(&["rev-parse", "HEAD^{tree}"]);
            let base = String::from_utf8_lossy(&base).trim().to_string();
            fixture.write_file("nested/file.txt", b"nested\n");
            fixture.oracle_ok(&["add", "nested/file.txt"]);
            let new_tree = fixture.oracle_ok(&["write-tree"]);
            let new_tree = String::from_utf8_lossy(&new_tree).trim().to_string();
            fixture.oracle(&["diff", "--name-status", &base, &new_tree])
        },
    );
}
