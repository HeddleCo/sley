//! Byte-exactness parity for the tree editor: a tree assembled with
//! [`git::Repository::edit_tree`]/`write_tree` must hash to the *same* OID the
//! system `git` binary produces with `git write-tree`, including Git's
//! canonical directory-suffix entry ordering.

use std::path::Path;
use std::process::Command;

use git::{EntryKind, ObjectId, Repository};

/// Run `git <args>` in `cwd`, returning stdout on success (or `None` so callers
/// can skip gracefully when `git` is not installed).
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[test]
fn tree_builder_matches_git_write_tree() {
    let tmp = std::env::temp_dir().join(format!("git-rs-tree-editor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Skip cleanly if the system git binary is unavailable.
    if run_git(&tmp, &["--version"]).is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    assert!(run_git(&tmp, &["init", "-q"]).is_some());

    // A file set that exercises every entry kind plus the dir-suffix sort: a
    // plain blob, an executable, a subtree ("foo"), and a blob ("foo.txt") that
    // must sort *before* the subtree because '.' < '/'.
    std::fs::write(tmp.join("a.txt"), b"alpha\n").unwrap();
    std::fs::write(tmp.join("foo.txt"), b"foo file\n").unwrap();
    let run = tmp.join("run.sh");
    std::fs::write(&run, b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::create_dir_all(tmp.join("foo")).unwrap();
    std::fs::write(tmp.join("foo/bar.txt"), b"bar\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.txt", tmp.join("link")).unwrap();
    }

    assert!(run_git(&tmp, &["add", "-A"]).is_some());
    let expected = run_git(&tmp, &["write-tree"]).unwrap().trim().to_string();

    // Read git's own top-level tree entries: "<mode> <type> <oid>\t<name>".
    let listing = run_git(&tmp, &["ls-tree", &expected]).unwrap();
    let mut entries: Vec<(u32, ObjectId, Vec<u8>)> = Vec::new();
    for line in listing.lines() {
        let (meta, name) = line.split_once('\t').expect("ls-tree line has a tab");
        let mut fields = meta.split_whitespace();
        let mode = u32::from_str_radix(fields.next().unwrap(), 8).unwrap();
        let _type = fields.next().unwrap();
        let oid: ObjectId = fields.next().unwrap().parse().unwrap();
        entries.push((mode, oid, name.as_bytes().to_vec()));
    }
    assert!(entries.len() >= 3, "expected several top-level entries");

    let repo = Repository::discover(&tmp).unwrap();
    let mut builder = repo
        .edit_tree(&ObjectId::empty_tree(repo.object_format()))
        .unwrap();
    // Insert in reverse on purpose: the builder must re-derive Git's canonical
    // order regardless of insertion order.
    for (mode, oid, name) in entries.into_iter().rev() {
        match EntryKind::from_mode(mode) {
            Some(kind) => builder.upsert(name, kind, oid),
            None => builder.upsert_raw(name, mode, oid),
        }
    }
    let got = repo.write_tree(builder).unwrap();

    assert_eq!(
        got.to_string(),
        expected,
        "tree OID must be byte-identical to `git write-tree`"
    );
    // The object we wrote must parse back as a tree.
    assert!(!repo.read_tree(&got).unwrap().entries.is_empty());

    let _ = std::fs::remove_dir_all(&tmp);
}
