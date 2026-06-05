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
    std::fs::create_dir_all(&tmp).unwrap();
    if run_git(&tmp, &["--version"]).is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    assert!(run_git(&tmp, &["init", "-q"]).is_some());

    std::fs::write(tmp.join("a.txt"), b"alpha\n").unwrap();
    std::fs::create_dir_all(tmp.join("dir/sub")).unwrap();
    std::fs::write(tmp.join("dir/b.txt"), b"bee\n").unwrap();
    std::fs::write(tmp.join("dir/sub/c.txt"), b"cee\n").unwrap();
    let run = tmp.join("run.sh");
    std::fs::write(&run, b"#!/bin/sh\necho hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.txt", tmp.join("link")).unwrap();
    }

    assert!(run_git(&tmp, &["add", "-A"]).is_some());
    let tree = run_git(&tmp, &["write-tree"]).unwrap().trim().to_string();

    // git's view of the staged tree: "<mode> <oid> <stage>\t<path>".
    let staged = run_git(&tmp, &["ls-files", "--stage"]).unwrap();
    let mut expected: BTreeMap<Vec<u8>, (String, String)> = BTreeMap::new();
    for line in staged.lines() {
        let (meta, path) = line.split_once('\t').unwrap();
        let mut fields = meta.split_whitespace();
        let mode = fields.next().unwrap().to_string();
        let oid = fields.next().unwrap().to_string();
        assert_eq!(fields.next().unwrap(), "0", "expected stage 0 entries");
        expected.insert(path.as_bytes().to_vec(), (mode, oid));
    }
    assert!(expected.len() >= 4);

    let repo = Repository::discover(&tmp).unwrap();
    let tree_oid: ObjectId = tree.parse().unwrap();
    let index = repo.index_from_tree(&tree_oid).unwrap();

    let mut got: BTreeMap<Vec<u8>, (String, String)> = BTreeMap::new();
    for entry in &index.entries {
        assert_eq!(entry.stage(), IndexStage::Normal);
        // git read-tree leaves the stat zeroed.
        assert_eq!(entry.size, 0);
        assert_eq!(entry.mtime_seconds, 0);
        got.insert(
            entry.path.clone(),
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
