use sley_core::GitError;
use std::fs;

#[test]
fn merge_writer_refuses_configured_git_aliases() {
    for path in [b".git./hooks/x".as_slice(), b"GIT~1/hooks/x"] {
        let root = tempfile::tempdir().expect("worktree");
        fs::create_dir(root.path().join(".git")).expect("git directory");
        let result = sley_sequencer::apply::merge_write_worktree_file(
            None,
            root.path(),
            path,
            b"content",
            0o100644,
        );
        assert!(
            matches!(result, Err(GitError::InvalidPath(_))),
            "{result:?}"
        );
        assert_eq!(fs::read_dir(root.path()).expect("listing").count(), 1);
    }
}

#[cfg(unix)]
#[test]
fn merge_removal_does_not_follow_a_parent_symlink() {
    let root = tempfile::tempdir().expect("worktree");
    let outside = tempfile::tempdir().expect("outside directory");
    fs::write(outside.path().join("file"), b"outside").expect("outside file");
    std::os::unix::fs::symlink(outside.path(), root.path().join("dir")).expect("parent link");
    sley_sequencer::apply::merge_remove_worktree_file(None, root.path(), b"dir/file")
        .expect("remove absent entry");
    assert_eq!(
        fs::read(outside.path().join("file")).expect("outside file survives"),
        b"outside"
    );
}
