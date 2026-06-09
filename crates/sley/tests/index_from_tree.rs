//! `Repository::index_from_tree` must reproduce the entries `git read-tree`
//! produces for the same tree: same mode/oid/path at stage 0, with a zeroed
//! stat, sorted by path.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use sley::{IndexStage, ObjectId, Repository};

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[test]
fn index_from_tree_matches_git_read_tree() {
    let tmp = std::env::temp_dir().join(format!("sley-idx-from-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("test operation should succeed");
    if run_git(&tmp, &["--version"]).is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    assert!(run_git(&tmp, &["init", "-q"]).is_some());

    std::fs::write(tmp.join("a.txt"), b"alpha\n").expect("test operation should succeed");
    std::fs::create_dir_all(tmp.join("dir/sub")).expect("test operation should succeed");
    std::fs::write(tmp.join("dir/b.txt"), b"bee\n").expect("test operation should succeed");
    std::fs::write(tmp.join("dir/sub/c.txt"), b"cee\n").expect("test operation should succeed");
    let run = tmp.join("run.sh");
    std::fs::write(&run, b"#!/bin/sh\necho hi\n").expect("test operation should succeed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755))
            .expect("test operation should succeed");
        std::os::unix::fs::symlink("a.txt", tmp.join("link"))
            .expect("test operation should succeed");
    }

    assert!(run_git(&tmp, &["add", "-A"]).is_some());
    let tree = run_git(&tmp, &["write-tree"])
        .expect("test operation should succeed")
        .trim()
        .to_string();

    // git's view of the staged tree: "<mode> <oid> <stage>\t<path>".
    let staged = run_git(&tmp, &["ls-files", "--stage"]).expect("test operation should succeed");
    let mut expected: BTreeMap<Vec<u8>, (String, String)> = BTreeMap::new();
    for line in staged.lines() {
        let (meta, path) = line
            .split_once('\t')
            .expect("test operation should succeed");
        let mut fields = meta.split_whitespace();
        let mode = fields
            .next()
            .expect("test operation should succeed")
            .to_string();
        let oid = fields
            .next()
            .expect("test operation should succeed")
            .to_string();
        assert_eq!(
            fields.next().expect("test operation should succeed"),
            "0",
            "expected stage 0 entries"
        );
        expected.insert(path.as_bytes().to_vec(), (mode, oid));
    }
    assert!(expected.len() >= 4);

    let repo = Repository::discover(&tmp).expect("test operation should succeed");
    let tree_oid: ObjectId = tree.parse().expect("test operation should succeed");
    let index = repo
        .index_from_tree(&tree_oid)
        .expect("test operation should succeed");

    let mut got: BTreeMap<Vec<u8>, (String, String)> = BTreeMap::new();
    for entry in &index.entries {
        assert_eq!(entry.stage(), IndexStage::Normal);
        // git read-tree leaves the stat zeroed.
        assert_eq!(entry.size, 0);
        assert_eq!(entry.mtime_seconds, 0);
        got.insert(
            entry.path.as_bytes().to_vec(),
            (format!("{:o}", entry.mode), entry.oid.to_string()),
        );
    }
    assert_eq!(
        got, expected,
        "index_from_tree entries must match git's staged tree"
    );

    // Entries must be in git's index order (path bytes ascending).
    let mut sorted = index.entries.clone();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(index.entries, sorted);

    let _ = std::fs::remove_dir_all(&tmp);
}
