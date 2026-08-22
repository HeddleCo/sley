#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
#![allow(
    clippy::collapsible_if,
    clippy::if_same_then_else,
    clippy::ptr_arg,
    clippy::too_many_arguments
)]

use sley_config::GitConfig;
use sley_core::{
    BString, GitError, MissingObjectContext, MissingObjectKind, ObjectFormat, ObjectId,
    Result,
};
use sley_index::{
    BorrowedIndex, CacheTree, Index, IndexEntry, IndexEntryRef, SPARSE_DIR_MODE, SplitIndexLink,
    Stage, UntrackedCache, UntrackedCacheDir, UntrackedCacheOidStat, UntrackedCacheStatData,
};
use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntry, tree_entry_object_type};
use sley_odb::{FileObjectDatabase, ObjectPresenceChecker, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry, branch_ref_name};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs};

// --- Mechanical module split (wave-47) -------------------------------------
// The former single 21.8k-line lib.rs is partitioned into contiguous
// submodules along its existing function-cluster seams. Each submodule pulls
// the crate-root scope in via `use super::*` and is re-exported below so every
// `sley_worktree::X` path (public API and intra-crate) resolves unchanged.
// This is a pure code move: no function body was altered.
pub mod admin;
mod attributes;
mod checkout;
mod filter;
mod fsmonitor;
mod ignore;
mod index;
mod index_io;
mod move_remove;
mod read_tree;
mod status;
mod status_plan;
mod types_admin;

// `attributes` and `index_io` hold only crate-internal helpers (no `pub`
// items), so they are reached via direct `use crate::{attributes,index_io}::*`
// at their call sites (including the test module) rather than a public glob
// re-export, which would re-export nothing and warn.
pub use checkout::*;
pub use filter::*;
pub use fsmonitor::*;
pub use ignore::*;
pub use index::*;
pub use index_io::{StatCleanFilterValidator, fill_index_entry_stat_cache};
pub use move_remove::*;
pub use read_tree::*;
pub use status::*;
pub use status_plan::*;
pub use types_admin::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::*;
    use crate::index_io::*;
    use sley_odb::ObjectReader;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn short_status(
        worktree_root: impl AsRef<Path>,
        git_dir: impl AsRef<Path>,
        format: ObjectFormat,
    ) -> Result<Vec<ShortStatusEntry>> {
        let mut entries = Vec::new();
        stream_short_status(worktree_root, git_dir, format, |entry| {
            entries.push(entry.to_owned_entry());
            Ok(StreamControl::Continue)
        })?;
        Ok(entries)
    }

    #[test]
    fn atomic_metadata_writer_writes_and_reports_stat() {
        let root = temp_root();
        let path = root.join(".git").join("HEAD");

        let result = write_metadata_file_atomic(
            &path,
            b"ref: refs/heads/main\n",
            AtomicMetadataWriteOptions::default(),
        )
        .expect("write metadata");

        assert_eq!(
            fs::read(&path).expect("read metadata"),
            b"ref: refs/heads/main\n"
        );
        assert_eq!(result.path, path);
        assert_eq!(result.len, b"ref: refs/heads/main\n".len() as u64);
        assert!(result.mtime.is_some());
        assert!(!path.with_file_name("HEAD.lock").exists());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn atomic_metadata_writer_existing_lock_preserves_original() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("create git dir");
        let path = git_dir.join("HEAD");
        let lock = git_dir.join("HEAD.lock");
        fs::write(&path, b"ref: refs/heads/main\n").expect("write original");
        fs::write(&lock, b"held\n").expect("write lock");

        let err = write_metadata_file_atomic(
            &path,
            b"ref: refs/heads/other\n",
            AtomicMetadataWriteOptions::default(),
        )
        .expect_err("held lock must fail");

        assert!(matches!(err, GitError::Transaction(_)));
        assert_eq!(
            fs::read(&path).expect("read original"),
            b"ref: refs/heads/main\n"
        );
        assert_eq!(fs::read(&lock).expect("read lock"), b"held\n");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    // --- `ls-files --eol` stat/attr helpers (mirror convert.c) ---------------

    #[test]
    fn convert_stats_ascii_classifies_eol_content() {
        assert_eq!(convert_stats_ascii(b""), "none");
        assert_eq!(convert_stats_ascii(b"abc"), "none");
        assert_eq!(convert_stats_ascii(b"a\nb\n"), "lf");
        assert_eq!(convert_stats_ascii(b"a\r\nb\r\n"), "crlf");
        assert_eq!(convert_stats_ascii(b"a\r\nb\n"), "mixed");
        // A lone CR makes the content binary (-text), matching git.
        assert_eq!(convert_stats_ascii(b"a\rb"), "-text");
        // A NUL byte is binary.
        assert_eq!(convert_stats_ascii(b"a\0b\n"), "-text");
        // A trailing ^Z (EOF) is not counted as non-printable.
        assert_eq!(convert_stats_ascii(b"abc\n\x1a"), "lf");
    }

    fn attr_check(name: &[u8], state: Option<AttributeState>) -> AttributeCheck {
        AttributeCheck {
            attribute: name.to_vec(),
            state,
        }
    }

    #[test]
    fn convert_attr_ascii_matches_git_attr_action() {
        // No attributes at all: empty attr field.
        assert_eq!(convert_attr_ascii(&[]), "");
        // text (set) -> "text"; -text (unset) -> "-text".
        assert_eq!(
            convert_attr_ascii(&[attr_check(b"text", Some(AttributeState::Set))]),
            "text"
        );
        assert_eq!(
            convert_attr_ascii(&[attr_check(b"text", Some(AttributeState::Unset))]),
            "-text"
        );
        // text=auto -> "text=auto"; with eol=crlf/lf the AUTO variants.
        assert_eq!(
            convert_attr_ascii(&[attr_check(
                b"text",
                Some(AttributeState::Value(b"auto".to_vec()))
            )]),
            "text=auto"
        );
        assert_eq!(
            convert_attr_ascii(&[
                attr_check(b"text", Some(AttributeState::Value(b"auto".to_vec()))),
                attr_check(b"eol", Some(AttributeState::Value(b"crlf".to_vec()))),
            ]),
            "text=auto eol=crlf"
        );
        assert_eq!(
            convert_attr_ascii(&[
                attr_check(b"text", Some(AttributeState::Value(b"auto".to_vec()))),
                attr_check(b"eol", Some(AttributeState::Value(b"lf".to_vec()))),
            ]),
            "text=auto eol=lf"
        );
        // eol=crlf/lf alone (no text) forces text + the eol direction.
        assert_eq!(
            convert_attr_ascii(&[attr_check(
                b"eol",
                Some(AttributeState::Value(b"crlf".to_vec()))
            )]),
            "text eol=crlf"
        );
        assert_eq!(
            convert_attr_ascii(&[attr_check(
                b"eol",
                Some(AttributeState::Value(b"lf".to_vec()))
            )]),
            "text eol=lf"
        );
        // -text overrides any eol attribute (binary wins).
        assert_eq!(
            convert_attr_ascii(&[
                attr_check(b"text", Some(AttributeState::Unset)),
                attr_check(b"eol", Some(AttributeState::Value(b"crlf".to_vec()))),
            ]),
            "-text"
        );
    }

    #[test]
    fn smudge_safety_guard_skips_irreversible_autocrlf() {
        // text=auto eol=crlf (AUTO_CRLF): convert pure-LF, but leave content
        // alone when it already has a CR or CRLF, or is binary.
        let auto = ContentFilterPlan {
            text: TextDecision::Auto,
            eol: EolConversion::Crlf,
            ident: false,
            driver: None,
            encoding: WtEncoding::None,
        };
        assert!(auto.will_convert_lf_to_crlf(b"a\nb\n"));
        assert!(!auto.will_convert_lf_to_crlf(b"a\r\nb\n")); // has CRLF
        assert!(!auto.will_convert_lf_to_crlf(b"a\nb\rc")); // lone CR (binary)
        assert!(!auto.will_convert_lf_to_crlf(b"abc")); // no naked LF

        // text eol=crlf (TEXT_CRLF): no safety guard — always convert naked LF
        // even when a CR/CRLF is already present.
        let text = ContentFilterPlan {
            text: TextDecision::Text,
            eol: EolConversion::Crlf,
            ident: false,
            driver: None,
            encoding: WtEncoding::None,
        };
        assert!(text.will_convert_lf_to_crlf(b"a\r\nb\nc\n"));
        assert!(!text.will_convert_lf_to_crlf(b"a\r\nb\r\n")); // no naked LF
    }

    /// Build an in-memory ignore matcher from raw `.gitignore` lines (no disk).
    fn ignore_matcher(patterns: &[&[u8]]) -> IgnoreMatcher {
        let mut matcher = IgnoreMatcher::default();
        let owned: Vec<Vec<u8>> = patterns.iter().map(|p| p.to_vec()).collect();
        matcher.extend_patterns(&owned);
        matcher
    }

    #[test]
    fn ignore_match_kind_fast_paths_match_the_wildcard_engine() {
        // Literal: exact basename anywhere; not a superstring.
        let matcher = ignore_matcher(&[b"Pods"]);
        assert!(matcher.is_ignored(b"a/b/Pods", true));
        assert!(matcher.is_ignored(b"Pods", false));
        assert!(!matcher.is_ignored(b"Pods_not", false));
        assert!(matches!(
            classify_ignore_pattern(b"Pods"),
            MatchKind::Literal
        ));

        // Suffix `*.log`: basename ending in `.log` at any depth.
        let matcher = ignore_matcher(&[b"*.log"]);
        assert!(matcher.is_ignored(b"x.log", false));
        assert!(matcher.is_ignored(b"a/b/x.log", false));
        assert!(matcher.is_ignored(b".log", false));
        assert!(!matcher.is_ignored(b"x.logx", false));
        assert!(matches!(
            classify_ignore_pattern(b"*.log"),
            MatchKind::Suffix
        ));

        // Prefix `build*`: basename starting with `build`.
        let matcher = ignore_matcher(&[b"build*"]);
        assert!(matcher.is_ignored(b"buildfoo", false));
        assert!(matcher.is_ignored(b"a/build", false));
        assert!(!matcher.is_ignored(b"xbuild", false));
        assert!(matches!(
            classify_ignore_pattern(b"build*"),
            MatchKind::Prefix
        ));
    }

    #[test]
    fn ignore_anchored_suffix_does_not_cross_slash() {
        // `/*.log` is anchored: matches `.log` files only at the matcher base,
        // never in a subdirectory — the slash guard in `match_segment`.
        let matcher = ignore_matcher(&[b"/*.log"]);
        assert!(matcher.is_ignored(b"x.log", false));
        assert!(!matcher.is_ignored(b"sub/x.log", false));

        // Anchored literal likewise only matches at root.
        let matcher = ignore_matcher(&[b"/foo"]);
        assert!(matcher.is_ignored(b"foo", false));
        assert!(!matcher.is_ignored(b"a/foo", false));
    }

    #[test]
    fn ignore_anchored_directory_glob_matches_root_directory() {
        let matcher = ignore_matcher(&[b"/tmp-*/"]);
        assert!(matcher.is_ignored(b"tmp-info-only", true));
        assert!(matcher.is_ignored(b"tmp-info-only/file.txt", false));
        assert!(!matcher.is_ignored(b"nested/tmp-info-only", true));
        assert!(!matcher.is_ignored(b"tmp-info-only", false));
    }

    #[test]
    fn ignore_negated_directory_glob_does_not_reinclude_files() {
        // t0008-ignores "directories and ** matches": a negated directory-only
        // pattern re-includes *directories* but never the *files* inside them
        // (git: re-including a dir with `!dir/` still needs an explicit
        // `!dir/*` to reach its files). Verified against git 2.54 check-ignore:
        //   data/file              -> data/**           (ignored)
        //   data/data1/file1       -> data/**           (ignored, NOT !data/**/)
        //   data/data1/file1.txt   -> !data/**/*.txt    (re-included)
        //   data/data1   (dir)     -> !data/**/         (re-included)
        let matcher = ignore_matcher(&[b"data/**", b"!data/**/", b"!data/**/*.txt"]);
        // Files stay ignored: `!data/**/` must not win the file leaf scan.
        assert!(matcher.is_ignored(b"data/file", false));
        assert!(matcher.is_ignored(b"data/data1/file1", false));
        assert!(matcher.is_ignored(b"data/data2/file2", false));
        // `.txt` files are re-included by the explicit non-dir negation.
        assert!(!matcher.is_ignored(b"data/data1/file1.txt", false));
        assert!(!matcher.is_ignored(b"data/data2/file2.txt", false));
        // Directories ARE re-included by `!data/**/` (the directory-glob gain
        // from `fix: match git status ignored directory globs`).
        assert!(!matcher.is_ignored(b"data/data1", true));
        assert!(!matcher.is_ignored(b"data/data2", true));
    }

    #[test]
    fn ignore_double_star_prefix_collapses_to_basename() {
        // `**/X` ≡ `X` for slash-free X (verified against `git check-ignore`).
        let matcher = ignore_matcher(&[b"**/Pods"]);
        assert!(matcher.is_ignored(b"a/b/Pods", true));
        assert!(matcher.is_ignored(b"Pods", true));
        assert!(!matcher.is_ignored(b"Pods_not", false));

        let matcher = ignore_matcher(&[b"**/*.jks"]);
        assert!(matcher.is_ignored(b"x.jks", false));
        assert!(matcher.is_ignored(b"a/deep/y.jks", false));
        assert!(!matcher.is_ignored(b"x.jksx", false));

        // `**/A/B` keeps a slash in the tail, so it stays a real glob and must
        // match the trailing path at any depth.
        let matcher = ignore_matcher(&[b"**/Flutter/ephemeral"]);
        assert!(matcher.is_ignored(b"Flutter/ephemeral", true));
        assert!(matcher.is_ignored(b"a/Flutter/ephemeral", true));
        assert!(!matcher.is_ignored(b"Flutter/other", true));
        assert!(matches!(
            classify_ignore_pattern(b"**/Flutter/ephemeral"),
            MatchKind::PathSuffix
        ));
    }

    #[test]
    fn ignore_slash_glob_literal_basename_bucket_preserves_matches() {
        let matcher = ignore_matcher(&[b"**/android/**/GeneratedPluginRegistrant.java"]);
        assert!(
            matcher
                .buckets
                .glob_path_literal_basename
                .contains_key(b"GeneratedPluginRegistrant.java".as_slice())
        );
        assert!(matcher.is_ignored(
            b"packages/app/android/src/GeneratedPluginRegistrant.java",
            false
        ));
        assert!(matcher.is_ignored(
            b"android/app/src/main/java/io/flutter/GeneratedPluginRegistrant.java",
            false
        ));
        assert!(!matcher.is_ignored(b"android/app/src/main/java/io/flutter/Other.java", false));

        let matcher = ignore_matcher(&[b"**/ios/**/Pods/"]);
        assert!(
            matcher
                .buckets
                .glob_directory_literal_basename
                .contains_key(b"Pods".as_slice())
        );
        assert!(matcher.is_ignored(b"ios/Runner/Pods", true));
        assert!(matcher.is_ignored(b"dev/app/ios/Runner/Pods/Manifest.lock", false));
        assert!(!matcher.is_ignored(b"dev/app/ios/Runner/Podfile", false));

        let matcher = ignore_matcher(&[b"**/ios/**/*.mode1v3"]);
        assert!(
            !matcher.buckets.glob_path_suffix_basename.is_empty(),
            "suffix-final slash glob should be prefiltered by basename suffix"
        );
        assert!(matcher.is_ignored(b"apps/ios/Runner/default.mode1v3", false));
        assert!(!matcher.is_ignored(b"apps/ios/Runner/default.mode2v3", false));

        let matcher = ignore_matcher(&[b"**/ios/Runner/GeneratedPluginRegistrant.*"]);
        assert!(
            !matcher.buckets.glob_path_prefix_basename.is_empty(),
            "prefix-final slash glob should be prefiltered by basename prefix"
        );
        assert!(matcher.is_ignored(b"apps/ios/Runner/GeneratedPluginRegistrant.swift", false));
        assert!(!matcher.is_ignored(
            b"apps/ios/Runner/OtherGeneratedPluginRegistrant.swift",
            false
        ));

        let matcher = ignore_matcher(&[b"ios/Scenarios/*.framework/"]);
        assert!(
            !matcher.buckets.glob_directory_suffix_basename.is_empty(),
            "directory suffix-final slash glob should be prefiltered by directory component"
        );
        assert!(matcher.is_ignored(b"ios/Scenarios/App.framework", true));
        assert!(matcher.is_ignored(b"ios/Scenarios/App.framework/Info.plist", false));
        assert!(!matcher.is_ignored(b"ios/Scenarios/App.xcframework/Info.plist", false));
    }

    #[test]
    fn ignore_complex_globs_still_use_the_engine() {
        let matcher = ignore_matcher(&[b"*.[Cc]ache"]);
        assert!(matcher.is_ignored(b"x.cache", false));
        assert!(matcher.is_ignored(b"x.Cache", false));
        assert!(!matcher.is_ignored(b"x.xache", false));
        assert!(matches!(
            classify_ignore_pattern(b"*.[Cc]ache"),
            MatchKind::Glob
        ));

        let matcher = ignore_matcher(&[b"Icon?"]);
        assert!(matcher.is_ignored(b"IconA", false));
        assert!(!matcher.is_ignored(b"Icon", false));
        assert!(!matcher.is_ignored(b"IconAB", false));

        // Multi-star is not a simple prefix/suffix.
        assert!(matches!(
            classify_ignore_pattern(b"app.*.symbols"),
            MatchKind::Glob
        ));
        assert!(matches!(classify_ignore_pattern(b"a*b*c"), MatchKind::Glob));

        let matcher = ignore_matcher(&[b".vscode/*", b"dev/devicelab/ABresults*.json"]);
        assert!(matcher.is_ignored(b".vscode/settings.json", false));
        assert!(!matcher.is_ignored(b"pkg/.vscode/settings.json", false));
        assert!(matcher.is_ignored(b"dev/devicelab/ABresults-1.json", false));
        assert!(!matcher.is_ignored(b"dev/devicelab/results-1.json", false));
    }

    #[test]
    fn ignore_negation_still_applies_after_fast_paths() {
        // Last match wins: a negated literal un-ignores a suffix-matched file.
        let matcher = ignore_matcher(&[b"*.log", b"!keep.log"]);
        assert!(matcher.is_ignored(b"a/x.log", false));
        assert!(!matcher.is_ignored(b"a/keep.log", false));
    }

    #[test]
    fn read_expected_object_missing_blob_exposes_oid_and_kind() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let missing = ObjectId::empty_blob(ObjectFormat::Sha1);

        let err = read_expected_object(&db, &missing, ObjectType::Blob)
            .expect_err("missing blob should error");
        let kind = err.not_found_kind().expect("typed not found");
        assert_eq!(kind.object_id(), Some(missing));
        assert_eq!(kind.missing_object_kind(), Some(MissingObjectKind::Blob));
        assert_eq!(
            kind.missing_object_context(),
            Some(MissingObjectContext::WorktreeMaterialize)
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn update_index_adds_file_entry_and_blob() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 1);
        let index = Index::parse_v2_sha1(
            &fs::read(repository_index_path(git_dir)).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        assert_eq!(index.entries[0].path, b"hello.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn update_index_and_write_tree_support_sha256() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha256,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 1);

        let index = Index::parse(
            &fs::read(repository_index_path(&git_dir)).expect("test operation should succeed"),
            ObjectFormat::Sha256,
        )
        .expect("test operation should succeed");
        assert_eq!(index.entries[0].path, b"hello.txt");
        assert_eq!(index.entries[0].oid.format(), ObjectFormat::Sha256);

        let tree_oid = write_tree_from_index(&git_dir, ObjectFormat::Sha256)
            .expect("test operation should succeed");
        assert_eq!(tree_oid.format(), ObjectFormat::Sha256);
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);
        let tree = odb
            .read_object(&tree_oid)
            .expect("test operation should succeed");
        assert_eq!(tree.object_type, ObjectType::Tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn write_tree_from_index_writes_nested_tree_objects() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src")).expect("test operation should succeed");
        fs::write(root.join("README.md"), b"readme\n").expect("test operation should succeed");
        fs::write(root.join("src").join("lib.rs"), b"pub fn demo() {}\n")
            .expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("README.md"), PathBuf::from("src/lib.rs")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 2);
        let tree_oid = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = odb
            .read_object(&tree_oid)
            .expect("test operation should succeed");
        assert_eq!(tree.object_type, ObjectType::Tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn write_tree_from_index_expands_empty_primary_split_index() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("f.txt")],
        )
        .expect("test operation should succeed");
        let expected = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");

        enable_split_index(&git_dir, ObjectFormat::Sha1).expect("test operation should succeed");
        let primary = read_index(&git_dir);
        assert!(
            primary.entries.is_empty(),
            "fixture should put all entries in the shared index"
        );
        assert!(
            primary
                .split_index_link(ObjectFormat::Sha1)
                .expect("test operation should succeed")
                .is_some(),
            "fixture should write a split-index link extension"
        );

        let actual = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(actual, expected);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_reports_added_and_untracked_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        fs::write(root.join("extra.txt"), b"extra\n").expect("test operation should succeed");
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(
            status
                .iter()
                .map(ShortStatusEntry::line)
                .collect::<Vec<_>>(),
            vec!["A  hello.txt", "?? extra.txt"]
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn borrowed_untracked_frontier_preserves_directory_ignore_scopes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("tracked.txt"), b"tracked\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["tracked.txt"]);

        fs::create_dir_all(root.join("alpha").join("nested"))
            .expect("test operation should succeed");
        fs::write(root.join("alpha").join(".gitignore"), b"blocked.txt\n")
            .expect("test operation should succeed");
        fs::write(
            root.join("alpha").join("nested").join("blocked.txt"),
            b"ignored\n",
        )
        .expect("test operation should succeed");
        fs::write(
            root.join("alpha").join("nested").join("visible.txt"),
            b"visible\n",
        )
        .expect("test operation should succeed");

        fs::create_dir_all(root.join("beta").join("nested"))
            .expect("test operation should succeed");
        fs::write(
            root.join("beta").join("nested").join("blocked.txt"),
            b"visible\n",
        )
        .expect("test operation should succeed");

        let index_bytes = read_borrowed_index_bytes(&repository_index_path(&git_dir))
            .expect("test operation should succeed");
        let borrowed = BorrowedIndex::parse(index_bytes.as_ref(), ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut ignores =
            IgnoreMatcher::from_worktree_base(&root).expect("test operation should succeed");
        let paths = status_untracked_paths_from_borrowed_index(
            &root,
            &git_dir,
            &borrowed,
            &mut ignores,
            StatusUntrackedMode::All,
            None,
        )
        .expect("test operation should succeed");

        assert_eq!(
            paths,
            vec![
                b"alpha/.gitignore".to_vec(),
                b"alpha/nested/visible.txt".to_vec(),
                b"beta/nested/blocked.txt".to_vec(),
            ]
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_root_is_none_for_bare_repository() {
        // A bare git_dir (basename `.git`) with `core.bare = true` must resolve to
        // `Ok(None)` rather than falling through to the "parent of .git" case.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("create bare git dir");
        // Hermetic minimal config — do not depend on host gitconfig.
        fs::write(git_dir.join("config"), b"[core]\n\tbare = true\n").expect("write bare config");

        assert_eq!(
            worktree_root_for_git_dir(&git_dir).expect("resolve bare worktree root"),
            None,
            "a bare repository has no working tree"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_root_is_parent_for_non_bare_dot_git() {
        // A non-bare `.git` directory (no core.bare / core.bare = false) still
        // resolves to its parent — the ordinary non-bare layout.
        let root = temp_root();
        let work = root.join("work");
        let git_dir = work.join(".git");
        fs::create_dir_all(&git_dir).expect("create non-bare git dir");
        fs::write(git_dir.join("config"), b"[core]\n\tbare = false\n")
            .expect("write non-bare config");

        assert_eq!(
            worktree_root_for_git_dir(&git_dir).expect("resolve non-bare worktree root"),
            Some(work),
            "a non-bare .git dir resolves to its parent"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-worktree-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test operation should succeed");
        path
    }

    fn index_entry_for<'a>(index: &'a Index, path: &[u8]) -> &'a IndexEntry {
        index
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("missing index entry for {}", String::from_utf8_lossy(path)))
    }

    fn read_index(git_dir: &Path) -> Index {
        Index::parse(
            &fs::read(repository_index_path(git_dir)).expect("test operation should succeed"),
            ObjectFormat::Sha1,
        )
        .expect("test operation should succeed")
    }

    /// Stages `paths` from the worktree, writes their tree, wraps it in a commit
    /// object, and points `refs/heads/main` + `HEAD` at it. Returns the commit
    /// id. After this call the index reflects the committed tree.
    fn build_commit(root: &Path, git_dir: &Path, paths: &[&str]) -> ObjectId {
        let path_bufs = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        add_paths_to_index(root, git_dir, ObjectFormat::Sha1, &path_bufs)
            .expect("test operation should succeed");
        let tree = write_tree_from_index(git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author Test <test@example.com> 0 +0000\n");
        body.extend_from_slice(b"committer Test <test@example.com> 0 +0000\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"sparse fixture\n");
        let odb = FileObjectDatabase::from_git_dir(git_dir, ObjectFormat::Sha1);
        let commit = odb
            .write_object(EncodedObject::new(ObjectType::Commit, body))
            .expect("test operation should succeed");
        let refs = FileRefStore::new(git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(commit),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/main".into()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        commit
    }

    fn full_sparse(patterns: &[&[u8]]) -> SparseCheckout {
        SparseCheckout {
            patterns: patterns.iter().map(|pattern| pattern.to_vec()).collect(),
            sparse_index: false,
        }
    }

    #[test]
    fn checkout_index_paths_uses_injected_replacement_content() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::write(root.join("file"), b"original\n").expect("write original");
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("file")],
        )
        .expect("stage original");
        let original_oid = index_entry_for(&read_index(&git_dir), b"file").oid;
        let raw_db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let replacement_oid = raw_db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"replacement\n".to_vec(),
            ))
            .expect("write replacement");
        let db = raw_db.with_replacements(sley_odb::ObjectReplacements::new([(
            original_oid,
            replacement_oid,
        )]));
        fs::remove_file(root.join("file")).expect("remove worktree copy");

        checkout_index_paths_with_database(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[root.join("file")],
            &db,
            CheckoutIndexPathOptions {
                force: false,
                merge: false,
                overlay: true,
                stage: None,
                conflict_style: CheckoutConflictStyle::Merge,
                smudge_config: None,
            },
        )
        .expect("restore replacement content");

        assert_eq!(
            fs::read(root.join("file")).expect("read file"),
            b"replacement\n"
        );
        assert_eq!(
            index_entry_for(&read_index(&git_dir), b"file").oid,
            original_oid
        );
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn path_checkout_honors_and_can_explicitly_clear_skip_worktree() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("sub")).expect("create selected directory");
        fs::write(root.join("outside"), b"outside\n").expect("write outside file");
        fs::write(root.join("sub/file"), b"selected\n").expect("write selected file");
        build_commit(&root, &git_dir, &["outside", "sub/file"]);

        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/sub/"]),
            SparseCheckoutMode::Full,
        )
        .expect("apply sparse checkout");
        assert!(!root.join("outside").exists());
        assert!(index_entry_for(&read_index(&git_dir), b"outside").is_skip_worktree());

        fs::write(root.join("sub/file"), b"dirty\n").expect("dirty selected file");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let options = CheckoutIndexPathOptions {
            force: false,
            merge: false,
            overlay: true,
            stage: None,
            conflict_style: CheckoutConflictStyle::Merge,
            smudge_config: None,
        };
        let outcome = checkout_index_paths_with_database_outcome_sparse(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[root.join(".")],
            &db,
            CheckoutIndexSparsePolicy::Honor,
            options,
        )
        .expect("honor sparse path checkout");
        assert_eq!(outcome.restored, 1);
        assert!(!root.join("outside").exists());
        assert_eq!(
            fs::read(root.join("sub/file")).expect("selected file"),
            b"selected\n"
        );
        assert!(index_entry_for(&read_index(&git_dir), b"outside").is_skip_worktree());

        let outcome = checkout_index_paths_with_database_outcome_sparse(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[root.join(".")],
            &db,
            CheckoutIndexSparsePolicy::Ignore,
            options,
        )
        .expect("override sparse path checkout");
        assert_eq!(outcome.restored, 1);
        assert_eq!(
            fs::read(root.join("outside")).expect("outside file"),
            b"outside\n"
        );
        assert!(!index_entry_for(&read_index(&git_dir), b"outside").is_skip_worktree());

        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn apply_sparse_checkout_full_mode_skips_out_of_cone_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("in")).expect("test operation should succeed");
        fs::create_dir_all(root.join("out")).expect("test operation should succeed");
        fs::write(root.join("in").join("keep.txt"), b"keep\n")
            .expect("test operation should succeed");
        fs::write(root.join("out").join("drop.txt"), b"drop\n")
            .expect("test operation should succeed");
        fs::write(root.join("top.txt"), b"top\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["in/keep.txt", "out/drop.txt", "top.txt"]);

        // Full (non-cone) pattern: keep only the `in/` subtree.
        let sparse = full_sparse(&[b"/in/"]);
        let result = apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");

        assert!(root.join("in").join("keep.txt").exists());
        assert!(!root.join("out").join("drop.txt").exists());
        assert!(!root.join("top.txt").exists());
        assert!(result.materialized.contains(&b"in/keep.txt".to_vec()));
        assert!(result.skipped.contains(&b"out/drop.txt".to_vec()));
        assert!(result.skipped.contains(&b"top.txt".to_vec()));

        let index = read_index(&git_dir);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"in/keep.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"out/drop.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index, b"top.txt"
        )));
        // Out-of-cone entries are preserved in the index, just not on disk.
        assert_eq!(index.entries.len(), 3);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_reports_unmerged_paths_once() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("out")).expect("test operation should succeed");
        fs::write(root.join("out").join("conflicted.txt"), b"conflict\n")
            .expect("test operation should succeed");
        build_commit(&root, &git_dir, &["out/conflicted.txt"]);

        let mut index = read_index(&git_dir);
        let entry = index
            .entries
            .iter_mut()
            .find(|entry| entry.path.as_bytes() == b"out/conflicted.txt")
            .expect("fixture index entry");
        entry.flags = (entry.flags & !sley_index::INDEX_FLAG_STAGE_MASK) | 0x1000;
        write_repository_index_ref(&git_dir, ObjectFormat::Sha1, &index)
            .expect("write conflicted fixture index");

        let result = apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/kept/"]),
            SparseCheckoutMode::Full,
        )
        .expect("apply sparse checkout");

        assert_eq!(result.unmerged, vec![b"out/conflicted.txt".to_vec()]);
        assert!(root.join("out").join("conflicted.txt").exists());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_toggle_rematerializes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("a")).expect("test operation should succeed");
        fs::create_dir_all(root.join("b")).expect("test operation should succeed");
        fs::write(root.join("a").join("file.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("b").join("file.txt"), b"b\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["a/file.txt", "b/file.txt"]);

        // First narrow to `a/`.
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/a/"]),
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(root.join("a").join("file.txt").exists());
        assert!(!root.join("b").join("file.txt").exists());
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"b/file.txt"
        )));

        // Now switch the cone to `b/`: `a/` must leave, `b/` must come back with
        // the correct content, and the skip-worktree bits must flip.
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/b/"]),
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("a").join("file.txt").exists());
        assert!(root.join("b").join("file.txt").exists());
        assert_eq!(
            fs::read(root.join("b").join("file.txt")).expect("test operation should succeed"),
            b"b\n"
        );
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"a/file.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"b/file.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_cone_mode_matches_directory_prefixes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("kept").join("nested"))
            .expect("test operation should succeed");
        fs::create_dir_all(root.join("other")).expect("test operation should succeed");
        fs::write(root.join("kept").join("a.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("kept").join("nested").join("b.txt"), b"b\n")
            .expect("test operation should succeed");
        fs::write(root.join("other").join("c.txt"), b"c\n").expect("test operation should succeed");
        fs::write(root.join("root.txt"), b"r\n").expect("test operation should succeed");
        build_commit(
            &root,
            &git_dir,
            &["kept/a.txt", "kept/nested/b.txt", "other/c.txt", "root.txt"],
        );

        // Standard cone patterns: top-level files plus the whole `kept/` tree.
        let sparse = SparseCheckout {
            patterns: vec![b"/*".to_vec(), b"!/*/".to_vec(), b"/kept/".to_vec()],
            sparse_index: false,
        };
        // Auto mode should detect cone shape on its own.
        assert!(patterns_are_cone(&sparse.patterns));
        apply_sparse_checkout(&root, &git_dir, ObjectFormat::Sha1, &sparse)
            .expect("test operation should succeed");

        assert!(root.join("root.txt").exists());
        assert!(root.join("kept").join("a.txt").exists());
        assert!(root.join("kept").join("nested").join("b.txt").exists());
        assert!(!root.join("other").join("c.txt").exists());

        let index = read_index(&git_dir);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"root.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"kept/a.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"kept/nested/b.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"other/c.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_cone_parent_guards_keep_only_direct_files() {
        let sparse = SparseCheckout {
            patterns: vec![
                b"/*".to_vec(),
                b"!/*/".to_vec(),
                b"/deep/".to_vec(),
                b"!/deep/*/".to_vec(),
                b"/deep/kept/".to_vec(),
            ],
            sparse_index: false,
        };

        assert!(path_in_sparse_checkout(
            b"deep/file.txt",
            &sparse,
            SparseCheckoutMode::Cone
        ));
        assert!(path_in_sparse_checkout(
            b"deep/kept/file.txt",
            &sparse,
            SparseCheckoutMode::Cone
        ));
        assert!(!path_in_sparse_checkout(
            b"deep/dropped/file.txt",
            &sparse,
            SparseCheckoutMode::Cone
        ));
    }

    #[test]
    fn full_sparse_checkout_later_ancestor_exclusion_wins() {
        let sparse = SparseCheckout {
            patterns: vec![
                b"a".to_vec(),
                b"!/x".to_vec(),
                b"y/".to_vec(),
                b"!x/y/z".to_vec(),
            ],
            sparse_index: false,
        };

        assert!(path_in_sparse_checkout(
            b"a",
            &sparse,
            SparseCheckoutMode::Full
        ));
        assert!(path_in_sparse_checkout(
            b"x/y/keep",
            &sparse,
            SparseCheckoutMode::Full
        ));
        assert!(!path_in_sparse_checkout(
            b"x/y/z/new-a",
            &sparse,
            SparseCheckoutMode::Full
        ));
    }

    #[test]
    fn apply_sparse_checkout_honors_preexisting_skip_worktree_via_idempotence() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("in")).expect("test operation should succeed");
        fs::create_dir_all(root.join("out")).expect("test operation should succeed");
        fs::write(root.join("in").join("keep.txt"), b"keep\n")
            .expect("test operation should succeed");
        fs::write(root.join("out").join("drop.txt"), b"drop\n")
            .expect("test operation should succeed");
        build_commit(&root, &git_dir, &["in/keep.txt", "out/drop.txt"]);

        let sparse = full_sparse(&[b"/in/"]);
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("out").join("drop.txt").exists());

        // Re-applying the same spec is a no-op: the already-skipped file stays
        // absent and the bit stays set (we do not resurrect it).
        let result = apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("out").join("drop.txt").exists());
        assert!(root.join("in").join("keep.txt").exists());
        assert!(result.skipped.contains(&b"out/drop.txt".to_vec()));
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"out/drop.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn configure_cone_sparse_index(root: &Path, git_dir: &Path, _commit: &ObjectId) {
        fs::create_dir_all(git_dir.join("info")).expect("create sparse info directory");
        fs::write(
            git_dir.join("config"),
            b"[core]\n\tsparseCheckout = true\n\tsparseCheckoutCone = true\n[index]\n\tsparse = true\n",
        )
        .expect("write sparse config");
        fs::write(git_dir.join("info/sparse-checkout"), b"/*\n!/*/\n/keep/\n")
            .expect("write cone patterns");
        let sparse = SparseCheckout {
            patterns: vec![b"/*".to_vec(), b"!/*/".to_vec(), b"/keep/".to_vec()],
            sparse_index: true,
        };
        apply_sparse_checkout_with_mode(
            root,
            git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Cone,
        )
        .expect("seed sparse index through explicit sparse-checkout application");
    }

    #[test]
    fn sparse_reapply_distinguishes_clean_and_dirty_files_after_index_expansion() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("keep")).expect("create included directory");
        fs::create_dir_all(root.join("skip")).expect("create excluded directory");
        fs::write(root.join("keep/a"), b"keep\n").expect("write included file");
        fs::write(root.join("skip/b"), b"skip\n").expect("write excluded file");
        let commit = build_commit(&root, &git_dir, &["keep/a", "skip/b"]);
        configure_cone_sparse_index(&root, &git_dir, &commit);

        let before = read_index(&git_dir);
        assert!(before.is_sparse());
        assert!(before.entries.iter().any(|entry| entry.path == b"skip/"));
        assert!(!root.join("skip/b").exists());
        let (sparse, mode) = active_sparse_checkout(&git_dir)
            .expect("read sparse checkout")
            .expect("active sparse checkout");

        // A clean file may be materialized by a preceding transition while the
        // sparse index still represents its directory as one collapsed entry.
        // Expansion gives the leaf a blank stat cache, so content identity must
        // decide whether it is safe to evict.
        fs::create_dir_all(root.join("skip")).expect("recreate excluded directory");
        fs::write(root.join("skip/b"), b"skip\n").expect("recreate clean excluded file");
        let clean =
            apply_sparse_checkout_with_mode(&root, &git_dir, ObjectFormat::Sha1, &sparse, mode)
                .expect("reapply sparse checkout to clean file");
        assert!(clean.not_up_to_date.is_empty());
        assert!(!root.join("skip/b").exists());
        assert!(
            read_index(&git_dir)
                .entries
                .iter()
                .any(|entry| entry.path == b"skip/")
        );

        fs::create_dir_all(root.join("skip")).expect("recreate excluded directory");
        fs::write(root.join("skip/b"), b"modified\n").expect("write dirty excluded file");
        let dirty =
            apply_sparse_checkout_with_mode(&root, &git_dir, ObjectFormat::Sha1, &sparse, mode)
                .expect("reapply sparse checkout to dirty file");
        assert_eq!(dirty.not_up_to_date, vec![b"skip/b".to_vec()]);
        assert_eq!(
            fs::read(root.join("skip/b")).expect("read dirty file"),
            b"modified\n"
        );

        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn tree_path_checkout_replaces_expanded_sparse_leaf_without_duplicate_stage_zero() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("keep")).expect("create included directory");
        fs::create_dir_all(root.join("skip/dir")).expect("create excluded directory");
        fs::write(root.join("keep/a"), b"keep\n").expect("write included file");
        fs::write(root.join("skip/b"), b"old\n").expect("write excluded file");
        fs::write(root.join("skip/dir/child"), b"child\n").expect("write excluded child");
        let commit = build_commit(&root, &git_dir, &["keep/a", "skip/b", "skip/dir/child"]);
        configure_cone_sparse_index(&root, &git_dir, &commit);

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let replacement_oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"new\n".to_vec()))
            .expect("write replacement blob");
        let replacement_dir_oid = db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"directory became a file\n".to_vec(),
            ))
            .expect("write directory replacement blob");
        let source_entries = BTreeMap::from([
            (
                b"skip/b".to_vec(),
                TrackedEntry {
                    mode: 0o100644,
                    oid: replacement_oid,
                },
            ),
            (
                b"skip/dir".to_vec(),
                TrackedEntry {
                    mode: 0o100644,
                    oid: replacement_dir_oid,
                },
            ),
        ]);
        restore_index_and_worktree_paths_from_entries(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            read_index(&git_dir),
            &source_entries,
            &[PathBuf::from(".")],
            true,
        )
        .expect("restore path from replacement tree");

        let mut expanded = read_index(&git_dir);
        // Both out-of-cone replacements were explicitly materialized by this
        // path checkout. Their cleared skip-worktree bits keep the directory
        // physically expanded until an explicit sparse-checkout reapply.
        assert!(!expanded.is_sparse());
        assert!(!expanded.entries.iter().any(IndexEntry::is_sparse_dir));
        expand_sparse_index_in_memory(&mut expanded, &db, ObjectFormat::Sha1)
            .expect("expand resulting sparse index");
        let matching = expanded
            .entries
            .iter()
            .filter(|entry| entry.path == b"skip/b")
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].stage(), Stage::Normal);
        assert_eq!(matching[0].oid, replacement_oid);
        assert!(
            !expanded
                .entries
                .iter()
                .any(|entry| entry.path == b"skip/dir/child")
        );
        assert_eq!(
            index_entry_for(&expanded, b"skip/dir").oid,
            replacement_dir_oid
        );
        assert_eq!(
            fs::read(root.join("skip/b")).expect("read restored file"),
            b"new\n"
        );

        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn tree_path_checkout_preserves_unselected_conflict_stages() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::write(root.join("selected"), b"old\n").expect("write selected file");
        build_commit(&root, &git_dir, &["selected"]);
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let conflict_two = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"ours\n".to_vec()))
            .expect("write stage two blob");
        let conflict_three = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"theirs\n".to_vec()))
            .expect("write stage three blob");
        let mut index = read_index(&git_dir);
        index.entries.push(resolve_undo_index_entry(
            b"conflict".to_vec(),
            0o100644,
            conflict_two,
            2,
        ));
        index.entries.push(resolve_undo_index_entry(
            b"conflict".to_vec(),
            0o100644,
            conflict_three,
            3,
        ));
        index.entries.sort_by(compare_index_key);

        let replacement_oid = db
            .write_object(EncodedObject::new(
                ObjectType::Blob,
                b"replacement\n".to_vec(),
            ))
            .expect("write selected replacement blob");
        let source_entries = BTreeMap::from([(
            b"selected".to_vec(),
            TrackedEntry {
                mode: 0o100644,
                oid: replacement_oid,
            },
        )]);
        restore_index_and_worktree_paths_from_entries(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            index,
            &source_entries,
            &[PathBuf::from("selected")],
            true,
        )
        .expect("restore selected path");

        let result = read_index(&git_dir);
        let conflicts = result
            .entries
            .iter()
            .filter(|entry| entry.path == b"conflict")
            .collect::<Vec<_>>();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].stage(), Stage::Ours);
        assert_eq!(conflicts[0].oid, conflict_two);
        assert_eq!(conflicts[1].stage(), Stage::Theirs);
        assert_eq!(conflicts[1].oid, conflict_three);
        assert_eq!(index_entry_for(&result, b"selected").oid, replacement_oid);

        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn checkout_change_summary_reports_sparse_index_changes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("keep")).expect("create included directory");
        fs::create_dir_all(root.join("skip")).expect("create excluded directory");
        fs::write(root.join("keep/a"), b"keep\n").expect("write included file");
        fs::write(root.join("skip/b"), b"old\n").expect("write excluded file");
        let base = build_commit(&root, &git_dir, &["keep/a", "skip/b"]);
        configure_cone_sparse_index(&root, &git_dir, &base);

        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let replacement_oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"new\n".to_vec()))
            .expect("write replacement blob");
        let source_entries = BTreeMap::from([(
            b"skip/b".to_vec(),
            TrackedEntry {
                mode: 0o100644,
                oid: replacement_oid,
            },
        )]);
        restore_index_and_worktree_paths_from_entries(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &db,
            read_index(&git_dir),
            &source_entries,
            &[PathBuf::from(".")],
            true,
        )
        .expect("restore sparse path");
        let (sparse, mode) = active_sparse_checkout(&git_dir)
            .expect("read sparse checkout")
            .expect("active sparse checkout");
        apply_sparse_checkout_with_mode(&root, &git_dir, ObjectFormat::Sha1, &sparse, mode)
            .expect("reapply sparse checkout");

        let summary = checkout_change_summary(&root, &git_dir, ObjectFormat::Sha1, &base)
            .expect("compute checkout summary");
        assert_eq!(summary.changes.len(), 1);
        assert_eq!(
            summary.changes[0].status,
            sley_diff_merge::NameStatus::Modified
        );
        assert_eq!(summary.changes[0].path, b"skip/b");

        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn mixed_reset_preserves_collapsed_out_of_cone_directory() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("keep")).expect("create included directory");
        fs::create_dir_all(root.join("skip")).expect("create excluded directory");
        fs::write(root.join("keep/a"), b"keep\n").expect("write included file");
        fs::write(root.join("skip/b"), b"skip\n").expect("write excluded file");
        let commit = build_commit(&root, &git_dir, &["keep/a", "skip/b"]);
        configure_cone_sparse_index(&root, &git_dir, &commit);

        let before = read_index(&git_dir);
        assert!(before.is_sparse());
        assert!(before.entries.iter().any(|entry| entry.path == b"skip/"));
        reset_index_to_commit(&root, &git_dir, ObjectFormat::Sha1, &commit)
            .expect("mixed reset sparse index");

        let after = read_index(&git_dir);
        assert!(after.is_sparse());
        assert!(after.entries.iter().any(|entry| entry.path == b"skip/"));
        assert!(!after.entries.iter().any(|entry| entry.path == b"skip/b"));
        assert!(!root.join("skip/b").exists());
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn hard_reset_recreates_removed_sparse_directory_entry_without_materializing_it() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("keep")).expect("create included directory");
        fs::create_dir_all(root.join("skip")).expect("create excluded directory");
        fs::write(root.join("keep/a"), b"keep\n").expect("write included file");
        fs::write(root.join("skip/b"), b"skip\n").expect("write excluded file");
        let commit = build_commit(&root, &git_dir, &["keep/a", "skip/b"]);
        configure_cone_sparse_index(&root, &git_dir, &commit);

        let mut removed = read_index(&git_dir);
        removed.entries.retain(|entry| entry.path != b"skip/");
        removed
            .clear_sparse_extension()
            .expect("clear stale sparse marker");
        write_repository_index_ref(&git_dir, ObjectFormat::Sha1, &removed)
            .expect("write index with staged sparse directory deletion");
        reset_index_and_worktree_to_commit(&root, &git_dir, ObjectFormat::Sha1, &commit)
            .expect("hard reset sparse index");

        let restored = read_index(&git_dir);
        assert!(restored.is_sparse());
        assert!(restored.entries.iter().any(|entry| entry.path == b"skip/"));
        assert!(!restored.entries.iter().any(|entry| entry.path == b"skip/b"));
        assert!(!root.join("skip/b").exists());
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn remove_sparse_directory_expands_selects_leaves_and_recollapses_result() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("keep")).expect("create included directory");
        fs::create_dir_all(root.join("skip")).expect("create excluded directory");
        fs::write(root.join("keep/a"), b"keep\n").expect("write included file");
        fs::write(root.join("skip/b"), b"skip\n").expect("write excluded file");
        fs::write(root.join("skip/c"), b"skip too\n").expect("write second excluded file");
        let commit = build_commit(&root, &git_dir, &["keep/a", "skip/b", "skip/c"]);
        configure_cone_sparse_index(&root, &git_dir, &commit);

        let result = remove_index_and_worktree_paths(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[root.join("skip")],
            RemoveOptions {
                recursive: true,
                cached: false,
                force: false,
                dry_run: false,
                ignore_unmatch: false,
                sparse: true,
            },
            None,
        )
        .expect("remove collapsed sparse directory");

        assert_eq!(result.removed, vec![b"skip/".to_vec()]);
        let after = read_index(&git_dir);
        assert!(after.entries.iter().all(|entry| {
            entry.path != b"skip/" && !entry.path.as_bytes().starts_with(b"skip/")
        }));
        assert!(root.join("keep/a").exists());
        assert!(!root.join("skip").exists());
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn checkout_detached_sparse_only_writes_in_cone_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("keep")).expect("test operation should succeed");
        fs::create_dir_all(root.join("skip")).expect("test operation should succeed");
        fs::write(root.join("keep").join("a.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("skip").join("b.txt"), b"b\n").expect("test operation should succeed");
        let commit = build_commit(&root, &git_dir, &["keep/a.txt", "skip/b.txt"]);

        // The worktree is clean and matches the commit. A sparse checkout must
        // keep the in-cone file and evict the out-of-cone one.
        let sparse = full_sparse(&[b"/keep/"]);
        let result = checkout_detached_sparse(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &commit,
            b"Test <test@example.com> 0 +0000".to_vec(),
            b"checkout".to_vec(),
            &sparse,
        )
        .expect("test operation should succeed");
        assert_eq!(result.files, 2);

        assert!(root.join("keep").join("a.txt").exists());
        assert_eq!(
            fs::read(root.join("keep").join("a.txt")).expect("test operation should succeed"),
            b"a\n"
        );
        assert!(!root.join("skip").join("b.txt").exists());

        let index = read_index(&git_dir);
        assert_eq!(index.entries.len(), 2);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"keep/a.txt"
        )));
        let skipped = index_entry_for(&index, b"skip/b.txt");
        assert!(index_entry_skip_worktree(skipped));
        // The skipped entry still carries the committed blob id and mode.
        assert_eq!(skipped.mode, 0o100644);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn checkout_current_branch_reapplies_active_sparse_rules() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(git_dir.join("info")).expect("create info directory");
        fs::create_dir_all(root.join("done")).expect("create tracked directory");
        fs::write(root.join("done/.gitignore"), b"ignored\n").expect("write dotfile");
        fs::write(root.join("done/one"), b"one\n").expect("write included file");
        build_commit(&root, &git_dir, &["done/.gitignore", "done/one"]);
        fs::write(git_dir.join("config"), b"[core]\n\tsparseCheckout = true\n")
            .expect("enable sparse checkout");
        fs::write(git_dir.join("info/sparse-checkout"), b"done/[a-z]*\n")
            .expect("write sparse patterns");
        let config = GitConfig::read(git_dir.join("config")).expect("read config");

        let result = checkout_branch_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            "main",
            b"Test <test@example.com> 0 +0000".to_vec(),
            &config,
        )
        .expect("checkout current branch");

        assert_eq!(result.files, 0, "same-HEAD checkout remains a ref no-op");
        assert!(!root.join("done/.gitignore").exists());
        assert!(root.join("done/one").is_file());
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"done/.gitignore"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"done/one"
        )));
        let ignore_oid = index_entry_for(&index, b"done/.gitignore").oid;
        let primed_cache = build_untracked_cache(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &index,
            StatusUntrackedMode::Normal,
        )
        .expect("prime sparse untracked cache");
        let primed_done = primed_cache
            .root
            .as_ref()
            .and_then(|root| root.dirs.iter().find(|dir| dir.name == b"done"))
            .expect("primed done cache node");
        assert_eq!(
            primed_done.exclude_oid, None,
            "skip-worktree ignore OID stays lazy without untracked content"
        );
        fs::write(root.join("done/ignored"), b"ignored\n").expect("write ignored file");
        fs::write(root.join("done/visible"), b"visible\n").expect("write visible file");
        let status =
            short_status(&root, &git_dir, ObjectFormat::Sha1).expect("scan sparse worktree status");
        assert!(
            status.iter().all(|entry| entry.path != b"done/ignored"),
            "skip-worktree .gitignore blob still supplies ignore patterns"
        );
        assert!(status.iter().any(|entry| entry.path == b"done/visible"));
        let cache = build_untracked_cache(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &index,
            StatusUntrackedMode::Normal,
        )
        .expect("build sparse untracked cache");
        let done = cache
            .root
            .as_ref()
            .and_then(|root| root.dirs.iter().find(|dir| dir.name == b"done"))
            .expect("done cache node");
        assert_eq!(done.exclude_oid, Some(ignore_oid));
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[cfg(unix)]
    #[test]
    fn modified_entries_ignore_executable_bit_when_core_filemode_is_false() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::write(root.join("script.sh"), b"true\n").expect("write script");
        fs::set_permissions(root.join("script.sh"), fs::Permissions::from_mode(0o755))
            .expect("make script executable");
        build_commit(&root, &git_dir, &["script.sh"]);
        fs::write(git_dir.join("config"), b"[core]\n\tfilemode = false\n")
            .expect("disable filemode trust");
        fs::set_permissions(root.join("script.sh"), fs::Permissions::from_mode(0o644))
            .expect("remove physical executable bit");

        let entry = index_entry_for(&read_index(&git_dir), b"script.sh").clone();
        assert_eq!(
            worktree_entry_state_by_git_path(
                &root,
                &git_dir,
                ObjectFormat::Sha1,
                b"script.sh",
                &entry.oid,
                entry.mode,
                None,
            )
            .expect("probe worktree state with untrusted executable bit"),
            WorktreeEntryState::Clean
        );
        assert!(
            modified_index_entries(&root, &git_dir, ObjectFormat::Sha1)
                .expect("inspect worktree with untrusted executable bit")
                .is_empty()
        );

        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[cfg(unix)]
    #[test]
    fn checkout_replaces_symlink_with_tracked_directory() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::create_dir_all(root.join("one")).expect("create first directory");
        fs::create_dir_all(root.join("two")).expect("create second directory");
        fs::write(root.join("one/file"), b"").expect("write first file");
        fs::write(root.join("two/file"), b"").expect("write second file");
        let directory_commit = build_commit(&root, &git_dir, &["one/file", "two/file"]);

        fs::remove_dir_all(root.join("one")).expect("remove first directory");
        std::os::unix::fs::symlink("two", root.join("one")).expect("create symlink");
        add_paths_to_index(&root, &git_dir, ObjectFormat::Sha1, &[PathBuf::from("one")])
            .expect("stage symlink type change");
        let symlink_commit = build_commit(&root, &git_dir, &[]);
        assert_ne!(directory_commit, symlink_commit);
        assert!(
            modified_index_entries(&root, &git_dir, ObjectFormat::Sha1)
                .expect("inspect clean symlink worktree")
                .is_empty()
        );
        let config = GitConfig::default();

        checkout_detached_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &directory_commit,
            b"Test <test@example.com> 0 +0000".to_vec(),
            b"checkout".to_vec(),
            &config,
        )
        .expect("checkout directory commit over symlink");

        assert!(root.join("one").is_dir());
        assert!(root.join("one/file").is_file());
        assert!(
            !fs::symlink_metadata(root.join("one"))
                .expect("one metadata")
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn checkout_extension_preservation_keeps_only_valid_untracked_cache() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        fs::write(root.join("tracked"), b"tracked\n").expect("write tracked file");
        build_commit(&root, &git_dir, &["tracked"]);
        let mut previous = read_index(&git_dir);
        let mut cache = UntrackedCache::new(
            ObjectFormat::Sha1,
            b"test-ident\0".to_vec(),
            sley_index::untracked_cache_normal_flags(),
        );
        cache.root = Some(UntrackedCacheDir {
            valid: true,
            recurse: true,
            untracked: vec![b"cached-untracked".to_vec()],
            ..UntrackedCacheDir::default()
        });
        previous
            .set_untracked_cache(ObjectFormat::Sha1, Some(&cache))
            .expect("attach untracked cache");
        previous.extensions.extend_from_slice(b"FSMN\0\0\0\0");

        let mut next = previous.clone();
        next.entries.clear();
        next.extensions.clear();
        preserve_checkout_index_extensions(&previous, &mut next, ObjectFormat::Sha1)
            .expect("preserve checkout-safe extensions");

        let cache = next
            .untracked_cache(ObjectFormat::Sha1)
            .expect("parse preserved cache")
            .expect("UNTR remains present");
        let cache_root = cache.root.expect("cache root");
        assert!(
            !cache_root.valid,
            "changed tracked path invalidates cache root"
        );
        assert!(cache_root.untracked.is_empty());
        assert!(
            next.extension(b"FSMN").expect("parse extensions").is_none(),
            "entry-table fsmonitor state must not be copied blindly"
        );
        fs::remove_dir_all(root).expect("clean fixture");
    }

    #[test]
    fn normal_untracked_rollup_descends_below_tracked_parent() {
        let mut index = BTreeMap::new();
        index.insert(
            b"done/tracked".to_vec(),
            TrackedEntry {
                mode: 0o100644,
                oid: ObjectId::empty_blob(ObjectFormat::Sha1),
            },
        );
        let ignores = IgnoreMatcher::default();

        assert_eq!(
            untracked_normal_rollup_path(b"done/sub/sub/file", &index, &ignores),
            b"done/sub/"
        );
    }

    #[test]
    fn untracked_cache_trace_events_follow_changed_nodes() {
        let done = UntrackedCacheDir {
            name: b"done".to_vec(),
            valid: true,
            recurse: true,
            ..UntrackedCacheDir::default()
        };
        let old = UntrackedCacheDir {
            valid: true,
            recurse: true,
            dirs: vec![done],
            ..UntrackedCacheDir::default()
        };
        let mut new = old.clone();
        new.stat.mtime_seconds = 1;
        new.dirs[0].stat.mtime_seconds = 1;
        new.dirs[0].exclude_oid = Some(ObjectId::empty_blob(ObjectFormat::Sha1));
        let mut events = UntrackedCacheTraversalEvents::default();
        collect_untracked_cache_traversal_events(Some(&old), &new, false, true, &mut events);
        assert_eq!(
            events,
            UntrackedCacheTraversalEvents {
                node_creation: 0,
                gitignore_invalidation: 1,
                directory_invalidation: 2,
                opendir: 2,
            }
        );

        let old = new;
        let mut new = old.clone();
        new.dirs[0].stat.mtime_seconds = 2;
        new.dirs[0].dirs.push(UntrackedCacheDir {
            name: b"sub".to_vec(),
            valid: true,
            recurse: true,
            dirs: vec![UntrackedCacheDir {
                name: b"sub".to_vec(),
                valid: true,
                recurse: true,
                ..UntrackedCacheDir::default()
            }],
            ..UntrackedCacheDir::default()
        });
        let mut events = UntrackedCacheTraversalEvents::default();
        collect_untracked_cache_traversal_events(Some(&old), &new, false, true, &mut events);
        assert_eq!(
            events,
            UntrackedCacheTraversalEvents {
                node_creation: 2,
                gitignore_invalidation: 0,
                directory_invalidation: 1,
                opendir: 3,
            }
        );
    }

    #[test]
    fn restore_from_tree_default_wildcard_matches_subdirectory_file() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("subdir")).expect("test operation should succeed");
        fs::write(root.join("subdir").join("file3"), b"file3-1\n")
            .expect("test operation should succeed");
        let old_commit = build_commit(&root, &git_dir, &["subdir/file3"]);
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let old_tree = read_commit(&db, ObjectFormat::Sha1, &old_commit)
            .expect("test operation should succeed")
            .tree;

        fs::write(root.join("subdir").join("file3"), b"file3-2\n")
            .expect("test operation should succeed");
        build_commit(&root, &git_dir, &["subdir/file3"]);

        let result = restore_index_and_worktree_paths_from_tree(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &old_tree,
            &[PathBuf::from("*file3")],
            false,
        )
        .expect("wildcard pathspec should select subdir/file3");

        assert_eq!(result.restored, 1);
        assert_eq!(
            fs::read(root.join("subdir").join("file3")).expect("test operation should succeed"),
            b"file3-1\n"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    // ----- content filtering: EOL / autocrlf + clean/smudge drivers -----

    /// Build a [`GitConfig`] from raw config text.
    fn config_from(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("test operation should succeed")
    }

    /// Conformance grid for git's `output_eol(crlf_action)` decision table
    /// (convert.c) on the smudge side, exercised across the same
    /// attr × autocrlf × eol × content matrix as upstream t0027/t0026.
    ///
    /// Each row asserts the smudge output for a representative content shape.
    /// The cases that historically under-converted are the non-`auto` `text`
    /// paths (the auto-only safety guard must NOT fire) and the
    /// `autocrlf=true overrides core.eol` precedence rows.
    #[test]
    fn smudge_output_eol_decision_table() {
        // Naked-LF-only blob (the canonical "should gain CRLF" case).
        const LF: &[u8] = b"a\nb\nc\n";
        // Mixed CRLF + naked LF: a non-auto crlf action converts the naked LFs
        // to CRLF (whole file becomes CRLF); an auto action leaves it untouched.
        const CRLF_MIX_LF: &[u8] = b"a\r\nb\nc\r\n";
        // Naked LF plus a lone CR: non-auto converts LFs, keeping the lone CR.
        const LF_MIX_CR: &[u8] = b"a\nb\rc\n";

        let smudge = |cfg: &str, attrline: Option<&[u8]>, input: &[u8]| -> Vec<u8> {
            let config = config_from(cfg);
            let checks = match attrline {
                Some(line) => {
                    let mut matcher = AttributeMatcher::default();
                    read_attribute_patterns_from_bytes(line, &mut matcher, &[], b".gitattributes");
                    matcher.attributes_for_path(b"f.txt", filter_attribute_names(), false)
                }
                None => Vec::new(),
            };
            apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", input)
                .expect("smudge must succeed")
        };

        // --- attr=text (CRLF_TEXT_*): non-auto, the safety guard must not fire.
        // text + eol=crlf => CRLF_TEXT_CRLF: every naked LF gains CR.
        let attr_text_crlf: &[u8] = b"*.txt text eol=crlf";
        for cfg in [
            "[core]\n\tautocrlf = false\n\teol = lf\n",
            "[core]\n\tautocrlf = false\n\teol = crlf\n",
            "[core]\n\tautocrlf = true\n\teol = lf\n",
            "[core]\n\tautocrlf = input\n",
        ] {
            assert_eq!(
                smudge(cfg, Some(attr_text_crlf), LF),
                b"a\r\nb\r\nc\r\n",
                "text eol=crlf must add CR to naked LF (cfg={cfg:?})"
            );
            assert_eq!(
                smudge(cfg, Some(attr_text_crlf), CRLF_MIX_LF),
                b"a\r\nb\r\nc\r\n",
                "text eol=crlf must convert mixed content fully (cfg={cfg:?})"
            );
            assert_eq!(
                smudge(cfg, Some(attr_text_crlf), LF_MIX_CR),
                b"a\r\nb\rc\r\n",
                "text eol=crlf keeps the lone CR but adds CR to naked LF (cfg={cfg:?})"
            );
        }

        // --- attr=text, no eol attr: CRLF_TEXT, resolved by text_eol_is_crlf().
        // autocrlf=true wins over core.eol=lf (the precedence fix).
        assert_eq!(
            smudge(
                "[core]\n\tautocrlf = true\n\teol = lf\n",
                Some(b"*.txt text"),
                LF
            ),
            b"a\r\nb\r\nc\r\n",
            "autocrlf=true must override core.eol=lf for plain text attr"
        );
        // autocrlf unset, core.eol=crlf => CRLF.
        assert_eq!(
            smudge("[core]\n\teol = crlf\n", Some(b"*.txt text"), LF),
            b"a\r\nb\r\nc\r\n",
            "core.eol=crlf adds CR to naked LF for plain text attr"
        );
        // autocrlf unset, core.eol=lf (and native LF on this host) => no CR.
        assert_eq!(
            smudge("[core]\n\teol = lf\n", Some(b"*.txt text"), LF),
            LF,
            "core.eol=lf leaves naked LF untouched on smudge"
        );
        // text + autocrlf=input => CRLF_TEXT_INPUT: no CR on smudge.
        assert_eq!(
            smudge("[core]\n\tautocrlf = input\n", Some(b"*.txt text"), LF),
            LF,
            "autocrlf=input overrides core.eol; no CR on smudge"
        );

        // --- attr=text=auto (CRLF_AUTO_*): the safety guard DOES fire.
        // auto + autocrlf=true + naked-LF-only => convert.
        assert_eq!(
            smudge("[core]\n\tautocrlf = true\n", Some(b"*.txt text=auto"), LF),
            b"a\r\nb\r\nc\r\n",
            "text=auto converts a clean naked-LF file"
        );
        // auto + already has a CR/CRLF => leave untouched (irreversible guard).
        assert_eq!(
            smudge(
                "[core]\n\tautocrlf = true\n",
                Some(b"*.txt text=auto"),
                CRLF_MIX_LF
            ),
            CRLF_MIX_LF,
            "text=auto must not touch content that already has CRLF"
        );
        assert_eq!(
            smudge(
                "[core]\n\tautocrlf = true\n",
                Some(b"*.txt text=auto"),
                LF_MIX_CR
            ),
            LF_MIX_CR,
            "text=auto must not touch content that already has a lone CR"
        );

        // --- no attr, autocrlf=true => CRLF_AUTO_CRLF (auto guard applies).
        assert_eq!(
            smudge("[core]\n\tautocrlf = true\n\teol = lf\n", None, LF),
            b"a\r\nb\r\nc\r\n",
            "autocrlf=true (no attr) converts clean naked-LF and overrides core.eol=lf"
        );
        // --- no attr, autocrlf=false => CRLF_BINARY: never convert.
        assert_eq!(
            smudge("[core]\n\teol = crlf\n", None, LF),
            LF,
            "no attr + autocrlf=false leaves content untouched even with core.eol=crlf"
        );
        // --- -text (CRLF_BINARY): never convert regardless of config.
        assert_eq!(
            smudge("[core]\n\tautocrlf = true\n", Some(b"*.txt -text"), LF),
            LF,
            "-text is binary: never convert"
        );
    }

    /// Resolve attribute checks against an on-disk `.gitattributes` in `root`.
    fn attrs(root: &Path, path: &[u8]) -> Vec<AttributeCheck> {
        filter_attribute_checks(root, path).expect("test operation should succeed")
    }

    #[test]
    fn standard_attribute_matcher_matches_per_path_lookup() {
        let root = temp_root();
        fs::create_dir_all(root.join(".git").join("info")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src").join("nested")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"*.rs diff=rust\n")
            .expect("test operation should succeed");
        fs::write(
            root.join("src").join(".gitattributes"),
            b"*.rs diff=python\n",
        )
        .expect("test operation should succeed");
        fs::write(
            root.join(".git").join("info").join("attributes"),
            b"src/nested/*.rs diff=java\n",
        )
        .expect("test operation should succeed");

        let requested = vec![b"diff".to_vec()];
        let path = b"src/nested/file.rs";
        let per_path = standard_attributes_for_path(&root, path, &requested, false)
            .expect("test operation should succeed");
        let matcher = StandardAttributeMatcher::from_worktree_root(&root)
            .expect("test operation should succeed");
        assert_eq!(
            matcher.attributes_for_path(path, &requested, false),
            per_path
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn filter_attribute_lookup_reads_only_path_chain() {
        let root = temp_root();
        fs::create_dir_all(root.join(".git").join("info")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src").join("nested")).expect("test operation should succeed");
        fs::create_dir_all(root.join("sibling")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"*.txt text\n")
            .expect("test operation should succeed");
        fs::write(root.join("src").join(".gitattributes"), b"*.txt -text\n")
            .expect("test operation should succeed");
        fs::write(
            root.join("sibling").join(".gitattributes"),
            b"*.txt eol=crlf\n",
        )
        .expect("test operation should succeed");
        fs::write(
            root.join(".git").join("info").join("attributes"),
            b"src/nested/*.txt eol=lf\n",
        )
        .expect("test operation should succeed");

        let path = b"src/nested/file.txt";
        let full = standard_attributes_for_path(&root, path, filter_attribute_names(), false)
            .expect("test operation should succeed");
        assert_eq!(
            filter_attribute_checks(&root, path).expect("attribute checks should load"),
            full
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_attributes_matches_per_path_clean_filter() {
        let root = temp_root();
        fs::create_dir_all(root.join(".git").join("info")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"* text\n").expect("test operation should succeed");
        fs::write(
            root.join("src").join(".gitattributes"),
            b"*.txt text eol=lf\n",
        )
        .expect("test operation should succeed");
        let config = GitConfig::default();
        let attributes =
            WorktreeAttributes::from_worktree_root(&root).expect("test operation should succeed");
        for (path, input) in [
            (b"top.txt".as_slice(), b"a\r\nb\r\n".as_slice()),
            (b"src/a.txt".as_slice(), b"a\r\nb\r\n".as_slice()),
            (b"src/b.txt".as_slice(), b"x\r\ny\r\n".as_slice()),
        ] {
            let cached = attributes
                .apply_clean_filter(&config, path, input)
                .expect("cached clean filter");
            let per_path = apply_clean_filter(&root, root.join(".git"), &config, path, input)
                .expect("per-path clean filter");
            assert_eq!(
                cached,
                per_path,
                "cached clean filter must match per-path lookup for {}",
                String::from_utf8_lossy(path)
            );
        }
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn worktree_attributes_skips_unreadable_unrelated_directories() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        fs::create_dir_all(root.join(".git")).expect("test operation should succeed");
        fs::create_dir_all(root.join("ok")).expect("test operation should succeed");
        let secret = root.join("secret");
        fs::create_dir_all(&secret).expect("test operation should succeed");
        fs::write(secret.join("hidden.txt"), b"nope\n").expect("test operation should succeed");
        let mut permissions = fs::metadata(&secret)
            .expect("secret metadata")
            .permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&secret, permissions.clone()).expect("lock secret dir");

        let constructed = WorktreeAttributes::from_worktree_root(&root);
        permissions.set_mode(0o755);
        fs::set_permissions(&secret, permissions).expect("unlock secret dir");
        let attributes = constructed.expect("must not walk unrelated directories");
        let blob = attributes
            .apply_clean_filter(&GitConfig::default(), b"ok/file.txt", b"hello\n")
            .expect("path-chain lookup must ignore unreadable siblings");
        assert_eq!(blob, b"hello\n");

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn crlf_to_lf_collapses_only_pairs() {
        assert_eq!(
            convert_crlf_to_lf_cow(Cow::Borrowed(b"a\r\nb\r\n")).as_ref(),
            b"a\nb\n"
        );
        // A lone CR (no following LF) is preserved.
        assert_eq!(
            convert_crlf_to_lf_cow(Cow::Borrowed(b"a\rb")).as_ref(),
            b"a\rb"
        );
        // An already-LF stream is unchanged.
        assert!(matches!(
            convert_crlf_to_lf_cow(Cow::Borrowed(b"a\nb\n")),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn lf_to_crlf_does_not_double_convert() {
        assert_eq!(convert_lf_to_crlf(b"a\nb\n"), b"a\r\nb\r\n");
        // Existing CRLF is left intact (no extra CR added).
        assert_eq!(convert_lf_to_crlf(b"a\r\nb\r\n"), b"a\r\nb\r\n");
    }

    #[test]
    fn autocrlf_round_trip_clean_then_smudge() {
        // autocrlf=true: worktree CRLF -> blob LF on clean, blob LF -> worktree
        // CRLF on smudge.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let worktree = b"line1\r\nline2\r\n";
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"file.txt", worktree)
            .expect("test operation should succeed");
        assert_eq!(blob, b"line1\nline2\n", "clean must normalize CRLF to LF");
        let restored = apply_smudge_filter_with_attributes(&config, &checks, b"file.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            restored, worktree,
            "smudge must restore CRLF from the LF blob"
        );
    }

    #[test]
    fn working_tree_encoding_utf16_smudge_uses_platform_iconv_order() {
        let config = config_from("");
        let mut matcher = AttributeMatcher::default();
        read_attribute_patterns_from_bytes(
            b"*.utf16 text working-tree-encoding=utf-16",
            &mut matcher,
            &[],
            b".gitattributes",
        );
        let checks = matcher.attributes_for_path(b"test.utf16", filter_attribute_names(), false);

        let restored = apply_smudge_filter_with_attributes(&config, &checks, b"test.utf16", b"A\n")
            .expect("smudge must encode UTF-16");
        let expected =
            encode_utf16(b"A\n", ICONV_UTF_DEFAULT_LE, true).expect("valid UTF-8 encodes");

        assert_eq!(restored, expected);
    }

    #[test]
    fn working_tree_encoding_smudge_applies_eol_before_utf32() {
        let config = config_from("[core]\n\teol = crlf\n");
        let mut matcher = AttributeMatcher::default();
        read_attribute_patterns_from_bytes(
            b"*.utf32 text working-tree-encoding=utf-32",
            &mut matcher,
            &[],
            b".gitattributes",
        );
        let checks = matcher.attributes_for_path(b"eol.utf32", filter_attribute_names(), false);

        let restored =
            apply_smudge_filter_with_attributes(&config, &checks, b"eol.utf32", b"one\ntwo\n")
                .expect("smudge must encode UTF-32");
        let expected = encode_utf32(b"one\r\ntwo\r\n", ICONV_UTF_DEFAULT_LE, true)
            .expect("valid UTF-8 encodes");

        assert_eq!(restored, expected);
    }

    #[test]
    fn working_tree_encoding_shift_jis_clean_and_roundtrip_config() {
        let mut matcher = AttributeMatcher::default();
        read_attribute_patterns_from_bytes(
            b"*.shift text working-tree-encoding=SHIFT-JIS",
            &mut matcher,
            &[],
            b".gitattributes",
        );
        let checks =
            matcher.attributes_for_path(b"roundtrip.shift", filter_attribute_names(), false);
        let (shift_jis, _, had_errors) = encoding_rs::SHIFT_JIS.encode("hallo\n");
        assert!(!had_errors);

        let config = config_from("");
        assert!(should_check_roundtrip_encoding(&config, b"SHIFT-JIS"));
        let blob =
            apply_clean_filter_with_attributes(&config, &checks, b"roundtrip.shift", &shift_jis)
                .expect("SHIFT-JIS clean must decode to UTF-8");
        assert_eq!(blob, b"hallo\n");

        let disabled = config_from("[core]\n\tcheckRoundtripEncoding = garbage\n");
        assert!(!should_check_roundtrip_encoding(&disabled, b"SHIFT-JIS"));
    }

    #[test]
    fn working_tree_encoding_missing_bom_is_fatal_when_writing_object() {
        let config = config_from("");
        let mut matcher = AttributeMatcher::default();
        read_attribute_patterns_from_bytes(
            b"*.utf16 text working-tree-encoding=utf-16",
            &mut matcher,
            &[],
            b".gitattributes",
        );
        let checks =
            matcher.attributes_for_path(b"nonsense.utf16", filter_attribute_names(), false);

        let err = apply_clean_filter_cow_inner(
            &config,
            &checks,
            b"nonsense.utf16",
            b"\0a\0b\0c",
            ConvFlags::Off,
            SafeCrlfIndexBlob::None,
            true,
            false,
        )
        .expect_err("UTF-16 without a BOM must be rejected when writing an object");

        assert!(matches!(err, GitError::Exit(128)));
    }

    #[test]
    fn conv_flags_from_config_matches_git_defaults() {
        // Unset core.safecrlf defaults to WARN (git's global_conv_flags_eol).
        assert_eq!(ConvFlags::from_config(&config_from("")), ConvFlags::Warn);
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = warn\n")),
            ConvFlags::Warn
        );
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = WARN\n")),
            ConvFlags::Warn
        );
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = true\n")),
            ConvFlags::Die
        );
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = false\n")),
            ConvFlags::Off
        );
    }

    #[test]
    fn safecrlf_warn_does_not_change_clean_bytes() {
        // The warning is purely additive: byte output is identical whether
        // safecrlf is off or warn.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let worktree = b"a\nb\nc\n";
        let plain = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", worktree)
            .expect("clean");
        let warned = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            worktree,
            ConvFlags::Warn,
            SafeCrlfIndexBlob::None,
        )
        .expect("clean with safecrlf")
        .into_owned();
        assert_eq!(plain, warned, "safecrlf must not alter the cleaned bytes");
    }

    #[test]
    fn safecrlf_die_errors_on_lf_to_crlf_round_trip() {
        // autocrlf=true on a pure-LF file: checkout would add CRLF, so the
        // round-trip is irreversible and safecrlf=true dies (exit 128).
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let err = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\nb\n",
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect_err("die must error");
        assert!(matches!(err, GitError::Exit(128)));
    }

    #[test]
    fn safecrlf_die_errors_on_crlf_to_lf_round_trip() {
        // autocrlf=input on a CRLF file: clean strips CRLF and checkout never
        // restores it, so safecrlf=true dies.
        let config = config_from("[core]\n\tautocrlf = input\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let err = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\r\nb\r\n",
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect_err("die must error");
        assert!(matches!(err, GitError::Exit(128)));
    }

    #[test]
    fn safecrlf_reversible_round_trip_does_not_warn_or_die() {
        // A CRLF file under autocrlf=true survives the round trip (clean to LF,
        // smudge back to CRLF), so even safecrlf=true is silent.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let out = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\r\nb\r\n",
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect("reversible round trip must not die");
        assert_eq!(out.as_ref(), b"a\nb\n");
    }

    #[test]
    fn safecrlf_binary_content_is_silent() {
        // autocrlf=true with NUL-containing (binary) content: no conversion and
        // no warning/die, mirroring git's early-return in crlf_to_git.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let body: &[u8] = b"a\nb\0c\n";
        let out = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.bin",
            body,
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect("binary content must not die");
        assert_eq!(out.as_ref(), body, "binary content is never converted");
    }

    #[test]
    fn safecrlf_off_is_silent_even_on_irreversible_round_trip() {
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let out = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\nb\n",
            ConvFlags::Off,
            SafeCrlfIndexBlob::None,
        )
        .expect("safecrlf=off never errors");
        // autocrlf=true does not convert on clean (only smudge), so bytes pass through.
        assert_eq!(out.as_ref(), b"a\nb\n");
    }

    #[test]
    fn autocrlf_input_normalizes_on_clean_but_not_smudge() {
        // autocrlf=input: clean normalizes to LF, smudge leaves LF as-is.
        let config = config_from("[core]\n\tautocrlf = input\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"file.txt", b"a\r\nb\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"a\nb\n");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"file.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            smudged, b"a\nb\n",
            "input mode must not add carriage returns"
        );
    }

    #[test]
    fn eol_crlf_attribute_drives_conversion_without_config() {
        // No core.autocrlf; the `eol=crlf` attribute alone forces conversion.
        let config = config_from("");
        let checks = vec![AttributeCheck {
            attribute: b"eol".to_vec(),
            state: Some(AttributeState::Value(b"crlf".to_vec())),
        }];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"a.txt", b"x\r\ny\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"x\ny\n");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"a.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(smudged, b"x\r\ny\r\n");
    }

    #[test]
    fn binary_attribute_disables_eol_conversion() {
        // `-text` (binary) must leave CRLF/NUL content untouched in both
        // directions even when autocrlf=true.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"text".to_vec(),
            state: Some(AttributeState::Unset),
        }];
        let content = b"\x00\x01\r\n\x02\r\n".to_vec();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"data.bin", &content)
            .expect("test operation should succeed");
        assert_eq!(blob, content, "binary file must not be CRLF-normalized");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"data.bin", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            smudged, content,
            "binary file must not gain carriage returns"
        );
    }

    #[test]
    fn autocrlf_auto_skips_binary_looking_content() {
        // text=auto (via autocrlf) must not convert content that contains NUL.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let content = b"a\r\n\x00b\r\n".to_vec();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f", &content)
            .expect("test operation should succeed");
        assert_eq!(blob, content, "binary-looking content stays untouched");
    }

    #[test]
    fn autocrlf_via_add_and_checkout_round_trips() {
        // End-to-end: a CRLF worktree file is stored as an LF blob by the
        // filtered add path, and restored as CRLF by the filtered checkout.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let config = config_from("[core]\n\tautocrlf = true\n");

        fs::write(root.join("crlf.txt"), b"alpha\r\nbeta\r\n")
            .expect("test operation should succeed");
        add_paths_to_index_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("crlf.txt")],
            &config,
        )
        .expect("test operation should succeed");

        // The stored blob must be LF-normalized.
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"crlf.txt");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let blob = odb
            .read_object(&entry.oid)
            .expect("test operation should succeed");
        assert_eq!(blob.body, b"alpha\nbeta\n");

        // Commit and point HEAD at it, then re-checkout with smudge filtering.
        let tree = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author T <t@e> 0 +0000\ncommitter T <t@e> 0 +0000\n\nm\n");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let commit = odb
            .write_object(EncodedObject::new(ObjectType::Commit, body))
            .expect("test operation should succeed");
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Direct(commit),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        // Make the worktree match the committed (LF) blob so the tree is clean
        // for checkout; `short_status`/`worktree_entries` compare by content
        // hash and are not filter-aware. Checkout will then smudge it to CRLF.
        fs::write(root.join("crlf.txt"), b"alpha\nbeta\n").expect("test operation should succeed");
        checkout_detached_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &commit,
            b"T <t@e> 0 +0000".to_vec(),
            b"co".to_vec(),
            &config,
        )
        .expect("test operation should succeed");
        assert_eq!(
            fs::read(root.join("crlf.txt")).expect("test operation should succeed"),
            b"alpha\r\nbeta\r\n",
            "checkout must restore CRLF line endings"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn normal_add_preserves_auto_crlf_index_blob_until_renormalize() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("create object directory");
        let path = PathBuf::from("legacy.txt");
        let crlf = b"alpha\r\nbeta\r\n";
        fs::write(root.join(&path), crlf).expect("write CRLF worktree file");

        // Seed a legacy CRLF blob without conversion. A subsequent normal add
        // under auto text must preserve it (git's safer-autocrlf rule).
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            std::slice::from_ref(&path),
        )
        .expect("seed unfiltered index entry");
        let config_text = "[core]\n\tautocrlf = true\n\tsafecrlf = false\n";
        fs::write(git_dir.join("config"), config_text).expect("write repository config");
        let config = config_from(config_text);
        update_index_paths_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            std::slice::from_ref(&path),
            UpdateIndexOptions {
                add: true,
                remove: false,
                force_remove: false,
                chmod: None,
                info_only: false,
                ignore_skip_worktree_entries: false,
                allow_skip_worktree_entries: false,
            },
            &config,
        )
        .expect("normal filtered add");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"legacy.txt");
        assert_eq!(odb.read_object(&entry.oid).expect("read blob").body, crlf);
        assert_eq!(
            worktree_entry_state(
                &root,
                &git_dir,
                ObjectFormat::Sha1,
                &path,
                &entry.oid,
                entry.mode,
                None,
            )
            .expect("compare legacy CRLF worktree file"),
            WorktreeEntryState::Clean,
        );

        renormalize_index_paths_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            std::slice::from_ref(&path),
            &config,
        )
        .expect("renormalize tracked path");
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"legacy.txt");
        assert_eq!(
            odb.read_object(&entry.oid)
                .expect("read normalized blob")
                .body,
            b"alpha\nbeta\n"
        );

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn driver_filter_clean_and_smudge_transform_both_directions() {
        // filter=case: clean upper-cases (worktree -> blob), smudge lower-cases
        // (blob -> worktree).
        let config =
            config_from("[filter \"case\"]\n\tclean = tr a-z A-Z\n\tsmudge = tr A-Z a-z\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"case".to_vec())),
        }];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", b"Hello World")
            .expect("test operation should succeed");
        assert_eq!(blob, b"HELLO WORLD", "clean driver must upper-case");
        let worktree =
            apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", b"HELLO WORLD")
                .expect("test operation should succeed");
        assert_eq!(worktree, b"hello world", "smudge driver must lower-case");
    }

    #[test]
    fn driver_filter_resolved_from_gitattributes_file() {
        // The filter name is read from a real `.gitattributes`, the commands from
        // config; exercises the public worktree-rooted entry points.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"*.dat filter=rot\n")
            .expect("test operation should succeed");
        let config =
            config_from("[filter \"rot\"]\n\tclean = sed s/a/b/g\n\tsmudge = sed s/b/a/g\n");
        // Clean reads attributes from the live worktree `.gitattributes`.
        let blob = apply_clean_filter(&root, &git_dir, &config, b"x.dat", b"banana")
            .expect("test operation should succeed");
        assert_eq!(blob, b"bbnbnb");
        // Smudge reads attributes from the index (the worktree file may not
        // exist yet during checkout), so stage `.gitattributes` first.
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from(".gitattributes")],
        )
        .expect("test operation should succeed");
        let smudged = apply_smudge_filter(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &config,
            b"x.dat",
            &blob,
        )
        .expect("test operation should succeed");
        // sed s/b/a/g is not a perfect inverse, but verifies the smudge command
        // ran on the blob bytes.
        assert_eq!(smudged, b"aanana");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn required_filter_failure_is_fatal() {
        // A required filter whose command fails must surface an error.
        let config = config_from("[filter \"boom\"]\n\tclean = false\n\trequired = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"boom".to_vec())),
        }];
        let err = apply_clean_filter_with_attributes(&config, &checks, b"f", b"data")
            .expect_err("required filter failure must error");
        assert!(matches!(err, GitError::Command(_)), "got {err:?}");
    }

    #[test]
    fn required_filter_missing_command_is_fatal() {
        // required=true but no clean command for this direction is also fatal.
        let config = config_from("[filter \"need\"]\n\tsmudge = cat\n\trequired = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"need".to_vec())),
        }];
        let err = apply_clean_filter_with_attributes(&config, &checks, b"f", b"data")
            .expect_err("required filter without a clean command must error");
        assert!(matches!(err, GitError::Exit(128)), "got {err:?}");
    }

    #[test]
    fn non_required_filter_failure_passes_through() {
        // A non-required filter that fails must pass the content through
        // unchanged rather than erroring.
        let config = config_from("[filter \"opt\"]\n\tclean = false\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"opt".to_vec())),
        }];
        let out = apply_clean_filter_with_attributes(&config, &checks, b"f", b"keepme")
            .expect("test operation should succeed");
        assert_eq!(
            out, b"keepme",
            "optional filter failure passes content through"
        );
    }

    #[test]
    fn filter_with_no_command_is_noop() {
        // filter=name with no configured commands and not required is ignored.
        let config = config_from("");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"ghost".to_vec())),
        }];
        let out = apply_clean_filter_with_attributes(&config, &checks, b"f", b"unchanged")
            .expect("test operation should succeed");
        assert_eq!(out, b"unchanged");
    }

    #[test]
    fn driver_and_eol_compose_on_clean_and_smudge() {
        // filter=case + autocrlf=true: clean runs the driver then CRLF->LF;
        // smudge runs LF->CRLF then the driver.
        let config = config_from(
            "[core]\n\tautocrlf = true\n[filter \"case\"]\n\tclean = tr a-z A-Z\n\tsmudge = tr A-Z a-z\n",
        );
        let checks = vec![
            AttributeCheck {
                attribute: b"filter".to_vec(),
                state: Some(AttributeState::Value(b"case".to_vec())),
            },
            AttributeCheck {
                attribute: b"text".to_vec(),
                state: Some(AttributeState::Set),
            },
        ];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", b"ab\r\ncd\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"AB\nCD\n", "clean: upper-case then CRLF->LF");
        let worktree = apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            worktree, b"ab\r\ncd\r\n",
            "smudge: LF->CRLF then lower-case"
        );
    }

    #[test]
    fn attrs_helper_reads_filter_from_disk() {
        let root = temp_root();
        fs::write(root.join(".gitattributes"), b"*.txt text\n*.bin -text\n")
            .expect("test operation should succeed");
        let text = attrs(&root, b"a.txt");
        assert!(
            text.iter()
                .any(|c| c.attribute == b"text" && c.state == Some(AttributeState::Set))
        );
        let bin = attrs(&root, b"a.bin");
        assert!(
            bin.iter()
                .any(|c| c.attribute == b"text" && c.state == Some(AttributeState::Unset))
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    /// Builds a stat cache holding a single stage-0 entry whose size+mtime match
    /// `file`'s real metadata, with the index-file mtime placed strictly after
    /// the entry mtime so the entry reads as non-racy by default. The entry's oid
    /// is `oid` and its mode is `mode`.
    fn stat_cache_for(file: &Path, oid: ObjectId, mode: u32) -> (IndexStatCache, IndexEntry) {
        let metadata = fs::metadata(file).expect("test operation should succeed");
        let mut entry = index_entry_from_metadata(b"f.txt".to_vec(), oid, &metadata);
        entry.mode = mode;
        let index_mtime = Some((u64::from(entry.mtime_seconds) + 10, 0));
        let mut entries = HashMap::new();
        entries.insert(entry.path.as_bytes().to_vec(), entry.clone());
        (
            IndexStatCache {
                entries,
                index_mtime,
            },
            entry,
        )
    }

    #[test]
    fn reuse_tracked_entry_only_reuses_clean_non_racy_match() {
        let root = temp_root();
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        let file = root.join("f.txt");
        let metadata = fs::metadata(&file).expect("test operation should succeed");
        let real_mode = file_mode(&metadata);
        let oid = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec())
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        // Clean, non-racy, matching stat + mode -> reuse the cached oid.
        let (cache, _) = stat_cache_for(&file, oid, real_mode);
        let reused = cache.reuse_tracked_entry(b"f.txt", &metadata);
        assert_eq!(
            reused,
            Some(TrackedEntry {
                mode: real_mode,
                oid,
            }),
            "a clean non-racy stat+mode match must reuse the staged oid"
        );

        // No stage-0 entry for the path -> must hash.
        assert_eq!(
            cache.reuse_tracked_entry(b"other.txt", &metadata),
            None,
            "a path with no cached entry must fall through to hashing"
        );

        // Size differs from the file -> must hash.
        let (mut size_cache, mut shrunk) = stat_cache_for(&file, oid, real_mode);
        shrunk.size = shrunk.size.saturating_sub(1);
        size_cache.entries.insert(shrunk.path.to_vec(), shrunk);
        assert_eq!(
            size_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a size mismatch must fall through to hashing"
        );

        // Mode differs (e.g. a chmod that did not move mtime) -> must hash.
        let (mode_cache, _) = stat_cache_for(&file, oid, 0o100755);
        assert_eq!(
            mode_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a mode mismatch must fall through to hashing"
        );
        assert_eq!(
            mode_cache.reuse_tracked_entry_with_filemode(b"f.txt", &metadata, false),
            Some(TrackedEntry {
                mode: 0o100755,
                oid,
            }),
            "an untrusted executable bit must retain the cached oid"
        );

        // Racily clean (index mtime not strictly after the entry mtime) -> hash.
        let (mut racy_cache, entry) = stat_cache_for(&file, oid, real_mode);
        racy_cache.index_mtime = Some((
            u64::from(entry.mtime_seconds),
            u64::from(entry.mtime_nanoseconds),
        ));
        assert_eq!(
            racy_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a racily-clean entry must always be re-hashed"
        );

        // Unknown index mtime is treated as racy -> hash.
        let (mut unknown_cache, _) = stat_cache_for(
            &file,
            EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec())
                .object_id(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
            real_mode,
        );
        unknown_cache.index_mtime = None;
        assert_eq!(
            unknown_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "an unknown index mtime must be treated conservatively as racy"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn index_stat_probe_cache_serves_many_paths_from_one_index_parse() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("a.txt"), b"alpha\n").expect("test operation should succeed");
        fs::write(root.join("b.txt"), b"bravo\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["a.txt", "b.txt"]);

        let cache = IndexStatProbeCache::from_repository_index(&git_dir, ObjectFormat::Sha1)
            .expect("probe cache");
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_git_path(b"a.txt"));
        assert!(cache.contains_git_path(b"b.txt"));
        let a = cache.probe_for_git_path(b"a.txt").expect("a probe");
        let b = cache.probe_for_git_path(b"b.txt").expect("b probe");
        assert_eq!(a.entry().path, b"a.txt");
        assert_eq!(b.entry().path, b"b.txt");
        assert_eq!(a.index_mtime(), cache.index_mtime());
        assert_eq!(b.index_mtime(), cache.index_mtime());
        assert!(
            cache.probe_for_git_path(b"missing.txt").is_none(),
            "missing paths should not allocate probes"
        );

        let one_shot =
            IndexStatProbe::from_repository_index(&git_dir, ObjectFormat::Sha1, b"a.txt")
                .expect("legacy one-shot probe")
                .expect("a probe");
        assert_eq!(one_shot.entry().path, b"a.txt");
        assert_eq!(one_shot.index_mtime(), cache.index_mtime());

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_detects_same_length_content_change() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"aaaa\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        // Overwrite with the SAME byte length but different content. Right after
        // staging the entry is racily clean (index mtime >= entry mtime), so the
        // stat shortcut must not be trusted and the change must surface as M.
        fs::write(root.join("f.txt"), b"bbbb\n").expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(
            status
                .iter()
                .map(ShortStatusEntry::line)
                .collect::<Vec<_>>(),
            vec![" M f.txt"],
            "a same-length content change must be reported modified"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_clean_after_byte_identical_rewrite() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        // Rewrite with byte-identical content; the mtime moves so the stat
        // shortcut declines to reuse and the fallback hash proves it clean.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            status.is_empty(),
            "a byte-identical rewrite must be clean via the fallback hash, got {status:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_trusts_stat_cache_and_skips_rehash() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);

        // Plant a BOGUS oid in the stage-0 entry while preserving its size+mtime,
        // so a real re-hash of the (unchanged) worktree file would NOT match it.
        let index_path = repository_index_path(&git_dir);
        let mut index = read_index(&git_dir);
        let bogus = ObjectId::from_hex(ObjectFormat::Sha1, &"0".repeat(40))
            .expect("test operation should succeed");
        let real_oid = index_entry_for(&index, b"f.txt").oid;
        assert_ne!(
            real_oid, bogus,
            "fixture oid should differ from the bogus oid"
        );
        index
            .entries
            .iter_mut()
            .find(|entry| entry.path == b"f.txt")
            .expect("test operation should succeed")
            .oid = bogus.clone();
        fs::write(
            &index_path,
            index
                .write(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        // Make the index file STRICTLY newer than the entry mtime (non-racy) by
        // waiting past one-second filesystem granularity and rewriting it, so the
        // racy-clean guard does not force a re-hash.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            &index_path,
            fs::read(&index_path).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        // The file is unchanged on disk, so a trusted stat reuses the bogus index
        // oid for the worktree entry: worktree-oid == index-oid == bogus, so the
        // WORKTREE column is clean. Had status re-hashed the file, the real oid
        // would differ from the bogus index oid and the worktree column would be
        // 'M'. (The index-vs-HEAD column is 'M' because we corrupted the index
        // oid away from HEAD; that is expected and not what this test asserts.)
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let entry = status
            .iter()
            .find(|entry| entry.path == b"f.txt")
            .expect("f.txt should appear (its index oid now differs from HEAD)");
        assert_eq!(
            entry.worktree, b' ',
            "non-racy stat match must trust the cached oid (no re-hash); worktree column was {}",
            entry.worktree as char
        );
        assert_eq!(
            entry.index_oid.as_ref(),
            Some(&bogus),
            "the worktree entry must have reused the planted bogus index oid, not the real hash"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_detects_same_size_content_change() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"aaaa\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"f.txt").clone();
        let probe = IndexStatProbe::from_index_entry_and_index_path(
            entry.clone(),
            repository_index_path(&git_dir),
        );

        fs::write(root.join("f.txt"), b"bbbb\n").expect("test operation should succeed");
        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            Some(&probe),
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Modified);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_reports_deleted_for_missing_and_parent_not_directory() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("dir")).expect("test operation should succeed");
        fs::write(root.join("dir").join("f.txt"), b"hello\n")
            .expect("test operation should succeed");
        build_commit(&root, &git_dir, &["dir/f.txt"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"dir/f.txt").clone();

        fs::remove_file(root.join("dir").join("f.txt")).expect("test operation should succeed");
        let missing = worktree_entry_state_by_git_path(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            b"dir/f.txt",
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(missing, WorktreeEntryState::Deleted);

        fs::remove_dir(root.join("dir")).expect("test operation should succeed");
        fs::write(root.join("dir"), b"not a directory").expect("test operation should succeed");
        let parent_not_directory = worktree_entry_state_by_git_path(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            b"dir/f.txt",
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(parent_not_directory, WorktreeEntryState::Deleted);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_trusts_clean_non_racy_probe() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index_path = repository_index_path(&git_dir);
        let mut index = read_index(&git_dir);
        let bogus = ObjectId::from_hex(ObjectFormat::Sha1, &"1".repeat(40))
            .expect("test operation should succeed");
        index
            .entries
            .iter_mut()
            .find(|entry| entry.path == b"f.txt")
            .expect("test operation should succeed")
            .oid = bogus;
        fs::write(
            &index_path,
            index
                .write(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            &index_path,
            fs::read(&index_path).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"f.txt").clone();
        let probe = IndexStatProbe::from_index_entry_and_index_path(
            entry.clone(),
            repository_index_path(&git_dir),
        );

        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            Some(&probe),
        )
        .expect("test operation should succeed");
        assert_eq!(
            state,
            WorktreeEntryState::Clean,
            "a non-racy stat match must be enough to prove this path clean"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_rehashes_racy_probe() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index = read_index(&git_dir);
        let mut entry = index_entry_for(&index, b"f.txt").clone();
        entry.oid = ObjectId::from_hex(ObjectFormat::Sha1, &"2".repeat(40))
            .expect("test operation should succeed");
        let probe = IndexStatProbe::from_index_entry(
            entry.clone(),
            Some((
                u64::from(entry.mtime_seconds),
                u64::from(entry.mtime_nanoseconds),
            )),
        );

        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            Some(&probe),
        )
        .expect("test operation should succeed");
        assert_eq!(
            state,
            WorktreeEntryState::Modified,
            "a racily-clean stat match must fall through to hashing"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn worktree_entry_state_detects_chmod_only_change() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"f.txt").clone();

        let file = root.join("f.txt");
        let mut permissions = fs::metadata(&file)
            .expect("test operation should succeed")
            .permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(&file, permissions).expect("test operation should succeed");
        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Modified);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn worktree_entry_state_detects_symlink_target_change() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        symlink("one", root.join("link")).expect("test operation should succeed");
        build_commit(&root, &git_dir, &["link"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"link").clone();

        fs::remove_file(root.join("link")).expect("test operation should succeed");
        symlink("two", root.join("link")).expect("test operation should succeed");
        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("link"),
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Modified);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_treats_present_unpopulated_gitlink_directory_as_clean() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("submodule")).expect("test operation should succeed");
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, &"3".repeat(40))
            .expect("test operation should succeed");

        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("submodule"),
            &oid,
            sley_index::GITLINK_MODE,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Clean);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_empty_on_unborn_repository() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            status.is_empty(),
            "an unborn repository with an empty worktree must be clean, got {status:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn untracked_paths_skips_embedded_git_internals() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let nested = root.join("not-a-submodule");
        fs::create_dir_all(nested.join(".git")).expect("test operation should succeed");
        fs::write(nested.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        fs::write(nested.join("file.txt"), b"inside\n").expect("test operation should succeed");
        let paths = untracked_paths(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            paths.iter().any(|path| path == b"not-a-submodule/"),
            "embedded repository directory should be listed, got {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with(b"not-a-submodule/.git")),
            "embedded .git internals must not be listed, got {paths:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn untracked_paths_lists_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        fs::write(root.join("target.txt"), b"target\n").expect("test operation should succeed");
        symlink(root.join("target.txt"), root.join("path1")).expect("create symlink");
        let paths = untracked_paths(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            paths.contains(&b"path1".to_vec()),
            "untracked symlink must be listed, got {paths:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }
}
