//! `write-tree` engine parity via [`Repository::write_tree`].

use sley::{EntryKind, ObjectId, Repository, TreeEditor};
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase, git_oid_line};

fn empty_tree_oid(repo: &Repository) -> ObjectId {
    ObjectId::empty_tree(repo.object_format())
}

#[test]
fn empty_tree_matches_oracle() {
    EngineParityCase::new("write-tree-empty").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree = repo.write_tree(TreeEditor::new()).expect("write_tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "4b825dc642cb6eb9a060e54bf8d69288fbee4904"]),
    );
}

#[test]
fn single_blob_tree_matches_oracle() {
    EngineParityCase::new("write-tree-single-blob").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("hello.txt", b"hello\n");
            fixture.commit_paths("initial", &["hello.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let blob = repo.write_blob(b"hello\n").expect("blob");
            let mut builder = TreeEditor::new();
            builder.upsert("hello.txt", EntryKind::Blob, blob);
            let tree = repo.write_tree(builder).expect("write_tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}

#[test]
fn two_file_tree_matches_oracle() {
    EngineParityCase::new("write-tree-two-files").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("alpha.txt", b"alpha\n");
            fixture.write_file("beta.txt", b"beta\n");
            fixture.commit_paths("initial", &["alpha.txt", "beta.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let alpha = repo.write_blob(b"alpha\n").expect("alpha");
            let beta = repo.write_blob(b"beta\n").expect("beta");
            let mut builder = TreeEditor::new();
            builder.upsert("alpha.txt", EntryKind::Blob, alpha);
            builder.upsert("beta.txt", EntryKind::Blob, beta);
            let tree = repo.write_tree(builder).expect("write_tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}

#[test]
fn nested_directory_tree_matches_oracle() {
    EngineParityCase::new("write-tree-nested").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("nested/dir/file.txt", b"nested\n");
            fixture.commit_paths("initial", &["nested/dir/file.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let file = repo.write_blob(b"nested\n").expect("blob");
            let mut dir = TreeEditor::new();
            dir.upsert("file.txt", EntryKind::Blob, file);
            let dir_tree = repo.write_tree(dir).expect("dir tree");
            let mut nested = TreeEditor::new();
            nested.upsert("dir", EntryKind::Tree, dir_tree);
            let nested_tree = repo.write_tree(nested).expect("nested tree");
            let mut root = TreeEditor::new();
            root.upsert("nested", EntryKind::Tree, nested_tree);
            let tree = repo.write_tree(root).expect("root tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}

#[test]
fn tree_roundtrip_preserves_oid_matches_oracle() {
    EngineParityCase::new("write-tree-roundtrip").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("keep.txt", b"keep\n");
            fixture.commit_paths("initial", &["keep.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let tree_oid = repo.rev_parse("HEAD^{tree}").expect("tree");
            let tree = repo.read_tree(&tree_oid).expect("read_tree");
            let rebuilt = repo
                .write_tree(TreeEditor::from_tree(tree))
                .expect("write_tree");
            assert_eq!(tree_oid, rebuilt);
            EngineOutput::stdout(git_oid_line(rebuilt.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}

#[test]
fn edit_tree_add_file_matches_oracle() {
    EngineParityCase::new("write-tree-edit-add").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("base.txt", b"base\n");
            fixture.commit_paths("base", &["base.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let base_tree = repo.rev_parse("HEAD^{tree}").expect("base tree");
            let blob = repo.write_blob(b"added\n").expect("blob");
            let mut editor = repo.edit_tree(&base_tree).expect("edit_tree");
            editor.upsert("added.txt", EntryKind::Blob, blob);
            let tree = repo.write_tree(editor).expect("write_tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| {
            fixture.write_file("added.txt", b"added\n");
            fixture.oracle_ok(&["add", "added.txt"]);
            fixture.oracle(&["write-tree"])
        },
    );
}

#[test]
fn staged_write_tree_matches_oracle() {
    EngineParityCase::new("write-tree-staged").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("solo.txt", b"solo\n");
            fixture.oracle_ok(&["add", "solo.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let blob = repo.write_blob(b"solo\n").expect("blob");
            let mut builder = TreeEditor::new();
            builder.upsert("solo.txt", EntryKind::Blob, blob);
            let tree = repo.write_tree(builder).expect("write_tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| fixture.oracle(&["write-tree"]),
    );
}

#[test]
fn subdirectory_tree_matches_oracle() {
    EngineParityCase::new("write-tree-subdir").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("pkg/mod.rs", b"pub fn ok() {}\n");
            fixture.commit_paths("initial", &["pkg/mod.rs"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let blob = repo.write_blob(b"pub fn ok() {}\n").expect("blob");
            let mut pkg = TreeEditor::new();
            pkg.upsert("mod.rs", EntryKind::Blob, blob);
            let pkg_tree = repo.write_tree(pkg).expect("pkg tree");
            let mut root = TreeEditor::new();
            root.upsert("pkg", EntryKind::Tree, pkg_tree);
            let tree = repo.write_tree(root).expect("root tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}

#[test]
fn empty_tree_roundtrip_matches_oracle() {
    EngineParityCase::new("write-tree-empty-roundtrip").run(
        |fixture| fixture.init_default(),
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let empty = empty_tree_oid(&repo);
            let tree = repo.read_tree(&empty).expect("read empty tree");
            let rebuilt = repo
                .write_tree(TreeEditor::from_tree(tree))
                .expect("write_tree");
            assert_eq!(empty, rebuilt);
            EngineOutput::stdout(git_oid_line(rebuilt.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "4b825dc642cb6eb9a060e54bf8d69288fbee4904"]),
    );
}

#[test]
fn three_file_tree_matches_oracle() {
    EngineParityCase::new("write-tree-three-files").run(
        |fixture| {
            fixture.init_default();
            fixture.write_file("a.txt", b"a\n");
            fixture.write_file("m.txt", b"m\n");
            fixture.write_file("z.txt", b"z\n");
            fixture.commit_paths("initial", &["a.txt", "m.txt", "z.txt"]);
        },
        |fixture| {
            let repo = Repository::discover(fixture.path()).expect("discover");
            let a = repo.write_blob(b"a\n").expect("a");
            let m = repo.write_blob(b"m\n").expect("m");
            let z = repo.write_blob(b"z\n").expect("z");
            let mut builder = TreeEditor::new();
            builder.upsert("a.txt", EntryKind::Blob, a);
            builder.upsert("m.txt", EntryKind::Blob, m);
            builder.upsert("z.txt", EntryKind::Blob, z);
            let tree = repo.write_tree(builder).expect("write_tree");
            EngineOutput::stdout(git_oid_line(tree.to_hex()))
        },
        |fixture| fixture.oracle(&["rev-parse", "HEAD^{tree}"]),
    );
}
