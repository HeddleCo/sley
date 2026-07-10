//! Git diff and merge engine.

mod blob_merge;
pub mod format;
mod line_diff;
mod merge_trees;
mod name;
mod name_status;
mod patch;
pub mod range;
pub mod render;
pub mod ws;

pub use sley_core::BString;

pub use blob_merge::*;
pub use format::*;
pub use line_diff::*;
pub use merge_trees::*;
pub use name_status::*;
pub use patch::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::name_status::{
        TREE_ENTRY_MODE, changed_tree_entries, collect_full_tree_pair, diff_name_status_maps,
        diff_name_status_maps_with_renames, read_blob_bytes,
    };
    use crate::patch::{parse_leading_usize, split_blob_lines};
    use sley_core::{ObjectFormat, ObjectId};
    use sley_formats::RepositoryLayout;
    use sley_index::Index;
    use sley_object::{EncodedObject, ObjectType, Tree, TreeEntry};
    use sley_odb::{FileObjectDatabase, ObjectWriter};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn name_status_reports_added_from_index() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec()))
            .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                size: 6,
                oid,
                flags: "hello.txt".len() as u16,
                flags_extended: 0,
                path: BString::from(b"hello.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let changes = diff_name_status_head_worktree(&root, &layout.git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(changes[0].line(), "A\thello.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn tree_worktree_diff_treats_absent_skip_worktree_entries_as_clean() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = write_blob(&mut db, b"clean\n");
        let tree = write_tree(
            &mut db,
            &[(b"removeme", 0o100644, oid), (b"untouched", 0o100644, oid)],
        );
        write_index(
            &layout.git_dir,
            vec![
                skip_worktree_entry(b"removeme", oid),
                skip_worktree_entry(b"untouched", oid),
            ],
        );

        let changes = diff_name_status_tree_worktree_with_options(
            &root,
            &layout.git_dir,
            ObjectFormat::Sha1,
            &tree,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");

        assert!(
            changes.is_empty(),
            "absent sparse entries should not appear as deletes: {changes:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn tree_worktree_diff_trusts_present_skip_worktree_entry_when_sparse_disabled() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = write_blob(&mut db, b"clean\n");
        let tree = write_tree(&mut db, &[(b"modified", 0o100644, oid)]);
        write_index(&layout.git_dir, vec![skip_worktree_entry(b"modified", oid)]);
        fs::write(root.join("modified"), b"dirty\n").expect("test operation should succeed");

        let changes = diff_name_status_tree_worktree_with_options(
            &root,
            &layout.git_dir,
            ObjectFormat::Sha1,
            &tree,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");

        assert!(
            changes.is_empty(),
            "present skip-worktree dirt should be ignored when sparse checkout is disabled: {changes:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn tree_worktree_diff_adds_index_blob_for_present_skip_worktree_entry() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = write_blob(&mut db, b"");
        let tree = write_tree(&mut db, &[]);
        write_index(&layout.git_dir, vec![skip_worktree_entry(b"added", oid)]);
        fs::write(root.join("added"), b"dirty\n").expect("test operation should succeed");

        let changes = diff_name_status_tree_worktree_with_options(
            &root,
            &layout.git_dir,
            ObjectFormat::Sha1,
            &tree,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].line(), "A\tadded");
        assert_eq!(changes[0].new_oid, Some(oid));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn index_worktree_diff_returns_staged_gitlinks() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: sley_index::GITLINK_MODE,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: "deps/sub".len() as u16,
                flags_extended: 0,
                path: BString::from(b"deps/sub"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        let diff = diff_name_status_index_worktree_with_options_and_gitlinks(
            &root,
            &layout.git_dir,
            ObjectFormat::Sha1,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");

        assert_eq!(diff.entries.len(), 1);
        let gitlinks = diff.staged_gitlinks;
        assert_eq!(gitlinks.len(), 1);
        assert_eq!(gitlinks[0].path.as_bytes(), b"deps/sub");
        assert_eq!(gitlinks[0].oid, oid);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn index_worktree_diff_ignores_untracked_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"clean\n".to_vec()))
            .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                size: 6,
                oid,
                flags: "tracked.txt".len() as u16,
                flags_extended: 0,
                path: BString::from(b"tracked.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        fs::write(root.join("tracked.txt"), b"clean\n").expect("test operation should succeed");
        symlink("missing-target", root.join("untracked-link"))
            .expect("test operation should succeed");

        let changes = diff_name_status_index_worktree_with_options(
            &root,
            &layout.git_dir,
            ObjectFormat::Sha1,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,

                ..Default::default()
            },
        )
        .expect("untracked dangling symlink should be ignored");
        assert!(changes.is_empty());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn index_worktree_diff_trusts_non_racy_stat_cache() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let worktree_path = root.join("tracked.txt");
        fs::write(&worktree_path, b"clean\n").expect("test operation should succeed");
        let metadata = fs::symlink_metadata(&worktree_path).expect("test operation should succeed");
        let (mtime_seconds, mtime_nanoseconds) =
            sley_index::file_mtime_parts(&metadata).expect("test operation should succeed");
        let bogus_oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let index = Index {
            version: 2,
            entries: vec![sley_index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: mtime_seconds as u32,
                mtime_nanoseconds: mtime_nanoseconds as u32,
                dev: 0,
                ino: 0,
                mode: sley_index::worktree_metadata_mode(&metadata),
                uid: 0,
                gid: 0,
                size: metadata.len() as u32,
                oid: bogus_oid,
                flags: "tracked.txt".len() as u16,
                flags_extended: 0,
                path: BString::from(b"tracked.txt"),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            layout.git_dir.join("index"),
            index
                .write_v2_sha1()
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        let changes = diff_name_status_index_worktree(&root, &layout.git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            changes.is_empty(),
            "a clean non-racy stat match must reuse the cached index oid"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-diff-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test operation should succeed");
        path
    }

    // ---- line diff / blob merge tests ---------------------------------------

    fn merge_opts() -> MergeBlobOptions<'static> {
        MergeBlobOptions {
            ours_label: "ours",
            theirs_label: "theirs",
            base_label: "base",
            style: ConflictStyle::Merge,
            favor: MergeFavor::None,
            ws_ignore: WsIgnore::EMPTY,
            marker_size: 7,
        }
    }

    #[test]
    fn split_lines_preserves_content_and_newlines() {
        let lines = split_lines(b"a\nb\nc\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content, b"a\n");
        assert!(lines[0].has_newline);
        assert_eq!(lines[2].content, b"c\n");
        assert!(lines[2].has_newline);
        assert!(split_lines(b"").is_empty());
    }

    #[test]
    fn split_lines_tracks_missing_final_newline() {
        let lines = split_lines(b"a\nb");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].has_newline);
        assert!(!lines[1].has_newline);
        assert_eq!(lines[1].content, b"b");
        assert_eq!(lines[1].bytes_without_newline(), b"b");
        // A line that lost its newline must not compare equal to one that has it.
        let with_nl = split_lines(b"b\n");
        assert_ne!(lines[1], with_nl[0]);
    }

    #[test]
    fn myers_replace_single_line() {
        let old = split_lines(b"a\nb\nc\n");
        let new = split_lines(b"a\nx\nc\n");
        assert_eq!(
            myers_diff_lines(&old, &new),
            vec![
                DiffOp::Equal(1),
                DiffOp::Delete(1),
                DiffOp::Insert(1),
                DiffOp::Equal(1),
            ]
        );
    }

    #[test]
    fn myers_identical_is_single_equal() {
        let old = split_lines(b"a\nb\nc\n");
        let new = split_lines(b"a\nb\nc\n");
        assert_eq!(myers_diff_lines(&old, &new), vec![DiffOp::Equal(3)]);
    }

    #[test]
    fn myers_pure_insert_and_delete() {
        let empty = split_lines(b"");
        let two = split_lines(b"a\nb\n");
        assert_eq!(myers_diff_lines(&empty, &two), vec![DiffOp::Insert(2)]);
        assert_eq!(myers_diff_lines(&two, &empty), vec![DiffOp::Delete(2)]);

        let old = split_lines(b"a\nb\nc\nd\n");
        let new = split_lines(b"a\nc\nd\n");
        assert_eq!(
            myers_diff_lines(&old, &new),
            vec![DiffOp::Equal(1), DiffOp::Delete(1), DiffOp::Equal(2)]
        );
    }

    #[test]
    fn myers_reconstructs_new_and_is_minimal() {
        // Apply the script to `old` and confirm it yields `new`; also count edits.
        let old = split_lines(b"the\nquick\nbrown\nfox\n");
        let new = split_lines(b"the\nlazy\nbrown\ncat\n");
        let ops = myers_diff_lines(&old, &new);
        let mut oi = 0usize;
        let mut ni = 0usize;
        let mut edits = 0usize;
        let mut rebuilt: Vec<u8> = Vec::new();
        for op in &ops {
            match *op {
                DiffOp::Equal(n) => {
                    for _ in 0..n {
                        assert_eq!(old[oi], new[ni]);
                        rebuilt.extend_from_slice(old[oi].content);
                        oi += 1;
                        ni += 1;
                    }
                }
                DiffOp::Delete(n) => {
                    oi += n;
                    edits += n;
                }
                DiffOp::Insert(n) => {
                    for _ in 0..n {
                        rebuilt.extend_from_slice(new[ni].content);
                        ni += 1;
                    }
                    edits += n;
                }
            }
        }
        assert_eq!(rebuilt, b"the\nlazy\nbrown\ncat\n");
        // Two lines changed -> 2 deletes + 2 inserts is the minimal SES here.
        assert_eq!(edits, 4);
    }

    #[test]
    fn merge_non_overlapping_changes_is_clean() {
        let base = b"a\nb\nc\nd\ne\n";
        let ours = b"A\nb\nc\nd\ne\n";
        let theirs = b"a\nb\nc\nd\nE\n";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"A\nb\nc\nd\nE\n");
    }

    #[test]
    fn merge_identical_changes_no_conflict() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nX\nc\n";
        let theirs = b"a\nX\nc\n";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nX\nc\n");
    }

    #[test]
    fn merge_overlapping_change_emits_exact_markers() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nOURS\nc\n";
        let theirs = b"a\nTHEIRS\nc\n";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\nOURS\n=======\nTHEIRS\n>>>>>>> theirs\nc\n".to_vec(),
        );
    }

    #[test]
    fn merge_diff3_style_includes_base_section() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nOURS\nc\n";
        let theirs = b"a\nTHEIRS\nc\n";
        let options = MergeBlobOptions {
            style: ConflictStyle::Diff3,
            ..merge_opts()
        };
        let result = merge_blobs(base, ours, theirs, &options);
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\nOURS\n||||||| base\nb\n=======\nTHEIRS\n>>>>>>> theirs\nc\n"
                .to_vec(),
        );
    }

    #[test]
    fn merge_zdiff3_hoists_shared_side_context() {
        let base = b"a\nold\nz\n";
        let ours = b"a\nshared\nOURS\ntail\nz\n";
        let theirs = b"a\nshared\nTHEIRS\ntail\nz\n";
        let options = MergeBlobOptions {
            style: ConflictStyle::ZDiff3,
            ..merge_opts()
        };
        let result = merge_blobs(base, ours, theirs, &options);
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"a\nshared\n<<<<<<< ours\nOURS\n||||||| base\nold\n=======\nTHEIRS\n>>>>>>> theirs\ntail\nz\n"
                .to_vec(),
        );
    }

    #[test]
    fn merge_empty_label_omits_trailing_space() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nOURS\nc\n";
        let theirs = b"a\nTHEIRS\nc\n";
        let options = MergeBlobOptions {
            ours_label: "",
            theirs_label: "",
            base_label: "",
            style: ConflictStyle::Merge,
            favor: MergeFavor::None,
            ws_ignore: WsIgnore::EMPTY,
            marker_size: 7,
        };
        let result = merge_blobs(base, ours, theirs, &options);
        assert!(result.conflicted);
        // No trailing space after the 7 marker chars when the label is empty.
        assert_eq!(
            result.content,
            b"a\n<<<<<<<\nOURS\n=======\nTHEIRS\n>>>>>>>\nc\n".to_vec(),
        );
    }

    #[test]
    fn merge_add_add_empty_base_conflicts() {
        let result = merge_blobs(b"", b"x\ny\n", b"p\nq\n", &merge_opts());
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"<<<<<<< ours\nx\ny\n=======\np\nq\n>>>>>>> theirs\n".to_vec(),
        );
    }

    #[test]
    fn merge_ignore_space_change_resolves_clean_keeping_ours() {
        // ours: only-whitespace change (collapsed run); theirs: real change.
        // Under -Xignore-space-change the whitespace-only line is not a conflict
        // and ours' actual bytes survive (xdl_merge copies common spans from
        // file1); theirs' real change to a different line wins on its own line.
        let base = b"alpha   beta\nsecond line\n";
        let ours = b"alpha beta\nsecond line\n"; // collapsed the run
        let theirs = b"alpha   beta\nsecond CHANGED\n"; // real change on line 2
        let options = MergeBlobOptions {
            ws_ignore: WsIgnore {
                space_change: true,
                ..WsIgnore::EMPTY
            },
            ..merge_opts()
        };
        let result = merge_blobs(base, ours, theirs, &options);
        assert!(
            !result.conflicted,
            "whitespace-only divergence is not a conflict"
        );
        assert_eq!(result.content, b"alpha beta\nsecond CHANGED\n".to_vec());
    }

    #[test]
    fn merge_ignore_space_change_still_conflicts_on_real_divergence() {
        // Both sides make a real (non-whitespace) change to the same line: still
        // a conflict even under -Xignore-space-change.
        let base = b"one\n";
        let ours = b"OURS\n";
        let theirs = b"THEIRS\n";
        let options = MergeBlobOptions {
            ws_ignore: WsIgnore {
                space_change: true,
                ..WsIgnore::EMPTY
            },
            ..merge_opts()
        };
        let result = merge_blobs(base, ours, theirs, &options);
        assert!(result.conflicted);
    }

    #[test]
    fn merge_add_add_empty_base_identical_is_clean() {
        let result = merge_blobs(b"", b"x\ny\n", b"x\ny\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"x\ny\n");
    }

    #[test]
    fn merge_deletion_one_side_takes_deletion() {
        // ours deletes line b; theirs leaves it -> clean, deletion wins.
        let result = merge_blobs(b"a\nb\nc\n", b"a\nc\n", b"a\nb\nc\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nc\n");
    }

    #[test]
    fn merge_deletion_vs_modification_conflicts() {
        // ours deletes b; theirs modifies b -> conflict.
        let result = merge_blobs(b"a\nb\nc\n", b"a\nc\n", b"a\nB!\nc\n", &merge_opts());
        assert!(result.conflicted);
        // ours side of the conflict is empty (the line was deleted).
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\n=======\nB!\n>>>>>>> theirs\nc\n".to_vec(),
        );
    }

    #[test]
    fn merge_missing_final_newline_marker_starts_on_own_line() {
        // Both sides drop the trailing newline AND conflict at the end. The
        // closing marker section must still begin on its own line.
        let base = b"a\nb";
        let ours = b"a\nOURS";
        let theirs = b"a\nTHEIRS";
        let result = merge_blobs(base, ours, theirs, &merge_opts());
        assert!(result.conflicted);
        assert_eq!(
            result.content,
            b"a\n<<<<<<< ours\nOURS\n=======\nTHEIRS\n>>>>>>> theirs\n".to_vec(),
        );
    }

    #[test]
    fn merge_clean_preserves_missing_final_newline() {
        // ours removes the trailing newline; theirs is unchanged -> ours wins,
        // and the result keeps the missing newline.
        let result = merge_blobs(b"a\nb\n", b"a\nb", b"a\nb\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nb");
    }

    #[test]
    fn merge_both_append_identical_tail_is_clean() {
        let result = merge_blobs(b"a\n", b"a\nz\n", b"a\nz\n", &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, b"a\nz\n");
    }

    #[test]
    fn merge_when_ours_equals_base_yields_theirs() {
        // Regression: a side that did not change must not suppress the other
        // side's edits anywhere in the file.
        let base = b"b\na\n";
        let theirs = b"b\nb\nc\na\nc\n";
        let result = merge_blobs(base, base, theirs, &merge_opts());
        assert!(!result.conflicted);
        assert_eq!(result.content, theirs.to_vec());
    }
    fn applied(outcome: ApplyOutcome) -> Vec<u8> {
        match outcome {
            ApplyOutcome::Applied(bytes) => bytes,
            ApplyOutcome::Rejected => panic!("expected Applied, got Rejected"),
        }
    }

    #[test]
    fn parse_multi_file_patch() {
        let patch = b"\
diff --git a/one.txt b/one.txt
index aaaaaaa..bbbbbbb 100644
--- a/one.txt
+++ b/one.txt
@@ -1,3 +1,3 @@
 alpha
-beta
+BETA
 gamma
diff --git a/two.txt b/two.txt
index ccccccc..ddddddd 100644
--- a/two.txt
+++ b/two.txt
@@ -1,2 +1,3 @@
 first
+inserted
 second
";
        let patches = parse_unified_patch(patch).expect("test operation should succeed");
        assert_eq!(patches.len(), 2);

        assert_eq!(patches[0].old_path.as_deref(), Some(b"one.txt".as_slice()));
        assert_eq!(patches[0].new_path.as_deref(), Some(b"one.txt".as_slice()));
        // The `index <a>..<b> 100644` line carries the unchanged-file mode, which
        // git's gitdiff_index records as old_mode.
        assert_eq!(patches[0].old_mode, Some(0o100644));
        assert_eq!(
            patches[0].old_oid_hex.as_deref(),
            Some(b"aaaaaaa".as_slice())
        );
        assert_eq!(
            patches[0].new_oid_hex.as_deref(),
            Some(b"bbbbbbb".as_slice())
        );
        assert_eq!(patches[0].hunks.len(), 1);
        let h = &patches[0].hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 3, 1, 3)
        );
        assert_eq!(
            h.lines,
            vec![
                HunkLine::Context(b"alpha".to_vec()),
                HunkLine::Delete(b"beta".to_vec()),
                HunkLine::Insert(b"BETA".to_vec()),
                HunkLine::Context(b"gamma".to_vec()),
            ]
        );

        assert_eq!(patches[1].new_path.as_deref(), Some(b"two.txt".as_slice()));
        assert_eq!(patches[1].hunks[0].new_len, 3);
    }

    #[test]
    fn parse_default_hunk_range_length() {
        // `@@ -1 +1,2 @@` (no comma) means a length of 1 on the old side.
        let patch = b"\
--- a/x
+++ b/x
@@ -1 +1,2 @@
 line
+added
";
        let patches = parse_unified_patch(patch).expect("test operation should succeed");
        let h = &patches[0].hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 1, 1, 2)
        );
    }

    #[test]
    fn parse_hunk_header_before_file_errors() {
        let patch = b"@@ -1,1 +1,1 @@\n context\n";
        assert!(parse_unified_patch(patch).is_err());
    }

    #[test]
    fn parse_mismatched_counts_errors() {
        // Header promises two old lines but only one is present.
        let patch = b"--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n only\n+new\n";
        assert!(parse_unified_patch(patch).is_err());
    }

    #[test]
    fn apply_clean_hunk() {
        let base = b"alpha\nbeta\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"alpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_with_line_offset() {
        // The hunk's recorded position (line 2) is a couple of lines above where
        // the matching context actually lives (line 4); the outward search must
        // find it. The hunk is NOT anchored at the file start (old_start > 1, so
        // no match_beginning) and has trailing context (`tail`, so no
        // match_end), which is exactly the shape a real drifted patch takes —
        // verified against `git apply` ("Hunk #1 succeeded at 4 (offset 2)").
        let base = b"pre1\npre2\npre3\nalpha\nbeta\ngamma\ntail\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -2,4 +2,4 @@\n alpha\n-beta\n+BETA\n gamma\n tail\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"pre1\npre2\npre3\nalpha\nBETA\ngamma\ntail\n");
    }

    #[test]
    fn apply_with_negative_line_offset() {
        // Recorded position is well past the real location; search backward.
        let base = b"alpha\nbeta\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -50,3 +50,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"alpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_multiple_hunks() {
        let base = b"a\nb\nc\nd\ne\nf\ng\nh\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n\
@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n\
@@ -6,3 +6,3 @@\n f\n-g\n+G\n h\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"a\nB\nc\nd\ne\nf\nG\nh\n");
    }

    #[test]
    fn reject_on_context_mismatch() {
        let base = b"alpha\nDIFFERENT\ngamma\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn reject_when_match_end_required_but_not_at_eof() {
        // git's `apply.c`: a hunk with NO trailing context must match the END of
        // the file (`match_end`). Here the leading context (`tail`/`anchor`)
        // matches at the middle of the base, but there are further lines after
        // it, so the preimage does not reach EOF. git rejects this; the old
        // sley matcher wrongly applied it (duplicating the appended block). This
        // is the t4150-am cell-34 lever: rejection forces `am -3`'s 3-way path.
        let base = b"one\ntwo\nanchor\nalready\nappended\n";
        // Hunk: context `anchor`, then append `added1`/`added2`. No trailing
        // context => match_end. At line 3 (`anchor`) the preimage is just one
        // line and does not reach EOF, so it must be rejected.
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -3,1 +3,3 @@\n anchor\n+added1\n+added2\n")
                .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn append_at_eof_matches_when_context_reaches_end() {
        // The mirror of the rejection case: the same shape applies cleanly when
        // the matching context IS the last line of the file (preimage reaches
        // EOF), so `match_end` is satisfied.
        let base = b"one\ntwo\nanchor\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -3,1 +3,3 @@\n anchor\n+added1\n+added2\n")
                .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"one\ntwo\nanchor\nadded1\nadded2\n");
    }

    #[test]
    fn reject_when_match_beginning_required_but_not_at_start() {
        // A hunk anchored at line 1 (`old_start <= 1`) must match the START of
        // the file (`match_beginning`). If the matching context only appears
        // later, git rejects rather than wandering to it.
        let base = b"junk\nalpha\nbeta\ngamma\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n alpha\n+INSERT\n beta\n")
                .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn no_default_fuzz_rejects_on_trailing_context_mismatch() {
        // `git apply` / `git am` keep `p_context = UINT_MAX` by default, so they
        // do NOT fuzz a hunk in by dropping context. Here the trailing context
        // line (`gamma`) differs from the base (`DIVERGED`), and because the
        // anchor is line 1 the hunk must match the beginning with its FULL
        // preimage. Verified against real `git apply`: this is rejected.
        let base = b"alpha\nbeta\nDIVERGED\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
        )
        .expect("test operation should succeed");
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn parse_and_apply_new_file() {
        let patch = parse_unified_patch(
            b"\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
",
        )
        .expect("test operation should succeed");
        assert!(patches_first_is_new(&patch));
        assert_eq!(patch[0].old_path, None);
        assert_eq!(patch[0].new_path.as_deref(), Some(b"new.txt".as_slice()));
        assert_eq!(patch[0].new_mode, Some(0o100644));
        // Base is ignored for a new file.
        let out = applied(apply_file_patch(b"garbage that is ignored", &patch[0]));
        assert_eq!(out, b"hello\nworld\n");
    }

    fn patches_first_is_new(patches: &[FilePatch]) -> bool {
        patches.first().map(|p| p.is_new).unwrap_or(false)
    }

    #[test]
    fn parse_and_apply_delete_file() {
        let patch = parse_unified_patch(
            b"\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 1111111..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-hello
-world
",
        )
        .expect("test operation should succeed");
        assert!(patch[0].is_delete);
        assert_eq!(patch[0].old_path.as_deref(), Some(b"gone.txt".as_slice()));
        assert_eq!(patch[0].new_path, None);
        assert_eq!(patch[0].old_mode, Some(0o100644));
        let out = applied(apply_file_patch(b"hello\nworld\n", &patch[0]));
        assert_eq!(out, b"");
    }

    #[test]
    fn parse_rename_headers() {
        let patch = parse_unified_patch(
            b"\
diff --git a/old/name.txt b/new/name.txt
similarity index 100%
rename from old/name.txt
rename to new/name.txt
",
        )
        .expect("test operation should succeed");
        assert!(patch[0].is_rename);
        assert_eq!(
            patch[0].old_path.as_deref(),
            Some(b"old/name.txt".as_slice())
        );
        assert_eq!(
            patch[0].new_path.as_deref(),
            Some(b"new/name.txt".as_slice())
        );
        assert!(patch[0].hunks.is_empty());
    }

    #[test]
    fn parse_mode_change_headers() {
        let patch = parse_unified_patch(
            b"\
diff --git a/script.sh b/script.sh
old mode 100644
new mode 100755
",
        )
        .expect("test operation should succeed");
        assert_eq!(patch[0].old_mode, Some(0o100644));
        assert_eq!(patch[0].new_mode, Some(0o100755));
        assert!(!patch[0].is_new);
        assert!(!patch[0].is_delete);
    }

    #[test]
    fn no_final_newline_base_preserved_when_untouched() {
        // The change is on line 1; the final line has no newline and is not
        // modified, so its no-newline state must survive. This uses the patch
        // shape real `git diff` emits for such a change — `@@ -1,3 +1,3 @@` with
        // the two unchanged lines as trailing context (the `\ No newline`
        // marker rides the last context line). A hand-rolled `@@ -1,1 +1,1 @@`
        // with NO trailing context would (correctly) be rejected by git, since
        // a no-trailing-context hunk anchored at line 1 must span the whole
        // file (`match_beginning` && `match_end`).
        let base = b"alpha\nbeta\nnotail"; // "notail" has no trailing \n
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n-alpha\n+ALPHA\n beta\n notail\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"ALPHA\nbeta\nnotail");
    }

    #[test]
    fn no_final_newline_added_by_patch() {
        // Old file ends with a newline; patch rewrites the last line to one
        // without a trailing newline.
        let base = b"alpha\nbeta\n";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -2,1 +2,1 @@\n-beta\n+beta-notail\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].hunks[0].new_no_newline);
        assert!(!patch[0].hunks[0].old_no_newline);
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"alpha\nbeta-notail");
    }

    #[test]
    fn no_final_newline_in_base_matched_and_kept() {
        // Both sides lack a trailing newline; context match must require the
        // base's final line to itself be newline-free.
        let base = b"alpha\nbeta"; // no trailing newline
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-alpha\n+ALPHA\n beta\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].hunks[0].old_no_newline);
        assert!(patch[0].hunks[0].new_no_newline);
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"ALPHA\nbeta");
    }

    #[test]
    fn no_final_newline_mismatch_rejected() {
        // Patch asserts the old file has no trailing newline, but the base does.
        // That must be rejected rather than silently mis-applied.
        let base = b"alpha\nbeta\n"; // HAS trailing newline
        let patch = parse_unified_patch(
            b"--- a/x\n+++ b/x\n@@ -2,1 +2,1 @@\n-beta\n\\ No newline at end of file\n+beta2\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].hunks[0].old_no_newline);
        assert_eq!(apply_file_patch(base, &patch[0]), ApplyOutcome::Rejected);
    }

    #[test]
    fn delete_with_no_final_newline() {
        // Deleting the entire content of a file that had no trailing newline.
        let base = b"only line no newline";
        let patch = parse_unified_patch(
            b"--- a/x\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-only line no newline\n\\ No newline at end of file\n",
        )
        .expect("test operation should succeed");
        assert!(patch[0].is_delete);
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"");
    }

    #[test]
    fn apply_pure_insertion_hunk() {
        let base = b"first\nsecond\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n first\n+middle\n second\n")
                .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"first\nmiddle\nsecond\n");
    }

    #[test]
    fn apply_pure_deletion_hunk() {
        let base = b"first\nmiddle\nsecond\n";
        let patch =
            parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,3 +1,2 @@\n first\n-middle\n second\n")
                .expect("test operation should succeed");
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"first\nsecond\n");
    }

    #[test]
    fn apply_then_reparse_round_trip() {
        // Hand-written unified diff -> apply -> the result is exactly the new
        // file content the diff describes. Re-parsing the same patch yields an
        // identical structure (idempotent parse).
        let base = b"l1\nl2\nl3\nl4\nl5\n";
        let text = b"--- a/f\n+++ b/f\n@@ -2,3 +2,4 @@\n l2\n-l3\n+L3\n+L3b\n l4\n";
        let p1 = parse_unified_patch(text).expect("test operation should succeed");
        let p2 = parse_unified_patch(text).expect("test operation should succeed");
        assert_eq!(p1, p2);
        let out = applied(apply_file_patch(base, &p1[0]));
        assert_eq!(out, b"l1\nl2\nL3\nL3b\nl4\nl5\n");
    }

    #[test]
    fn empty_context_line_without_trailing_space() {
        // Some transports strip the single leading space from blank context
        // lines; the parser treats a wholly empty body line as blank context.
        let base = b"a\n\nb\n";
        let patch = parse_unified_patch(b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n")
            .expect("test operation should succeed");
        assert_eq!(patch[0].hunks[0].lines[1], HunkLine::Context(Vec::new()));
        let out = applied(apply_file_patch(base, &patch[0]));
        assert_eq!(out, b"a\n\nB\n");
    }

    #[test]
    fn split_blob_lines_handles_edge_cases() {
        assert!(split_blob_lines(b"").is_empty());
        let single = split_blob_lines(b"abc");
        assert_eq!(single.len(), 1);
        assert!(single[0].no_newline);
        let terminated = split_blob_lines(b"abc\n");
        assert_eq!(terminated.len(), 1);
        assert!(!terminated[0].no_newline);
        let blank_then_eof = split_blob_lines(b"x\n");
        assert_eq!(blank_then_eof.len(), 1);
    }

    // ---- content similarity & inexact rename/copy detection -----------------

    #[test]
    fn similarity_identical_and_empty_conventions() {
        // Byte-identical blobs are always 100% similar.
        assert_eq!(blob_similarity(b"hello\nworld\n", b"hello\nworld\n"), 100);
        // Two empty blobs are identical -> 100.
        assert_eq!(blob_similarity(b"", b""), 100);
        // An empty blob vs a non-empty one shares nothing -> 0.
        assert_eq!(blob_similarity(b"", b"hello\n"), 0);
        assert_eq!(blob_similarity(b"hello\n", b""), 0);
    }

    #[test]
    fn similarity_one_changed_line_is_75_and_symmetric() {
        // A = one/two/three/four/five (bytes: 4+4+6+5+5 = 24).
        // B changes "three\n" -> "THREE\n" (same total size 24).
        // Common spans: one,two,four,five = 4+4+5+5 = 18 bytes.
        // score = round(18 * 100 / max(24, 24)) = round(75) = 75.
        // Verified against `git diff -M` which reports "similarity index 75%".
        let a = b"one\ntwo\nthree\nfour\nfive\n";
        let b = b"one\ntwo\nTHREE\nfour\nfive\n";
        assert_eq!(blob_similarity(a, b), 75);
        // The metric is symmetric.
        assert_eq!(blob_similarity(b, a), 75);
    }

    #[test]
    fn similarity_one_edited_line_of_three_is_66_not_67() {
        // "a\nb\nc\n" -> "a\nB\nc\n": one of three lines edited (4 common bytes of
        // 6). git reports `R066` / "similarity index 66%". git's two-step integer
        // math is `4 * 60000 / 6 = 40000`, then `40000 * 100 / 60000 = 66` (both
        // truncated); a single rounded `4 * 100 / 6` would give 67. This pins the
        // MAX_SCORE-based rounding so it stays aligned with diffcore-rename.
        assert_eq!(blob_similarity(b"a\nb\nc\n", b"a\nB\nc\n"), 66);
        assert_eq!(blob_similarity(b"a\nB\nc\n", b"a\nb\nc\n"), 66);
    }

    #[test]
    fn similarity_small_append_is_88() {
        // A: 8 lines totalling 46 bytes. B: same 8 lines + "ADDED\n" (6 bytes) = 52.
        // Common = the 46 original bytes; score = round(46*100/52) = 88.
        // Verified against `git diff -M` -> "similarity index 88%".
        let a = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n";
        let b = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\nADDED\n";
        assert_eq!(blob_similarity(a, b), 88);
    }

    #[test]
    fn similarity_half_rewrite_is_50() {
        // 6 lines, last 3 rewritten. Common = l1,l2,l3 = 9 bytes; total each 18.
        // score = round(9*100/18) = 50. Verified against `git diff -M`.
        let a = b"l1\nl2\nl3\nl4\nl5\nl6\n";
        let b = b"l1\nl2\nl3\nX4\nX5\nX6\n";
        assert_eq!(blob_similarity(a, b), 50);
    }

    // ---- tree-diff based inexact detection ----------------------------------

    /// Write a blob and return its oid.
    fn write_blob(db: &mut FileObjectDatabase, bytes: &[u8]) -> ObjectId {
        db.write_object(EncodedObject::new(ObjectType::Blob, bytes.to_vec()))
            .expect("test operation should succeed")
    }

    /// Write a tree from `(name, mode, oid)` entries (sorted by name as git
    /// requires) and return its oid.
    fn write_tree(db: &mut FileObjectDatabase, entries: &[(&[u8], u32, ObjectId)]) -> ObjectId {
        let mut tree_entries: Vec<TreeEntry> = entries
            .iter()
            .map(|(name, mode, oid)| TreeEntry {
                mode: *mode,
                name: BString::from(*name),
                oid: *oid,
            })
            .collect();
        tree_entries.sort_by(|a, b| a.name.cmp(&b.name));
        let tree = Tree {
            entries: tree_entries,
        };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("test operation should succeed")
    }

    fn write_tree_in_order(
        db: &mut FileObjectDatabase,
        entries: &[(&[u8], u32, ObjectId)],
    ) -> ObjectId {
        let tree_entries: Vec<TreeEntry> = entries
            .iter()
            .map(|(name, mode, oid)| TreeEntry {
                mode: *mode,
                name: BString::from(*name),
                oid: *oid,
            })
            .collect();
        let tree = Tree {
            entries: tree_entries,
        };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("test operation should succeed")
    }

    fn skip_worktree_entry(path: &[u8], oid: ObjectId) -> sley_index::IndexEntry {
        let mut entry = sley_index::IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: path.len() as u16,
            flags_extended: 0,
            path: BString::from(path),
        };
        entry.set_skip_worktree(true);
        entry
    }

    fn write_index(git_dir: &Path, mut entries: Vec<sley_index::IndexEntry>) {
        entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
        let index = Index {
            version: 3,
            entries,
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            git_dir.join("index"),
            index.write_sha1().expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
    }

    #[test]
    fn inexact_rename_detected_with_plausible_score() {
        // a.txt (one changed line vs the new b.txt) should be detected as a
        // rename with score 75 (see `similarity_one_changed_line_is_75`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(&mut db, b"one\ntwo\nthree\nfour\nfive\n");
        let new = write_blob(&mut db, b"one\ntwo\nTHREE\nfour\nfive\n");
        let left = write_tree(&mut db, &[(b"a.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"b.txt", 0o100644, new)]);

        let opts = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
        };
        let diff = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");
        let entries = diff.entries;

        assert_eq!(
            entries.len(),
            1,
            "expected a single rename entry: {entries:?}"
        );
        assert_eq!(entries[0].status, NameStatus::Renamed(75));
        assert_eq!(
            entries[0].old_path.as_ref().map(|p| p.as_bytes()),
            Some(b"a.txt".as_slice())
        );
        assert_eq!(entries[0].path, b"b.txt");
        assert_eq!(entries[0].line(), "R075\ta.txt\tb.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn tree_diff_preserves_duplicate_entry_multiplicity() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let blob_one = write_blob(&mut db, b"one\n");
        let blob_two = write_blob(&mut db, b"two\n");
        let inner_one_a = write_tree_in_order(&mut db, &[(b"inner", 0o100644, blob_one)]);
        let inner_one_b = write_tree_in_order(
            &mut db,
            &[
                (b"inner", 0o100644, blob_two),
                (b"inner", 0o100644, blob_two),
                (b"inner", 0o100644, blob_two),
            ],
        );
        let outer_one = write_tree_in_order(
            &mut db,
            &[
                (b"outer", TREE_ENTRY_MODE, inner_one_a),
                (b"outer", TREE_ENTRY_MODE, inner_one_b),
            ],
        );
        let inner_two = write_tree_in_order(
            &mut db,
            &[
                (b"inner", 0o100644, blob_one),
                (b"inner", 0o100644, blob_two),
                (b"inner", 0o100644, blob_two),
                (b"inner", 0o100644, blob_two),
            ],
        );
        let outer_two = write_tree_in_order(&mut db, &[(b"outer", TREE_ENTRY_MODE, inner_two)]);
        let outer_three = write_tree_in_order(&mut db, &[(b"renamed", 0o100644, blob_one)]);

        let raw_options = DiffNameStatusOptions {
            detect_renames: false,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            ..Default::default()
        };
        let raw = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &outer_one,
            &outer_two,
            raw_options,
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&raw),
            vec![
                "A\touter/inner",
                "A\touter/inner",
                "A\touter/inner",
                "D\touter/inner",
                "D\touter/inner",
                "D\touter/inner",
            ]
        );

        let rename_options = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
        };
        let no_op_renames = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &outer_one,
            &outer_two,
            rename_options,
        )
        .expect("test operation should succeed");
        assert!(no_op_renames.entries.is_empty());

        let renamed = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &outer_one,
            &outer_three,
            rename_options,
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&renamed.entries),
            vec![
                "D\touter/inner",
                "D\touter/inner",
                "D\touter/inner",
                "R100\touter/inner\trenamed",
            ]
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_rename_below_threshold_not_detected() {
        // A half-rewrite scores 50%. With a 60% threshold it must NOT be paired;
        // the change shows up as a separate Add + Delete instead.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(&mut db, b"l1\nl2\nl3\nl4\nl5\nl6\n");
        let new = write_blob(&mut db, b"l1\nl2\nl3\nX4\nX5\nX6\n");
        let left = write_tree(&mut db, &[(b"a.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"b.txt", 0o100644, new)]);

        let opts = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: 60,
            copy_threshold: 60,
            rename_limit: 0,
        };
        let diff = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");
        let entries = diff.entries;

        let statuses: Vec<_> = entries.iter().map(|e| e.status).collect();
        assert!(
            statuses.contains(&NameStatus::Added) && statuses.contains(&NameStatus::Deleted),
            "expected separate add/delete below threshold, got {entries:?}"
        );
        assert!(
            !statuses.iter().any(|s| matches!(s, NameStatus::Renamed(_))),
            "no rename should be reported below threshold: {entries:?}"
        );

        // Sanity: lowering the threshold to 50 *does* detect it (boundary is
        // inclusive), and the score is exactly 50.
        let opts_low = DiffNameStatusOptions {
            rename_threshold: 50,
            ..opts
        };
        let entries_low = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts_low,
        )
        .expect("test operation should succeed");
        assert_eq!(entries_low.entries.len(), 1);
        assert_eq!(entries_low.entries[0].status, NameStatus::Renamed(50));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn exact_rename_scores_100_and_takes_priority() {
        // Identical content moved to a new path is an exact rename: score 100,
        // detected even with inexact disabled, and still 100 with it enabled.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let oid = write_blob(&mut db, b"identical\ncontent\nhere\n");
        let left = write_tree(&mut db, &[(b"old.txt", 0o100644, oid)]);
        let right = write_tree(&mut db, &[(b"new.txt", 0o100644, oid)]);

        for inexact in [false, true] {
            let opts = DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,
                detect_inexact: inexact,
                rename_threshold: DEFAULT_RENAME_THRESHOLD,
                copy_threshold: DEFAULT_RENAME_THRESHOLD,
                rename_limit: 0,
            };
            let diff = diff_name_status_trees_with_options_and_diagnostics(
                &db,
                ObjectFormat::Sha1,
                &left,
                &right,
                opts,
            )
            .expect("test operation should succeed");
            let entries = diff.entries;
            assert_eq!(entries.len(), 1, "inexact={inexact}: {entries:?}");
            assert_eq!(entries[0].status, NameStatus::Renamed(100));
            assert_eq!(
                entries[0].old_path.as_ref().map(|p| p.as_bytes()),
                Some(b"old.txt".as_slice())
            );
            assert_eq!(entries[0].path, b"new.txt");
        }
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_copy_detected_with_score() {
        // orig.txt is unchanged and a near-copy (one line differs, 80% similar)
        // is added. With copy detection + find_copies_harder + inexact, the new
        // file is reported as a copy with score 80 (matches `git diff -C
        // --find-copies-harder`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let orig = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\neee\n");
        let copy = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\nEEE\n");
        let left = write_tree(&mut db, &[(b"orig.txt", 0o100644, orig.clone())]);
        let right = write_tree(
            &mut db,
            &[(b"orig.txt", 0o100644, orig), (b"copy.txt", 0o100644, copy)],
        );

        let opts = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: true,
            find_copies_harder: true,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
        };
        let diff = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");
        let entries = diff.entries;

        let copy_entry = entries
            .iter()
            .find(|e| e.path == b"copy.txt")
            .unwrap_or_else(|| panic!("no copy.txt entry: {entries:?}"));
        assert_eq!(copy_entry.status, NameStatus::Copied(80));
        assert_eq!(
            copy_entry.old_path.as_ref().map(|p| p.as_bytes()),
            Some(b"orig.txt".as_slice())
        );
        // The source remains present (copies do not consume the original).
        assert!(
            entries.iter().all(|e| e.status != NameStatus::Deleted),
            "copy must not delete the source: {entries:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn copy_detection_keeps_deleted_source_for_copy_then_rename() {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        let contents = write_blob(&mut db, b"same contents\n");
        let left = write_tree(
            &mut db,
            &[
                // Same OID but a different file type: diffcore must not steal
                // a symlink destination for this regular-file source.
                (b"decoy", 0o100644, contents.clone()),
                (b"source", 0o120000, contents.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"destination-a", 0o120000, contents.clone()),
                (b"destination-z", 0o120000, contents),
            ],
        );
        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: true,
                rename_empty: true,
                ..Default::default()
            },
        )
        .expect("test operation should succeed");

        assert_eq!(
            status_lines(&entries),
            vec![
                "D\tdecoy",
                "C100\tsource\tdestination-a",
                "R100\tsource\tdestination-z",
            ]
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_copy_skipped_over_rename_limit() {
        // git's `too_many_rename_candidates`: when the copy matrix
        // (sources × dests) exceeds `rename_limit²`, inexact copy detection is
        // skipped wholesale and the new file is reported as a plain Add — the
        // same `A` real git emits (`git diff -C --find-copies-harder -l1` warns
        // "rename detection was skipped" and shows `A copy.txt`). A `rename_limit`
        // comfortably above the matrix still detects the copy, proving the gate
        // fires *only* over-limit and not on any positive limit.
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let orig = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\neee\n");
        let extra = write_blob(&mut db, b"111\n222\n333\n444\n555\n");
        let copy = write_blob(&mut db, b"aaa\nbbb\nccc\nddd\nEEE\n");
        // Two unchanged left files → under `--find-copies-harder` both are copy
        // sources, so the matrix is 2 (sources) × 1 (dest) = 2.
        let left = write_tree(
            &mut db,
            &[
                (b"orig.txt", 0o100644, orig.clone()),
                (b"extra.txt", 0o100644, extra.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"orig.txt", 0o100644, orig),
                (b"extra.txt", 0o100644, extra),
                (b"copy.txt", 0o100644, copy),
            ],
        );

        let opts_for = |rename_limit| DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: true,
            find_copies_harder: true,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit,
        };

        // Over limit: 2 × 1 = 2 > 1² ⇒ copy detection skipped, copy.txt is Added.
        let over = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts_for(1),
        )
        .expect("test operation should succeed");
        let copy_over = over
            .entries
            .iter()
            .find(|e| e.path == b"copy.txt")
            .unwrap_or_else(|| panic!("no copy.txt entry: {over:?}"));
        assert_eq!(
            copy_over.status,
            NameStatus::Added,
            "over rename_limit, copy must degrade to a plain Add: {over:?}"
        );

        // Under limit: 2 × 1 = 2 ≤ 4² ⇒ copy still detected (score 80).
        let under = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts_for(4),
        )
        .expect("test operation should succeed");
        let copy_under = under
            .entries
            .iter()
            .find(|e| e.path == b"copy.txt")
            .unwrap_or_else(|| panic!("no copy.txt entry: {under:?}"));
        assert_eq!(
            copy_under.status,
            NameStatus::Copied(80),
            "below rename_limit, copy detection is unaffected: {under:?}"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_rename_with_small_edit_scores_88() {
        // A rename that also appends a single line scores 88% (see
        // `similarity_small_append_is_88`).
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(
            &mut db,
            b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n",
        );
        let new = write_blob(
            &mut db,
            b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\nADDED\n",
        );
        let left = write_tree(&mut db, &[(b"src.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"dst.txt", 0o100644, new)]);

        let opts = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            ..Default::default()
        }
        .inexact();
        let diff = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            opts,
        )
        .expect("test operation should succeed");
        let entries = diff.entries;

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].status, NameStatus::Renamed(88));
        assert_eq!(
            entries[0].old_path.as_ref().map(|p| p.as_bytes()),
            Some(b"src.txt".as_slice())
        );
        assert_eq!(entries[0].path, b"dst.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn inexact_disabled_default_preserves_exact_only_behavior() {
        // With DiffNameStatusOptions::default() (detect_inexact == false), a
        // similar-but-not-identical pair is NOT a rename — identical to the
        // legacy exact-only path. Defaults must not silently turn on inexact.
        assert!(!DiffNameStatusOptions::default().detect_inexact);
        assert_eq!(
            DiffNameStatusOptions::default().rename_threshold,
            DEFAULT_RENAME_THRESHOLD
        );

        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let mut db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);

        let old = write_blob(&mut db, b"one\ntwo\nthree\nfour\nfive\n");
        let new = write_blob(&mut db, b"one\ntwo\nTHREE\nfour\nfive\n");
        let left = write_tree(&mut db, &[(b"a.txt", 0o100644, old)]);
        let right = write_tree(&mut db, &[(b"b.txt", 0o100644, new)]);

        let diff = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");
        let entries = diff.entries;
        let statuses: Vec<_> = entries.iter().map(|e| e.status).collect();
        assert!(statuses.contains(&NameStatus::Added));
        assert!(statuses.contains(&NameStatus::Deleted));
        assert!(!statuses.iter().any(|s| matches!(s, NameStatus::Renamed(_))));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    // ---- patience / histogram diff tests ------------------------------------

    /// Apply an edit script to `old` and return the reconstructed `new` bytes.
    ///
    /// Panics (test-only) if the script ever references a line out of range or
    /// claims a line is `Equal` when the corresponding `old`/`new` lines differ
    /// — that is exactly the invariant a correct LCS diff must uphold.
    fn apply_ops(old: &[DiffLine<'_>], new: &[DiffLine<'_>], ops: &[DiffOp]) -> Vec<u8> {
        let mut oi = 0usize;
        let mut ni = 0usize;
        let mut rebuilt: Vec<u8> = Vec::new();
        for op in ops {
            match *op {
                DiffOp::Equal(n) => {
                    for _ in 0..n {
                        // Equal must mean genuinely-equal lines (LCS-correct).
                        assert_eq!(old[oi], new[ni], "Equal op covered unequal lines");
                        rebuilt.extend_from_slice(old[oi].content);
                        oi += 1;
                        ni += 1;
                    }
                }
                DiffOp::Delete(n) => oi += n,
                DiffOp::Insert(n) => {
                    for _ in 0..n {
                        rebuilt.extend_from_slice(new[ni].content);
                        ni += 1;
                    }
                }
            }
        }
        // The script must consume every line of both sides exactly once.
        assert_eq!(oi, old.len(), "script did not consume all of old");
        assert_eq!(ni, new.len(), "script did not consume all of new");
        rebuilt
    }

    /// Assert that `ops` is a valid LCS-correct script: it reconstructs `new`
    /// from `old`, and consecutive ops are coalesced (no two same-kind in a row).
    fn assert_valid_script(old_bytes: &[u8], new_bytes: &[u8], ops: &[DiffOp]) {
        let old = split_lines(old_bytes);
        let new = split_lines(new_bytes);
        let rebuilt = apply_ops(&old, &new, ops);
        assert_eq!(rebuilt, new_bytes, "script did not rebuild new");
        for pair in ops.windows(2) {
            let same_kind = matches!(
                (pair[0], pair[1]),
                (DiffOp::Equal(_), DiffOp::Equal(_))
                    | (DiffOp::Delete(_), DiffOp::Delete(_))
                    | (DiffOp::Insert(_), DiffOp::Insert(_))
            );
            assert!(!same_kind, "ops not coalesced: {:?}", ops);
        }
    }

    /// Run all three real algorithms over a byte pair and assert each produces a
    /// valid, coalesced, LCS-correct script.
    fn check_all_algorithms(old_bytes: &[u8], new_bytes: &[u8]) {
        let old = split_lines(old_bytes);
        let new = split_lines(new_bytes);
        for algo in [
            DiffAlgorithm::Myers,
            DiffAlgorithm::Minimal,
            DiffAlgorithm::Patience,
            DiffAlgorithm::Histogram,
        ] {
            let ops = diff_lines_with_algorithm(&old, &new, algo);
            assert_valid_script(old_bytes, new_bytes, &ops);
        }
    }

    #[test]
    fn patience_and_histogram_match_myers_on_simple_cases() {
        // For localized single-line edits with no repeated lines, all three
        // algorithms agree with the canonical Myers script.
        let cases: &[(&[u8], &[u8], Vec<DiffOp>)] = &[
            (
                b"a\nb\nc\n",
                b"a\nx\nc\n",
                vec![
                    DiffOp::Equal(1),
                    DiffOp::Delete(1),
                    DiffOp::Insert(1),
                    DiffOp::Equal(1),
                ],
            ),
            (b"a\nb\nc\n", b"a\nb\nc\n", vec![DiffOp::Equal(3)]),
            (b"", b"a\nb\n", vec![DiffOp::Insert(2)]),
            (b"a\nb\n", b"", vec![DiffOp::Delete(2)]),
            (
                b"a\nb\nc\nd\n",
                b"a\nc\nd\n",
                vec![DiffOp::Equal(1), DiffOp::Delete(1), DiffOp::Equal(2)],
            ),
        ];
        for (old_bytes, new_bytes, expected) in cases {
            let old = split_lines(old_bytes);
            let new = split_lines(new_bytes);
            assert_eq!(&patience_diff_lines(&old, &new), expected);
            assert_eq!(&histogram_diff_lines(&old, &new), expected);
            assert_eq!(&myers_diff_lines(&old, &new), expected);
        }
    }

    #[test]
    fn patience_handles_both_empty() {
        let empty = split_lines(b"");
        assert!(patience_diff_lines(&empty, &empty).is_empty());
        assert!(histogram_diff_lines(&empty, &empty).is_empty());
    }

    #[test]
    fn patience_aligns_unique_anchors_across_moved_block() {
        // Reordering two unique blocks: patience anchors on the unique lines and
        // produces a delete-then-insert (or insert-then-delete) that still
        // reconstructs `new`. Validity is the contract; exact shape may differ
        // from Myers, so we only assert reconstruction here.
        check_all_algorithms(
            b"alpha\nbeta\ngamma\ndelta\n",
            b"gamma\ndelta\nalpha\nbeta\n",
        );
    }

    #[test]
    fn histogram_differs_from_myers_keeping_block_contiguous() {
        // A case where histogram diverges from Myers. With old = "b a" and a new
        // that surrounds an intact "b a" with inserted "b" lines, Myers splits
        // the common run into two single-line Equals (matching the leading and
        // trailing `b`/`a` separately), while histogram anchors on the rare line
        // and keeps the original two lines together as one Equal(2) block.
        let old = b"b\na\n";
        let new = b"a\nb\nb\na\nb\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);

        let myers = myers_diff_lines(&old_l, &new_l);
        let histogram = histogram_diff_lines(&old_l, &new_l);

        // All variants must reconstruct `new`.
        assert_valid_script(old, new, &myers);
        assert_valid_script(old, new, &histogram);

        // Exact, pinned shapes: Myers interleaves single-line equals; histogram
        // keeps "b\na\n" contiguous.
        assert_eq!(
            myers,
            vec![
                DiffOp::Insert(1),
                DiffOp::Equal(1),
                DiffOp::Insert(1),
                DiffOp::Equal(1),
                DiffOp::Insert(1),
            ]
        );
        assert_eq!(
            histogram,
            vec![DiffOp::Insert(2), DiffOp::Equal(2), DiffOp::Insert(1)]
        );
        // The contract the task calls out: histogram differs from Myers here.
        assert_ne!(myers, histogram);
    }

    #[test]
    fn patience_differs_from_myers_on_repeated_lines() {
        // A case where patience diverges from Myers. old = "b a", new = "a a b".
        // Myers deletes the leading `b` and appends; patience anchors on the
        // single unique-in-both line `a`... but `a` occurs twice in `new`, so it
        // is NOT unique there; patience instead falls through to its recursive
        // structure and produces the mirror script. Both reconstruct `new`.
        let old = b"b\na\n";
        let new = b"a\na\nb\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);

        let myers = myers_diff_lines(&old_l, &new_l);
        let patience = patience_diff_lines(&old_l, &new_l);

        assert_valid_script(old, new, &myers);
        assert_valid_script(old, new, &patience);

        assert_eq!(
            myers,
            vec![DiffOp::Delete(1), DiffOp::Equal(1), DiffOp::Insert(2)]
        );
        assert_eq!(
            patience,
            vec![DiffOp::Insert(2), DiffOp::Equal(1), DiffOp::Delete(1)]
        );
        assert_ne!(myers, patience);
    }

    #[test]
    fn realistic_function_insertion_all_valid() {
        // A more lifelike example: a new function is inserted ahead of an
        // existing one that shares structural lines ("}", blank line). We don't
        // pin exact shapes (they depend on trim interactions) but every
        // algorithm must produce a valid LCS-correct script.
        let old = b"int f() {\n    return 1;\n}\n";
        let new = b"int g() {\n    return 2;\n}\n\nint f() {\n    return 1;\n}\n";
        check_all_algorithms(old, new);
    }

    #[test]
    fn histogram_anchors_on_rare_line_when_no_unique_line_exists() {
        // No line is globally unique on both sides (every distinct line repeats
        // on at least one side), so plain patience would fall straight to Myers.
        // Histogram still anchors on the least-frequent shared line. We assert
        // both produce valid, reconstructing scripts.
        check_all_algorithms(b"x\nx\nmid\nx\nx\n", b"x\nmid\nx\nx\nx\n");
        check_all_algorithms(
            b"dup\ndup\nrare\ndup\ndup\n",
            b"dup\nrare\ndup\ndup\ndup\ndup\n",
        );
    }

    #[test]
    fn all_algorithms_treat_missing_final_newline_as_change() {
        // "b" (no newline) vs "b\n" is a real change for every algorithm.
        let old = split_lines(b"a\nb");
        let new = split_lines(b"a\nb\n");
        for algo in [
            DiffAlgorithm::Myers,
            DiffAlgorithm::Minimal,
            DiffAlgorithm::Patience,
            DiffAlgorithm::Histogram,
        ] {
            let ops = diff_lines_with_algorithm(&old, &new, algo);
            assert_eq!(
                ops,
                vec![DiffOp::Equal(1), DiffOp::Delete(1), DiffOp::Insert(1)],
                "algorithm {:?} mishandled missing final newline",
                algo
            );
        }
    }

    #[test]
    fn dispatcher_routes_each_variant() {
        let old = split_lines(b"a\nb\nc\n");
        let new = split_lines(b"a\nx\nc\n");
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Myers),
            myers_diff_lines(&old, &new)
        );
        // Minimal aliases Myers (the Myers search is already a minimal SES).
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Minimal),
            myers_diff_lines(&old, &new)
        );
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Patience),
            patience_diff_lines(&old, &new)
        );
        assert_eq!(
            diff_lines_with_algorithm(&old, &new, DiffAlgorithm::Histogram),
            histogram_diff_lines(&old, &new)
        );
    }

    #[test]
    fn patience_recurses_into_gaps_between_anchors() {
        // Unique anchors `head`/`tail` bracket an inner edit; patience must
        // recurse into the middle gap and diff `mid1`->`MID` there.
        let old = b"head\nmid1\nmid2\ntail\n";
        let new = b"head\nMID\nmid2\ntail\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);
        let ops = patience_diff_lines(&old_l, &new_l);
        assert_eq!(
            ops,
            vec![
                DiffOp::Equal(1),
                DiffOp::Delete(1),
                DiffOp::Insert(1),
                DiffOp::Equal(2),
            ]
        );
        assert_valid_script(old, new, &ops);
    }

    #[test]
    fn patience_falls_back_to_myers_with_no_unique_lines() {
        // Every line is duplicated within its own side, so there are no
        // unique-in-both anchors; patience must defer to Myers but still return
        // a valid script.
        let old = b"a\na\nb\nb\n";
        let new = b"a\na\na\nb\n";
        let old_l = split_lines(old);
        let new_l = split_lines(new);
        let ops = patience_diff_lines(&old_l, &new_l);
        // The contract for the fallback path is validity, not minimality: after
        // the greedy prefix/suffix trim (which git's patience does too) the
        // leftover block is handed to Myers, and the whole script must still
        // reconstruct `new`.
        assert_valid_script(old, new, &ops);
    }

    #[test]
    fn algorithms_agree_with_myers_when_all_lines_distinct() {
        // When every line is globally unique, patience's anchor set is the full
        // LCS, so patience and histogram must produce exactly the Myers script.
        let cases: &[(&[u8], &[u8])] = &[
            (b"a\nb\nc\nd\ne\n", b"a\nc\nd\nf\ne\n"),
            (b"1\n2\n3\n4\n5\n6\n", b"1\n3\n2\n4\n6\n5\n"),
            (b"q\nw\ne\nr\nt\ny\n", b"q\nw\nx\nr\nt\nz\n"),
        ];
        for (old_bytes, new_bytes) in cases {
            let old = split_lines(old_bytes);
            let new = split_lines(new_bytes);
            let myers = myers_diff_lines(&old, &new);
            assert_eq!(
                patience_diff_lines(&old, &new),
                myers,
                "patience must equal Myers when all lines are distinct: {:?}",
                old_bytes
            );
            assert_eq!(
                histogram_diff_lines(&old, &new),
                myers,
                "histogram must equal Myers when all lines are distinct: {:?}",
                old_bytes
            );
        }
    }

    #[test]
    fn fuzz_all_algorithms_reconstruct_new() {
        // A small deterministic LCG drives many random small inputs over a tiny
        // alphabet (so lines repeat and exercise the anchor/fallback paths).
        // Every algorithm must produce a valid LCS-correct script for each pair.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let alphabet = [b"a\n", b"b\n", b"c\n", b"d\n"];
        let build = |rng: &mut dyn FnMut() -> u32| -> Vec<u8> {
            let len = (rng() % 9) as usize; // 0..=8 lines
            let mut buf = Vec::new();
            for _ in 0..len {
                let pick = (rng() % alphabet.len() as u32) as usize;
                buf.extend_from_slice(alphabet[pick]);
            }
            // Occasionally drop the trailing newline to exercise that path.
            if !buf.is_empty() && rng().is_multiple_of(4) {
                buf.pop();
            }
            buf
        };
        for _ in 0..400 {
            let old_bytes = build(&mut next);
            let new_bytes = build(&mut next);
            check_all_algorithms(&old_bytes, &new_bytes);
        }
    }

    #[test]
    fn exhaustive_small_inputs_all_algorithms_reconstruct() {
        // Brute force over a 3-symbol alphabet up to 5 lines per side: every
        // algorithm must produce a valid LCS-correct script for *every* pair.
        // This is the strongest correctness net for the recursion/fallback
        // paths; apply_ops asserts both reconstruction and Equal-correctness.
        let syms = [b"a\n".to_vec(), b"b\n".to_vec(), b"c\n".to_vec()];
        let make = |n: usize, mut code: usize| -> Vec<u8> {
            let mut v = Vec::new();
            for _ in 0..n {
                v.extend_from_slice(&syms[code % 3]);
                code /= 3;
            }
            v
        };
        for la in 0..=5usize {
            for lb in 0..=5usize {
                for ca in 0..3usize.pow(la as u32) {
                    for cb in 0..3usize.pow(lb as u32) {
                        let ob = make(la, ca);
                        let nb = make(lb, cb);
                        let ol = split_lines(&ob);
                        let nl = split_lines(&nb);
                        assert_eq!(apply_ops(&ol, &nl, &myers_diff_lines(&ol, &nl)), nb);
                        assert_eq!(apply_ops(&ol, &nl, &patience_diff_lines(&ol, &nl)), nb);
                        assert_eq!(apply_ops(&ol, &nl, &histogram_diff_lines(&ol, &nl)), nb);
                    }
                }
            }
        }
    }

    #[test]
    fn fuzz_distinct_lines_patience_histogram_equal_myers() {
        // When inputs are permutations/subsequences of globally-unique lines,
        // patience and histogram must match Myers exactly. We generate sequences
        // of distinct tokens to guarantee global uniqueness on both sides.
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for _ in 0..200 {
            // Random subset+order of tokens "0\n".."9\n" for each side; tokens
            // are globally unique, so any common line is unique in both.
            let pick_subseq = |rng: &mut dyn FnMut() -> u32| -> Vec<u8> {
                let mut buf = Vec::new();
                for t in 0..10u32 {
                    if rng().is_multiple_of(2) {
                        buf.extend_from_slice(format!("{t}\n").as_bytes());
                    }
                }
                buf
            };
            let old_bytes = pick_subseq(&mut next);
            let new_bytes = pick_subseq(&mut next);
            let old = split_lines(&old_bytes);
            let new = split_lines(&new_bytes);
            let myers = myers_diff_lines(&old, &new);
            assert_eq!(patience_diff_lines(&old, &new), myers);
            assert_eq!(histogram_diff_lines(&old, &new), myers);
        }
    }

    // ===================================================================
    // Subtree-skip-by-OID tree-diff optimization: the pruned simultaneous
    // walk (`changed_tree_entries`) must produce byte-identical name-status
    // output to the legacy "flatten both sides fully" walk
    // (`collect_full_tree_pair`) on every representative diff shape.
    // ===================================================================

    /// Format a name-status result into stable, comparable lines.
    fn status_lines(entries: &[NameStatusEntry]) -> Vec<String> {
        entries.iter().map(|entry| entry.line()).collect()
    }

    /// Assert the pruned walk and the full flatten agree, both as raw map diffs
    /// and through the public tree-diff entry points, for the given options.
    fn assert_tree_diff_matches_full(
        db: &FileObjectDatabase,
        left: &ObjectId,
        right: &ObjectId,
        options: DiffNameStatusOptions,
    ) {
        // Reference ("old") behaviour: fully flatten both trees, then diff.
        let (full_left, full_right) = collect_full_tree_pair(db, ObjectFormat::Sha1, left, right)
            .expect("test operation should succeed");
        let reference = diff_name_status_maps(
            &full_left,
            &full_right,
            full_left.keys().chain(full_right.keys()),
            options,
        )
        .expect("test operation should succeed");

        // Optimized ("new") behaviour: prune identical subtrees, then diff.
        let (pruned_left, pruned_right) = changed_tree_entries(db, ObjectFormat::Sha1, left, right)
            .expect("test operation should succeed");
        let pruned = diff_name_status_maps(
            &pruned_left,
            &pruned_right,
            pruned_left.keys().chain(pruned_right.keys()),
            options,
        )
        .expect("test operation should succeed");

        assert_eq!(
            status_lines(&reference),
            status_lines(&pruned),
            "pruned map diff diverged from full map diff for {options:?}"
        );

        // And the public entry point (which itself selects pruned vs full) must
        // match the reference too.
        let public =
            diff_name_status_trees_with_options(db, ObjectFormat::Sha1, left, right, options)
                .expect("test operation should succeed");
        assert_eq!(
            status_lines(&reference),
            status_lines(&public),
            "public tree diff diverged from full map diff for {options:?}"
        );

        // The pruned maps must be a subset of the full maps and must contain
        // exactly the paths that actually changed (no identical entries leak in,
        // no changed entries get dropped).
        for (path, tracked) in &pruned_left {
            assert_eq!(
                full_left.get(path),
                Some(tracked),
                "pruned left entry not present (or differs) in full left map: {:?}",
                String::from_utf8_lossy(path)
            );
        }
        for (path, tracked) in &pruned_right {
            assert_eq!(
                full_right.get(path),
                Some(tracked),
                "pruned right entry not present (or differs) in full right map: {:?}",
                String::from_utf8_lossy(path)
            );
        }
        // Every path the full diff reports as changed must survive pruning on
        // whichever side(s) it lives.
        for entry in &reference {
            let path = entry.path.as_bytes();
            match entry.status {
                NameStatus::Added => assert!(
                    pruned_right.contains_key(path),
                    "added path dropped by pruning: {:?}",
                    String::from_utf8_lossy(path)
                ),
                NameStatus::Deleted => assert!(
                    pruned_left.contains_key(path),
                    "deleted path dropped by pruning: {:?}",
                    String::from_utf8_lossy(path)
                ),
                NameStatus::Modified => {
                    assert!(
                        pruned_left.contains_key(path) && pruned_right.contains_key(path),
                        "modified path dropped by pruning: {:?}",
                        String::from_utf8_lossy(path)
                    );
                }
                _ => {}
            }
        }
    }

    /// Run the equivalence assertion across the option matrix that the pruned
    /// path serves (everything except `--find-copies-harder`, which uses the
    /// full maps and is checked separately).
    fn assert_tree_diff_matches_full_all_modes(
        db: &FileObjectDatabase,
        left: &ObjectId,
        right: &ObjectId,
    ) {
        for detect_renames in [false, true] {
            for detect_copies in [false, true] {
                let options = DiffNameStatusOptions {
                    detect_renames,
                    detect_copies,
                    find_copies_harder: false,
                    rename_empty: true,
                    ..Default::default()
                };
                assert_tree_diff_matches_full(db, left, right, options);
            }
        }
    }

    /// Build a DB pre-seeded with a fixed bank of blobs for the structural tests.
    fn structural_db() -> (PathBuf, FileObjectDatabase) {
        let root = temp_root();
        let layout = RepositoryLayout::init_at(&root, ObjectFormat::Sha1, false)
            .expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&layout.git_dir, ObjectFormat::Sha1);
        (root, db)
    }

    #[test]
    fn pruned_walk_skips_identical_subtree_and_matches_full() {
        // A large shared subtree (`shared/`) is byte-identical on both sides; the
        // only change lives in `app/`. The pruned walk must skip `shared/`
        // entirely yet still produce the exact same diff as flattening it.
        let (root, mut db) = structural_db();

        // shared/ — identical on both sides, several nested files.
        let s1 = write_blob(&mut db, b"shared one\n");
        let s2 = write_blob(&mut db, b"shared two\n");
        let s3 = write_blob(&mut db, b"deep nested\n");
        let shared_inner = write_tree(&mut db, &[(b"c.txt", 0o100644, s3.clone())]);
        let shared = write_tree(
            &mut db,
            &[
                (b"a.txt", 0o100644, s1.clone()),
                (b"b.txt", 0o100644, s2.clone()),
                (b"inner", 0o040000, shared_inner.clone()),
            ],
        );

        // app/ — one file modified between sides.
        let app_old = write_blob(&mut db, b"version 1\n");
        let app_new = write_blob(&mut db, b"version 2\n");
        let app_left = write_tree(&mut db, &[(b"main.rs", 0o100644, app_old)]);
        let app_right = write_tree(&mut db, &[(b"main.rs", 0o100644, app_new)]);

        let left = write_tree(
            &mut db,
            &[
                (b"app", 0o040000, app_left),
                (b"shared", 0o040000, shared.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[(b"app", 0o040000, app_right), (b"shared", 0o040000, shared)],
        );

        // Sanity: the only change is the nested app/main.rs modification.
        let (pruned_left, pruned_right) =
            changed_tree_entries(&db, ObjectFormat::Sha1, &left, &right)
                .expect("test operation should succeed");
        assert_eq!(
            pruned_left.keys().collect::<Vec<_>>(),
            vec![&b"app/main.rs".to_vec()],
            "pruning should leave only the changed path on the left"
        );
        assert_eq!(
            pruned_right.keys().collect::<Vec<_>>(),
            vec![&b"app/main.rs".to_vec()],
            "pruning should leave only the changed path on the right"
        );
        assert!(
            !pruned_left.contains_key(b"shared/a.txt".as_slice()),
            "identical shared subtree must not appear in pruned maps"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_add_delete_modify_nested() {
        // Mixed shape: a top-level add, a top-level delete, a nested modify, and
        // an untouched nested subtree that must be skipped.
        let (root, mut db) = structural_db();

        let keep = write_blob(&mut db, b"unchanged\n");
        let untouched_dir = write_tree(&mut db, &[(b"keep.txt", 0o100644, keep.clone())]);

        let nested_old = write_blob(&mut db, b"nested old\n");
        let nested_new = write_blob(&mut db, b"nested new\n");
        let dir_left = write_tree(
            &mut db,
            &[
                (b"changed.txt", 0o100644, nested_old),
                (b"stable.txt", 0o100644, keep.clone()),
            ],
        );
        let dir_right = write_tree(
            &mut db,
            &[
                (b"changed.txt", 0o100644, nested_new),
                (b"stable.txt", 0o100644, keep.clone()),
            ],
        );

        let only_left = write_blob(&mut db, b"will be deleted\n");
        let only_right = write_blob(&mut db, b"freshly added\n");

        let left = write_tree(
            &mut db,
            &[
                (b"dir", 0o040000, dir_left),
                (b"gone.txt", 0o100644, only_left),
                (b"untouched", 0o040000, untouched_dir.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"dir", 0o040000, dir_right),
                (b"new.txt", 0o100644, only_right),
                (b"untouched", 0o040000, untouched_dir),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,

                ..Default::default()
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec![
                "M\tdir/changed.txt".to_string(),
                "D\tgone.txt".to_string(),
                "A\tnew.txt".to_string(),
            ],
            "unexpected raw status for mixed nested diff"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_rename_across_dirs() {
        // An exact rename (same blob oid) moving between directories. Rename
        // detection runs on the pruned add/delete set and must match the full
        // walk's result exactly.
        let (root, mut db) = structural_db();

        let moved = write_blob(&mut db, b"i get moved across directories\n");
        let companion = write_blob(&mut db, b"i stay put\n");
        let stable_dir = write_tree(&mut db, &[(b"keep.txt", 0o100644, companion.clone())]);

        let src_dir = write_tree(&mut db, &[(b"file.txt", 0o100644, moved.clone())]);
        let dst_dir = write_tree(&mut db, &[(b"renamed.txt", 0o100644, moved.clone())]);

        let left = write_tree(
            &mut db,
            &[
                (b"src", 0o040000, src_dir),
                (b"stable", 0o040000, stable_dir.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"dst", 0o040000, dst_dir),
                (b"stable", 0o040000, stable_dir),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,

                ..Default::default()
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec!["R100\tsrc/file.txt\tdst/renamed.txt".to_string()],
            "rename across dirs should be detected on pruned set"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_binary_and_mode_change() {
        // Binary blob modification plus an executable-bit (mode) change on an
        // otherwise-identical blob. Mode-only changes must still register as a
        // Modify (the pruned walk compares mode + oid, like the full map).
        let (root, mut db) = structural_db();

        let bin_old = write_blob(&mut db, &[0u8, 159, 146, 150, 0, 255, 1, 2, 3]);
        let bin_new = write_blob(&mut db, &[0u8, 159, 146, 150, 0, 254, 9, 8, 7]);
        let script = write_blob(&mut db, b"#!/bin/sh\necho hi\n");

        let left = write_tree(
            &mut db,
            &[
                (b"image.bin", 0o100644, bin_old),
                (b"run.sh", 0o100644, script.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"image.bin", 0o100644, bin_new),
                // same blob oid, executable bit flipped on
                (b"run.sh", 0o100755, script),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,

                ..Default::default()
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec!["M\timage.bin".to_string(), "M\trun.sh".to_string()],
            "binary edit and mode-only change should both be Modify"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_dir_replaced_by_file() {
        // A name that is a directory on the left and a regular file on the right
        // (and vice versa). The flattened paths differ (`thing/...` vs `thing`),
        // so the pruned walk must treat them as unrelated add/delete pairs,
        // exactly as the full flatten does.
        let (root, mut db) = structural_db();

        let inner_a = write_blob(&mut db, b"inner a\n");
        let inner_b = write_blob(&mut db, b"inner b\n");
        let thing_dir = write_tree(
            &mut db,
            &[(b"a.txt", 0o100644, inner_a), (b"b.txt", 0o100644, inner_b)],
        );
        let thing_file = write_blob(&mut db, b"now i am a file\n");

        // other/ is a file on the left, a directory on the right.
        let other_file = write_blob(&mut db, b"i was a file\n");
        let other_inner = write_blob(&mut db, b"now nested\n");
        let other_dir = write_tree(&mut db, &[(b"x.txt", 0o100644, other_inner)]);

        let left = write_tree(
            &mut db,
            &[
                (b"other", 0o100644, other_file),
                (b"thing", 0o040000, thing_dir),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"other", 0o040000, other_dir),
                (b"thing", 0o100644, thing_file),
            ],
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            DiffNameStatusOptions {
                detect_renames: false,
                detect_copies: false,
                find_copies_harder: false,
                rename_empty: true,

                ..Default::default()
            },
        )
        .expect("test operation should succeed");
        assert_eq!(
            status_lines(&entries),
            vec![
                "D\tother".to_string(),
                "A\tother/x.txt".to_string(),
                "A\tthing".to_string(),
                "D\tthing/a.txt".to_string(),
                "D\tthing/b.txt".to_string(),
            ],
            "dir<->file swap should flatten to independent adds/deletes"
        );

        assert_tree_diff_matches_full_all_modes(&db, &left, &right);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_for_identical_trees() {
        // Two identical root trees: zero changes, and the root must be skipped
        // without reading anything below it.
        let (root, mut db) = structural_db();

        let blob = write_blob(&mut db, b"same\n");
        let sub = write_tree(&mut db, &[(b"f.txt", 0o100644, blob.clone())]);
        let tree = write_tree(
            &mut db,
            &[(b"sub", 0o040000, sub), (b"top.txt", 0o100644, blob)],
        );

        let (pruned_left, pruned_right) =
            changed_tree_entries(&db, ObjectFormat::Sha1, &tree, &tree)
                .expect("test operation should succeed");
        assert!(
            pruned_left.is_empty() && pruned_right.is_empty(),
            "identical trees must produce no changed entries"
        );

        let entries = diff_name_status_trees_with_options(
            &db,
            ObjectFormat::Sha1,
            &tree,
            &tree,
            DiffNameStatusOptions::default(),
        )
        .expect("test operation should succeed");
        assert!(entries.is_empty(), "identical trees must produce no diff");

        assert_tree_diff_matches_full_all_modes(&db, &tree, &tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn find_copies_harder_uses_full_left_map_and_finds_unchanged_source() {
        // `--find-copies-harder` must still see an *unchanged* file as a copy
        // source. This is the case where the public entry point deliberately
        // falls back to the full flatten; verify the full-map fallback both
        // behaves correctly and matches a direct full-map computation.
        let (root, mut db) = structural_db();

        // `template.txt` is unchanged between sides (lives in an untouched
        // subtree), and `copy.txt` is added on the right with the same content.
        let template = write_blob(&mut db, b"reusable boilerplate content\n");
        let lib_dir = write_tree(&mut db, &[(b"template.txt", 0o100644, template.clone())]);

        let trigger_old = write_blob(&mut db, b"trigger old\n");
        let trigger_new = write_blob(&mut db, b"trigger new\n");

        let left = write_tree(
            &mut db,
            &[
                (b"lib", 0o040000, lib_dir.clone()),
                (b"trigger.txt", 0o100644, trigger_old),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"copy.txt", 0o100644, template.clone()),
                (b"lib", 0o040000, lib_dir),
                (b"trigger.txt", 0o100644, trigger_new),
            ],
        );

        let options = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: true,
            find_copies_harder: true,
            rename_empty: true,
            ..Default::default()
        };

        // Reference via the full flatten (the old algorithm).
        let (full_left, full_right) =
            collect_full_tree_pair(&db, ObjectFormat::Sha1, &left, &right)
                .expect("test operation should succeed");
        let reference = diff_name_status_maps(
            &full_left,
            &full_right,
            full_left.keys().chain(full_right.keys()),
            options,
        )
        .expect("test operation should succeed");

        let public =
            diff_name_status_trees_with_options(&db, ObjectFormat::Sha1, &left, &right, options)
                .expect("test operation should succeed");
        assert_eq!(
            status_lines(&reference),
            status_lines(&public),
            "find-copies-harder public diff must match full-map reference"
        );
        // The copy must be detected from the unchanged template source.
        assert!(
            public
                .iter()
                .any(|entry| matches!(entry.status, NameStatus::Copied(_))
                    && entry.old_path.as_ref().map(|p| p.as_bytes())
                        == Some(b"lib/template.txt".as_slice())
                    && entry.path == b"copy.txt"),
            "copy from unchanged source must be found with find_copies_harder: {public:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn pruned_walk_matches_full_with_inexact_rename_options() {
        // Exercise the rename-options entry point (which also selects pruned vs
        // full) with inexact detection enabled, across an untouched subtree.
        let (root, mut db) = structural_db();

        let untouched = write_blob(&mut db, b"untouched file\n");
        let untouched_dir = write_tree(&mut db, &[(b"u.txt", 0o100644, untouched.clone())]);

        // a.txt -> b.txt with one changed line (a 75% inexact rename).
        let old = write_blob(&mut db, b"one\ntwo\nthree\nfour\nfive\n");
        let new = write_blob(&mut db, b"one\ntwo\nTHREE\nfour\nfive\n");

        let left = write_tree(
            &mut db,
            &[
                (b"a.txt", 0o100644, old),
                (b"keep", 0o040000, untouched_dir.clone()),
            ],
        );
        let right = write_tree(
            &mut db,
            &[
                (b"b.txt", 0o100644, new),
                (b"keep", 0o040000, untouched_dir),
            ],
        );

        let options = DiffNameStatusOptions {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            detect_inexact: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
        };

        // Reference: full flatten + same detection.
        let (full_left, full_right) =
            collect_full_tree_pair(&db, ObjectFormat::Sha1, &left, &right)
                .expect("test operation should succeed");
        let reference = diff_name_status_maps_with_renames(
            &full_left,
            &full_right,
            full_left.keys().chain(full_right.keys()),
            options,
            |oid| read_blob_bytes(&db, oid),
        )
        .expect("test operation should succeed");

        let public = diff_name_status_trees_with_options_and_diagnostics(
            &db,
            ObjectFormat::Sha1,
            &left,
            &right,
            options,
        )
        .expect("test operation should succeed");

        assert_eq!(
            status_lines(&reference),
            status_lines(&public.entries),
            "inexact rename via pruned walk must match full-map reference"
        );
        assert_eq!(
            status_lines(&public.entries),
            vec!["R075\ta.txt\tb.txt".to_string()],
            "expected a 75% inexact rename"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn write_delta_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value as u8) & 0x7f;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    /// Delta whose result-size varint lies while carrying a tiny real instruction
    /// stream (sley#35 regression shape).
    fn lying_result_size_delta(declared_result_size: usize) -> (Vec<u8>, Vec<u8>) {
        let base = b"hello";
        let result = b"hello world";
        let mut delta = Vec::new();
        write_delta_varint(&mut delta, base.len() as u64);
        write_delta_varint(&mut delta, declared_result_size as u64);
        let suffix = &result[base.len()..];
        delta.push(0x90);
        delta.push(base.len() as u8);
        delta.push(suffix.len() as u8);
        delta.extend_from_slice(suffix);
        (base.to_vec(), delta)
    }

    #[test]
    fn bounded_inflate_reserve_caps_attacker_declared_size() {
        use sley_pack::inflate::{MAX_INFLATE_RESERVE, bounded_inflate_reserve};
        assert_eq!(bounded_inflate_reserve(u64::MAX as usize, 10), 10 * 1032);
        assert_eq!(
            bounded_inflate_reserve(usize::MAX, usize::MAX),
            MAX_INFLATE_RESERVE
        );
        assert_eq!(bounded_inflate_reserve(1000, 500), 1000);
        assert_eq!(bounded_inflate_reserve(0, 0), 64);
    }

    #[test]
    fn parse_leading_usize_errors_on_overflow() {
        assert_eq!(parse_leading_usize(b""), Ok(0));
        assert_eq!(parse_leading_usize(b"42 junk"), Ok(42));
        assert!(parse_leading_usize(b"999999999999999999999999999999999999999999").is_err());
    }

    /// Regression (sley#35): `git_patch_delta` must not OOM on a lying result-size
    /// varint; the post-decode length check rejects the bomb cleanly.
    #[test]
    fn git_patch_delta_rejects_result_size_bomb_without_oom() {
        let bombs = [usize::MAX, 1024 * 1024 * 1024 * 1024];
        for declared in bombs {
            let (base, delta) = lying_result_size_delta(declared);
            let handle = std::thread::spawn(move || git_patch_delta(&base, &delta));
            let join_result = handle.join();
            assert!(
                join_result.is_ok(),
                "delta bomb (declared={declared}) panicked/aborted instead of erroring cleanly"
            );
            assert!(
                join_result
                    .expect("parse thread should not panic on a delta bomb")
                    .is_none(),
                "delta bomb (declared={declared}) should be rejected as invalid"
            );
        }
    }

    #[test]
    fn git_patch_delta_applies_legitimate_delta_after_result_size_bound() {
        let (base, delta) = lying_result_size_delta(b"hello world".len());
        let patched = git_patch_delta(&base, &delta).expect("legitimate delta should resolve");
        assert_eq!(patched, b"hello world");
    }
}
